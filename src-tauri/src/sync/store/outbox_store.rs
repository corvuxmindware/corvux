// len + is_empty exposed for status/observability callers
#![allow(dead_code)]

//! Durable client outbox for store-and-forward delivery.
//!
//! When a local change can't reach a known peer right now — the peer is
//! offline and the queue push failed (network down, queue server
//! unreachable), or a live peer's channel closed mid-send — we record a
//! delivery *intent* here so the change isn't lost until the next time the
//! user happens to edit that same note.
//!
//! # Why intent-only (no payload)
//!
//! We deliberately store only `(peer_device, note_path)` plus retry
//! metadata — **never** the encrypted payload. The canonical Automerge doc
//! already lives durably in the [`DocStore`](super::doc_store::DocStore);
//! the queue push regenerates the ciphertext from it at drain time. Copying
//! payloads here would double disk usage, risk pushing a stale snapshot when
//! a newer edit exists, and (because a throwaway doc has a different actor
//! history) risk the "two independent docs welded into one" CRDT divergence.
//! Regenerating from the single source of truth sidesteps all three.
//!
//! This depends on the DocStore being authoritative on *every* local edit —
//! see `SyncEngine::apply_local_change`, which is the single writer.
//!
//! # Storage
//!
//! A single `outbox.bin` file (postcard-encoded, versioned envelope),
//! rewritten atomically (temp + rename) on every mutation. The set is
//! bounded by `offline-peers × notes-edited-while-offline`, which is tiny in
//! practice, so full-file rewrites are fine — same rationale as
//! [`PeerStore`](super::peer_store::PeerStore). No secrets live here, so
//! unlike the peer store there's no keychain indirection.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::sync::error::StoreError;
use crate::sync::identity::DeviceId;
use crate::sync::protocol::NotePath;

use super::outbox_file;

/// File format version. Bump on any incompatible on-disk layout change.
const OUTBOX_FILE_VERSION: u16 = 1;

/// Per-entry retry backoff schedule (milliseconds), indexed by the number
/// of failed attempts so far. Exponential-ish, capped at 1h: a peer that's
/// been offline for hours doesn't need sub-minute retries, but a transient
/// blip recovers quickly. Entry 0 (just enqueued, not yet tried) is due
/// immediately so the write-ahead path attempts delivery at once.
fn backoff_delay_ms(attempt_count: u32) -> u64 {
    match attempt_count {
        0 => 0,
        1 => 30_000,      // 30s
        2 => 60_000,      // 1m
        3 => 5 * 60_000,  // 5m
        4 => 15 * 60_000, // 15m
        _ => 60 * 60_000, // 1h cap
    }
}

/// One pending delivery: "note `note_path` needs to reach peer
/// `device_id`." Content is NOT stored — see the module docs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct OutboxEntry {
    device_id: [u8; 16],
    note_path: String,
    /// Last time this (peer, note) was enqueued. Unix millis. This is the
    /// TTL-reap anchor: it's refreshed on *every* enqueue (not just the
    /// first), so a note the user keeps editing while a peer is offline
    /// isn't reaped on a stale original-failure timestamp. The queue copy
    /// the recipient eventually pulls reflects the *current* doc anyway, so
    /// "kept alive by recent edits" is the correct semantic.
    ///
    /// Wire-compatible with the pre-rename `first_failed_at_millis` (same
    /// position + type); old entries simply use their first-failure time as
    /// the anchor until the next enqueue refreshes it.
    last_enqueued_at_millis: u64,
    /// How many delivery attempts have failed. Drives the backoff.
    attempt_count: u32,
    /// Earliest time the next attempt should run. Unix millis.
    next_retry_at_millis: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct Envelope {
    version: u16,
    entries: Vec<OutboxEntry>,
}

type Key = ([u8; 16], String);

