//! Roost file watcher that keeps the graph / search index live for *external*
//! edits — MCP captures, Obsidian, vim, agent shells — whether or not sync is
//! enabled.
//!
//! Background: the only other live filesystem watcher (`SyncEngine`'s, in
//! [`crate::sync::engine::watcher`]) is owned by the sync engine and only runs
//! once the user enables sync. With sync off, an external write went unindexed
//! until the user happened to open + save the file in the editor — so
//! MCP-created notes never showed up in the graph live. This watcher closes
//! that gap: it watches the open roost and emits the same `sync://event`
//! `NoteWritten` / `NoteDeleted` the frontend already consumes
//! (`graphStore.indexNote`), so the existing reindex path fires with no
//! frontend change.
//!
//! When sync IS running, the engine's watcher already emits those events, so
//! this watcher stays inert (see [`sync_active`]) to avoid double work. The
//! handoff is not perfectly exclusive: on the *enable* edge the engine arms
//! its watcher (`sync.rs` `start_watcher`) slightly before it lands in managed
//! state, so a change in that narrow window can be emitted by both watchers.
//! That is harmless — `graphStore.indexNote` is idempotent, so a duplicate
//! `NoteWritten` just re-indexes identical content.
//!
//! Scope: when sync is off, an external *delete* fires only the frontend graph
//! purge — it is NOT turned into a CRDT tombstone (there is no CRDT layer
//! without sync). That matches existing offline behavior; a note deleted while
//! sync is off can resurrect from a peer when sync is later enabled.
//!
//! Desktop-only in practice: on Android SAF (`content://`) `notify` can't
//! install a recursive watch, so [`RoostWatcher::start`] soft-fails and we run
//! without a live watcher — the window-focus refresh in the frontend covers
//! that platform, exactly as it does for sync.

use std::path::Path;
use std::sync::Mutex;

use tauri::{AppHandle, Manager};
use tauri_specta::Event as _;
use tracing::{debug, warn};

use crate::commands::sync::{SyncEngineState, SyncEventPayload};
use crate::events::SyncStreamEvent;
use crate::sync::engine::watcher::{LocalChange, RoostWatcher};

/// Managed handle to the running index watcher. Replaced on each roost open;
/// `None` when no roost is open or the platform has no live watcher.
#[derive(Default)]
pub struct IndexWatcherState(pub Mutex<Option<WatcherHandle>>);

/// Keeps the [`RoostWatcher`] (drop = stop the FS watch) and its consumer task
/// (aborted on drop) alive for the lifetime of the open roost.
pub struct WatcherHandle {
    _watcher: RoostWatcher,
    consumer: tokio::task::JoinHandle<()>,
}

impl Drop for WatcherHandle {
    fn drop(&mut self) {
        self.consumer.abort();
    }
}

/// Map a filesystem change to the `sync://event` payload the frontend reindex
/// path expects. Pure so it's unit-testable without a running app.
fn change_to_payload(change: &LocalChange) -> SyncEventPayload {
    match change {
        // Desktop `notify` path: absolute path only; the frontend re-reads the
        // file content itself before indexing.
        LocalChange::Modified { abs, .. } => SyncEventPayload::NoteWritten {
            path: abs.to_string_lossy().into_owned(),
        },
        // SAF/IPC path carries a roost-relative path. Rare here (SAF has no
        // live watcher), kept for completeness so the mapping is total.
        LocalChange::ModifiedWithContent { path, .. } => SyncEventPayload::NoteWritten {
            path: path.0.clone(),
        },
        LocalChange::Removed { path } => SyncEventPayload::NoteDeleted {
            path: path.0.clone(),
        },
        // Defensive fallback only — NOT the live path. The consumer task
        // intercepts `RemovedPrefix` before calling this and expands it into
        // one `note_deleted` per nested note (see `nested_indexed_notes`), so
        // this arm is unreached in normal flow. It exists to keep the mapping
        // total and to degrade safely (a folder-path "delete" is a harmless
        // frontend no-op) if a future caller ever forgets that guard.
        LocalChange::RemovedPrefix { prefix } => SyncEventPayload::NoteDeleted {
            path: prefix.0.clone(),
        },
    }
}

