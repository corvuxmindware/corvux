//! Local-change fan-out + docstore reset on `SyncEngine`.
//!
//! Routes a single local write/remove to every live peer connection
//! plus, when a queue client is installed, to every *offline* known
//! peer via the store-and-forward path. Also owns the destructive
//! `reset_docstore` — wipes Automerge state so a fresh adoption cycle
//! converges both sides on a single history per note.
//!
//! Split off `engine.rs` to keep the orchestrator focused on lifecycle.

use tracing::{debug, info, warn};

use crate::sync::crdt::NoteDoc;
use crate::sync::error::SyncResult;
use crate::sync::identity::DeviceId;
use crate::sync::protocol::NotePath;

use super::engine::SyncEngine;
use super::peer::PeerHandle;
use super::state::SyncEvent;
use super::util::clone_handle;
use super::watcher::LocalChange;

/// How long a note's delete tombstone is retained locally before
/// `reap_old_tombstones` GCs it. This is the guaranteed window for correct
/// delete propagation to a device that's been offline: as long as the
/// tombstone survives, a returning peer converges to deleted via normal
/// Automerge merge. 180 days ≈ a phone untouched for ~6 months. Must stay
/// **>= the queue TTL** (`DEFAULT_MESSAGE_TTL_SECS`) — see the
/// `tombstone_retention_outlasts_queue_ttl` guard test. Decoupling these
/// (the reap used to use the 30-day queue TTL) is what fixed the
/// "offline 45 days → deletes resurrect" surprise.
pub(super) const TOMBSTONE_RETENTION_SECS: u64 = 180 * 24 * 60 * 60;

// Compile-time guard: tombstone retention must outlast the queue TTL. If it
// were shorter, a tombstone could be reaped while an offline peer still had a
// queued (pre-delete or delete) copy in flight. Re-coupling these — the
// original "reap at the queue TTL" bug that resurrected deletes — fails the
// build here instead of shipping.
const _: () =
    assert!(TOMBSTONE_RETENTION_SECS >= crate::sync::queue::protocol::DEFAULT_MESSAGE_TTL_SECS);

/// Clear a note's tombstone if it carries one, returning `true` if it did.
/// Called on the LOCAL write path when the file is present on disk again:
/// the user recreating a note at a previously-deleted path is explicit intent
/// to keep it, so we un-delete before applying their text. Distinct from a
/// remote edit racing a delete (that keeps "delete wins"); this only fires for
/// a local create/modify we can see on disk.
fn restore_if_tombstoned(doc: &mut NoteDoc) -> SyncResult<bool> {
    if doc.is_deleted()? {
        doc.restore()?;
        Ok(true)
    } else {
        Ok(false)
    }
}