/// In-memory outbox backed by `outbox.bin`. Loads on construction; every
/// mutation rewrites the whole file atomically.
pub struct OutboxStore {
    path: PathBuf,
    entries: BTreeMap<Key, OutboxEntry>,
}

impl OutboxStore {
    fn key(device: DeviceId, note: &NotePath) -> Key {
        (device.0, note.0.clone())
    }

    /// Load the outbox from the sync state dir, or return an empty one if
    /// the file doesn't exist yet.
    pub fn load(state_dir: &std::path::Path) -> Result<Self, StoreError> {
        let path = outbox_file(state_dir);
        if !path.exists() {
            return Ok(Self {
                path,
                entries: BTreeMap::new(),
            });
        }
        let bytes = fs::read(&path)?;
        let env: Envelope = postcard::from_bytes(&bytes)
            .map_err(|e| StoreError::Decode(format!("outbox file: {e}")))?;
        if env.version != OUTBOX_FILE_VERSION {
            return Err(StoreError::Decode(format!(
                "outbox file version {} unsupported (expected {OUTBOX_FILE_VERSION})",
                env.version
            )));
        }
        let entries = env
            .entries
            .into_iter()
            .map(|e| ((e.device_id, e.note_path.clone()), e))
            .collect();
        Ok(Self { path, entries })
    }

    /// Write-ahead enqueue: record that `note` must reach `device`. Called
    /// *before* the first push attempt, so a crash anywhere after this still
    /// leaves a durable intent that the drainer will retry.
    ///
    /// Idempotent per (peer, note): if an entry already exists we preserve
    /// its `attempt_count` + `next_retry_at` (so a fresh edit doesn't restart
    /// the backoff clock and hammer a down server) but DO refresh
    /// `last_enqueued_at` — recent edits keep the intent alive against TTL
    /// reaping. The push regenerates the *current* doc anyway, so the
    /// existing entry already covers the new content.
    ///
    /// Returns whether the caller should attempt delivery **now**: `true`
    /// for a freshly-inserted intent or an existing one whose backoff has
    /// elapsed; `false` for an existing intent still inside its backoff
    /// window (the periodic drainer will pick it up when due). This is the
    /// gate that stops frequent saves from hammering a dead queue.
    pub fn enqueue(
        &mut self,
        device: DeviceId,
        note: &NotePath,
        now_millis: u64,
    ) -> Result<bool, StoreError> {
        use std::collections::btree_map::Entry;
        let key = Self::key(device, note);
        let attempt_now = match self.entries.entry(key) {
            Entry::Occupied(mut slot) => {
                let entry = slot.get_mut();
                // Refresh the TTL anchor; preserve backoff state.
                entry.last_enqueued_at_millis = now_millis;
                entry.next_retry_at_millis <= now_millis
            }
            Entry::Vacant(slot) => {
                slot.insert(OutboxEntry {
                    device_id: device.0,
                    note_path: note.0.clone(),
                    last_enqueued_at_millis: now_millis,
                    attempt_count: 0,
                    next_retry_at_millis: now_millis, // due immediately
                });
                true
            }
        };
        // Both arms mutate (insert, or anchor refresh), so always persist.
        self.flush()?;
        Ok(attempt_now)
    }

    /// Drop every intent addressed to `device`. Called when a peer is
    /// unpaired so its queued deliveries don't linger as ghost pending work
    /// (the peer no longer has a queue key, so they could never deliver).
    /// Returns the number removed.
    pub fn remove_peer(&mut self, device: DeviceId) -> Result<usize, StoreError> {
        let before = self.entries.len();
        self.entries.retain(|(dev, _), _| *dev != device.0);
        let removed = before - self.entries.len();
        if removed > 0 {
            self.flush()?;
        }
        Ok(removed)
    }

    /// Drop every intent. Called when the docstore is wiped
    /// (`reset_docstore`): the canonical docs the outbox would regenerate
    /// payloads from are gone, so the intents are stale.
    pub fn clear(&mut self) -> Result<(), StoreError> {
        if !self.entries.is_empty() {
            self.entries.clear();
            self.flush()?;
        }
        Ok(())
    }

