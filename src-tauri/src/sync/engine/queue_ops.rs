//! Queue (store-and-forward) operations on `SyncEngine`.
//!
//! Owns the lifecycle of the optional `QueueClient` plus the push/pull
//! plumbing that mirrors a `FullDoc` payload through the queue server
//! when a peer is currently offline. Split off `engine.rs` to keep the
//! orchestrator file focused on lifecycle and direct peer wiring.
//!
//! The queue is **opt-in** — `set_queue_client(None)` is the strict
//! P2P-only mode and every method here no-ops in that state.

use std::sync::Arc;
use std::time::Duration;

use tracing::{debug, warn};

use crate::sync::crdt::NoteDoc;
use crate::sync::crypto::envelope::EnvelopeCipher;
use crate::sync::error::{SyncError, SyncResult};
use crate::sync::identity::{DeviceId, Identity};
use crate::sync::protocol::NotePath;
use crate::sync::queue::client::{QueueClient, QueueClientError};
use crate::sync::queue::protocol::{QueueErrorCode, QueuePayload};

use super::engine::SyncEngine;
use super::peer::ConflictBackup;
use super::state::SyncEvent;

/// How often the periodic queue puller runs while a `QueueClient` is
/// installed. 5 minutes balances "user notices new messages soon enough"
/// against "we're not hammering a beta-tier service".
const QUEUE_PULL_INTERVAL: Duration = Duration::from_secs(300);

/// Domain tag prefixing the store-and-forward queue envelope AAD (M-2).
/// Distinct from the live-transport AAD so the two contexts can't be confused.
const QUEUE_ENV_AAD_TAG: &[u8] = b"corvux-queue-env-v2";

/// SYN-M7: build the DIRECTIONAL AAD for a queue envelope. Binds the domain
/// tag, the SENDER then RECIPIENT NodeId, and the protocol version. The per-pair
/// queue key is the SAME in both directions (one HKDF subkey of the shared
/// pairing `SessionSecret`), so a CONSTANT AAD (the old `-v1`) let a hostile
/// relay REFLECT a frame A queued for B back into A's inbox as if from B — A
/// held the same key + same AAD, so it decrypted. Binding sender→recipient
/// makes an A→B frame (`aad(A,B)`) fail to open as B→A (`aad(B,A)`), closing the
/// reflection. Bumping the tag to `-v2` is a deliberate clean break: an in-flight
/// pre-upgrade frame no longer decrypts and is reaped as unrecoverable (M-8) —
/// harmless, since the live path re-exchanges that change on reconnect.
fn queue_env_aad(sender: &[u8; 32], recipient: &[u8; 32]) -> Vec<u8> {
    let mut aad = Vec::with_capacity(QUEUE_ENV_AAD_TAG.len() + 32 + 32 + 2);
    aad.extend_from_slice(QUEUE_ENV_AAD_TAG);
    aad.extend_from_slice(sender);
    aad.extend_from_slice(recipient);
    aad.extend_from_slice(&crate::sync::queue::protocol::QUEUE_PROTOCOL_VERSION.to_le_bytes());
    aad
}

impl SyncEngine {
    /// Install (or remove) the store-and-forward queue client. When
    /// `Some`, spins up a background puller that runs every
    /// `QUEUE_PULL_INTERVAL`; when `None`, aborts the puller (if any) and
    /// drops the client. Calling with the same value back-to-back is
    /// safe — the prior puller is always aborted first.
    ///
    /// The puller task is detached (no JoinHandle exposed outside the
    /// engine) and self-exits as soon as the client is set back to
    /// `None`, so callers don't need to thread cancellation through.
    pub async fn set_queue_client(&self, client: Option<Arc<QueueClient>>) {
        // SYN-B6: serialize against stop(). A stopped engine must not install a
        // puller/drainer; `set_queue_client(None)` on a stopped engine is a
        // harmless no-op (stop() already cleared the client + tasks).
        let _life = self.inner.lifecycle.lock().await;
        if self.is_stopped() {
            return;
        }
        // Abort any prior puller/drainer before swapping the client,
        // otherwise an old task could fire one last call against a stale
        // client.
        if let Some(task) = self.inner.queue_pull_task.lock().await.take() {
            task.abort();
        }
        if let Some(task) = self.inner.outbox_drain_task.lock().await.take() {
            task.abort();
        }
        *self.inner.queue_client.write().await = client.clone();
        if client.is_some() {
            let engine = self.clone();
            // SYN-I4: tracked + cancel-aware. Periodic queue puller — breaks on
            // cancel so `stop()` drains it; stored handle backs it up.
            let task = self.spawn_tracked(move |token| async move {
                if let Err(e) = engine.try_queue_pull_all().await {
                    tracing::warn!(error = %e, "queue startup pull failed");
                }
                let mut interval = tokio::time::interval(QUEUE_PULL_INTERVAL);
                interval.tick().await; // drop the immediate first tick
                loop {
                    tokio::select! {
                        _ = token.cancelled() => return,
                        _ = interval.tick() => {}
                    }
                    // If the client was removed between ticks, exit.
                    if engine.inner.queue_client.read().await.is_none() {
                        return;
                    }
                    if let Err(e) = engine.try_queue_pull_all().await {
                        tracing::warn!(error = %e, "queue periodic pull failed");
                    }
                }
            });
            *self.inner.queue_pull_task.lock().await = Some(task);

            // Outbox drainer: flush anything left from a previous run first
            // (startup drain), then re-attempt due entries on a tighter
            // cadence than the puller.
            let engine = self.clone();
            // SYN-I4: tracked + cancel-aware (twin of the queue puller).
            let drain_task = self.spawn_tracked(move |token| async move {
                engine.drain_outbox().await;
                let mut interval = tokio::time::interval(super::outbox::OUTBOX_DRAIN_INTERVAL);
                interval.tick().await; // drop the immediate first tick
                loop {
                    tokio::select! {
                        _ = token.cancelled() => return,
                        _ = interval.tick() => {}
                    }
                    if engine.inner.queue_client.read().await.is_none() {
                        return;
                    }
                    engine.drain_outbox().await;
                }
            });
            *self.inner.outbox_drain_task.lock().await = Some(drain_task);
        }
    }