impl SyncEngine {
    /// Route a local write-through-IPC change to every connected peer.
    /// On desktop the notify-based watcher covers this already; on Android
    /// the notify crate silently no-ops on SAF `content://` URIs, so the
    /// IPC layer (commands::files::write_file) has to hand us the change
    /// directly or nothing local ever reaches the peers.
    ///
    /// Safe to call on all platforms: on desktop the watcher also fires,
    /// which lands in `handle_local` but is deduplicated at the Automerge
    /// layer (`apply_markdown` short-circuits when content is unchanged).
    ///
    /// After the live fan-out, also pushes an encrypted snapshot to the
    /// store-and-forward queue for every *known* peer that is currently
    /// disconnected (and only when a `QueueClient` is installed). The
    /// queue server de-dupes by `(sender, recipient, dedup_key=note path)`
    /// so a flurry of edits collapses into a single pending entry per
    /// recipient.
    pub async fn notify_local_write(&self, note_path: NotePath, content: String) {
        let change = LocalChange::ModifiedWithContent {
            path: note_path.clone(),
            content,
        };
        // Single-writer: persist to the docstore HERE, before fan-out, so
        // the queue push below reads current content and connected peers can
        // flush from an already-current doc. (See `apply_local_change`.)
        match self.apply_local_change(&change).await {
            Ok(true) => {}
            // Unchanged content (e.g. the desktop watcher double-fires the
            // same write): nothing to propagate.
            Ok(false) => return,
            Err(e) => {
                warn!(note = %note_path, error = %e, "notify_local_write: docstore apply failed");
                // SYN-I5/M1: a swallowed local-persist failure is invisible on
                // Android (no watcher — this funnel is the only sync signal), so
                // the edit sits on disk, never enters the CRDT, and never syncs
                // while the UI shows it saved. Surface it. Same silent-failure
                // class as B8's receive-side gaps, on the local-write leg.
                let _ = self.inner.events.send(SyncEvent::NoteSyncFailed {
                    path: note_path.0.clone(),
                    reason: format!("could not record edit for sync: {e}"),
                });
                return;
            }
        }
        let peers: Vec<PeerHandle> = self
            .inner
            .peers
            .read()
            .await
            .values()
            .map(clone_handle)
            .collect();
        for handle in peers {
            if let Err(e) = handle.send_local(change.clone()) {
                debug!(
                    peer = %handle.display_name,
                    error = %e,
                    "notify_local_write: peer channel closed/full, queueing for retry"
                );
                // Live-drop coverage: the peer task is gone OR its channel is
                // full (wedged/slow peer). Either way this change didn't reach
                // it — record durable intents instead of dropping it (or
                // blocking the caller, which holds the engine lock). The outbox
                // covers the offline/queue path; live_pending covers the
                // still-connected wedged-channel case with no queue installed.
                self.enqueue_outbox_for_peer(handle.device_id, note_path.clone())
                    .await;
                self.record_live_pending(handle.device_id, note_path.clone())
                    .await;
            }
        }
        self.queue_push_to_offline_peers(note_path).await;
    }

