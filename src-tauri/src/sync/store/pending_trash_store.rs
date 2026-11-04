// `contains` is exercised by engine.rs's own #[cfg(test)] assertions (same-crate,
// so it's a real caller under `cargo test --lib`) but has no caller in the
// non-test-compiled library a separate integration test binary links against —
// same situation `outbox_store.rs` documents for its own status-only methods.
#![allow(dead_code)]

//! Durable retry-intent set for tombstone-trash failures (SYN-B8, audit
//! follow-up 2026-07-13).
//!
//! When a note's DocStore tombstone commits successfully but the disk trash
//! itself fails (read-only roost, permission denied, transient I/O — see
//! `reconcile_doc_to_roost`'s delete branch and `mark_note_deleted` in
//! `peer.rs`/`fanout.rs`), the note is enrolled here so the reconnect
//! supervisor's `SyncEngine::retry_pending_trashes` keeps re-attempting the
//! trash until it succeeds or the tombstone is cleared.
//!
//! Before this store existed the set lived in memory only. If the app
//! restarted before a retry succeeded, the enrollment was lost — and because
//! the startup scan deliberately SKIPS tombstoned-but-present notes (to avoid
//! auto-un-deleting a file the user recreated), nothing else would ever
//! retry it: a permanent DocStore-tombstone-vs-on-disk-file divergence that
//! only an explicit Restore or a fresh adopt from a peer could clear.
//!
//! # Why persisting this is safe (unlike blindly re-scanning on startup)
//!
//! `retry_pending_trashes` ALWAYS re-checks `is_deleted()` under the note's
//! lock immediately before attempting the trash — whether the enrollment
//! came from this session or was loaded from a previous one. So replaying a
//! stale enrollment across a restart can never wrongly delete a file the
//! user has since restored: if the tombstone was cleared in the meantime,
//! the retry observes that and abandons (removes) the entry instead of
//! trashing. Persistence only makes the RETRY INTENT durable; it does not
//! change what gets trashed or when — that safety property is unchanged.
//!
//! # Storage
//!
//! A single `pending_trash.bin` file (postcard, versioned envelope),
//! rewritten atomically (temp + fsync + rename) on every mutation. Same
//! rationale as [`OutboxStore`](super::outbox_store::OutboxStore): the set is
//! bounded by genuinely-failed deletes, which should be rare in practice, so
//! full-file rewrites are fine.

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::sync::error::StoreError;
use crate::sync::protocol::NotePath;

const PENDING_TRASH_FILE_VERSION: u16 = 1;

#[derive(Debug, Serialize, Deserialize)]
struct Envelope {
    version: u16,
    paths: Vec<String>,
}

/// In-memory retry set backed by `pending_trash.bin`. Loads on construction;
/// every mutation rewrites the whole file atomically. `path: None` (test-only
/// via [`Self::in_memory`]) makes every flush a no-op, so unit tests that
/// construct a bare `SyncCtx` don't need a real state directory.
#[derive(Debug)]
pub struct PendingTrashStore {
    path: Option<PathBuf>,
    paths: BTreeSet<String>,
    /// The in-memory set differs from the last successfully persisted
    /// snapshot. Kept true after a failed flush so the supervisor can retry
    /// persistence even when the same note is already enrolled.
    dirty: bool,
}

impl PendingTrashStore {
    /// Load from the sync state dir, or start empty if the file doesn't
    /// exist yet (fresh install / never had a failed trash).
    pub fn load(state_dir: &std::path::Path) -> Result<Self, StoreError> {
        let path = state_dir.join("pending_trash.bin");
        if !path.exists() {
            return Ok(Self {
                path: Some(path),
                paths: BTreeSet::new(),
                dirty: false,
            });
        }
        let bytes = fs::read(&path)?;
        let env: Envelope = postcard::from_bytes(&bytes)
            .map_err(|e| StoreError::Decode(format!("pending_trash file: {e}")))?;
        if env.version != PENDING_TRASH_FILE_VERSION {
            return Err(StoreError::Decode(format!(
                "pending_trash file version {} unsupported (expected {PENDING_TRASH_FILE_VERSION})",
                env.version
            )));
        }
        Ok(Self {
            path: Some(path),
            paths: env.paths.into_iter().collect(),
            dirty: false,
        })
    }

    /// Test-only: an ephemeral store with no backing file. Every mutation
    /// still updates the in-memory set; `flush` is just a no-op. Lets a bare
    /// `SyncCtx`/engine test fixture construct one without a real state dir.
    #[cfg(test)]
    pub fn in_memory() -> Self {
        Self {
            path: None,
            paths: BTreeSet::new(),
            dirty: false,
        }
    }

    pub fn contains(&self, note: &NotePath) -> bool {
        self.paths.contains(&note.0)
    }