    /// True iff a `QueueClient` is currently installed.
    pub async fn queue_client_active(&self) -> bool {
        self.inner.queue_client.read().await.is_some()
    }

    /// Shared `Arc<Identity>` — needed by `QueueClient::new`. Cheap
    /// clone (already an Arc internally).
    pub fn identity_arc(&self) -> Arc<Identity> {
        self.inner.identity.clone()
    }

    /// Encrypt the local Automerge save for `note` with the per-pair
    /// queue key for `peer_device` and push it to the configured queue.
    /// Skips silently if no queue client is installed, the peer has no
    /// persisted queue key, or the peer has no NodeId (TLS-only pair).
    ///
    /// Intended call site: anywhere a local change has been written to
    /// the docstore for a peer that's currently *disconnected*. The
    /// queue server de-dupes by `(sender, recipient, dedup_key)` so
    /// rapid edits to one note collapse to a single pending entry per
    /// recipient.
    pub async fn try_queue_push(&self, peer_device: DeviceId, note: NotePath) -> SyncResult<bool> {
        let client = match self.inner.queue_client.read().await.clone() {
            Some(c) => c,
            None => return Ok(false),
        };

        // Pick up the persisted recipient address + per-pair key.
        let (recipient_node_id, pair_key) = {
            let store = self.inner.peer_store.lock().await;
            match store.get(peer_device) {
                Some(rec) => match (rec.peer_node_id, rec.queue_pair_key) {
                    (Some(n), Some(k)) => (n, k),
                    _ => return Ok(false),
                },
                None => return Ok(false),
            }
        };

        // Compute the FullDoc save for this note.
        let doc_save = {
            let store = self.inner.doc_store.lock().await;
            let mut doc = store.load_or_create(&note)?;
            doc.save()
        };
        let plaintext = crate::sync::queue::protocol::encode(&QueuePayload {
            path: note.0.clone(),
            doc_save,
        })
        .map_err(|e| SyncError::Io(std::io::Error::other(e)))?;

        // Encrypt with the per-pair queue key. Wire shape: nonce(24) || ciphertext.
        // AUDIT-048: `pair_key` is bytes persisted from the original
        // `derive_subkey(session, b"corvux-queue-pair-v1")` call at
        // handshake time (peer.rs:169). Re-wrap via the audited escape
        // hatch so the type system still enforces domain separation at
        // the cipher boundary.
        let cipher = EnvelopeCipher::from_raw_key(
            crate::sync::crypto::pairing::DomainSeparatedKey::from_derived_bytes(pair_key),
        );
        // M-2 + SYN-M7: directional AAD (sender=us, recipient=peer) so a queue
        // frame can't be opened as a live-transport frame NOR reflected back to
        // us as if the peer sent it (the per-pair key is bidirectional).
        let sender_node_id = self.inner.identity.verifying_key().to_bytes();
        let aad = queue_env_aad(&sender_node_id, &recipient_node_id);
        let sealed = cipher.seal(&plaintext, &aad)?;
        let mut combined = Vec::with_capacity(24 + sealed.ciphertext.len());
        combined.extend_from_slice(&sealed.nonce);
        combined.extend_from_slice(&sealed.ciphertext);

        // AUDIT-001: dedup-key is HMAC(pair_key, "queue-dedup-v1" || path).
        // The server still dedups (same sender + recipient + path → same
        // tag → collapse), but it never sees the cleartext path. Locked
        // Decision #6 floor: vendor never sees plaintext, even in metadata.
        // 16-byte truncation is plenty: collision space dwarfs any
        // realistic per-peer queue depth.
        let dedup_key = crate::sync::queue::dedup_tag(&pair_key, &note.0);

        if let Err(e) = client.push(recipient_node_id, dedup_key, combined).await {
            warn!(error = %e, "try_queue_push: push failed (will retry on next local change)");
            return Ok(false);
        }
        Ok(true)
    }