    /// Single-writer entry point: apply one local *modification* to the
    /// shared DocStore and report whether the doc actually changed.
    ///
    /// This is the fix underpinning the durable outbox. Previously the
    /// docstore was only written as a side effect of a *connected* peer's
    /// `handle_local`, so an edit made with zero peers connected never
    /// reached the docstore — and the queue push (which reads
    /// `doc.save()`) would have shipped stale content. Centralizing the
    /// write here makes the DocStore authoritative on every edit, which is
    /// exactly what the outbox regenerates payloads from at retry time.
    ///
    /// Only handles the modification variants — deletions go through
    /// `mark_note_deleted`, which writes the tombstone centrally already.
    pub(super) async fn apply_local_change(&self, change: &LocalChange) -> SyncResult<bool> {
        // A roost switch latches `stopped` and aborts the watcher, but a change
        // already dequeued can still be mid-flight here. Without this guard the
        // write completes against the OLD roost's DocStore after the new roost's
        // engine is up — cross-roost contamination. Match the guard on the other
        // write paths (dial.rs / engine.rs). See L-6.
        if self.is_stopped() {
            return Ok(false);
        }
        match change {
            LocalChange::Modified { path, abs: _ } => {
                // Serialize this note's read+commit against a concurrent remote
                // adopt's commit+render (live/queue). Without it, an adopt's
                // stale disk render could land between our disk read and our
                // commit — we'd then ingest and fan out the stale text. Held
                // outermost, before the DocStore lock. See `note_locks`.
                let _note_guard = self.inner.note_locks.lock(path).await;
                // Content lives on disk; read it through the guarded backend
                // (resolve_fs → backend or on-demand NativeFs) by ROOST-RELATIVE
                // path rather than the watcher's raw absolute path — so a
                // symlinked component can't make us ingest an out-of-roost file
                // into the CRDT (and then sync it to peers). A failed read (file
                // removed between watcher fire and now, or a rejected escape) is
                // a no-op.
                let Ok(bytes) =
                    super::peer::resolve_fs(&self.inner.roost, self.inner.roost_fs.as_deref())
                        .as_fs()
                        .read(&path.0)
                        .await
                else {
                    return Ok(false);
                };
                let text = String::from_utf8_lossy(&bytes).into_owned();
                let store = self.inner.doc_store.lock().await;
                let mut doc = store.load_or_create(path)?;
                // A local write to a path we hold as tombstoned means the user
                // recreated the file (same name again, folder rename round-trip,
                // git checkout, restore-from-backup). Local file reappearance is
                // explicit intent to have this note, so clear the tombstone
                // BEFORE applying text — otherwise every device re-trashes it on
                // the next merge (`reconcile_doc_to_roost` trashes any doc whose
                // `deleted` flag is set). This is local-only; the remote
                // edit-vs-delete "delete wins" policy is untouched.
                let restored = restore_if_tombstoned(&mut doc)?;
                let changed = doc.apply_markdown(&text)?;
                if changed || restored {
                    store.save(path, &mut doc)?;
                }
                Ok(changed || restored)
            }
            LocalChange::ModifiedWithContent { path, content } => {
                // Content is already in memory (Android IPC / editor save passed
                // it), so the COMMIT skips a disk read. The authoritative disk
                // write is the locked render below (B3).
                let _note_guard = self.inner.note_locks.lock(path).await;
                let mut doc = {
                    let store = self.inner.doc_store.lock().await;
                    let mut doc = store.load_or_create(path)?;
                    let restored = restore_if_tombstoned(&mut doc)?; // see Modified arm
                    let changed = doc.apply_markdown(content)?;
                    if !(changed || restored) {
                        return Ok(false);
                    }
                    store.save(path, &mut doc)?;
                    doc
                }; // DocStore lock released before disk I/O; note lock still held
                   // B3 (SYN-I3): the authoritative disk write for a local save is
                   // this render, done UNDER the same per-note lock as the commit,
                   // so a remote adopt can't interleave between commit and disk and
                   // leave disk != DocStore. The command layer's own write_atomic is
                   // a fast path; this locked render reconciles it and short-circuits
                   // when disk already matches (`render_to_roost` re-reads first) —
                   // so no double-write in the common no-race case, on desktop OR
                   // SAF (SafFs::read works; the old "would EIO" note was wrong).
                   // Cost: one extra read per save (a JNI round-trip on Android);
                   // a "content already written" hint could skip it — follow-up.
                use super::peer::RenderOutcome;
                match super::peer::render_to_roost(
                    &self.inner.roost,
                    self.inner.roost_fs.as_deref(),
                    path,
                    &mut doc,
                )
                .await
                {
                    Ok(RenderOutcome::Wrote | RenderOutcome::Unchanged) => {}
                    // SYN-I5 (mirrors reconcile_doc_to_roost, H-6): a hard write
                    // failure OR the data-loss-guard skip means disk doesn't hold
                    // what the DocStore — and the peers we're about to fan out to
                    // — say it should. Surface it rather than report the save as
                    // clean. render_to_roost returns these as Ok(Failed/Skipped),
                    // NOT Err, so an `if let Err` here would silently swallow the
                    // exact failure it targets. This is the note the user is
                    // editing, so surfacing matters more, not less.
                    Ok(RenderOutcome::Skipped { reason } | RenderOutcome::Failed { reason }) => {
                        let _ = self.inner.events.send(SyncEvent::NoteSyncFailed {
                            path: path.0.clone(),
                            reason,
                        });
                    }
                    Err(e) => {
                        let _ = self.inner.events.send(SyncEvent::NoteSyncFailed {
                            path: path.0.clone(),
                            reason: format!("render error: {e}"),
                        });
                    }
                }
                Ok(true)
            }
            // Deletions are written centrally by `mark_note_deleted`.
            LocalChange::Removed { .. } | LocalChange::RemovedPrefix { .. } => Ok(false),
        }
    }