    pub fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }

    /// Every currently-enrolled path, for the retry sweep to iterate.
    pub fn snapshot(&self) -> Vec<NotePath> {
        self.paths.iter().cloned().map(NotePath).collect()
    }

    /// Enroll `note` for retry. If a prior persistence attempt failed, an
    /// idempotent duplicate insert retries that pending flush instead of
    /// silently treating the in-memory membership as durable.
    pub fn insert(&mut self, note: &NotePath) -> Result<(), StoreError> {
        if self.paths.insert(note.0.clone()) {
            self.dirty = true;
        }
        self.flush_pending()
    }

    /// Drop `note` from the retry set — trashed successfully, confirmed
    /// already gone, or abandoned because it's no longer tombstoned. No-op
    /// if not present.
    pub fn remove(&mut self, note: &NotePath) -> Result<(), StoreError> {
        if self.paths.remove(&note.0) {
            self.dirty = true;
        }
        self.flush_pending()
    }

    /// Drop every entry. Called when the docstore is wiped (`reset_docstore`)
    /// — the tombstones these entries reference are gone, so retrying them
    /// would operate on stale state.
    pub fn clear(&mut self) -> Result<(), StoreError> {
        if !self.paths.is_empty() {
            self.paths.clear();
            self.dirty = true;
        }
        self.flush_pending()
    }

    /// Retry any state-file write that previously failed. The reconnect
    /// supervisor calls this every tick before processing entries, so a
    /// transient app-private storage failure cannot leave an enrollment only
    /// in memory until process exit.
    pub fn flush_pending(&mut self) -> Result<(), StoreError> {
        if !self.dirty {
            return Ok(());
        }
        self.flush()?;
        self.dirty = false;
        Ok(())
    }

    fn flush(&self) -> Result<(), StoreError> {
        let Some(path) = &self.path else {
            return Ok(()); // in-memory test store — nothing to persist
        };
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let env = Envelope {
            version: PENDING_TRASH_FILE_VERSION,
            paths: self.paths.iter().cloned().collect(),
        };
        let bytes = postcard::to_allocvec(&env).map_err(|e| StoreError::Encode(e.to_string()))?;
        let tmp = path.with_extension("bin.tmp");
        {
            use std::io::Write;
            let mut f = fs::File::create(&tmp)?;
            f.write_all(&bytes)?;
            f.sync_all()?;
        }
        fs::rename(&tmp, path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn note(p: &str) -> NotePath {
        NotePath(p.into())
    }

    #[test]
    fn load_on_fresh_dir_is_empty() {
        let dir = tempdir().unwrap();
        let store = PendingTrashStore::load(dir.path()).unwrap();
        assert!(store.is_empty());
    }

    #[test]
    fn insert_persists_and_reloads() {
        let dir = tempdir().unwrap();
        {
            let mut store = PendingTrashStore::load(dir.path()).unwrap();
            store.insert(&note("a.md")).unwrap();
            store.insert(&note("dir/b.md")).unwrap();
        }
        // Fresh load from the same dir must see both enrollments — this is
        // the actual durability guarantee the SYN-B8 gap was missing.
        let reloaded = PendingTrashStore::load(dir.path()).unwrap();
        assert!(reloaded.contains(&note("a.md")));
        assert!(reloaded.contains(&note("dir/b.md")));
        assert_eq!(reloaded.snapshot().len(), 2);
    }

    #[test]
    fn remove_persists_across_reload() {
        let dir = tempdir().unwrap();
        {
            let mut store = PendingTrashStore::load(dir.path()).unwrap();
            store.insert(&note("a.md")).unwrap();
            store.insert(&note("b.md")).unwrap();
            store.remove(&note("a.md")).unwrap();
        }
        let reloaded = PendingTrashStore::load(dir.path()).unwrap();
        assert!(!reloaded.contains(&note("a.md")));
        assert!(reloaded.contains(&note("b.md")));
    }

    #[test]
    fn clear_empties_and_persists() {
        let dir = tempdir().unwrap();
        {
            let mut store = PendingTrashStore::load(dir.path()).unwrap();
            store.insert(&note("a.md")).unwrap();
            store.clear().unwrap();
        }
        let reloaded = PendingTrashStore::load(dir.path()).unwrap();
        assert!(reloaded.is_empty());
    }

    #[test]
    fn in_memory_store_never_touches_disk() {
        let mut store = PendingTrashStore::in_memory();
        store.insert(&note("a.md")).unwrap();
        assert!(store.contains(&note("a.md")));
        store.remove(&note("a.md")).unwrap();
        assert!(store.is_empty());
    }

    #[test]
    fn failed_insert_stays_dirty_and_later_flush_persists() {
        let dir = tempdir().unwrap();
        // Make the would-be state directory a regular file so create_dir_all
        // deterministically fails on every platform (permissions-based tests
        // are unreliable when the runner has elevated privileges).
        let blocked_state_dir = dir.path().join("blocked-state");
        fs::write(&blocked_state_dir, b"not a directory").unwrap();
        let path = blocked_state_dir.join("pending_trash.bin");
        let target = note("retry.md");
        let mut store = PendingTrashStore {
            path: Some(path),
            paths: BTreeSet::new(),
            dirty: false,
        };

        assert!(store.insert(&target).is_err());
        assert!(store.contains(&target), "retry work must remain in memory");
        assert!(store.dirty, "a failed flush must remain marked for retry");

        // Storage recovers. The same operation the supervisor calls must now
        // persist the already-enrolled entry without requiring another
        // reconcile event or a new insertion.
        fs::remove_file(&blocked_state_dir).unwrap();
        fs::create_dir_all(&blocked_state_dir).unwrap();
        store.flush_pending().unwrap();
        assert!(!store.dirty);

        let reloaded = PendingTrashStore::load(&blocked_state_dir).unwrap();
        assert!(reloaded.contains(&target));
    }

    #[test]
    fn rejects_unsupported_file_version() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("pending_trash.bin");
        let bad = Envelope {
            version: PENDING_TRASH_FILE_VERSION + 1,
            paths: vec!["x.md".into()],
        };
        fs::write(&path, postcard::to_allocvec(&bad).unwrap()).unwrap();
        let err = PendingTrashStore::load(dir.path()).unwrap_err();
        assert!(matches!(err, StoreError::Decode(_)));
    }
}