    /// Pull every pending queue message addressed to this device's
    /// NodeId, decrypt with the matching per-pair key, apply as a
    /// FullDoc into the local docstore, render to disk, and ACK-delete.
    ///
    /// Returns the number of messages successfully applied. Per-message
    /// failures are logged and skipped — one bad payload mustn't block
    /// the rest of the queue draining.
    ///
    /// Intended call site: app startup, after a peer reconnects, or on
    /// explicit user action ("Sync queue now"). Safe to call repeatedly
    /// — the server's ACK-and-delete semantics ensure no duplicate
    /// application.
    ///
    /// A concurrent local edit can dedup-**replace** a queued message between
    /// our `manifest()` snapshot and our `pull(id)` — the store evicts the old
    /// row and appends a fresh one with a NEW id (see `InMemoryMessageStore::
    /// store`). Our `pull(id)` then 404s while a valid message for the same
    /// slot sits in the queue under an id we never saw. A single pass would
    /// skip it and defer delivery to the next scheduled drain (racy for the
    /// on-demand "Sync now" path). So we re-fetch the manifest and re-drain
    /// within this call whenever we observe such a stale (404) entry, bounded
    /// by `MAX_DRAIN_PASSES` so a continuous push storm can't spin the drain.
    pub async fn try_queue_pull_all(&self) -> SyncResult<usize> {
        /// Liveness bound on the stale-id re-drain. A replacement lands under a
        /// new id within one round-trip, so 2 passes covers the common race;
        /// the extra headroom absorbs a short burst. Beyond it, the periodic
        /// drain picks up whatever's left — we never loop unboundedly.
        const MAX_DRAIN_PASSES: usize = 8;

        let client = match self.inner.queue_client.read().await.clone() {
            Some(c) => c,
            None => return Ok(0),
        };

        let mut applied = 0usize;
        for _ in 0..MAX_DRAIN_PASSES {
            let entries = match client.manifest().await {
                Ok(e) => e,
                Err(e) => {
                    warn!(error = %e, "try_queue_pull_all: manifest fetch failed");
                    break;
                }
            };
            if entries.is_empty() {
                break;
            }

            // Set when a manifest id 404s on pull — a newer message for that
            // slot may be queued under an id we haven't seen, so re-fetch.
            let mut saw_stale = false;

            for entry in entries {
                // Resolve sender NodeId → peer record so we know which key to use.
                let resolved = {
                    let store = self.inner.peer_store.lock().await;
                    store.list().into_iter().find_map(|rec| {
                        if rec.peer_node_id == Some(entry.sender) {
                            rec.queue_pair_key.map(|k| (rec.device(), k))
                        } else {
                            None
                        }
                    })
                };
                let (peer_device, pair_key) = match resolved {
                    Some(t) => t,
                    None => {
                        // M-8 head-of-line fix: we hold no key for this sender and
                        // never will without re-pairing, so this ciphertext is dead
                        // weight — DELETE it rather than skip. A skipped entry sits
                        // at the FIFO head for up to the 30-day TTL and blocks every
                        // newer message behind it; an attacker who pushes garbage to
                        // our NodeId weaponizes exactly that. Re-pairing re-pushes
                        // current state, so deleting loses nothing recoverable.
                        debug!(
                            sender = %hex::encode(entry.sender),
                            "try_queue_pull_all: no key for sender — deleting unrecoverable queued message"
                        );
                        let _ = client.delete(entry.message_id).await;
                        continue;
                    }
                };

                let combined = match client.pull(entry.message_id).await {
                    Ok(b) => b,
                    Err(QueueClientError::Server(QueueErrorCode::NotFound, _)) => {
                        // Dedup-replaced (new id) or already ACK-deleted between
                        // our manifest snapshot and this pull. A replacement may
                        // be queued under a fresh id — re-drain to pick it up.
                        saw_stale = true;
                        continue;
                    }
                    Err(e) => {
                        // Transient (network / server) — keep it and retry next
                        // cycle; do NOT delete.
                        warn!(error = %e, "try_queue_pull_all: pull failed — skipping (will retry)");
                        continue;
                    }
                };
                // The remaining failures below are all UNRECOVERABLE for this
                // message (malformed frame, undecryptable under our current key,
                // undecodable plaintext). Deleting them keeps the FIFO head moving
                // (M-8) — a message we can't ever process must not block the queue.
                if combined.len() < 24 + 16 {
                    warn!("try_queue_pull_all: payload shorter than nonce+tag — deleting");
                    let _ = client.delete(entry.message_id).await;
                    continue;
                }
                let mut nonce = [0u8; 24];
                nonce.copy_from_slice(&combined[..24]);
                // AUDIT-048: re-wrap persisted derived bytes (see push path).
                let cipher = EnvelopeCipher::from_raw_key(
                    crate::sync::crypto::pairing::DomainSeparatedKey::from_derived_bytes(pair_key),
                );
                // SYN-M7: open with the directional AAD (sender=the queued
                // sender, recipient=us). A frame the relay reflected back to us
                // (claiming a different sender than it was sealed for) fails
                // here instead of decrypting.
                let recipient_node_id = self.inner.identity.verifying_key().to_bytes();
                let aad = queue_env_aad(&entry.sender, &recipient_node_id);
                let plaintext = match cipher.open(&nonce, &combined[24..], &aad) {
                    Ok(p) => p,
                    Err(e) => {
                        // Keys rotate forward on every handshake, so an old-key
                        // ciphertext is permanently undecryptable — delete it.
                        warn!(error = %e, "try_queue_pull_all: decrypt failed — deleting (stale key, unrecoverable)");
                        let _ = client.delete(entry.message_id).await;
                        continue;
                    }
                };
                let payload: QueuePayload = match crate::sync::queue::protocol::decode(&plaintext) {
                    Ok(p) => p,
                    Err(e) => {
                        warn!(error = %e, "try_queue_pull_all: payload decode failed — deleting (malformed)");
                        let _ = client.delete(entry.message_id).await;
                        continue;
                    }
                };

                // Apply: merge the FullDoc into local. Mirrors the in-engine
                // FullDoc-adoption path in `peer.rs`, simplified — no
                // circuit breaker / poison set, since the queue server
                // already de-duped and the message is single-shot.
                let note = NotePath(payload.path.clone());
                // Validate the peer-supplied path BEFORE it reaches the store,
                // through the SAME `vet_remote_rel` policy the live frame handlers
                // use (SYN-B1): traversal/absolute/NUL/drive, hidden/reserved dirs
                // (`.git`, `.corvux`), and markdown-only. A malicious/buggy *paired*
                // peer could otherwise ship `.git/config` or a non-note file. A
                // policy-rejected path is permanently unprocessable, so delete it to
                // keep the FIFO head moving — never return Err here (that retries the
                // message forever). Twin of the live gate in peer.rs.
                if super::policy::vet_remote_rel(&note).is_err() {
                    warn!(path = %note, "try_queue_pull_all: note path rejected by write policy — deleting (unprocessable)");
                    let _ = client.delete(entry.message_id).await;
                    continue;
                }
                if let Err(e) = self
                    .apply_queue_full_doc(peer_device, note.clone(), &payload.doc_save)
                    .await
                {
                    warn!(error = %e, path = %note, "try_queue_pull_all: apply failed — skipping");
                    continue;
                }

                // ACK-and-delete. A NotFound here means the id we just pulled was
                // dedup-replaced (a newer message for this slot is queued under a
                // fresh id) or already ACK-deleted between our pull and now —
                // either way a replacement may be waiting, so mark the pass stale
                // to re-drain and pick it up. This mirrors the pull-404 handling
                // above; without it a replacement pushed *after* a successful pull
                // but *before* this delete is missed, and "Sync now" leaves stale
                // state on disk until the next periodic pull. Other delete
                // failures are transient: the message resurfaces next pull and
                // application is idempotent, so just warn.
                match client.delete(entry.message_id).await {
                    Ok(()) => {}
                    Err(QueueClientError::Server(QueueErrorCode::NotFound, _)) => {
                        saw_stale = true;
                    }
                    Err(e) => {
                        warn!(error = %e, "try_queue_pull_all: delete failed — message will be re-applied next pull (idempotent)");
                    }
                }
                applied += 1;
            }

            // Only re-drain when a manifest id went stale mid-pass (dedup
            // replacement may be waiting under a new id). Transient-skipped and
            // fully-drained passes fall through to the next scheduled drain.
            if !saw_stale {
                break;
            }
        }
        Ok(applied)
    }