    /// Fan a note out to every *offline* known peer via the
    /// store-and-forward queue. No-op when no queue is configured or
    /// when every known peer is currently connected (the live transport
    /// already delivers).
    pub async fn queue_push_to_offline_peers(&self, note: NotePath) {
        if self.inner.queue_client.read().await.is_none() {
            return;
        }
        let connected: std::collections::HashSet<DeviceId> =
            self.inner.peers.read().await.keys().copied().collect();
        let candidates: Vec<DeviceId> = self
            .inner
            .peer_store
            .lock()
            .await
            .list()
            .into_iter()
            .filter(|r| !connected.contains(&r.device()))
            .filter(|r| r.peer_node_id.is_some() && r.queue_pair_key.is_some())
            .map(|r| r.device())
            .collect();
        if candidates.is_empty() {
            return;
        }
        // Write-ahead: record every delivery intent in the durable outbox
        // BEFORE attempting any push. A crash mid-push then still leaves a
        // record the drainer retries — the change can't be silently lost.
        // `enqueue` returns whether this intent is due to attempt now: a new
        // intent, or an existing one past its backoff. We only spawn pushes
        // for those, so a burst of saves to an offline peer doesn't bypass
        // the backoff and hammer a dead queue.
        let mut to_attempt: Vec<DeviceId> = Vec::new();
        {
            let now = super::util::now_millis();
            let mut outbox = self.inner.outbox.lock().await;
            for peer in &candidates {
                match outbox.enqueue(*peer, &note, now) {
                    Ok(true) => to_attempt.push(*peer),
                    Ok(false) => {} // throttled — drainer retries when due
                    Err(e) => {
                        warn!(error = %e, "queue_push_to_offline_peers: outbox enqueue failed")
                    }
                }
            }
        }
        self.emit_outbox_status().await;
        for peer in to_attempt {
            // `attempt_outbox_delivery` clears the intent on success or
            // advances its backoff on failure. Spawned so one slow recipient
            // doesn't stall the others.
            let engine = self.clone();
            let note = note.clone();
            // SYN-I4/B7: tracked + cancel-aware. A queue push in flight at
            // `stop()` is dropped; the outbox intent persists and is retried on
            // next start (idempotent), so nothing is lost.
            self.spawn_tracked(move |token| async move {
                tokio::select! {
                    _ = token.cancelled() => {}
                    _ = engine.attempt_outbox_delivery(peer, note) => {}
                }
            });
        }
    }

    /// Nuke every locally-persisted Automerge doc for this roost.
    /// Paths on disk are left untouched — this only wipes the CRDT
    /// store. User-facing recovery path for the "two independent docs
    /// merged into one broken doc" bug: after reset, next peer connect
    /// triggers NoteList → RequestFullDoc → adoption, so both sides end
    /// up with a single, shared Automerge history per note.
    ///
    /// Disconnects every connected peer first so the sync state (heads,
    /// dirty set, sync.rs::PeerSyncState) doesn't get written back with
    /// stale info against the freshly-empty store.
    pub async fn reset_docstore(&self) -> SyncResult<()> {
        // Take the live peers out of the map and send each a Shutdown.
        let peers = {
            let mut guard = self.inner.peers.write().await;
            guard.drain().map(|(_, h)| h).collect::<Vec<_>>()
        };
        for peer in peers {
            peer.shutdown().await;
        }
        // SYN-B5: take the global mutation barrier so no in-flight per-note
        // mutation (local save, live/queue adopt, scan/recover) can interleave
        // with the wipe. Every mutator holds a READ of this barrier across its
        // commit+disk critical section (see `NoteLocks`); this WRITE waits for
        // them to drain and blocks new ones for the whole wipe + outbox clear.
        // Held OUTERMOST — before `doc_store` — matching the mutators' lock
        // order (barrier → note guard → doc_store), so there is no inversion.
        //
        // NOTE: `peer.shutdown()` above only *signals* each peer task (sends
        // `PeerControl::Shutdown`); it does NOT join the task. So a signalled
        // peer can still be mid-adopt (holding a NoteGuard) when we get here.
        // That's exactly what this barrier handles: the WRITE drains any such
        // in-flight adopt before the wipe. (Draining those tasks synchronously
        // is B7/W4's job; the barrier makes reset correct regardless.)
        let _barrier = self.inner.note_locks.block_all().await;
        let mut store = self.inner.doc_store.lock().await;
        store.wipe_all()?;
        drop(store);
        // The canonical docs the outbox regenerates payloads from are gone,
        // so every queued intent is now stale — clear them.
        if let Err(e) = self.inner.outbox.lock().await.clear() {
            warn!(error = %e, "reset_docstore: outbox clear failed");
        }
        // SYN-B8: pending tombstone-trash retries reference now-wiped docs;
        // clear them proactively (a stale entry would self-abandon on the next
        // tick anyway — `load_or_create` yields a fresh non-deleted doc — but
        // don't leave known-stale work sitting in the set).
        if let Err(e) = self.inner.pending_trash.lock().await.clear() {
            warn!(error = %e, "reset_docstore: pending_trash clear failed");
        }
        self.emit_outbox_status().await;
        info!(roost = %self.inner.roost.display(), "reset_docstore: wiped");
        Ok(())
    }

