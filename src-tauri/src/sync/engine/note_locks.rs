//! Per-note serialization.
//!
//! Guarding only the DocStore commit (the heads-CAS the adopt paths use) is
//! NOT enough. A remote adopt commits the merged doc, RELEASES the store lock
//! to do fail-closed backup I/O, then renders the merged text to disk. A
//! concurrent local save can slip its own disk write into the gap between the
//! commit and the render — and the adopt's now-stale render clobbers it. The
//! filesystem watcher then re-ingests that stale render as a genuine local
//! edit and fans it out to every peer: silent CRDT data loss. The heads-CAS
//! catches a racing *commit*; it cannot catch a racing *disk render*.
//!
//! `NoteLocks` hands out one exclusive async guard per note path so the WHOLE
//! "mutate DocStore → reconcile disk" operation runs serially per note, across
//! every entry point that can touch a note's doc or file:
//!
//! - local save — `apply_local_change` (commit only; disk already holds the
//!   user's file, so there is no render twin to guard)
//! - local delete — `mark_note_deleted` (commit the tombstone, then reconcile
//!   the disk trash under the SAME guard — re-removing a copy a racing adopt
//!   recreated, so disk can't disagree with the tombstone)
//! - local restore / undo-delete — `restore_note` (clear the tombstone, then
//!   reconcile the disk write back under the SAME guard)
//! - local rename / move — `notify_local_rename` (decomposes into
//!   `mark_note_deleted` on the OLD path + `apply_local_change` on the NEW
//!   path, each guarded per its own note; the raw disk move is transitively
//!   reconciled by those two)
//! - live adopt (peer task) — `handle_inbound` SyncMessage/FullDoc/NoteDelete,
//!   `handle_local` Removed
//! - SAF bootstrap seed — `seed_disk_only_notes` (commit only)
//! - queue adopt — `apply_queue_full_doc`
//! - startup scan / recovery — `scan_roost`, `recover_missing_files`
//!
//! The watcher never commits or writes disk itself — it funnels through
//! `apply_local_change` / `mark_note_deleted`, so locking those covers it. The
//! delete/rename/revert IPC commands DO touch disk first (their own trash /
//! rename / write_atomic), but the guarded engine method they then call
//! reconciles disk under the guard — so the net commit+disk is atomic even
//! though the IPC's initial disk op is not itself guarded.
//!
//! **Invariants (a violation deadlocks):**
//! - The guard is **non-reentrant** — a path already holding a note's guard
//!   must never re-acquire it for the same note. (The one self-recursion,
//!   `FullDocChunk` → `FullDoc`, deliberately does NOT hold the guard across
//!   the re-dispatch; the accumulation phase touches no DocStore/disk.)
//! - Acquire it **outermost** — before the DocStore lock — and hold it across
//!   the disk reconcile. Every path takes at most ONE note guard at a time and
//!   always in the order `barrier(read) → note guard → DocStore`, so there is
//!   no lock-ordering deadlock.
//!
//! **Global mutation barrier (SYN-B5).** Beyond per-note serialization, the
//! guard also carries a READ share of an engine-wide `barrier` RwLock for its
//! whole commit+disk critical section. `reset_docstore` — which wipes the
//! entire DocStore out from under every note — takes the WRITE via
//! [`NoteLocks::block_all`]. Because tokio's `RwLock` is write-preferring, once
//! reset is waiting for the write no new `lock()` can grab a read (they queue
//! behind it), and reset only proceeds once every in-flight mutation has
//! drained. Without this, a mutator that had committed to the DocStore and was
//! mid-disk-render could interleave with the wipe and leave a half-wiped store
//! (a doc listed but unloadable, or a fresh commit landing into a store reset
//! believes is empty). The barrier's safety RESTS ON the non-reentrant /
//! one-guard-at-a-time invariant above: a read holder that re-acquired `lock()`
//! (a new read) while reset's write was pending would deadlock, because the new
//! read queues behind the writer that is itself waiting on the first read.

use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};