    /// Internal: apply a queued FullDoc to local state. Same shape as
    /// the FullDoc adoption branch in `peer::handle_inbound`, narrowed
    /// to the queue's needs. Adopts under OUR actor — see the
    /// `our_actor_id` rationale on the peer-side path.
    async fn apply_queue_full_doc(
        &self,
        _peer_device: DeviceId,
        note: NotePath,
        bytes: &[u8],
    ) -> SyncResult<()> {
        // B1 recommit race (CAS): the adopt computes `doc`, then RELEASES the
        // doc-store lock to do the fail-closed backup I/O, then re-locks to
        // commit. A concurrent local save (the editor's own save path takes the
        // same lock) can land in that window — and blindly committing `doc`
        // would silently overwrite it. So we snapshot the stored doc's heads
        // (a content version token) under the first lock and, at commit, only
        // save if they're UNCHANGED. If a concurrent write bumped them, we
        // re-run the whole adoption against the fresh state (the reloaded local
        // now includes the concurrent edit, so the merge/quarantine logic does
        // the right thing) — bounded, then bail to "retry on next pull" so we
        // never spin or clobber. Backups re-run harmlessly: they're
        // content-addressed (idempotent).
        // Serialize the WHOLE queue adopt (CAS loop commit → post-loop disk
        // render/trash) against local saves and the live-peer adopt of this
        // note. The inner heads-CAS is now belt-and-suspenders under this
        // guard but kept for defence in depth. Held outermost, before the
        // DocStore lock. See `note_locks`.
        let _note_guard = self.inner.note_locks.lock(&note).await;
        const MAX_ADOPT_ATTEMPTS: usize = 4;
        let mut doc = None;
        for attempt in 1..=MAX_ADOPT_ATTEMPTS {
            // ── Snapshot + compute under the lock ──
            let mut conflict_backup: Option<String> = None;
            // Whether this is a FRESH adoption (no prior DocStore record). Drives
            // the Hole-A untracked-file guard below — the same guard the live
            // path has, previously absent here.
            let mut adopted_fresh = false;
            // The stored doc's heads BEFORE we touch it — our CAS token. `None`
            // means "no doc on disk for this note" (a concurrent create then
            // shows up as a heads mismatch too).
            let mut snapshot_heads: Option<Vec<automerge::ChangeHash>> = None;
            let mut candidate = {
                let store = self.inner.doc_store.lock().await;
                let our_actor = store.our_actor_id();
                if store.exists(&note) {
                    let mut local = store.load_or_create(&note)?;
                    // Capture heads BEFORE the merge mutates `local`.
                    snapshot_heads = Some(local.heads());
                    match NoteDoc::load_with_actor(bytes, our_actor.clone()) {
                        Ok(mut remote) => match local.merge(&mut remote) {
                            Ok(_) => local,
                            Err(_) => {
                                // QUARANTINE: a queued FullDoc that fails to
                                // merge would otherwise silently overwrite local
                                // edits made while this device was offline —
                                // exactly the window the queue exists to bridge.
                                // Save them first.
                                conflict_backup =
                                    local.render().ok().filter(|t| !t.trim().is_empty());
                                NoteDoc::load_with_actor(bytes, our_actor)?
                            }
                        },
                        Err(e) => return Err(e),
                    }
                } else {
                    adopted_fresh = true;
                    NoteDoc::load_with_actor(bytes, our_actor)?
                }
                // lock dropped here — backup I/O must not hold it
            };

            // ── FAIL-CLOSED backups, lock released ──
            // (a) Merge-divergence: local CRDT edits captured just before the
            //     adopt discarded them.
            if let Some(local_text) = conflict_backup.take() {
                match super::peer::write_conflict_artifact(
                    &self.inner.roost,
                    self.inner.roost_fs.as_deref(),
                    &note,
                    &local_text,
                )
                .await
                {
                    // Saved, or nothing worth saving — safe to adopt.
                    ConflictBackup::Saved(_) | ConflictBackup::NothingToSave => {}
                    // Couldn't save the local edits anywhere — refuse to adopt so
                    // the primary file (still the only copy) isn't overwritten.
                    ConflictBackup::Failed => {
                        warn!(
                            path = %note,
                            "apply_queue_full_doc: conflict backup FAILED — NOT adopting queued remote over divergent local edits; keeping local copy, will retry"
                        );
                        let _ = self.inner.events.send(SyncEvent::NoteSyncFailed {
                            path: note.0.clone(),
                            reason: "couldn't back up local edits before adopting queued remote — kept local copy, will retry".into(),
                        });
                        return Err(SyncError::Io(std::io::Error::other(
                            "queue conflict backup failed — adopt aborted",
                        )));
                    }
                }
            }

            // (b) Hole A: a FRESH adoption (we had no DocStore record) is about
            // to render the queued copy over whatever untracked file sits on
            // disk at this path. That on-disk content was never in any CRDT, so
            // the merge-divergence backup above can't fire. Snapshot it FIRST; a
            // failed snapshot must likewise block the adopt (same fail-closed
            // rule) so a first-drain overwrite (the SAF bootstrap case
            // especially) can't silently erase notes this device wrote before
            // the queue drained. Skipped for tombstone adoptions (trash is
            // already recoverable) and when the on-disk bytes match.
            if adopted_fresh && !candidate.is_deleted().unwrap_or(false) {
                if let Ok(incoming) = candidate.render() {
                    if let Some(existing) = super::peer::read_note_on_disk(
                        &self.inner.roost,
                        self.inner.roost_fs.as_deref(),
                        &note,
                    )
                    .await
                    {
                        let existing_text = String::from_utf8_lossy(&existing);
                        if !existing_text.trim().is_empty() && existing_text != incoming {
                            match super::peer::write_conflict_artifact(
                                &self.inner.roost,
                                self.inner.roost_fs.as_deref(),
                                &note,
                                &existing_text,
                            )
                            .await
                            {
                                ConflictBackup::Saved(conflict) => {
                                    // Surface the recovery copy so the file tree
                                    // refreshes (SAF has no live watcher).
                                    let _ = self
                                        .inner
                                        .events
                                        .send(SyncEvent::NoteWritten { path: conflict.0 });
                                }
                                ConflictBackup::NothingToSave => {}
                                ConflictBackup::Failed => {
                                    warn!(
                                        path = %note,
                                        "apply_queue_full_doc: Hole-A backup FAILED — NOT adopting over untracked on-disk note; keeping local file, will retry"
                                    );
                                    let _ = self.inner.events.send(SyncEvent::NoteSyncFailed {
                                        path: note.0.clone(),
                                        reason: "couldn't back up an existing local note before adopting queued remote — kept local copy, will retry".into(),
                                    });
                                    return Err(SyncError::Io(std::io::Error::other(
                                        "queue Hole-A backup failed — adopt aborted",
                                    )));
                                }
                            }
                        }
                    }
                }
            }

            // ── Commit under the lock, guarded by the CAS token ──
            {
                let store = self.inner.doc_store.lock().await;
                let current_heads: Option<Vec<automerge::ChangeHash>> = if store.exists(&note) {
                    Some(store.load_or_create(&note)?.heads())
                } else {
                    None
                };
                if current_heads == snapshot_heads {
                    store.save(&note, &mut candidate)?;
                    doc = Some(candidate);
                    break;
                }
                // else: a concurrent write landed in the backup window — fall
                // through, drop the lock, and re-run the adoption against the
                // now-current state.
            }
            if attempt == MAX_ADOPT_ATTEMPTS {
                warn!(
                    path = %note,
                    "apply_queue_full_doc: lost the recommit CAS race {MAX_ADOPT_ATTEMPTS}× (heavy concurrent local edits) — leaving the queued message for the next pull"
                );
                let _ = self.inner.events.send(SyncEvent::NoteSyncFailed {
                    path: note.0.clone(),
                    reason: "note was being edited during sync — will retry".into(),
                });
                return Err(SyncError::Io(std::io::Error::other(
                    "queue adopt lost CAS race repeatedly — will retry",
                )));
            }
        }
        // `doc` is Some once we break out of the loop with a committed adoption;
        // every other path above returns early.
        let doc = doc.expect("adopt loop commits or returns before here");

        // Honor the tombstone: a queued FullDoc can carry `deleted=true` (the
        // sender deleted the note while we were offline). Trash the file
        // instead of writing it — same convergence rule as the live sync path
        // in `peer::reconcile_doc_to_roost`. Without this, the queue path
        // would resurrect every note the peer deleted offline.
        if doc.is_deleted()? {
            // ACK-after-disk (H-6) for deletions, mirroring the write path
            // below. `trash_note` now distinguishes the three cases:
            //   Ok(true)  — file removed → fire NoteDeleted, then ACK.
            //   Ok(false) — nothing on disk (NotFound) → disk already agrees
            //               with the tombstone, safe to ACK, no event.
            //   Err(e)    — the delete genuinely failed → do NOT ACK. The CRDT
            //               is tombstoned but the note still exists on disk, so
            //               a later scan would resurrect it. Emit NoteSyncFailed
            //               and return Err so the caller keeps the message for
            //               retry (apply is idempotent — the tombstone is already
            //               saved, a retry only re-attempts the disk delete).
            match self.trash_note(&note).await {
                Ok(removed) => {
                    if removed {
                        let _ = self
                            .inner
                            .events
                            .send(SyncEvent::NoteDeleted { path: note.0 });
                    }
                    return Ok(());
                }
                Err(e) => {
                    warn!(error = %e, path = %note, "apply_queue_full_doc: trash to disk failed — not ACKing (will retry)");
                    let _ = self.inner.events.send(SyncEvent::NoteSyncFailed {
                        path: note.0.clone(),
                        reason: "trash on disk failed".into(),
                    });
                    return Err(e);
                }
            }
        }

        // Render to disk so the user sees the update. ACK-after-disk (H-6): a
        // failed write must NOT be reported as success. On failure, emit
        // NoteSyncFailed and return Err so the caller keeps the queue message
        // for retry (apply is idempotent — the CRDT is already saved, so a retry
        // only re-attempts the disk render). Previously this only `warn!`ed and
        // still emitted NoteWritten + deleted the message, leaving disk and
        // DocStore divergent; on desktop a later scan then re-applied the stale
        // disk copy back into the CRDT. Mirrors `peer::reconcile_doc_to_roost`.
        if let Err(e) = self.write_recovered_note(&note, &doc.render()?).await {
            warn!(error = %e, path = %note, "apply_queue_full_doc: render to disk failed — not ACKing (will retry)");
            let _ = self.inner.events.send(SyncEvent::NoteSyncFailed {
                path: note.0.clone(),
                reason: "render to disk failed".into(),
            });
            return Err(e);
        }
        let _ = self
            .inner
            .events
            .send(SyncEvent::NoteWritten { path: note.0 });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::identity::Identity;
    use crate::sync::transport::tcp_tls::TlsIdentity;
    use tempfile::tempdir;

    fn dev(n: u8) -> DeviceId {
        DeviceId([n; 16])
    }

    /// SYN-M7: the queue envelope's directional AAD must block a reflection —
    /// a frame sealed A→B, replayed by a hostile relay back to A as if from B,
    /// must FAIL to open even though the per-pair key is identical in both
    /// directions. Correct-direction open still succeeds.
    #[test]
    fn queue_aad_blocks_direction_reflection() {
        use crate::sync::crypto::pairing::DomainSeparatedKey;
        let a = [1u8; 32];
        let b = [2u8; 32];
        let cipher =
            EnvelopeCipher::from_raw_key(DomainSeparatedKey::from_derived_bytes([0x7u8; 32]));
        let sealed = cipher.seal(b"queued note", &queue_env_aad(&a, &b)).unwrap();

        // Correct direction (A→B) opens.
        assert!(
            cipher
                .open(&sealed.nonce, &sealed.ciphertext, &queue_env_aad(&a, &b))
                .is_ok(),
            "the intended recipient must be able to open the frame"
        );
        // Reflected (B→A) must fail despite the shared bidirectional key.
        assert!(
            cipher
                .open(&sealed.nonce, &sealed.ciphertext, &queue_env_aad(&b, &a))
                .is_err(),
            "a reflected A→B frame must not open as B→A (SYN-M7)"
        );
    }

    /// Mirrors `engine::tests::setup_engine` (that helper is private to its
    /// own test module, so it can't be reused across files) — a single,
    /// unpaired `SyncEngine` with a real roost + app-data temp dir and no
    /// `RoostFs` backend (desktop `std::fs` fallback).
    fn setup_engine() -> (SyncEngine, tempfile::TempDir, tempfile::TempDir) {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let roost = tempdir().unwrap();
        let app_data = tempdir().unwrap();
        let identity = Arc::new(Identity::load_or_create(app_data.path()).unwrap());
        let tls = Arc::new(TlsIdentity::generate().unwrap());
        let state_dir = app_data.path().join("sync-state");
        let engine = SyncEngine::new(
            roost.path().to_path_buf(),
            state_dir,
            None, // no RoostFs backend — tests use the std::fs fallback
            identity,
            tls,
            "test-device".into(),
        )
        .unwrap();
        (engine, roost, app_data)
    }

    /// H-6 / #5a (ACK-after-disk): a queued FullDoc whose render-to-disk
    /// fails must be reported as a *failure*, not silently ACKed. Forces the
    /// failure the same way `render_write_failure_emits_note_sync_failed`
    /// (peer.rs) does — occupy the note's parent folder with a plain file so
    /// `create_dir_all` can't create it — but drives it through the QUEUE
    /// path (`apply_queue_full_doc`) instead of the live-peer path. Before
    /// the fix this method only `warn!`ed, emitted `NoteWritten`, and
    /// returned `Ok(())`, so the caller ACK-deleted the queue message even
    /// though the note never reached disk — a silent, unrecoverable loss.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn apply_queue_full_doc_render_failure_is_err_and_not_note_written() {
        let (engine, roost, _app) = setup_engine();
        // Occupy "dir" with a regular file so create_dir_all(roost/dir) fails
        // when we try to write "dir/note.md".
        std::fs::write(roost.path().join("dir"), b"i am a file, not a folder").unwrap();

        let note = NotePath("dir/note.md".into());
        let save = {
            let mut d = NoteDoc::new(dev(9)).unwrap();
            d.apply_markdown("content that must reach disk").unwrap();
            d.save()
        };

        let mut rx = engine.inner.events.subscribe();

        let result = engine
            .apply_queue_full_doc(dev(2), note.clone(), &save)
            .await;
        assert!(
            result.is_err(),
            "a render-to-disk failure must surface as Err so the caller keeps \
             the queue message for retry, not Ok(()) which gets ACK-deleted"
        );

        let mut failed = false;
        let mut wrongly_written = false;
        while let Ok(ev) = rx.try_recv() {
            match ev {
                SyncEvent::NoteSyncFailed { path, .. } if path == "dir/note.md" => failed = true,
                SyncEvent::NoteWritten { path } if path == "dir/note.md" => wrongly_written = true,
                _ => {}
            }
        }
        assert!(
            failed,
            "a queue render-to-disk failure must emit NoteSyncFailed"
        );
        assert!(
            !wrongly_written,
            "a failed queue render must never also emit NoteWritten"
        );
    }