    /// SYN-B8 durable retry: re-attempt every failed tombstone-trash enrolled
    /// in `pending_trash` (by the live receive path or `mark_note_deleted`).
    /// Called on the reconnect-supervisor tick — the live-path analogue of the
    /// queue's retain-message → re-pull → re-attempt-trash. Only trashes notes
    /// STILL tombstoned in the DocStore: an explicit Restore drops the tombstone,
    /// and we then abandon the retry so a legitimately-restored file survives.
    /// Each trash runs under the note lock (SYN-I3).
    pub(super) async fn retry_pending_trashes(&self) {
        // SYN-I5: enrollment persistence can fail transiently (disk full,
        // app-private state temporarily unavailable). The store retains a
        // dirty in-memory snapshot in that case; retry it on every supervisor
        // tick so a later process exit cannot forget work after storage has
        // recovered.
        if let Err(e) = self.inner.pending_trash.lock().await.flush_pending() {
            warn!(error = %e, "retry_pending_trashes: pending intent persistence still failing");
        }
        let pending: Vec<NotePath> = {
            let set = self.inner.pending_trash.lock().await;
            if set.is_empty() {
                return;
            }
            set.snapshot()
        };
        for note in pending {
            // Hold the note lock across the tombstone check + disk trash so a
            // concurrent local save / adopt of the same note can't interleave.
            let _guard = self.inner.note_locks.lock(&note).await;
            // Three outcomes, deliberately distinguished (SYN-I5 — never drop
            // durable retry work on a *transient* error):
            //   Deleted   → still tombstoned, trash it below.
            //   Restored  → an explicit Restore dropped the tombstone (or the
            //               doc was recreated) → abandon; the file may stay.
            //   Transient → a docstore read/decode error → KEEP enrolled and
            //               retry next tick, don't abandon the delete.
            enum Check {
                Deleted,
                Restored,
                Transient,
            }
            let check = {
                let store = self.inner.doc_store.lock().await;
                match store.load_or_create(&note) {
                    // `load_or_create` yields a fresh non-deleted doc for an
                    // absent note → Restored (nothing to trash), which is right.
                    Ok(doc) => match doc.is_deleted() {
                        Ok(true) => Check::Deleted,
                        Ok(false) => Check::Restored,
                        Err(_) => Check::Transient,
                    },
                    Err(_) => Check::Transient,
                }
            };
            match check {
                Check::Restored => {
                    if let Err(e) = self.inner.pending_trash.lock().await.remove(&note) {
                        warn!(note = %note, error = %e, "retry_pending_trashes: remove (restored) failed");
                    }
                    continue;
                }
                // Keep the enrollment; a later tick re-checks once the docstore
                // read recovers.
                Check::Transient => continue,
                Check::Deleted => {}
            }
            match super::peer::trash_in_roost(
                &self.inner.roost,
                self.inner.roost_fs.as_deref(),
                &note,
            )
            .await
            {
                Ok(true) => {
                    let _ = self.inner.events.send(SyncEvent::NoteDeleted {
                        path: note.0.clone(),
                    });
                    if let Err(e) = self.inner.pending_trash.lock().await.remove(&note) {
                        warn!(note = %note, error = %e, "retry_pending_trashes: remove (trashed) failed");
                    }
                }
                // File already gone — the delete is effectively done.
                Ok(false) => {
                    if let Err(e) = self.inner.pending_trash.lock().await.remove(&note) {
                        warn!(note = %note, error = %e, "retry_pending_trashes: remove (already gone) failed");
                    }
                }
                // Still failing (roost still read-only) — keep it for next tick.
                Err(_) => {}
            }
        }
    }

