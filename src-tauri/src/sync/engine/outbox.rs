//! Durable-outbox operations on `SyncEngine`.
//!
//! The outbox makes store-and-forward delivery survive failure. The old
//! path was best-effort: a queue push that failed (network down, server
//! unreachable) was logged and dropped, so the change waited for the next
//! local edit to that same note to get another shot. Now every offline
//! delivery is recorded as a durable intent *before* it's attempted
//! (write-ahead), retried on a timer with exponential backoff, and only
//! cleared once the queue server accepts it.
//!
//! Intents store no payload — see [`OutboxStore`](crate::sync::store::outbox_store)
//! for why. Delivery regenerates the ciphertext from the canonical DocStore
//! via [`try_queue_push`](super::engine::SyncEngine::try_queue_push).

use std::collections::HashMap;
use std::time::Duration;

use tracing::{debug, info, warn};

use crate::sync::identity::DeviceId;
use crate::sync::protocol::NotePath;

use super::engine::SyncEngine;
use super::peer::PeerHandle;
use super::state::SyncEvent;
use super::util::{clone_handle, now_millis};

/// How often the periodic drainer re-attempts due outbox entries. Shorter
/// than the queue *pull* interval (5m): a push that failed on a transient
/// blip should recover within a minute, while per-entry backoff (30s → 1h)
/// keeps a genuinely-offline peer from being hammered.
pub(super) const OUTBOX_DRAIN_INTERVAL: Duration = Duration::from_secs(60);

impl SyncEngine {
    /// Enqueue a single (peer, note) delivery intent and kick an immediate
    /// attempt. Used by the live fan-out paths when a connected peer's
    /// channel closes mid-send — that change would otherwise be dropped
    /// (the old "peer channel closed, dropping change" hole). No-op when no
    /// queue client is installed or the peer can't receive queue messages
    /// (no NodeId / no per-pair key).
    pub(super) async fn enqueue_outbox_for_peer(&self, device: DeviceId, note: NotePath) {
        if self.inner.queue_client.read().await.is_none() {
            return;
        }
        let deliverable = {
            let store = self.inner.peer_store.lock().await;
            store
                .get(device)
                .map(|r| r.peer_node_id.is_some() && r.queue_pair_key.is_some())
                .unwrap_or(false)
        };
        if !deliverable {
            return;
        }
        let attempt_now = {
            let mut outbox = self.inner.outbox.lock().await;
            match outbox.enqueue(device, &note, now_millis()) {
                Ok(due) => due,
                Err(e) => {
                    warn!(error = %e, "enqueue_outbox_for_peer: persist failed");
                    return;
                }
            }
        };
        self.emit_outbox_status().await;
        // Only attempt immediately when the intent is new or its backoff has
        // elapsed; otherwise the periodic drainer picks it up when due.
        if attempt_now {
            let engine = self.clone();
            // SYN-I4/B7: tracked + cancel-aware (twin of the fanout outbox push).
            self.spawn_tracked(move |token| async move {
                tokio::select! {
                    _ = token.cancelled() => {}
                    _ = engine.attempt_outbox_delivery(device, note) => {}
                }
            });
        }
    }

    /// Attempt to deliver one outbox entry through the queue. On success
    /// (`try_queue_push` returns `Ok(true)`) the entry is cleared; on any
    /// failure the attempt counter is bumped and the backoff advances.
    pub(super) async fn attempt_outbox_delivery(&self, device: DeviceId, note: NotePath) {
        match self.try_queue_push(device, note.clone()).await {
            Ok(true) => {
                let removed = {
                    let mut outbox = self.inner.outbox.lock().await;
                    outbox.remove(device, &note).unwrap_or(false)
                };
                if removed {
                    self.emit_outbox_status().await;
                }
            }
            // `Ok(false)` = push failed / peer not deliverable; `Err` =
            // encode/crypto failure. Either way back off and retry later;
            // a permanently-stuck entry is eventually TTL-reaped in drain.
            Ok(false) | Err(_) => {
                let mut outbox = self.inner.outbox.lock().await;
                let _ = outbox.record_failure(device, &note, now_millis());
            }
        }
    }