/// Roost-relative paths of every indexed note under a removed directory,
/// read from the graph DB (the files are already gone from disk, so the graph
/// is the only record). Best-effort: returns empty if the graph isn't
/// initialized yet (nothing indexed → nothing to clean), the roost root is
/// unknown, or a lock is contended. The brief synchronous Cozo query runs
/// without holding the lock across an await, so it can't stall other tasks
/// beyond its own duration — acceptable for a rare folder-delete event.
fn nested_indexed_notes(app: &AppHandle, rel_prefix: &str) -> Vec<String> {
    let Some(graph) = app.try_state::<crate::commands::graph::GraphDb>() else {
        return Vec::new();
    };
    let Ok(db_guard) = graph.db.lock() else {
        return Vec::new();
    };
    let Some(db) = db_guard.as_ref() else {
        return Vec::new(); // graph not initialized → nothing was indexed
    };
    let roost_root = match graph.roost_path.lock() {
        Ok(g) => match g.as_deref() {
            Some(v) => v.to_string(),
            None => return Vec::new(),
        },
        Err(_) => return Vec::new(),
    };
    match crate::db::queries::note_paths_under_dir(db, &roost_root, rel_prefix) {
        Ok(paths) => paths,
        Err(e) => {
            warn!(error = %e, prefix = %rel_prefix, "index watcher: enumerate notes under prefix failed");
            Vec::new()
        }
    }
}

/// Is the sync engine running? If so, its own watcher already emits
/// `NoteWritten`/`NoteDeleted`, so this watcher must stay inert to avoid
/// double-indexing the same change. Non-blocking: a held lock means the engine
/// exists and is busy, so we treat that as active.
fn sync_active(app: &AppHandle) -> bool {
    match app.try_state::<SyncEngineState>() {
        Some(state) => sync_running(&state),
        None => false,
    }
}

/// Decide whether sync's engine is running, given its managed state. Split out
/// from [`sync_active`] (which only resolves the state off the `AppHandle`) so
/// the gating logic is unit-testable without a full Tauri app. A contended
/// lock counts as active: the only time the lock is held is while the engine
/// is present and in use, so suppressing a duplicate emit is the safe choice.
fn sync_running(state: &SyncEngineState) -> bool {
    match state.engine.try_lock() {
        Ok(guard) => guard.is_some(),
        Err(_) => true,
    }
}

/// Store `handle` in managed state, dropping (and thereby stopping) whatever
/// was there before. Tolerates a poisoned lock — this is best-effort glue and
/// a poisoned mutex shouldn't wedge roost-open.
fn store_handle(state: &IndexWatcherState, handle: Option<WatcherHandle>) {
    let mut guard = state.0.lock().unwrap_or_else(|p| p.into_inner());
    *guard = handle;
}

/// (Re)bind the index watcher to `roost_dir`. Mirrors `rebind_skill_watcher` /
/// `rebind_subagent_watcher`: best-effort, never blocks roost-open. Replacing
/// the stored handle drops the previous watcher and aborts its consumer task.
pub fn rebind_index_watcher(app: &AppHandle, roost_dir: &Path) {
    let Some(state) = app.try_state::<IndexWatcherState>() else {
        return; // not initialized (some test contexts)
    };

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<LocalChange>();
    let watcher = match RoostWatcher::start(roost_dir.to_path_buf(), tx) {
        Ok(w) => w,
        Err(e) => {
            // SAF/content URI or a permission issue — no live watcher on this
            // roost. Clear any stale handle so we don't keep watching the
            // previous roost, then bail (focus-refresh covers this platform).
            warn!(roost = %roost_dir.display(), error = %e, "index watcher: unavailable on this roost — relying on focus refresh");
            store_handle(&state, None);
            return;
        }
    };

    let app_for_task = app.clone();
    let consumer = tokio::spawn(async move {
        while let Some(change) = rx.recv().await {
            // Sync's own watcher handles emission while sync is enabled.
            if sync_active(&app_for_task) {
                continue;
            }
            // External folder delete: the files are gone, so the graph is the
            // only record of what was inside. Expand to one `note_deleted` per
            // nested note, reusing the single-delete cleanup (graph + FTS +
            // UI). Without this, nested notes linger as graph/FTS ghosts until
            // the next reconcile-on-open.
            if let LocalChange::RemovedPrefix { prefix } = &change {
                for rel in nested_indexed_notes(&app_for_task, &prefix.0) {
                    if let Err(e) = SyncStreamEvent(SyncEventPayload::NoteDeleted { path: rel })
                        .emit(&app_for_task)
                    {
                        debug!(error = %e, "index watcher: prefix-delete emit failed");
                    }
                }
                continue;
            }
            if let Err(e) = SyncStreamEvent(change_to_payload(&change)).emit(&app_for_task) {
                debug!(error = %e, "index watcher: event emit failed");
            }
        }
    });

    // Swap in the new handle; the old one (if any) drops here, stopping its FS
    // watch and aborting its consumer task.
    store_handle(
        &state,
        Some(WatcherHandle {
            _watcher: watcher,
            consumer,
        }),
    );
    debug!(roost = %roost_dir.display(), "index watcher: bound");
}