    /// Same as `notify_local_write` but for a deletion. Delegates to
    /// `mark_note_deleted`, the single chokepoint that records the CRDT
    /// tombstone, fans out to live peers, and pushes to offline peers.
    pub async fn notify_local_remove(&self, note_path: NotePath) {
        self.mark_note_deleted(note_path).await;
    }

    /// Propagate a note rename/move: tombstone the OLD path and write the NEW
    /// path with its current content.
    ///
    /// Matches what the desktop file watcher already synthesizes for a rename
    /// (tombstone + fresh doc), but is driven explicitly by the rename/move IPC
    /// so it ALSO works on Android — the SAF `content://` watcher never fires,
    /// so before this a rename left the old note live on every peer, only
    /// surfaced the new path on its next edit, and `recover_missing_files`
    /// re-rendered the old path back onto the renaming device (duplicate notes
    /// locally and on every peer). Per Locked Decision #7 (path-based note ids)
    /// a rename intentionally starts a fresh history at the new path rather
    /// than carrying the old CRDT history across.
    pub async fn notify_local_rename(&self, old: NotePath, new: NotePath, new_content: String) {
        self.mark_note_deleted(old).await;
        self.notify_local_write(new, new_content).await;
    }

    /// Record a delete as a CRDT tombstone and propagate it everywhere.
    ///
    /// Ordering matters: we write `deleted=true` into the docstore
    /// **synchronously first**, so the queue push below reads a `doc.save()`
    /// that already carries the tombstone. (The live-peer fan-out goes through
    /// each peer task's `handle_local`, but the queue path reads the docstore
    /// directly — if we queued before writing, an offline peer could receive a
    /// stale pre-delete snapshot.)
    ///
    /// The on-disk markdown is normally trashed by the caller first (the delete
    /// IPC command, or the watcher reacting to an external removal). We STILL
    /// reconcile the disk here — under the per-note guard, right after the
    /// tombstone commit — so the operation is atomic: a peer adopt that recreated
    /// the file between the caller's trash and this commit is re-trashed before
    /// the guard drops, and disk can never be left disagreeing with the
    /// tombstone. Idempotent (a no-op when the file is already gone).
    pub async fn mark_note_deleted(&self, note_path: NotePath) {
        // Same roost-switch guard as `apply_local_change` (L-6): a delete
        // dequeued just before a roost switch could otherwise write a tombstone
        // into the OLD roost's DocStore after the new engine is up. Bail if the
        // engine has been stopped.
        if self.is_stopped() {
            return;
        }
        // Commit the tombstone AND reconcile the disk trash as ONE guarded unit
        // (commit+disk atomicity — see `note_locks`). `newly_deleted` gates the
        // peer fan-out below: on the desktop double-fire (IPC + watcher both
        // reach here) the second pass still runs the disk reconcile but skips
        // re-propagating.
        let newly_deleted;
        {
            let _note_guard = self.inner.note_locks.lock(&note_path).await;
            // 1. Persist the tombstone.
            {
                let store = self.inner.doc_store.lock().await;
                match store.load_or_create(&note_path) {
                    Ok(mut doc) => match doc.is_deleted() {
                        // Already tombstoned (double-fire). Skip the redundant
                        // CRDT op — writing a second tombstone would clobber
                        // `deleted_at` — but still fall through to the disk
                        // reconcile below in case a copy was recreated.
                        Ok(true) => newly_deleted = false,
                        Ok(false) => {
                            if let Err(e) = doc.mark_deleted(super::util::now_millis()) {
                                warn!(note = %note_path, error = %e, "mark_note_deleted: mark_deleted failed");
                                return;
                            }
                            if let Err(e) = store.save(&note_path, &mut doc) {
                                warn!(note = %note_path, error = %e, "mark_note_deleted: docstore save failed");
                                return;
                            }
                            newly_deleted = true;
                        }
                        Err(e) => {
                            warn!(note = %note_path, error = %e, "mark_note_deleted: is_deleted check failed");
                            return;
                        }
                    },
                    Err(e) => {
                        warn!(note = %note_path, error = %e, "mark_note_deleted: load_or_create failed");
                        return;
                    }
                }
            }
            // 2. Reconcile disk under the SAME guard: make the file agree with
            //    the tombstone. Normal path — already trashed by the caller —
            //    returns Ok(false) (nothing to remove). If a concurrent adopt
            //    recreated the file, `Ok(true)` means we just re-trashed it, so
            //    emit NoteDeleted for UIs without a live watcher (SAF).
            match self.trash_note(&note_path).await {
                Ok(true) => {
                    let _ = self.inner.events.send(SyncEvent::NoteDeleted {
                        path: note_path.0.clone(),
                    });
                }
                Ok(false) => {}
                // SYN-I5/B8: the tombstone is committed but the disk trash
                // failed. The startup scan does NOT re-reconcile (it SKIPS
                // tombstoned-present notes to avoid auto-un-deleting a user's
                // recreated file — `scan.rs`), so without this the file would
                // survive silently. Surface it AND enroll for durable retry —
                // the supervisor re-attempts the trash each tick (twin of the
                // live receive path).
                Err(e) => {
                    warn!(note = %note_path, error = %e, "mark_note_deleted: disk reconcile-trash failed — enrolling for retry");
                    let _ = self.inner.events.send(SyncEvent::NoteSyncFailed {
                        path: note_path.0.clone(),
                        reason: format!("could not trash deleted note: {e}"),
                    });
                    if let Err(e) = self.inner.pending_trash.lock().await.insert(&note_path) {
                        warn!(note = %note_path, error = %e, "mark_note_deleted: pending_trash insert failed");
                    }
                }
            }
        }
        // Already tombstoned: the first pass fanned out + queued; only the disk
        // reconcile above needed re-running this time.
        if !newly_deleted {
            return;
        }

        // 3. Fan out to live peers. Their `handle_local` sees the doc is
        //    already deleted and just marks it dirty, so the tombstone
        //    propagates via the normal Automerge sync path.
        let change = LocalChange::Removed {
            path: note_path.clone(),
        };
        let peers: Vec<PeerHandle> = self
            .inner
            .peers
            .read()
            .await
            .values()
            .map(clone_handle)
            .collect();
        for handle in peers {
            if let Err(e) = handle.send_local(change.clone()) {
                debug!(
                    peer = %handle.display_name,
                    error = %e,
                    "mark_note_deleted: peer channel closed/full, queueing for retry"
                );
                // Live-drop coverage for deletes — mirror the write path.
                // The docstore tombstone is already written (step 1), so the
                // regenerated FullDoc carries `deleted=true`.
                self.enqueue_outbox_for_peer(handle.device_id, note_path.clone())
                    .await;
                self.record_live_pending(handle.device_id, note_path.clone())
                    .await;
            }
        }

        // 3. Push to offline peers via the queue. The FullDoc snapshot now
        //    carries `deleted=true`, so an offline peer learns of the delete
        //    on its next pull — no separate delete protocol needed.
        self.queue_push_to_offline_peers(note_path).await;
    }