    /// Record a failed delivery attempt: bump the attempt counter and push
    /// `next_retry_at` out by the backoff for the new count. No-op if the
    /// entry was cleared (delivered) in the meantime.
    pub fn record_failure(
        &mut self,
        device: DeviceId,
        note: &NotePath,
        now_millis: u64,
    ) -> Result<(), StoreError> {
        let key = Self::key(device, note);
        if let Some(entry) = self.entries.get_mut(&key) {
            entry.attempt_count = entry.attempt_count.saturating_add(1);
            entry.next_retry_at_millis =
                now_millis.saturating_add(backoff_delay_ms(entry.attempt_count));
            self.flush()?;
        }
        Ok(())
    }

    /// Clear an entry after successful delivery. Returns whether anything
    /// was removed (idempotent).
    pub fn remove(&mut self, device: DeviceId, note: &NotePath) -> Result<bool, StoreError> {
        let removed = self.entries.remove(&Self::key(device, note)).is_some();
        if removed {
            self.flush()?;
        }
        Ok(removed)
    }

    /// Every (peer, note) whose `next_retry_at` has arrived. The drainer
    /// walks this set and attempts delivery for each.
    pub fn due(&self, now_millis: u64) -> Vec<(DeviceId, NotePath)> {
        self.entries
            .values()
            .filter(|e| e.next_retry_at_millis <= now_millis)
            .map(|e| (DeviceId(e.device_id), NotePath(e.note_path.clone())))
            .collect()
    }

    /// Drop entries last enqueued before `cutoff_millis` (the queue message
    /// TTL window). After the TTL the peer's queued copies have expired, so
    /// a still-undelivered intent can never be picked up — reaping it stops
    /// pointless perpetual retries. Anchored on the *last* enqueue (not the
    /// first failure), so a note the user is actively re-editing stays alive.
    /// Returns the number reaped.
    pub fn reap(&mut self, cutoff_millis: u64) -> usize {
        let before = self.entries.len();
        self.entries
            .retain(|_, e| e.last_enqueued_at_millis >= cutoff_millis);
        let reaped = before - self.entries.len();
        if reaped > 0 {
            // Best-effort persist; an unflushed reap just re-reaps next pass.
            if let Err(e) = self.flush() {
                tracing::warn!(error = %e, "outbox: reap flush failed");
            }
        }
        reaped
    }