/// Stop watching the previous roost when no replacement is being installed.
pub fn clear_index_watcher(app: &AppHandle) {
    if let Some(state) = app.try_state::<IndexWatcherState>() {
        store_handle(&state, None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::protocol::NotePath;
    use std::path::PathBuf;

    #[test]
    fn modified_maps_to_note_written_with_absolute_path() {
        let change = LocalChange::Modified {
            path: NotePath("Corvux/Android App Journey.md".into()),
            abs: PathBuf::from("/roost/Corvux/Android App Journey.md"),
        };
        match change_to_payload(&change) {
            SyncEventPayload::NoteWritten { path } => {
                // Desktop path uses the absolute form (frontend re-reads it).
                assert_eq!(path, "/roost/Corvux/Android App Journey.md");
            }
            other => panic!("expected NoteWritten, got {other:?}"),
        }
    }

    #[test]
    fn modified_with_content_maps_to_note_written_with_relative_path() {
        let change = LocalChange::ModifiedWithContent {
            path: NotePath("notes/ideas.md".into()),
            content: "# hi".into(),
        };
        match change_to_payload(&change) {
            SyncEventPayload::NoteWritten { path } => assert_eq!(path, "notes/ideas.md"),
            other => panic!("expected NoteWritten, got {other:?}"),
        }
    }

    #[test]
    fn removed_maps_to_note_deleted() {
        let change = LocalChange::Removed {
            path: NotePath("notes/old.md".into()),
        };
        match change_to_payload(&change) {
            SyncEventPayload::NoteDeleted { path } => assert_eq!(path, "notes/old.md"),
            other => panic!("expected NoteDeleted, got {other:?}"),
        }
    }

    #[test]
    fn removed_prefix_change_to_payload_is_defensive_fallback() {
        // NOTE: this asserts the *defensive fallback* mapping only. The live
        // path is the consumer intercepting `RemovedPrefix` and emitting one
        // `note_deleted` per nested note (via `nested_indexed_notes`), which is
        // integration-level and not exercised here. This test just pins that
        // the total mapping degrades to a harmless folder-path no-op if the
        // consumer guard is ever bypassed.
        let change = LocalChange::RemovedPrefix {
            prefix: NotePath("projects/archived".into()),
        };
        match change_to_payload(&change) {
            SyncEventPayload::NoteDeleted { path } => assert_eq!(path, "projects/archived"),
            other => panic!("expected NoteDeleted, got {other:?}"),
        }
    }

    // ── sync_running gating (the duplicate-emit guard) ──────────────────────

    #[test]
    fn sync_running_is_false_when_engine_absent() {
        // Default state = no engine yet (sync off) → this watcher must emit.
        let state = SyncEngineState::default();
        assert!(!sync_running(&state));
    }

    #[test]
    fn sync_running_is_true_when_state_lock_is_held() {
        // A held lock means the engine exists and is busy (e.g. mid network
        // op holding the mutex across an await) → treat as active and skip,
        // so we don't double-emit alongside the engine's own watcher.
        let state = SyncEngineState::default();
        let _held = state.engine.try_lock().expect("first lock should succeed");
        assert!(sync_running(&state));
    }
}