    /// Clear a note's tombstone (the inverse of `mark_note_deleted`). This is
    /// the sync-side of "undo a delete": `revert_edit` writes the content back
    /// to disk, but unless we also clear `deleted=true` here, the next
    /// `reconcile_doc_to_roost` would re-trash the file and the restore would
    /// never reach peers.
    ///
    /// Returns immediately if the note isn't actually tombstoned, so callers
    /// can invoke it unconditionally after any revert. Live-peer propagation
    /// rides the watcher's subsequent `Modified` event (the same path a normal
    /// edit-revert uses) — by then the docstore flag is already cleared, so
    /// `handle_local` propagates `deleted=false`. We still push to offline
    /// peers here so the cleared flag reaches them via the queue's FullDoc.
    pub async fn restore_note(&self, note_path: NotePath) {
        {
            let _note_guard = self.inner.note_locks.lock(&note_path).await;
            let store = self.inner.doc_store.lock().await;
            let mut doc = match store.load_or_create(&note_path) {
                Ok(d) => d,
                Err(e) => {
                    warn!(note = %note_path, error = %e, "restore_note: load_or_create failed");
                    return;
                }
            };
            match doc.is_deleted() {
                Ok(true) => {}
                // Not deleted (or unreadable flag) → nothing to restore.
                Ok(false) => return,
                Err(e) => {
                    warn!(note = %note_path, error = %e, "restore_note: is_deleted check failed");
                    return;
                }
            }
            if let Err(e) = doc.restore() {
                warn!(note = %note_path, error = %e, "restore_note: restore failed");
                return;
            }
            if let Err(e) = store.save(&note_path, &mut doc) {
                warn!(note = %note_path, error = %e, "restore_note: docstore save failed");
                return;
            }
            // Capture the restored text before dropping the store lock, so the
            // disk reconcile below runs under the note guard but off the store
            // lock. A tombstone preserves the doc's text, so this equals the
            // pre-delete content the IPC's `write_atomic` already wrote.
            let restored_text = match doc.render() {
                Ok(t) => t,
                Err(e) => {
                    // Committed the restore; a later scan/recover renders disk.
                    warn!(note = %note_path, error = %e, "restore_note: render failed (restore committed; disk left for recover)");
                    return;
                }
            };
            drop(store);
            // Reconcile disk under the SAME guard: write the restored content
            // back so a concurrent adopt that trashed the file (between the
            // IPC's `write_atomic` and here) can't leave disk empty while the
            // doc says restored. Idempotent with the IPC write (same bytes).
            if let Err(e) = self.write_recovered_note(&note_path, &restored_text).await {
                warn!(note = %note_path, error = %e, "restore_note: disk reconcile-write failed (restore committed; a later scan re-renders)");
            }
        }
        // The cleared flag reaches offline peers via the queue FullDoc.
        self.queue_push_to_offline_peers(note_path).await;
    }