    /// Pending-delivery count per peer device id. Feeds the sync status
    /// surface so the UI can show "N changes waiting for <peer>".
    pub fn pending_by_peer(&self) -> BTreeMap<[u8; 16], u32> {
        let mut counts: BTreeMap<[u8; 16], u32> = BTreeMap::new();
        for entry in self.entries.values() {
            *counts.entry(entry.device_id).or_insert(0) += 1;
        }
        counts
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn flush(&self) -> Result<(), StoreError> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let env = Envelope {
            version: OUTBOX_FILE_VERSION,
            entries: self.entries.values().cloned().collect(),
        };
        let bytes = postcard::to_allocvec(&env).map_err(|e| StoreError::Encode(e.to_string()))?;
        let tmp = self.path.with_extension("bin.tmp");
        // fsync the temp file BEFORE the rename. The outbox is the write-ahead
        // log that redelivers a push after "app killed / power loss mid-push";
        // an unsynced fs::write can leave a zero-length/stale outbox behind the
        // rename on power loss, silently breaking that durability guarantee.
        {
            use std::io::Write;
            let mut f = fs::File::create(&tmp)?;
            f.write_all(&bytes)?;
            f.sync_all()?;
        }
        fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn dev(id: u8) -> DeviceId {
        DeviceId([id; 16])
    }
    fn note(p: &str) -> NotePath {
        NotePath(p.into())
    }
    fn load(dir: &std::path::Path) -> OutboxStore {
        OutboxStore::load(dir).unwrap()
    }

    #[test]
    fn empty_when_file_absent() {
        let dir = tempdir().unwrap();
        assert!(load(dir.path()).is_empty());
    }

    #[test]
    fn enqueue_then_reload_roundtrips() {
        let dir = tempdir().unwrap();
        let mut store = load(dir.path());
        store.enqueue(dev(1), &note("a.md"), 1000).unwrap();
        store.enqueue(dev(2), &note("b.md"), 1000).unwrap();

        let reloaded = load(dir.path());
        assert_eq!(reloaded.len(), 2);
        // Both are due at/after their enqueue time.
        assert_eq!(reloaded.due(1000).len(), 2);
    }

    #[test]
    fn new_entry_is_due_immediately_and_signals_attempt() {
        let dir = tempdir().unwrap();
        let mut store = load(dir.path());
        // A freshly-inserted intent signals "attempt now".
        assert!(store.enqueue(dev(1), &note("a.md"), 5000).unwrap());
        assert_eq!(store.due(5000), vec![(dev(1), note("a.md"))]);
    }

    #[test]
    fn enqueue_is_idempotent_preserves_backoff_and_throttles_attempts() {
        let dir = tempdir().unwrap();
        let mut store = load(dir.path());
        assert!(store.enqueue(dev(1), &note("a.md"), 1000).unwrap());
        store.record_failure(dev(1), &note("a.md"), 1000).unwrap();
        // After one failure, not due until 1000 + 30s.
        assert!(store.due(1000).is_empty());

        // A re-enqueue (user edits the note again) must NOT reset the backoff
        // clock AND must signal "do not attempt now" — this is the gate that
        // stops frequent saves from hammering a dead queue.
        assert!(
            !store.enqueue(dev(1), &note("a.md"), 1000).unwrap(),
            "re-enqueue inside the backoff window must not trigger an attempt"
        );
        assert!(store.due(1000).is_empty());
        assert_eq!(store.len(), 1);

        // Once the backoff has elapsed, a re-enqueue signals "attempt now"
        // again (without resetting the entry).
        assert!(
            store.enqueue(dev(1), &note("a.md"), 31_000).unwrap(),
            "re-enqueue after the backoff window should attempt"
        );
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn remove_peer_drops_only_that_peers_intents() {
        let dir = tempdir().unwrap();
        let mut store = load(dir.path());
        store.enqueue(dev(1), &note("a.md"), 0).unwrap();
        store.enqueue(dev(1), &note("b.md"), 0).unwrap();
        store.enqueue(dev(2), &note("c.md"), 0).unwrap();

        assert_eq!(store.remove_peer(dev(1)).unwrap(), 2);
        assert_eq!(store.len(), 1);
        assert_eq!(store.due(0), vec![(dev(2), note("c.md"))]);
        // Idempotent.
        assert_eq!(store.remove_peer(dev(1)).unwrap(), 0);

        // Survives a reload.
        assert_eq!(load(dir.path()).len(), 1);
    }

    #[test]
    fn clear_drops_everything() {
        let dir = tempdir().unwrap();
        let mut store = load(dir.path());
        store.enqueue(dev(1), &note("a.md"), 0).unwrap();
        store.enqueue(dev(2), &note("c.md"), 0).unwrap();
        store.clear().unwrap();
        assert!(store.is_empty());
        assert!(load(dir.path()).is_empty());
    }

    #[test]
    fn record_failure_advances_backoff() {
        let dir = tempdir().unwrap();
        let mut store = load(dir.path());
        store.enqueue(dev(1), &note("a.md"), 0).unwrap();

        store.record_failure(dev(1), &note("a.md"), 0).unwrap(); // count=1 → +30s
        assert!(store.due(29_999).is_empty());
        assert_eq!(store.due(30_000).len(), 1);

        store.record_failure(dev(1), &note("a.md"), 30_000).unwrap(); // count=2 → +60s
        assert!(store.due(89_999).is_empty());
        assert_eq!(store.due(90_000).len(), 1);
    }

    #[test]
    fn remove_clears_entry() {
        let dir = tempdir().unwrap();
        let mut store = load(dir.path());
        store.enqueue(dev(1), &note("a.md"), 0).unwrap();
        assert!(store.remove(dev(1), &note("a.md")).unwrap());
        assert!(!store.remove(dev(1), &note("a.md")).unwrap()); // idempotent
        assert!(load(dir.path()).is_empty());
    }

    #[test]
    fn reap_drops_expired_entries() {
        let dir = tempdir().unwrap();
        let mut store = load(dir.path());
        store.enqueue(dev(1), &note("old.md"), 0).unwrap();
        store.enqueue(dev(2), &note("new.md"), 1_000_000).unwrap();

        // Cutoff at 500_000: the entry last enqueued at 0 is expired.
        let reaped = store.reap(500_000);
        assert_eq!(reaped, 1);
        assert_eq!(store.len(), 1);
        assert_eq!(store.due(1_000_000), vec![(dev(2), note("new.md"))]);
    }

    #[test]
    fn re_enqueue_refreshes_ttl_anchor() {
        // A note that's been failing to deliver for a long time (near the
        // TTL) but is edited again must NOT be reaped on its stale original
        // timestamp — the fresh enqueue keeps it alive for another window.
        let dir = tempdir().unwrap();
        let mut store = load(dir.path());
        store.enqueue(dev(1), &note("a.md"), 0).unwrap();
        store.record_failure(dev(1), &note("a.md"), 0).unwrap(); // anchor stays at 0

        // User edits the same note again much later.
        let fresh = 1_000_000;
        store.enqueue(dev(1), &note("a.md"), fresh).unwrap();

        // A reap whose cutoff would have dropped the ORIGINAL t=0 anchor must
        // now KEEP the entry — the re-enqueue refreshed the anchor to `fresh`.
        assert_eq!(
            store.reap(fresh - 1),
            0,
            "fresh re-enqueue must reset the TTL anchor"
        );
        assert_eq!(store.len(), 1);

        // …and it's still reapable once the cutoff passes the refreshed anchor.
        assert_eq!(store.reap(fresh + 1), 1);
        assert!(store.is_empty());
    }

    #[test]
    fn pending_by_peer_counts() {
        let dir = tempdir().unwrap();
        let mut store = load(dir.path());
        store.enqueue(dev(1), &note("a.md"), 0).unwrap();
        store.enqueue(dev(1), &note("b.md"), 0).unwrap();
        store.enqueue(dev(2), &note("c.md"), 0).unwrap();
        let by_peer = store.pending_by_peer();
        assert_eq!(by_peer.get(&[1; 16]), Some(&2));
        assert_eq!(by_peer.get(&[2; 16]), Some(&1));
    }

    #[test]
    fn rejects_wrong_version() {
        let dir = tempdir().unwrap();
        let path = outbox_file(dir.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let bogus = Envelope {
            version: 99,
            entries: vec![],
        };
        std::fs::write(&path, postcard::to_allocvec(&bogus).unwrap()).unwrap();
        assert!(OutboxStore::load(dir.path()).is_err());
    }

    #[test]
    fn survives_simulated_crash_via_reload() {
        // Write-ahead durability: enqueue, drop the store (simulating a
        // crash before delivery), reload from disk — the intent is still
        // there and due.
        let dir = tempdir().unwrap();
        {
            let mut store = load(dir.path());
            store.enqueue(dev(7), &note("crash.md"), 100).unwrap();
        } // store dropped — only the on-disk file remains
        let reloaded = load(dir.path());
        assert_eq!(reloaded.due(100), vec![(dev(7), note("crash.md"))]);
    }
}