    /// B1 (fail-closed queue backup): the QUEUE apply path must mirror the
    /// live-peer path — when a queued `FullDoc` fails to merge into a divergent
    /// local doc AND the conflict backup can't be written, it must NOT adopt the
    /// remote (which would overwrite the local edits with no recoverable copy),
    /// must keep the local doc, must emit `NoteSyncFailed`, and must return
    /// `Err` so `try_queue_pull_all` keeps the message for retry. Before the fix
    /// the queue path committed `store.save(remote)` BEFORE the backup and
    /// discarded the backup's result, so a backup failure permanently discarded
    /// the local edits — exactly the release-blocker durability bug. Forced the
    /// same way as the live-path test `full_doc_merge_failure_does_not_adopt_
    /// when_backup_fails`: same-actor divergent history makes the merge fail;
    /// occupying the artifact's parent dir with a file makes the backup fail.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn apply_queue_full_doc_merge_failure_does_not_adopt_when_backup_fails() {
        use crate::sync::crdt::NoteDoc;

        let (engine, roost, _app) = setup_engine();
        let note = NotePath("sub/note.md".into());

        // Local doc with unsynced edits under actor dev(7), saved to the store.
        let actor = dev(7).as_actor_id();
        {
            let store = engine.inner.doc_store.lock().await;
            let mut local = NoteDoc::new_with_actor(actor.clone()).unwrap();
            local.apply_markdown("IMPORTANT LOCAL EDITS\n").unwrap();
            store.save(&note, &mut local).unwrap();
        }