    /// Reap expired entries, then re-attempt every entry whose backoff has
    /// elapsed. Called on a timer, at queue-client install (startup), and is
    /// safe to invoke manually. No-op in strict-P2P mode (no queue client).
    pub async fn drain_outbox(&self) {
        if self.inner.queue_client.read().await.is_none() {
            return;
        }
        let now = now_millis();
        let ttl_ms = crate::sync::queue::protocol::DEFAULT_MESSAGE_TTL_SECS * 1000;
        let cutoff = now.saturating_sub(ttl_ms);
        let (due, reaped) = {
            let mut outbox = self.inner.outbox.lock().await;
            let reaped = outbox.reap(cutoff);
            if reaped > 0 {
                info!(reaped, "drain_outbox: reaped expired outbox entries");
            }
            (outbox.due(now), reaped)
        };
        if due.is_empty() {
            // Even with nothing due, a reap changed the pending count — keep
            // the UI indicator honest.
            if reaped > 0 {
                self.emit_outbox_status().await;
            }
            return;
        }
        debug!(count = due.len(), "drain_outbox: attempting delivery");
        for (device, note) in due {
            self.attempt_outbox_delivery(device, note).await;
        }
        self.emit_outbox_status().await;
    }

    /// Record a `(peer, note)` whose *live* fan-out send was dropped (control
    /// channel full or peer task gone), so the reconnect supervisor can
    /// redeliver it over the control channel. Coalesced by construction —
    /// repeated edits to one note collapse to a single entry per peer. Unlike
    /// [`enqueue_outbox_for_peer`](Self::enqueue_outbox_for_peer) this is NOT
    /// queue-gated, so it also covers the default strict-P2P / no-queue config
    /// where the outbox is inert. See [`live_pending`](super::engine::SyncEngineInner::live_pending).
    pub(super) async fn record_live_pending(&self, device: DeviceId, note: NotePath) {
        self.inner
            .live_pending
            .lock()
            .await
            .entry(device)
            .or_default()
            .insert(note);
    }

    /// Redeliver changes whose live send was dropped by a momentarily-full peer
    /// control channel. NOT queue-gated — this is the strict-P2P counterpart to
    /// [`drain_outbox`](Self::drain_outbox). For every currently-connected peer
    /// with pending paths, re-poke its control channel with
    /// `PeerControl::MarkDirty`; a path that sends cleanly is cleared, while a
    /// still-full channel keeps it for the next tick. Disconnected peers are
    /// skipped — they converge via `NoteList` on reconnect, and `register_peer`
    /// clears their entries then. Called every reconnect-supervisor tick.
    pub(super) async fn drain_live_pending(&self) {
        // Snapshot the pending map so we never hold its lock across a send.
        let snapshot: Vec<(DeviceId, Vec<NotePath>)> = {
            let map = self.inner.live_pending.lock().await;
            if map.is_empty() {
                return;
            }
            map.iter()
                .map(|(device, paths)| (*device, paths.iter().cloned().collect()))
                .collect()
        };
        // Connected peers only. Snapshot handles (cheap Arc/sender clones) so we
        // don't hold the peers lock across the control-channel sends either.
        let connected: HashMap<DeviceId, PeerHandle> = self
            .inner
            .peers
            .read()
            .await
            .iter()
            .map(|(device, handle)| (*device, clone_handle(handle)))
            .collect();

        for (device, paths) in snapshot {
            let Some(handle) = connected.get(&device) else {
                continue;
            };
            for path in paths {
                if handle.send_mark_dirty(path.clone()).is_ok() {
                    // Delivered to the peer task — clear the intent. On failure
                    // (channel still full / peer just dropped) leave it for the
                    // next tick.
                    let mut map = self.inner.live_pending.lock().await;
                    if let Some(set) = map.get_mut(&device) {
                        set.remove(&path);
                        if set.is_empty() {
                            map.remove(&device);
                        }
                    }
                }
            }
        }
    }

    /// Pending-delivery counts per peer, for the sync status surface.
    pub async fn outbox_pending_by_peer(&self) -> Vec<(DeviceId, u32)> {
        self.inner
            .outbox
            .lock()
            .await
            .pending_by_peer()
            .into_iter()
            .map(|(id, count)| (DeviceId(id), count))
            .collect()
    }

    /// Total pending outbox entries across all peers.
    pub async fn outbox_pending_total(&self) -> u32 {
        self.inner.outbox.lock().await.len() as u32
    }

    /// Broadcast the current pending total so the UI can show a "syncing /
    /// N waiting" indicator without polling.
    pub(super) async fn emit_outbox_status(&self) {
        let total = self.inner.outbox.lock().await.len() as u32;
        let _ = self.inner.events.send(SyncEvent::OutboxChanged {
            total_pending: total,
        });
    }
}