use tokio::sync::{
    Mutex as AsyncMutex, OwnedMutexGuard, OwnedRwLockReadGuard, OwnedRwLockWriteGuard,
    RwLock as AsyncRwLock,
};

use crate::sync::protocol::NotePath;

/// Above this many tracked entries, opportunistically drop idle ones (those no
/// task holds or is awaiting) on the next `lock`. Keeps the map bounded to the
/// set of notes under concurrent mutation, not every note touched this run.
const PRUNE_THRESHOLD: usize = 512;

/// A keyed set of async mutexes — one exclusive guard per note path — plus a
/// global mutation barrier (see the module docs, SYN-B5).
pub(super) struct NoteLocks {
    map: StdMutex<HashMap<String, Arc<AsyncMutex<()>>>>,
    /// Engine-wide mutation barrier. Every per-note `lock()` holds a READ for
    /// its critical section; `block_all()` takes the WRITE so a DocStore wipe
    /// can't interleave with an in-flight mutation.
    barrier: Arc<AsyncRwLock<()>>,
}

/// Compound per-note guard: a READ share of the global mutation barrier held
/// for the whole critical section, plus the exclusive per-note mutex. Both
/// release when this drops (fields drop in declaration order: note mutex first
/// so a note frees promptly, then the barrier read). Callers only ever hold it
/// for scope, so it is a drop-in replacement for the bare note-mutex guard.
pub(super) struct NoteGuard {
    _note: OwnedMutexGuard<()>,
    _barrier: OwnedRwLockReadGuard<()>,
}

impl NoteLocks {
    pub(super) fn new() -> Self {
        Self {
            map: StdMutex::new(HashMap::new()),
            barrier: Arc::new(AsyncRwLock::new(())),
        }
    }

    /// Acquire the exclusive guard for `note`, awaiting any in-progress
    /// mutation of the same note (and any pending `block_all`). Hold the
    /// returned guard across the entire commit+disk critical section; drop it
    /// to release. Non-reentrant — see the module invariants.
    pub(super) async fn lock(&self, note: &NotePath) -> NoteGuard {
        // Barrier read FIRST: a mutation blocked out by a pending reset never
        // even claims the per-note mutex. Held for the guard's whole lifetime.
        let barrier = self.barrier.clone().read_owned().await;
        let mutex = {
            // Poison only occurs if a holder panicked mid-map-op; the map ops
            // below can't panic, so recover the guard rather than propagate.
            let mut map = match self.map.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            if map.len() > PRUNE_THRESHOLD {
                // strong_count == 1 ⇒ only the map references this mutex, so no
                // task holds it or is parked on `lock_owned` — safe to forget.
                // A held/awaited entry has strong_count ≥ 2 (map + guard/waiter).
                map.retain(|_, m| Arc::strong_count(m) > 1);
            }
            Arc::clone(
                map.entry(note.0.clone())
                    .or_insert_with(|| Arc::new(AsyncMutex::new(()))),
            )
        };
        let note = mutex.lock_owned().await;
        NoteGuard {
            _note: note,
            _barrier: barrier,
        }
    }