        // Occupy "<roost>/sub" with a FILE so the conflict artifact's
        // create_dir_all("<roost>/sub") fails → backup write fails.
        std::fs::write(roost.path().join("sub"), b"not a dir").unwrap();

        // Remote: SAME actor, independent history → merge fails, forcing the
        // adopt path (which the fail-closed backup must now abort).
        let remote_bytes = {
            let mut remote = NoteDoc::new_with_actor(actor).unwrap();
            remote.apply_markdown("remote replacement\n").unwrap();
            remote.save()
        };

        let mut rx = engine.inner.events.subscribe();

        let result = engine
            .apply_queue_full_doc(dev(2), note.clone(), &remote_bytes)
            .await;
        assert!(
            result.is_err(),
            "a queue backup failure must abort the adopt with Err so the caller \
             keeps the message for retry — not Ok(()) which gets ACK-deleted"
        );

        // The local edits MUST survive in the store — the adopt was refused.
        let kept = {
            let store = engine.inner.doc_store.lock().await;
            store.load_or_create(&note).unwrap().render().unwrap()
        };
        assert_eq!(
            kept, "IMPORTANT LOCAL EDITS\n",
            "the local doc must NOT be overwritten when the backup can't be written"
        );

        let mut failed = false;
        let mut wrongly_written = false;
        while let Ok(ev) = rx.try_recv() {
            match ev {
                SyncEvent::NoteSyncFailed { path, .. } if path == "sub/note.md" => failed = true,
                SyncEvent::NoteWritten { path } if path == "sub/note.md" => wrongly_written = true,
                _ => {}
            }
        }
        assert!(failed, "a refused adopt must emit NoteSyncFailed");
        assert!(
            !wrongly_written,
            "a refused adopt must never emit NoteWritten for the primary path"
        );
    }

    /// #trash_note-3-state: a queued tombstone whose disk delete genuinely
    /// fails (not "file already gone") must return `Err` from
    /// `apply_queue_full_doc` so `try_queue_pull_all` does NOT ACK-delete the
    /// queue message, and must emit `NoteSyncFailed`, never `NoteDeleted`.
    /// Forced by occupying the note's on-disk path with a directory so
    /// `trash_note`'s `std::fs::remove_file` fails for a reason other than
    /// `NotFound`. Before the `trash_note` three-state fix, this genuine
    /// failure collapsed to `false`/`Ok(())`, which this method treated as
    /// success — ACKing the queue message while the CRDT stayed tombstoned
    /// and the file survived on disk, so a later desktop scan would
    /// resurrect it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn apply_queue_full_doc_tombstone_disk_delete_failure_is_err_and_not_note_deleted() {
        let (engine, roost, _app) = setup_engine();
        let note = NotePath("occupied.md".into());
        // The note exists on disk (so trash isn't NotFound)...
        std::fs::write(roost.path().join("occupied.md"), b"content").unwrap();
        // ...but the trash dir can't be created (a file occupies its path), so
        // the backend's `trash` fails with a genuine, non-NotFound I/O error.
        std::fs::write(roost.path().join(".corvux-trash"), b"not a dir").unwrap();

        let save = {
            let mut d = NoteDoc::new(dev(9)).unwrap();
            d.apply_markdown("content that will never reach disk")
                .unwrap();
            d.mark_deleted(0).unwrap();
            d.save()
        };

        let mut rx = engine.inner.events.subscribe();

        let result = engine
            .apply_queue_full_doc(dev(2), note.clone(), &save)
            .await;
        assert!(
            result.is_err(),
            "a genuine disk-delete failure must surface as Err so the caller \
             keeps the queue message for retry, not Ok(()) which gets ACK-deleted"
        );

        let mut failed = false;
        let mut wrongly_deleted = false;
        while let Ok(ev) = rx.try_recv() {
            match ev {
                SyncEvent::NoteSyncFailed { path, .. } if path == "occupied.md" => failed = true,
                SyncEvent::NoteDeleted { path } if path == "occupied.md" => wrongly_deleted = true,
                _ => {}
            }
        }
        assert!(
            failed,
            "a tombstone disk-delete failure must emit NoteSyncFailed"
        );
        assert!(
            !wrongly_deleted,
            "a failed disk delete must never also emit NoteDeleted (that would \
             tell the caller it's safe to ACK a genuine failure)"
        );
    }

    /// R3-3 (per-note serialization). A queue adopt and a stream of local saves
    /// for the SAME note run concurrently. The per-note guard must:
    ///  (a) never deadlock — the whole race finishes well within the timeout
    ///      (a reentrant/mis-wired guard would hang here); and
    ///  (b) leave disk and DocStore CONSISTENT afterwards. Before the guard, a
    ///      queue adopt's commit→render span could interleave with a local
    ///      save's read→commit: the adopt commits, the local save commits +
    ///      lands on disk, then the adopt's stale render clobbers the disk — so
    ///      disk holds the remote text while the DocStore holds the local text.
    ///      The watcher would then re-ingest that stale render as a phantom
    ///      local edit and fan it to peers (silent CRDT data loss). With every
    ///      commit+disk op serialized per note, whichever op completes last
    ///      leaves the two in agreement.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_local_save_and_queue_adopt_stay_consistent() {
        use crate::sync::engine::watcher::LocalChange;

        let (engine, roost, _app) = setup_engine();
        let note = NotePath("concurrent.md".into());
        let abs = roost.path().join("concurrent.md");

        // Seed a real on-disk file + DocStore doc so both paths mutate a
        // genuinely-existing note.
        std::fs::write(&abs, "seed\n").unwrap();
        engine
            .apply_local_change(&LocalChange::Modified {
                path: note.clone(),
                abs: abs.clone(),
            })
            .await
            .unwrap();

        // Contender 1: local saves — rewrite disk, then ingest, mirroring the
        // watcher path (editor writes the file → apply_local_change reads it).
        let e1 = engine.clone();
        let n1 = note.clone();
        let abs1 = abs.clone();
        let saver = tokio::spawn(async move {
            for i in 0..40 {
                std::fs::write(&abs1, format!("local edit {i}\n")).unwrap();
                e1.apply_local_change(&LocalChange::Modified {
                    path: n1.clone(),
                    abs: abs1.clone(),
                })
                .await
                .unwrap();
            }
        });

        // Contender 2: queue adopts of independent remote copies.
        let e2 = engine.clone();
        let n2 = note.clone();
        let adopter = tokio::spawn(async move {
            for i in 0..40 {
                let bytes = {
                    let mut d = NoteDoc::new(dev(9)).unwrap();
                    d.apply_markdown(&format!("remote adopt {i}\n")).unwrap();
                    d.save()
                };
                // Per-round Err is fine (merge/CAS outcomes vary under the
                // race); we assert on liveness + the final invariant only.
                let _ = e2.apply_queue_full_doc(dev(2), n2.clone(), &bytes).await;
            }
        });

        tokio::time::timeout(std::time::Duration::from_secs(20), async {
            saver.await.unwrap();
            adopter.await.unwrap();
        })
        .await
        .expect("per-note lock deadlocked: concurrent save + queue adopt never finished");

        // No watcher / no peers on this engine, so nothing writes the note after
        // the tasks join. The last completed op left disk and DocStore in
        // agreement — assert they didn't diverge.
        let on_disk = std::fs::read_to_string(&abs).unwrap();
        let in_store = {
            let store = engine.inner.doc_store.lock().await;
            store.load_or_create(&note).unwrap().render().unwrap()
        };
        assert_eq!(
            on_disk.trim_end(),
            in_store.trim_end(),
            "after concurrent local-save + queue-adopt, disk and DocStore diverged \
             — the per-note commit+disk serialization did not hold"
        );
    }

    /// R3-3 Finding-1 regression (delete commit+disk atomicity). A local delete
    /// races a queue adopt of a NON-deleted remote copy whose render would
    /// recreate the file. `mark_note_deleted` commits the tombstone AND
    /// reconcile-trashes the disk under the per-note guard, so afterwards disk
    /// must AGREE with the doc's deleted flag: a tombstoned note is never left
    /// present on disk (the resurrection bug the watcher would otherwise turn
    /// into an un-delete + fan-out). Before the fix the tombstone commit skipped
    /// disk entirely, so an adopt's recreate left disk != DocStore.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn delete_reconciles_disk_and_never_resurrects_under_race() {
        use crate::sync::engine::watcher::LocalChange;

        let (engine, roost, _app) = setup_engine();
        let note = NotePath("doomed.md".into());
        let abs = roost.path().join("doomed.md");

        std::fs::write(&abs, "content\n").unwrap();
        engine
            .apply_local_change(&LocalChange::Modified {
                path: note.clone(),
                abs: abs.clone(),
            })
            .await
            .unwrap();

        // Contender 1: delete — commits the tombstone, then reconcile-trashes.
        let e1 = engine.clone();
        let n1 = note.clone();
        let deleter = tokio::spawn(async move {
            e1.mark_note_deleted(n1).await;
        });
        // Contender 2: queue adopt of a non-deleted remote copy — its render
        // would recreate the file on disk if it wins the interleave.
        let e2 = engine.clone();
        let n2 = note.clone();
        let adopter = tokio::spawn(async move {
            let bytes = {
                let mut d = NoteDoc::new(dev(9)).unwrap();
                d.apply_markdown("resurrected\n").unwrap();
                d.save()
            };
            let _ = e2.apply_queue_full_doc(dev(2), n2, &bytes).await;
        });

        tokio::time::timeout(std::time::Duration::from_secs(20), async {
            deleter.await.unwrap();
            adopter.await.unwrap();
        })
        .await
        .expect("delete-vs-adopt deadlocked");

        // Invariant: disk presence matches the doc's deleted flag. Whichever op
        // ran last reconciled disk to the doc UNDER the guard, so they cannot
        // disagree.
        let deleted = {
            let store = engine.inner.doc_store.lock().await;
            store.load_or_create(&note).unwrap().is_deleted().unwrap()
        };
        let on_disk = abs.exists();
        assert_eq!(
            on_disk, !deleted,
            "disk presence must match the doc's deleted flag (deleted={deleted}, \
             on_disk={on_disk}) — a tombstoned note left on disk is the resurrection bug"
        );
    }
}