    /// Reap tombstones older than [`TOMBSTONE_RETENTION_SECS`]. Run once per
    /// startup — slow accumulation doesn't warrant a perpetual timer.
    ///
    /// Retention is deliberately **decoupled from the queue's 30-day TTL**.
    /// The queue TTL bounds *server storage*; tombstone retention bounds *how
    /// long a device can be offline and still learn of a delete on reconnect*.
    /// While a tombstone is retained, a returning peer converges correctly:
    /// we still advertise the note in our `NoteList`, and the shared Automerge
    /// history carries the delete op, so the peer's live copy merges to
    /// deleted. Resurrection only becomes possible once we reap and forget the
    /// note entirely (the peer then re-introduces its stale live copy via
    /// FullDoc adoption). 180 days covers a phone left in a drawer for ~6
    /// months; a device offline longer than that should be re-paired.
    /// See `docs/decisions/tombstone-retention.md`.
    pub async fn reap_old_tombstones(&self) {
        let retention_ms = TOMBSTONE_RETENTION_SECS * 1000;
        let now = super::util::now_millis();
        let cutoff = now.saturating_sub(retention_ms);
        let store = self.inner.doc_store.lock().await;
        match store.gc_tombstones(now, cutoff) {
            Ok(0) => {}
            Ok(n) => info!(
                reaped = n,
                "reap_old_tombstones: GC'd expired note tombstones"
            ),
            Err(e) => warn!(error = %e, "reap_old_tombstones: GC sweep failed"),
        }
    }
}