    /// Acquire the global mutation barrier EXCLUSIVELY (SYN-B5). While the
    /// returned guard is held, no new per-note `lock()` can proceed and every
    /// in-flight one has drained — so the caller (`reset_docstore`) can wipe
    /// the DocStore with no mutation interleaving. The caller MUST have already
    /// stopped the peer tasks that could otherwise be mid-adopt; this only
    /// blocks the note-lock paths, not raw peer I/O.
    pub(super) async fn block_all(&self) -> OwnedRwLockWriteGuard<()> {
        self.barrier.clone().write_owned().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    fn note(s: &str) -> NotePath {
        NotePath(s.to_string())
    }

    /// Two critical sections for the SAME note never overlap: with the guard
    /// held, a second `lock` for that note must wait. We prove it by having the
    /// first holder observe that the concurrent count never exceeds 1.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn same_note_serializes() {
        let locks = Arc::new(NoteLocks::new());
        let concurrent = Arc::new(AtomicUsize::new(0));
        let max_seen = Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::new();
        for _ in 0..16 {
            let locks = locks.clone();
            let concurrent = concurrent.clone();
            let max_seen = max_seen.clone();
            handles.push(tokio::spawn(async move {
                let _g = locks.lock(&note("a.md")).await;
                let now = concurrent.fetch_add(1, Ordering::SeqCst) + 1;
                max_seen.fetch_max(now, Ordering::SeqCst);
                // Hold the critical section long enough that any lack of
                // mutual exclusion would show up as concurrency > 1.
                tokio::time::sleep(Duration::from_millis(5)).await;
                concurrent.fetch_sub(1, Ordering::SeqCst);
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        assert_eq!(
            max_seen.load(Ordering::SeqCst),
            1,
            "critical sections for the same note overlapped"
        );
    }

    /// Different notes proceed concurrently — the lock is per-note, not global.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn different_notes_run_concurrently() {
        let locks = Arc::new(NoteLocks::new());
        let g1 = locks.lock(&note("a.md")).await;
        // A different note must acquire without waiting on `a.md`.
        let g2 = tokio::time::timeout(Duration::from_secs(1), locks.lock(&note("b.md")))
            .await
            .expect("distinct note should not block on a held one");
        drop(g1);
        drop(g2);
    }

    /// SYN-B5: `block_all` (the reset write barrier) must wait until an
    /// in-flight per-note mutation drains — even one on an UNRELATED note —
    /// before it can proceed, so a DocStore wipe can't interleave with a
    /// commit+disk render. Regression: before the barrier, `reset_docstore`
    /// wiped the store while a mutator was mid-render.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn block_all_waits_for_held_note_guard() {
        let locks = Arc::new(NoteLocks::new());
        // Hold a note guard (a mutator mid critical-section).
        let held = locks.lock(&note("a.md")).await;

        // A reset trying to block all mutations must NOT acquire while the
        // guard is held.
        let got_barrier = tokio::time::timeout(Duration::from_millis(200), locks.block_all()).await;
        assert!(
            got_barrier.is_err(),
            "block_all acquired while a note guard was still held — the wipe could race a mutation"
        );

        // Once the mutator drops its guard, the barrier becomes available.
        drop(held);
        tokio::time::timeout(Duration::from_secs(1), locks.block_all())
            .await
            .expect("block_all should acquire once all note guards are released");
    }

    /// SYN-B5: while the reset write barrier is held, NEW per-note `lock()`s
    /// must block (tokio RwLock is write-preferring), so no fresh commit lands
    /// into a store the wipe believes is empty. They proceed once reset drops.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn block_all_blocks_new_locks() {
        let locks = Arc::new(NoteLocks::new());
        let barrier = locks.block_all().await;

        // A new mutation on any note must wait behind the pending/held write.
        let blocked =
            tokio::time::timeout(Duration::from_millis(200), locks.lock(&note("a.md"))).await;
        assert!(
            blocked.is_err(),
            "a new note lock acquired while the reset barrier was held — a mutation raced the wipe"
        );

        // Releasing the barrier lets the mutation through.
        drop(barrier);
        tokio::time::timeout(Duration::from_secs(1), locks.lock(&note("a.md")))
            .await
            .expect("note lock should acquire once the reset barrier is released");
    }

    /// Idle entries are pruned once the map crosses the threshold, so a long
    /// session touching many distinct notes doesn't grow the map without bound.
    #[tokio::test]
    async fn idle_entries_are_pruned() {
        let locks = NoteLocks::new();
        for i in 0..(PRUNE_THRESHOLD + 50) {
            let g = locks.lock(&note(&format!("n{i}.md"))).await;
            drop(g); // released immediately ⇒ entry becomes idle
        }
        let len = {
            let map = locks.map.lock().unwrap();
            map.len()
        };
        assert!(
            len <= PRUNE_THRESHOLD + 1,
            "idle entries were not pruned: map holds {len}"
        );
    }
}
