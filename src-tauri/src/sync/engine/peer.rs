// peer_device_id retained for logging/diagnostics
#![allow(dead_code)]

//! Per-peer connection actor.
//!
//! Once a TLS-wrapped [`Transport`](crate::sync::transport::Transport) is
//! established and the pairing handshake has completed, that connection
//! spends the rest of its life in the **sync loop** implemented here.
//!
//! # What this module is responsible for
//!
//! - Running the handshake FSM against a live transport.
//! - After success, driving the Automerge sync protocol for every note
//!   that either side knows about.
//! - Bootstrapping: if the peer has notes we don't (or vice-versa), fetch
//!   them via `RequestFullDoc` / `FullDoc`. Pure `sync::Message` exchanges
//!   alone can't convey the initial state for a doc the receiver has
//!   never seen.
//! - Reacting to local changes by re-generating sync messages for the
//!   affected note.
//! - Keepalives (Ping/Pong) on idle and clean shutdown via `Bye`.
//!
//! # What this module is NOT responsible for
//!
//! - File watching (see [`super::watcher`]).
//! - Global engine state (peer registry, listen socket). The engine owns
//!   one [`PeerActor`] per live connection and routes local changes to it.
//! - Rendering changes to disk. After a sync message updates a CRDT, the
//!   actor writes the rendered markdown back to the roost; this module
//!   holds that logic because it's the direct consequence of receiving a
//!   remote change, and the render-and-save step needs to dedupe against
//!   `md_hash` to avoid re-triggering the watcher.
//!
//! # Driving model
//!
//! One Tokio task per peer. The task `select!`s on:
//! - Inbound: `transport.recv()` → protocol message from peer.
//! - Control: a `PeerControl` channel the engine uses to push
//!   `LocalChange`s and shutdown requests.
//!
//! Outbound sync messages are generated *reactively*: we call
//! `sync_state.generate(path)` whenever we receive any message from the
//! peer *or* whenever a local change arrives, and keep looping until
//! `generate` returns `None` for every dirty path. This is the pattern
//! Automerge's docs recommend.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{broadcast, mpsc, Mutex};
use tracing::{debug, info, warn};

use super::note_locks::NoteLocks;
use super::state::SyncEvent;
use crate::roostfs::native::NativeFs;
use crate::roostfs::RoostFs;
use crate::sync::crdt::NoteDoc;
use crate::sync::crypto::envelope::EnvelopeCipher;
use crate::sync::crypto::pairing::{derive_subkey, SessionSecret};
use crate::sync::error::{EngineError, SyncError, SyncResult};
use crate::sync::identity::DeviceId;
use crate::sync::protocol::{
    handshake::{ClientHandshake, Outcome, ServerHandshake},
    sync::PeerSyncState,
    BlobUnavailableReason, Message, NotePath, SYNC_BLOB_MAX_BYTES,
};
use crate::sync::store::doc_store::DocStore;
use crate::sync::store::pending_trash_store::PendingTrashStore;
use crate::sync::transport::encrypted::EncryptedTransport;
use crate::sync::transport::Transport;

use super::watcher::LocalChange;

/// Wall-clock bound on the pairing handshake (both roles). Must exceed a
/// worst-case first-connection round-trip (relay-coordinated hole punch on a
/// slow mobile link) yet stay well under any human-noticeable "stuck" window.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);

/// If no inbound frame arrives within this window, treat the peer as dead and
/// drop the connection so the supervisor redials. We Ping every 30s and the
/// peer replies Pong, so a healthy (even idle) link produces inbound traffic
/// well inside this window; only a peer whose sync task is wedged — its QUIC
/// driver still ACKing keepalives, so the transport idle-timeout never fires —
/// goes silent this long. Without this a half-dead peer reads as "connected"
/// forever while nothing actually syncs. 90s ≈ 3 missed ping cycles.
const LIVENESS_TIMEOUT: Duration = Duration::from_secs(90);

/// Reject a peer-supplied `NotePath` that could escape the docs directory.
/// `DocStore` keys `.automerge` files by `docs_root.join(path)`, so an
/// unvalidated `../` (or an absolute path, NUL, or backslash separator) from a
/// malicious or buggy *paired* peer would let it write CRDT state outside the
/// store — and `list()` would never see it, leaving ghost state. Mirrors the
/// component rules in `store::from_note_path`. Returns `false` for unsafe paths;
/// callers skip the message rather than tear down the whole session.
pub(super) fn is_safe_note_path(path: &NotePath) -> bool {
    let p = &path.0;
    !(p.is_empty()
        || p.starts_with('/')
        || p.contains('\0')
        || p.contains('\\')
        || p.split('/').any(|c| c.is_empty() || c == "." || c == ".."))
}

/// Control messages the engine sends into a running peer actor.
#[derive(Debug)]
pub enum PeerControl {
    /// The watcher saw a local change. Re-diff the markdown into the CRDT
    /// and flush any resulting sync message to the peer.
    Local(LocalChange),
    /// Re-send `NoteList` to the peer. Used after a manual rescan so the
    /// peer can request any docs we freshly ingested from disk.
    ReannounceNotes,
    /// Re-mark `path` dirty and flush. Sent by the engine's live-pending
    /// drainer to redeliver a change whose original `Local` send was dropped
    /// by a momentarily-full control channel. No content travels — the flush
    /// regenerates the sync message from the authoritative DocStore.
    MarkDirty(NotePath),
    /// Clean shutdown: send `Bye` and drop the transport.
    Shutdown,
}

/// Public handle on a running peer actor. Dropping it does *not* stop the
/// actor — call [`Self::shutdown`] or send `PeerControl::Shutdown`.
pub struct PeerHandle {
    pub device_id: DeviceId,
    pub display_name: String,
    /// 32-byte symmetric key for the store-and-forward queue, derived
    /// from the just-completed pairing's `SessionSecret`. Engine
    /// persists this in `peer_store` so future offline pushes can
    /// encrypt without a fresh handshake. Zeroed copy of the buffer
    /// stays in the running peer task; this one is for the engine's
    /// one-time persistence on connect.
    pub queue_pair_key: [u8; 32],
    /// AUDIT-015: peer's roost id from the just-completed handshake.
    /// Engine persists this with the `PeerRecord` so the next handshake
    /// from this device can be validated against the same roost.
    pub peer_vault_id: [u8; 16],
    pub(super) control: mpsc::Sender<PeerControl>,
    /// Monotonic registration id assigned by `SyncEngine::register_peer`.
    /// Lets the peer task's async cleanup detect that a *newer* connection
    /// has since replaced this one in the peers map, so a dying old task
    /// doesn't evict a fresh reconnect (the connection-flap bug where the
    /// UI bounced back to "disconnected" right after reconnecting). 0 until
    /// registered.
    pub(super) generation: u64,
    /// Symmetric pairing: the dialer's offered redial credentials (ticket,
    /// pin), populated on the SERVER/accept side so the engine can persist
    /// them and dial the dialer back. `None` on the dialer side / legacy.
    pub peer_reconnect: Option<(String, String)>,
    /// SEC-2: the resumption token derived from this handshake's session. The
    /// engine persists it ONLY after a first pair (see peer_store); on
    /// reconnect it's re-derived here but must not be stored (would differ from
    /// the first-pair value both sides agreed on).
    pub resumption_token: [u8; 32],
}

impl PeerHandle {
    /// Enqueue a local change for the peer task — **non-blocking**. Uses
    /// `try_send`, never `send().await`: the file-save IPC path calls this
    /// (transitively) while holding the global engine lock, so blocking on a
    /// full channel would freeze the editor AND deadlock every other sync
    /// command behind that lock. A wedged or merely slow peer (control channel
    /// at capacity) returns `ChannelFull`; a dead peer returns `NotConnected`.
    /// Either way the fan-out caller records a durable outbox intent, so the
    /// change still converges once the peer drains or reconnects — nothing is
    /// silently dropped.
    pub fn send_local(&self, change: LocalChange) -> Result<(), SyncError> {
        use tokio::sync::mpsc::error::TrySendError;
        self.control
            .try_send(PeerControl::Local(change))
            .map_err(|e| match e {
                TrySendError::Full(_) => EngineError::ChannelFull.into(),
                TrySendError::Closed(_) => EngineError::NotConnected.into(),
            })
    }

    /// Non-blocking re-poke used by the live-pending drainer to redeliver a
    /// change whose original `send_local` was dropped by a full control
    /// channel. Same `try_send` discipline as [`Self::send_local`] — never
    /// blocks the caller. A still-full channel returns `ChannelFull`, so the
    /// drainer leaves the entry in `live_pending` for the next tick.
    pub(super) fn send_mark_dirty(&self, path: NotePath) -> Result<(), SyncError> {
        use tokio::sync::mpsc::error::TrySendError;
        self.control
            .try_send(PeerControl::MarkDirty(path))
            .map_err(|e| match e {
                TrySendError::Full(_) => EngineError::ChannelFull.into(),
                TrySendError::Closed(_) => EngineError::NotConnected.into(),
            })
    }

    /// Ask the peer actor to re-send its `NoteList`. Used after a manual
    /// roost rescan so the peer can pull anything new we ingested.
    pub async fn reannounce_notes(&self) -> Result<(), SyncError> {
        self.control
            .send(PeerControl::ReannounceNotes)
            .await
            .map_err(|_| EngineError::NotConnected.into())
    }

    pub async fn shutdown(&self) {
        let _ = self.control.send(PeerControl::Shutdown).await;
    }

    /// Clone just the control channel half. Used by the engine's watcher
    /// fan-out: each iteration builds short-lived handle copies so a
    /// blocked peer can't hold the peer-map read lock.
    pub(super) fn control_clone(&self) -> mpsc::Sender<PeerControl> {
        self.control.clone()
    }
}

/// What the connection opened as. Selects which handshake FSM to drive.
pub enum Role {
    Client { handshake: ClientHandshake },
    Server { handshake: ServerHandshake },
}

/// Run one peer connection to completion. Returns when the transport
/// closes, the peer sends `Bye`, or the engine requests shutdown.
///
/// The engine is expected to spawn this as its own Tokio task and keep the
/// returned `PeerHandle` in its peer map for routing local changes.
#[allow(clippy::too_many_arguments)]
pub(super) async fn spawn<T>(
    mut transport: T,
    role: Role,
    roost: PathBuf,
    roost_fs: Option<Arc<dyn RoostFs>>,
    doc_store: Arc<Mutex<DocStore>>,
    note_locks: Arc<NoteLocks>,
    pending_trash: Arc<Mutex<PendingTrashStore>>,
    events: broadcast::Sender<SyncEvent>,
    // SYN-I4: the peer sync loop runs on the engine's TaskTracker and selects
    // on this child cancel token, so `stop()` drains it even before the peer
    // is registered (an inbound accept task that hasn't reached `register_peer`
    // has no PeerHandle, so `stop()`'s per-peer `Bye` can't reach it — the
    // token is the only signal that gets there).
    tracker: tokio_util::task::TaskTracker,
    cancel_token: tokio_util::sync::CancellationToken,
) -> SyncResult<(PeerHandle, tokio::task::JoinHandle<SyncResult<()>>)>
where
    T: Transport + Send + 'static,
{
    // ── Phase 1: handshake ──────────────────────────────────────────────
    // Bound the handshake in wall-clock time. The QUIC keepalive holds an
    // established connection open indefinitely, so a peer that completes the
    // QUIC handshake but then stalls the app-layer PIN exchange (wedged accept
    // task, silent rate-limit, half-open link) would otherwise park this task
    // on `transport.recv()` forever — and the dialer's `PeerConnectionState`
    // sticks at `Handshaking`, which the reconnect supervisor deliberately
    // skips, so it is never retried until the app restarts. A generous 30s
    // covers slow first-hole-punch / relay round-trips; past that we fail so
    // backoff engages.
    // Captured before `run_handshake` consumes `role`; drives the envelope's
    // per-direction frame counter (M-2).
    let is_client = matches!(role, Role::Client { .. });
    let (peer_device_id, peer_display_name, session, peer_vault_id, peer_reconnect) =
        match tokio::time::timeout(HANDSHAKE_TIMEOUT, run_handshake(&mut transport, role)).await {
            Ok(res) => res?,
            Err(_) => {
                return Err(crate::sync::error::ProtocolError::Aborted(
                    "handshake timed out".into(),
                )
                .into())
            }
        };

    // Derive the app-layer envelope cipher from the just-established session
    // secret. Every post-handshake frame goes through `EncryptedTransport`
    // from this point — the inner transport sees only `EncryptedEnvelope`s,
    // and the relay / future store-and-forward queue cannot read content.
    let cipher = EnvelopeCipher::from_session(&session)?;
    // Derive the queue per-pair key BEFORE dropping the session. Both
    // peers run the same derivation off the same `SessionSecret`, so the
    // queue subkey lines up on both ends — neither side has to send it.
    // AUDIT-048: `derive_subkey` now returns a `DomainSeparatedKey`.
    // The PeerHandle stores the raw bytes so engine/dial.rs and
    // engine/accept.rs can persist them into the peer store without
    // taking on the wrapper's no-Clone/no-Copy semantics. The bytes
    // are re-wrapped via `from_derived_bytes` at the cipher boundary
    // (see queue_ops.rs).
    let queue_pair_key_wrapped = derive_subkey(&session, b"corvux-queue-pair-v1")?;
    let queue_pair_key: [u8; 32] = *queue_pair_key_wrapped.expose();
    drop(queue_pair_key_wrapped);
    // SEC-2: derive the per-peer resumption token the same way (off the same
    // shared `SessionSecret`, so both ends compute identical bytes without
    // sending it). The CALLER persists this only on a FIRST pair — on a
    // reconnect the session is derived from the existing token, so re-deriving
    // here would yield a different value that must NOT overwrite the stored one.
    let resumption_token_wrapped = derive_subkey(
        &session,
        crate::sync::crypto::pairing::RESUMPTION_TOKEN_LABEL,
    )?;
    let resumption_token: [u8; 32] = *resumption_token_wrapped.expose();
    drop(resumption_token_wrapped);
    drop(session); // Subkeys + cipher already captured; scrub the master.
    let encrypted_transport = EncryptedTransport::new(transport, cipher, is_client);

    let (control_tx, control_rx) = mpsc::channel::<PeerControl>(64);
    let handle = PeerHandle {
        device_id: peer_device_id,
        display_name: peer_display_name.clone(),
        queue_pair_key,
        peer_vault_id,
        control: control_tx,
        generation: 0,
        peer_reconnect,
        resumption_token,
    };

    // ── Phase 2: sync loop spawned as its own task ─────────────────────
    // Emit a PeerDisconnected event whenever the loop exits — clean close,
    // shutdown signal, or transport error all funnel here. Without this,
    // the frontend never learns the socket dropped and the indicator
    // silently shows stale "connected" state; it also blocks auto-
    // reconnect from kicking in.
    let events_on_exit = events.clone();
    let task = tracker.spawn(async move {
        let ctx = SyncCtx {
            roost,
            roost_fs,
            doc_store,
            note_locks,
            pending_trash,
            sync: PeerSyncState::new(),
            dirty: HashSet::new(),
            peer_device_id,
            peer_display_name,
            events,
            recovery_attempts: HashMap::new(),
            poisoned_paths: HashSet::new(),
            blob_inflight: HashMap::new(),
            blob_unavailable: HashSet::new(),
            blob_pending: HashSet::new(),
            scan_queue: Vec::new(),
            full_doc_buffers: HashMap::new(),
            seed_done: false,
        };
        // SYN-I4: exit on the normal loop end (clean close / Bye / transport
        // error) OR on engine cancel. Cancelling drops the sync loop (and its
        // transport) at its current await — safe: any in-flight per-note guard
        // it held is released on future-drop.
        let result = tokio::select! {
            _ = cancel_token.cancelled() => {
                Err(crate::sync::error::ProtocolError::Aborted("engine stopping".into()).into())
            }
            r = run_sync_loop(encrypted_transport, control_rx, ctx) => r,
        };
        let _ = events_on_exit.send(SyncEvent::PeerDisconnected {
            device_id: peer_device_id,
        });
        result
    });

    Ok((handle, task))
}

/// Drive the handshake FSM end-to-end. On success, returns the peer's
/// verified identity *and* the derived `SessionSecret`. The caller uses the
/// session to spin up the post-handshake `EncryptedTransport` decorator
/// that wraps every subsequent frame.
///
/// On failure, returns `Err` and the transport will be dropped by the
/// caller — the handshake is the only stage of the connection that runs
/// over plaintext (modulo iroh's QUIC encryption underneath), so a failure
/// here can be safely reported in the clear.
/// Returns the peer's verified identity, the session secret, the peer's roost
/// id, and — on the SERVER side only — the dialer's offered redial credentials
/// (symmetric pairing; `None` on the client side and when not offered).
#[allow(clippy::type_complexity)]
async fn run_handshake<T: Transport>(
    transport: &mut T,
    role: Role,
) -> SyncResult<(
    DeviceId,
    String,
    SessionSecret,
    [u8; 16],
    Option<(String, String)>,
)> {
    match role {
        Role::Client { mut handshake } => {
            let hello = handshake.start();
            transport.send(&hello).await?;
            loop {
                let incoming = transport.recv().await?;
                match handshake.step(incoming)? {
                    Outcome::Send(msg) => {
                        transport.send(&msg).await?;
                    }
                    Outcome::Done {
                        session,
                        peer_device_id,
                        peer_display_name,
                        peer_vault_id,
                    } => {
                        info!(peer = %peer_display_name, "client handshake done");
                        // Client side never captures a peer offer — WE are the
                        // one offering (in our Hello); the server stores it.
                        return Ok((
                            peer_device_id,
                            peer_display_name,
                            session,
                            peer_vault_id,
                            None,
                        ));
                    }
                    Outcome::Failed(reason) => {
                        return Err(crate::sync::error::ProtocolError::Aborted(format!(
                            "{reason:?}"
                        ))
                        .into());
                    }
                    Outcome::Wait => continue,
                }
            }
        }
        Role::Server { mut handshake } => {
            let (peer_device_id, peer_display_name, session, peer_vault_id) = 'server: loop {
                let incoming = transport.recv().await?;
                match handshake.step(incoming)? {
                    Outcome::Send(msg) => {
                        transport.send(&msg).await?;
                    }
                    Outcome::Done {
                        session,
                        peer_device_id,
                        peer_display_name,
                        peer_vault_id,
                    } => {
                        // Send OUR identity back to the client so they can
                        // pin us. Echoing peer_device_id/peer_display_name
                        // (the client's own values, received in Hello) was
                        // a latent bug that made both sides store the
                        // *client's* name in their peer list — paired
                        // devices appeared with the wrong name on at least
                        // one side.
                        //
                        // PairingOk is the LAST plaintext message on the
                        // wire — both sides install the envelope cipher
                        // immediately after this exchange returns.
                        transport
                            .send(&Message::PairingOk {
                                device_id: handshake.own_device_id().0,
                                display_name: handshake.own_display_name().to_string(),
                                vault_id: handshake.own_vault_id(),
                            })
                            .await?;
                        break 'server (peer_device_id, peer_display_name, session, peer_vault_id);
                    }
                    Outcome::Failed(_) => {
                        return Err(crate::sync::error::ProtocolError::Aborted(
                            "server handshake failed".into(),
                        )
                        .into());
                    }
                    Outcome::Wait => continue,
                }
            };
            // Symmetric pairing: pull the dialer's offered redial credentials
            // (if any) so the caller can persist them for dial-back.
            let peer_reconnect = handshake.peer_reconnect();
            Ok((
                peer_device_id,
                peer_display_name,
                session,
                peer_vault_id,
                peer_reconnect,
            ))
        }
    }
}

/// State carried through the post-handshake sync loop.
struct SyncCtx {
    roost: PathBuf,
    /// Present when a roost backend is installed (desktop NativeFs, Android
    /// SafFs). Used by `render_to_roost` to persist received notes via the
    /// backend that actually owns the storage — std::fs writes don't work
    /// on SAF `content://` roots.
    roost_fs: Option<Arc<dyn RoostFs>>,
    doc_store: Arc<Mutex<DocStore>>,
    /// Shared per-note lock table (same `Arc` the engine holds). Acquired at
    /// the top of every note-mutating `handle_inbound` arm and `handle_local`
    /// Removed, held across the DocStore commit AND the disk reconcile, so a
    /// live adopt can't interleave with a local save / queue adopt of the same
    /// note. See `note_locks::NoteLocks`.
    note_locks: Arc<NoteLocks>,
    /// SYN-B8: shared handle to the engine's pending-trash retry set. When a
    /// tombstone adopt's on-disk trash fails, the path is enrolled here so the
    /// reconnect supervisor re-attempts the trash (the live-path equivalent of
    /// the queue's retain-and-re-pull). Same `Arc` the engine holds.
    pending_trash: Arc<Mutex<PendingTrashStore>>,
    sync: PeerSyncState,
    /// NotePaths with pending outbound sync messages.
    dirty: HashSet<NotePath>,
    peer_device_id: DeviceId,
    peer_display_name: String,
    /// Broadcast channel into the engine so incoming writes can surface
    /// to the UI (e.g. "remote peer wrote foo.md" → refresh file tree).
    events: broadcast::Sender<SyncEvent>,
    /// Per-path circuit breaker. Tracks how many corruption-recoveries
    /// we've done for each note in the current session and at what time.
    /// When a peer holds a permanently-corrupt blob it keeps replying
    /// to our `RequestFullDoc` with the same broken bytes, and the
    /// recovery loop runs forever — burning CPU and blocking other
    /// peers. After the threshold trips, we mark the path as poisoned
    /// for this peer and stop driving recovery for it.
    recovery_attempts: HashMap<NotePath, RecoveryWindow>,
    /// Paths we've given up on for this peer. Skip them on inbound to
    /// avoid re-triggering the recovery loop.
    poisoned_paths: HashSet<NotePath>,
    /// Attachment rel-paths we've sent a `RequestBlob` for and are awaiting
    /// a reply on, each mapped to the `Instant` the request was sent. Dedups
    /// concurrent requests — a blob referenced by two notes is only asked for
    /// once. Keyed on the attachment path, NOT the note path (C-3). Cleared
    /// automatically when the ctx drops on peer disconnect (C-4). SYN-M8: the
    /// sent-time drives the request deadline — a peer that silently drops a
    /// `RequestBlob` would otherwise hold the slot (and block re-request)
    /// forever; the ping-tick sweep reclaims a timed-out slot back to
    /// `blob_pending`.
    blob_inflight: HashMap<String, Instant>,
    /// Attachment rel-paths the peer told us it can't provide (NotFound /
    /// OverCap / Rejected). Terminal until the referencing note changes
    /// again — stops a re-request loop for a blob no online peer has (C-2).
    blob_unavailable: HashSet<String>,
    /// Missing attachment rel-paths discovered but not yet requested because
    /// the in-flight window (`MAX_BLOB_INFLIGHT`) was full. Drained by
    /// `pump_blob_requests` as replies free slots, so a note with many
    /// attachments fully fetches (C-7).
    blob_pending: HashSet<String>,
    /// Local note paths still to scan for missing attachments on this
    /// connection. Populated once at connect from `DocStore::list()` and
    /// drained a `BLOB_SCAN_BATCH` at a time by the backfill select! arm, so
    /// the O(notes) render cost never blocks the loop.
    scan_queue: Vec<NotePath>,
    /// C-4: reassembly buffers for chunked FullDoc snapshots, keyed by note
    /// path. A note whose Automerge save exceeds one frame arrives as an
    /// ordered `FullDocChunk` sequence; we accumulate here until the final
    /// chunk, then adopt the reassembled bytes exactly as a one-shot FullDoc.
    /// Cleared on peer disconnect when the ctx drops.
    full_doc_buffers: HashMap<NotePath, FullDocBuffer>,
    /// Hole B (SAF bootstrap): on a scan-skipped `content://` roost the engine
    /// never auto-ingests on-disk notes (`scan_roost` is a no-op there to avoid
    /// dual-text-object divergence), so notes the joining device has that no
    /// peer has would stay stranded — never announced, never synced. Once per
    /// connection, on the first `NoteList`, we seed such peer-absent disk notes
    /// into the DocStore (via a deterministic path-derived actor so concurrent
    /// identical seeds converge) and re-announce. `true` after that pass has
    /// run so it never repeats on a reannounce. Only ever set on scan-skipped
    /// roosts; a no-op everywhere else.
    seed_done: bool,
}

/// Accumulator for an in-flight chunked FullDoc transfer (C-4). Enforces
/// in-order arrival and hard caps so a malicious peer can't exhaust memory by
/// claiming a huge chunk count or sending gaps forever.
struct FullDocBuffer {
    chunk_count: u32,
    next_index: u32,
    bytes: Vec<u8>,
    /// When the last chunk for this transfer arrived. A transfer that stalls
    /// (peer died mid-stream, or a hostile peer sends only the first chunk to
    /// pin a slot) is evicted once idle past `FULLDOC_BUFFER_IDLE_TTL` so it
    /// can't permanently occupy one of the bounded reassembly slots. See C-4.
    last_activity: Instant,
}

/// Max chunks a single chunked FullDoc may claim, and max reassembled size.
/// A note's Automerge history is bounded in practice; these are generous
/// backstops against a peer that lies about `chunk_count`.
///
/// The byte cap doubles as the AGGREGATE reassembly ceiling
/// (`MAX_FULLDOC_TOTAL_BYTES`), i.e. the most memory a peer can make us buffer
/// at once. It was 512 MiB — unsafe on a 2–4 GB phone. Attachments sync as
/// SEPARATE blobs capped at `SYNC_BLOB_MAX_BYTES` (8 MiB) and never live inside
/// a note's CRDT, so a FullDoc is text + Automerge history only. 64 MiB is tens
/// of millions of characters of note text — no realistic note is rejected —
/// while keeping the worst-case buffer mobile-safe. (`MAX_FULLDOC_CHUNKS` is a
/// secondary backstop; the byte cap binds first.)
const MAX_FULLDOC_CHUNKS: u32 = 8192;
const MAX_FULLDOC_BYTES: usize = 64 * 1024 * 1024;

/// Max number of *distinct* chunked FullDoc transfers we'll buffer at once
/// (C-4). Per-buffer caps bound each transfer's size, but without a cap on the
/// count of concurrent buffers a hostile peer could open thousands of
/// distinct-path transfers — sending only the first chunk of each, never the
/// last — and pin memory across all of them. A well-behaved sender streams one
/// note's chunks to completion before starting the next, so it never holds
/// more than one in flight; this is a generous backstop, not a real limit.
/// When exceeded, the *new* transfer is dropped rather than evicting an
/// in-progress legitimate one.
const MAX_CONCURRENT_FULLDOC_BUFFERS: usize = 16;

/// Aggregate ceiling on bytes buffered across ALL in-flight chunked transfers
/// (C-4). The per-buffer `MAX_FULLDOC_BYTES` bounds one transfer; without a
/// total ceiling, `MAX_CONCURRENT_FULLDOC_BUFFERS` near-max transfers could
/// still pin `16 × 512 MiB ≈ 8 GiB` and OOM a phone. Capping the sum at one
/// max-transfer's worth means a whole batch of concurrent transfers can never
/// buffer more than a single large note would — mobile-safe. A chunk that
/// would push the aggregate past this is refused (its partial transfer drops).
const MAX_FULLDOC_TOTAL_BYTES: usize = MAX_FULLDOC_BYTES;

/// How long a chunked transfer may sit without a new chunk before it's evicted
/// (C-4). Active transfers stream chunks continuously, so a multi-minute gap
/// means the transfer stalled or was abandoned; evicting frees its slot for a
/// legitimate transfer instead of letting first-chunk-only floods jam all
/// slots into a liveness DoS.
const FULLDOC_BUFFER_IDLE_TTL: Duration = Duration::from_secs(300);

/// Whether appending `incoming` bytes would push the aggregate buffered bytes
/// (`existing_total`, summed across all in-flight transfers) past the ceiling.
/// Pulled out so the boundary is unit-testable without allocating gigabytes.
fn fulldoc_total_exceeds(existing_total: usize, incoming: usize) -> bool {
    existing_total.saturating_add(incoming) > MAX_FULLDOC_TOTAL_BYTES
}

/// Chunk payload size for a FullDoc that exceeds one frame. Left well under
/// `MAX_FRAME_BYTES` so the postcard-encoded `FullDocChunk` (path + varints)
/// wrapped in the app envelope still fits a frame with comfortable headroom.
const FULLDOC_CHUNK_BYTES: usize =
    crate::sync::transport::frame::MAX_FRAME_BYTES as usize - 256 * 1024;

/// Sliding-window counter for corruption-recovery attempts. If the same
/// note triggers `RECOVERY_MAX_ATTEMPTS` recoveries inside
/// `RECOVERY_WINDOW`, it's poisoned.
#[derive(Debug)]
struct RecoveryWindow {
    first: Instant,
    count: u32,
}

const RECOVERY_MAX_ATTEMPTS: u32 = 3;
const RECOVERY_WINDOW: Duration = Duration::from_secs(30);

/// Main sync loop. Runs until the transport closes, peer sends `Bye`, or
/// the engine requests shutdown. All per-peer I/O flows through here.
async fn run_sync_loop<T: Transport>(
    mut transport: T,
    mut control_rx: mpsc::Receiver<PeerControl>,
    mut ctx: SyncCtx,
) -> SyncResult<()> {
    // On connect, advertise our note list so the peer can request anything
    // it doesn't have. (Peer will do the same.)
    announce_notes(&mut transport, &ctx).await?;

    // Drive the sync protocol loop. Kick once so initial sync messages go
    // out before we sit in the select!.
    flush_outbound(&mut transport, &mut ctx).await?;

    // Close the "missing blob, but note text never changes" gap: a note that
    // synced earlier (text identical now) whose attachment we still lack would
    // never re-trigger the per-write fetch. Seed a scan of every local note on
    // connect; the select! loop below drains it one batch per tick, INTERLEAVED
    // with live traffic. Best-effort — never fails the connection.
    //
    // PR-E (#6b): this used to be a blocking pre-loop drain that rendered every
    // local note before the loop even started, so a large roost appeared
    // connected while exchanging NO note traffic (handle_inbound only runs
    // inside the loop). We defer it into the loop instead. An earlier attempt
    // used an always-ready `biased` arm, which starved `recv` and broke real-TCP
    // convergence (the two_engines test stopped converging) — hence the prior
    // revert to blocking. The safe form is a TIMER arm (one batch per tick, NOT
    // biased): the select stays fair, so live inbound is never starved, and the
    // backfill trickles out in the background. Large roosts still ultimately
    // want a synced path→hash manifest (iroh-blobs Phase B) so this disappears.
    ctx.scan_queue = {
        let store = ctx.doc_store.lock().await;
        store.list().unwrap_or_default()
    };

    let mut last_inbound = Instant::now();
    let mut ping_interval = tokio::time::interval(Duration::from_secs(30));
    ping_interval.tick().await; // drop the immediate first tick
                                // Backfill cadence: one blob-scan batch per tick while the queue is
                                // non-empty. Gentle enough to never crowd out live traffic; the first tick
                                // fires immediately so a small roost's backfill still completes promptly.
    let mut scan_interval = tokio::time::interval(Duration::from_millis(50));

    loop {
        tokio::select! {
            // Inbound protocol message.
            msg = transport.recv() => {
                match msg {
                    Ok(m) => {
                        last_inbound = Instant::now();
                        if handle_inbound(&mut transport, &mut ctx, m).await? {
                            // Peer signaled clean close.
                            return Ok(());
                        }
                        flush_outbound(&mut transport, &mut ctx).await?;
                    }
                    Err(e) => {
                        warn!(peer = %ctx.peer_display_name, error = %e, "transport recv failed");
                        return Err(e.into());
                    }
                }
            }
            // Engine control message.
            ctl = control_rx.recv() => {
                match ctl {
                    Some(PeerControl::Local(change)) => {
                        handle_local(&mut ctx, change).await?;
                        flush_outbound(&mut transport, &mut ctx).await?;
                    }
                    Some(PeerControl::ReannounceNotes) => {
                        announce_notes(&mut transport, &ctx).await?;
                        flush_outbound(&mut transport, &mut ctx).await?;
                    }
                    Some(PeerControl::MarkDirty(path)) => {
                        // Redeliver a previously-dropped live change. The
                        // authoritative doc already holds the edit (the engine
                        // applied it before fan-out), so we only re-mark dirty
                        // and let flush regenerate the sync message.
                        ctx.dirty.insert(path);
                        flush_outbound(&mut transport, &mut ctx).await?;
                    }
                    Some(PeerControl::Shutdown) | None => {
                        let _ = transport.send(&Message::Bye).await;
                        let _ = transport.close().await;
                        return Ok(());
                    }
                }
            }
            // Idle keepalive + liveness check.
            _ = ping_interval.tick() => {
                if last_inbound.elapsed() > LIVENESS_TIMEOUT {
                    warn!(
                        peer = %ctx.peer_display_name,
                        secs = last_inbound.elapsed().as_secs(),
                        "peer liveness timeout — no inbound traffic (wedged peer); dropping so supervisor redials"
                    );
                    return Err(crate::sync::error::ProtocolError::Aborted(
                        "peer liveness timeout".into(),
                    )
                    .into());
                }
                transport.send(&Message::Ping).await?;
                // SYN-M8: reclaim blob-request slots whose reply never came
                // within BLOB_REQUEST_TIMEOUT (a peer that silently dropped the
                // RequestBlob). Reclaimed paths go back to blob_pending; re-pump
                // so the attachment retries instead of being stuck until a note
                // edit — and the in-flight window isn't held by dead requests.
                if sweep_timed_out_blob_requests(&mut ctx, Instant::now()) {
                    pump_blob_requests(&mut transport, &mut ctx).await?;
                }
            }
            // Low-priority attachment backfill (#6b): drain one blob-scan batch
            // per tick while the queue is non-empty. Disabled by the guard once
            // drained. A plain timer arm (not `biased`, not an always-ready
            // future) keeps the select fair so live inbound/control are never
            // starved — the property the earlier biased interleave violated.
            _ = scan_interval.tick(), if !ctx.scan_queue.is_empty() => {
                if let Err(e) = scan_blob_batch(&mut transport, &mut ctx).await {
                    warn!(peer = %ctx.peer_display_name, error = %e, "connect blob scan batch failed — stopping backfill");
                    // Give up the backfill on error (matches the old drain's
                    // `break`); note text sync is unaffected.
                    ctx.scan_queue.clear();
                }
            }
        }
    }
}

/// Sanitize an untrusted tombstone timestamp. Canonical definition (shared by
/// the legacy-frame path here and the CRDT-register GC in the store layer)
/// lives in [`crate::sync::store::sanitize_tombstone_millis`] — see M-1.
use crate::sync::store::sanitize_tombstone_millis;

/// String-match Automerge / DocStore error messages that mean "this doc's
/// bytes are unsalvageable on at least one side." These are recoverable
/// at the per-note level (drop local copy, ask peer for fresh) but NOT
/// at the connection level — a single bad note must not bring down the
/// whole sync session.
///
/// Empirically observed strings (extend as new failure modes show up):
///
/// - `"duplicate seq"` — two devices wrote the same actor's seq number
///   with different content. Happens after one side resets its DocStore
///   while keeping the same Identity (= same actor).
/// - `"failed to load"` — Automerge save bytes that won't deserialize
///   (truncation, write-mid-flight, or a version mismatch).
/// - `"changes out of order"` — the change graph has cycles or
///   non-monotonic Lamport timestamps. Saw this after a partial flush
///   crash on Windows during a typing burst.
/// - `"error inflating"` — the compressed save format is malformed.
fn is_crdt_corruption(msg: &str) -> bool {
    msg.contains("duplicate seq")
        || msg.contains("failed to load")
        || msg.contains("changes out of order")
        || msg.contains("error inflating")
}

/// Account a corruption-recovery attempt for `path` and return `true` if
/// the breaker has tripped (caller should poison the path). Resets the
/// window when stale so a path that was once flaky but later recovers
/// can re-arm; only sustained flapping inside the window trips the
/// breaker.
fn trip_recovery_breaker(
    attempts: &mut HashMap<NotePath, RecoveryWindow>,
    poisoned: &mut HashSet<NotePath>,
    path: &NotePath,
) -> bool {
    let now = Instant::now();
    let entry = attempts.entry(path.clone()).or_insert(RecoveryWindow {
        first: now,
        count: 0,
    });
    if now.duration_since(entry.first) > RECOVERY_WINDOW {
        entry.first = now;
        entry.count = 0;
    }
    entry.count += 1;
    if entry.count >= RECOVERY_MAX_ATTEMPTS {
        poisoned.insert(path.clone());
        true
    } else {
        false
    }
}

/// Send a `NoteList` describing everything we currently have. Receivers
/// compare against their own list and send `RequestFullDoc` for anything
/// missing.
async fn announce_notes<T: Transport>(transport: &mut T, ctx: &SyncCtx) -> SyncResult<()> {
    let paths = ctx.doc_store.lock().await.list()?;
    info!(
        peer = %ctx.peer_display_name,
        count = paths.len(),
        "sync: announcing NoteList"
    );
    transport.send(&Message::NoteList { paths }).await?;
    Ok(())
}

/// Hole B (SAF bootstrap): seed notes present on disk but absent from BOTH our
/// DocStore and the peer's announced list, so a scan-skipped `content://` roost
/// can push its own pre-existing notes to a peer instead of stranding them.
///
/// Why this is safe (it deliberately reintroduces the seeding `scan_roost`
/// refuses to do on SAF):
/// - Only paths the peer does NOT have are seeded. Every SHARED path still
///   flows through the divergence-free FullDoc-adopt path (peer announces it →
///   we `RequestFullDoc` and adopt), never a local seed — so we never build a
///   competing text object for a note a peer already holds.
/// - Each seed uses a deterministic path-derived actor
///   ([`NoteDoc::seed_from_markdown`]), so two SAF devices that seed the same
///   path from identical content produce byte-identical docs that converge to
///   one text object; differing content forks the actor's seq and surfaces as a
///   recoverable `duplicate seq` conflict (with a conflict-artifact backup),
///   never a silent dual-text-object loss.
///
/// No-op on non-scan-skipped roosts (desktop `scan_roost` already ingested
/// everything) and when no RoostFs backend is mounted. Returns the number of
/// notes seeded so the caller can decide whether to re-announce.
async fn seed_disk_only_notes(
    ctx: &SyncCtx,
    peer_paths: &HashSet<NotePath>,
    ours: &HashSet<NotePath>,
) -> usize {
    if !ctx.roost.to_string_lossy().starts_with("content://") {
        return 0;
    }
    let Some(fs) = ctx.roost_fs.as_ref() else {
        return 0;
    };
    // List paths first (cheap — one bulk call), then read content ONLY for the
    // notes we actually seed. On a reconnect every note is already in `ours`, so
    // this reduces to a single listing with zero content reads instead of
    // re-reading the whole roost each time.
    let entries = match fs.list_recursive("").await {
        Ok(e) => e,
        Err(e) => {
            warn!(error = %e, "seed_disk_only_notes: roost listing failed — nothing seeded");
            return 0;
        }
    };
    let mut seeded = 0usize;
    for entry in entries {
        if entry.is_dir || !crate::roostfs::walk::is_markdown_name(&entry.name) {
            continue;
        }
        let note = NotePath(entry.rel_path.trim_start_matches('/').replace('\\', "/"));
        if note.0.is_empty() {
            continue;
        }
        // Shared paths (we track it, or the peer has it) go through the
        // divergence-free FullDoc-adopt path — never seed those.
        if ours.contains(&note) || peer_paths.contains(&note) {
            continue;
        }
        let content = match fs.read(&note.0).await {
            Ok(bytes) => match String::from_utf8(bytes) {
                Ok(s) => s,
                Err(_) => continue, // non-UTF8: not a note we can seed
            },
            Err(e) => {
                warn!(note = %note, error = %e, "seed_disk_only_notes: read failed — skipping");
                continue;
            }
        };
        // An empty file isn't worth a CRDT doc (mirrors recover_missing_files).
        if content.trim().is_empty() {
            continue;
        }
        // Serialize the seed commit against every other mutator of this note
        // (local save, adopt) under the shared per-note guard. This path is
        // commit-only (no disk render), but taking the guard keeps it in the
        // same serialization domain so a future disk-touching change here can't
        // silently reintroduce the commit-vs-render race. See `note_locks`.
        let _note_guard = ctx.note_locks.lock(&note).await;
        let store = ctx.doc_store.lock().await;
        // Re-check under the lock: another peer task may have seeded/adopted it.
        if store.exists(&note) {
            continue;
        }
        let mut doc = match NoteDoc::seed_from_markdown(&note.0, &content) {
            Ok(d) => d,
            Err(e) => {
                warn!(note = %note, error = %e, "seed_disk_only_notes: seed build failed — skipping");
                continue;
            }
        };
        if let Err(e) = store.save(&note, &mut doc) {
            warn!(note = %note, error = %e, "seed_disk_only_notes: save failed — skipping");
            continue;
        }
        seeded += 1;
    }
    seeded
}

/// Process one inbound message. Returns `Ok(true)` if the peer signaled
/// clean close (`Bye`); the caller should then exit the loop.
async fn handle_inbound<T: Transport>(
    transport: &mut T,
    ctx: &mut SyncCtx,
    msg: Message,
) -> SyncResult<bool> {
    match msg {
        Message::SyncMessage { path, bytes } => {
            // Reject a path that would escape the docs dir before it reaches
            // load_or_create (which joins it under docs_root). Skip, don't
            // tear down the session — a single malformed path shouldn't be a
            // remote-triggerable disconnect.
            if super::policy::vet_remote_rel(&path).is_err() {
                warn!(peer = %ctx.peer_display_name, path = %path, "handle_inbound: rejecting unsafe SyncMessage path");
                return Ok(false);
            }
            // Honor the per-path circuit breaker. Once a peer is known
            // to hold a permanently-corrupt blob for a path, ignoring
            // their sync messages for that path is the only way to
            // stop the recovery loop from monopolizing the connection.
            if ctx.poisoned_paths.contains(&path) {
                return Ok(false);
            }
            // Serialize this note's commit+disk against every other entry point
            // (local save, queue adopt, other peer tasks). Held across the
            // DocStore commit AND `reconcile_doc_to_roost` below.
            let _note_guard = ctx.note_locks.lock(&path).await;
            let store = ctx.doc_store.lock().await;
            let mut doc = store.load_or_create(&path)?;
            // Self-heal on CRDT-history divergence. If the peer's history
            // contradicts ours for a shared actor (classic symptom:
            // "duplicate seq N found for actor ..."), the two docs can
            // never merge — Automerge refuses because the invariant is
            // broken. This happens in practice when one side's DocStore
            // was partially wiped/corrupted (crash, state-dir rebuild,
            // unpair without clearing docs). Killing the connection here
            // means NOTHING else can ever sync either. Instead we drop
            // our local doc, request a fresh full copy, and move on —
            // worst case we lose local uncommitted edits for this one
            // note, vs. permanently bricking sync for all notes.
            if let Err(e) = ctx.sync.receive(&path, &mut doc, &bytes) {
                let msg = e.to_string();
                if is_crdt_corruption(&msg) {
                    if trip_recovery_breaker(
                        &mut ctx.recovery_attempts,
                        &mut ctx.poisoned_paths,
                        &path,
                    ) {
                        warn!(
                            path = %path,
                            peer = %ctx.peer_display_name,
                            "handle_inbound: corruption-recovery breaker tripped — peer's blob is permanently corrupt; this note is poisoned for this peer"
                        );
                        // H-6: poisoning is terminal for this note+peer — the
                        // note will not converge again without intervention, so
                        // surface it rather than leaving it falsely "synced".
                        let _ = ctx.events.send(SyncEvent::NoteSyncFailed {
                            path: path.0.clone(),
                            reason: "repeated CRDT corruption — sync paused for this note".into(),
                        });
                        return Ok(false);
                    }
                    warn!(
                        path = %path,
                        error = %msg,
                        "handle_inbound: CRDT history conflict — dropping local doc and requesting fresh copy from peer"
                    );
                    // Before discarding local, preserve its content to a
                    // conflict artifact. This "drop local, adopt peer" recovery
                    // otherwise loses whatever local-only edits the doc held —
                    // silently. It's the differing-content escape hatch for the
                    // Hole-B seed collision (two SAF devices seeding the same
                    // path with different content fork the seed actor's seq and
                    // land here), and a general win for every history-conflict
                    // recovery. Best-effort; written after the store lock drops.
                    let local_backup = doc.render().ok().filter(|t| !t.trim().is_empty());
                    store.remove(&path)?;
                    ctx.sync.forget(&path);
                    drop(store);
                    if let Some(local_text) = local_backup {
                        // Best-effort here: this path does NOT overwrite the disk
                        // file (it only drops our local DOC and requests a fresh
                        // copy). The on-disk local content is preserved, and the
                        // fail-closed guard on the subsequent FullDoc adopt
                        // (Hole-A) re-backs-it-up before any overwrite — so a
                        // failure here isn't a data-loss point, just log it.
                        if let ConflictBackup::Failed = write_conflict_artifact(
                            &ctx.roost,
                            ctx.roost_fs.as_deref(),
                            &path,
                            &local_text,
                        )
                        .await
                        {
                            warn!(path = %path, "handle_inbound: pre-request conflict backup failed — on-disk copy still intact, adopt-time guard will re-attempt");
                        }
                    }
                    transport
                        .send(&Message::RequestFullDoc { path: path.clone() })
                        .await?;
                    return Ok(false);
                }
                return Err(e);
            }
            // `store.save` re-encodes the Automerge doc; corrupt internal
            // state surfaces here as "error inflating document chunk ops"
            // even though `receive` succeeded. Don't kill the peer task —
            // log, drop this note's local copy, and ask the peer to resend.
            if let Err(e) = store.save(&path, &mut doc) {
                let msg = e.to_string();
                if is_crdt_corruption(&msg) {
                    if trip_recovery_breaker(
                        &mut ctx.recovery_attempts,
                        &mut ctx.poisoned_paths,
                        &path,
                    ) {
                        warn!(
                            path = %path,
                            peer = %ctx.peer_display_name,
                            "handle_inbound: corruption-recovery breaker tripped on save (poisoned for this peer)"
                        );
                        // H-6: terminal for this note+peer — surface it.
                        let _ = ctx.events.send(SyncEvent::NoteSyncFailed {
                            path: path.0.clone(),
                            reason: "repeated CRDT corruption on save — sync paused for this note"
                                .into(),
                        });
                        let _ = store.remove(&path);
                        ctx.sync.forget(&path);
                        return Ok(false);
                    }
                    warn!(
                        path = %path,
                        error = %msg,
                        "handle_inbound: doc encode failed (CRDT corruption) — dropping local doc and requesting fresh copy from peer"
                    );
                    let _ = store.remove(&path);
                    ctx.sync.forget(&path);
                    drop(store);
                    transport
                        .send(&Message::RequestFullDoc { path: path.clone() })
                        .await?;
                    return Ok(false);
                }
                // Non-corruption save failure — almost always transient/local
                // I/O (ENOSPC disk-full, EACCES permissions). `receive`
                // already merged the change in memory, but it isn't persisted.
                // Tearing down the peer here (the old `return Err`) turned a
                // per-note, recoverable local error into a full connection
                // flap-loop: disconnect → redial → same note re-fails → repeat,
                // taking down every OTHER note's sync with it. Mirror the
                // corruption branch instead: isolate this one note (keep it —
                // do NOT remove; a later save may succeed once disk frees) and
                // keep the connection alive so the rest of the roost syncs.
                warn!(
                    path = %path,
                    error = %msg,
                    peer = %ctx.peer_display_name,
                    "handle_inbound: non-corruption save error (likely disk-full/permission) — isolating this note, keeping the connection"
                );
                // H-6: surface it so the note isn't falsely shown as synced.
                let _ = ctx.events.send(SyncEvent::NoteSyncFailed {
                    path: path.0.clone(),
                    reason: "could not save (disk full or permission denied)".into(),
                });
                // SYN-I5/B8 (durable retry, live path): `receive` already
                // advanced our am_sync::State for this path, but the save
                // failed — if we DON'T forget, we keep advertising heads we
                // never persisted, so the peer marks us caught-up and never
                // re-sends. Forget resets the path to the DocStore's real
                // (pre-merge) heads, so the peer re-offers the change on the
                // next exchange once the disk frees. This is the live-path
                // equivalent of the queue path's no-ACK→re-pull; no persistent
                // store is needed because the peer holds the authoritative copy.
                // KEEP the doc (no `store.remove`): a later save may succeed.
                // Do NOT `dirty.insert` — that would hot-loop the re-save while
                // the disk stays full; let the retry ride the next natural
                // exchange (NoteList / local edit / reconnect).
                ctx.sync.forget(&path);
                return Ok(false);
            }
            let mut doc_for_render = doc;
            drop(store);
            // After applying remote changes, reconcile disk: render the text,
            // or — if the remote change tombstoned the note — trash the file
            // and emit NoteDeleted instead.
            if reconcile_doc_to_roost(ctx, &path, &mut doc_for_render).await? {
                request_missing_blobs(transport, ctx, &mut doc_for_render).await?;
            }
            ctx.dirty.insert(path);
        }
        Message::NoteList { paths } => {
            // Three buckets:
            //   - peer-has, we-don't  → RequestFullDoc (bootstrap)
            //   - both-have           → mark dirty so the next
            //                           `flush_outbound` runs the
            //                           Automerge sync protocol on this
            //                           path. Without this step, files
            //                           that exist on both devices but
            //                           diverged offline never reconcile
            //                           until the user manually edits.
            //   - we-have, peer-doesn't → peer's symmetric NoteList
            //                             handling on its side already
            //                             requested them; nothing for
            //                             us to do here.
            let store = ctx.doc_store.lock().await;
            let ours: HashSet<NotePath> = store.list()?.into_iter().collect();
            drop(store);
            let mut requested = 0usize;
            let mut shared = 0usize;
            let total = paths.len();
            let peer_set: HashSet<NotePath> = paths.iter().cloned().collect();
            for p in paths {
                if !ours.contains(&p) {
                    transport.send(&Message::RequestFullDoc { path: p }).await?;
                    requested += 1;
                } else if !ctx.poisoned_paths.contains(&p) {
                    ctx.dirty.insert(p);
                    shared += 1;
                }
            }
            info!(
                peer = %ctx.peer_display_name,
                received = total,
                requested,
                shared,
                ours = ours.len(),
                "sync: received NoteList, kicked Automerge sync for shared paths"
            );

            // Hole B: once per connection, bootstrap notes this device has on
            // disk that neither we (DocStore) nor the peer track. On a
            // scan-skipped SAF roost these would otherwise never leave the
            // device. Seed them, then re-announce so the peer pulls them via
            // the divergence-free FullDoc-adopt path. No-op off SAF.
            if !ctx.seed_done {
                ctx.seed_done = true;
                let seeded = seed_disk_only_notes(ctx, &peer_set, &ours).await;
                if seeded > 0 {
                    info!(
                        peer = %ctx.peer_display_name,
                        seeded,
                        "sync: seeded peer-absent on-disk notes (SAF bootstrap) — re-announcing"
                    );
                    announce_notes(transport, ctx).await?;
                }
            }
        }
        Message::RequestFullDoc { path } => {
            if super::policy::vet_remote_rel(&path).is_err() {
                warn!(peer = %ctx.peer_display_name, path = %path, "handle_inbound: rejecting unsafe RequestFullDoc path");
                return Ok(false);
            }
            let store = ctx.doc_store.lock().await;
            // Don't FABRICATE a doc for a path we no longer hold. `load_or_create`
            // would mint a fresh empty NoteDoc (new independent text object); the
            // requester would adopt that empty doc, and when the real doc later
            // arrives from anyone we'd hit the dual-text-object divergence the
            // adoption path works hard to avoid. Reachable when corruption
            // recovery `remove()`d the doc between our NoteList announce and this
            // request. Skip instead — the peer keeps whatever it has.
            if !store.exists(&path) {
                drop(store);
                debug!(peer = %ctx.peer_display_name, path = %path, "sync: RequestFullDoc for a path we no longer hold — skipping (won't fabricate empty doc)");
                return Ok(false);
            }
            let mut doc = store.load_or_create(&path)?;
            let bytes = doc.save();
            drop(store);
            // C-4: a snapshot larger than one frame used to be dropped here,
            // making the note a permanent ghost to any new peer (advertised in
            // NoteList, never deliverable). Instead, send it as an ordered
            // `FullDocChunk` sequence when it doesn't fit; the receiver
            // reassembles and adopts identical bytes, so the (divergence-free)
            // adopt semantics are unchanged — only the single-frame size limit
            // is lifted. Small docs still take the one-shot FullDoc path.
            if bytes.len() > FULLDOC_CHUNK_BYTES {
                let chunk_count = bytes.len().div_ceil(FULLDOC_CHUNK_BYTES) as u32;
                info!(
                    peer = %ctx.peer_display_name,
                    path = %path,
                    bytes = bytes.len(),
                    chunk_count,
                    "sync: FullDoc exceeds one frame — sending chunked"
                );
                for (i, chunk) in bytes.chunks(FULLDOC_CHUNK_BYTES).enumerate() {
                    transport
                        .send(&Message::FullDocChunk {
                            path: path.clone(),
                            chunk_index: i as u32,
                            chunk_count,
                            bytes: chunk.to_vec(),
                        })
                        .await?;
                }
            } else {
                info!(
                    peer = %ctx.peer_display_name,
                    path = %path,
                    bytes = bytes.len(),
                    "sync: sending FullDoc on request"
                );
                transport.send(&Message::FullDoc { path, bytes }).await?;
            }
        }
        Message::FullDoc { path, bytes } => {
            if super::policy::vet_remote_rel(&path).is_err() {
                warn!(peer = %ctx.peer_display_name, path = %path, "handle_inbound: rejecting unsafe FullDoc path");
                return Ok(false);
            }
            // Poisoned paths: ignore incoming FullDoc. Without this the
            // peer's stuck-state doc gets re-adopted on every cycle and
            // the recovery loop restarts despite the breaker.
            if ctx.poisoned_paths.contains(&path) {
                return Ok(false);
            }
            // Serialize the WHOLE adopt (snapshot → backup I/O → commit →
            // reconcile disk) against local saves / queue adopts of this note.
            // The inner heads-CAS (below) becomes belt-and-suspenders under this
            // guard, but is kept so the invariant survives a future refactor.
            // NOTE: reached both directly and via the FullDocChunk final-chunk
            // re-dispatch — which deliberately does NOT hold this guard, so
            // acquiring here is not a re-entry (see note_locks invariants).
            let _note_guard = ctx.note_locks.lock(&path).await;
            let store = ctx.doc_store.lock().await;
            // Adoption vs. merge:
            //
            // If we have no existing doc for this note, we **adopt** the
            // remote save wholesale. Creating an empty local doc first and
            // then merging would produce two independently-created `text`
            // objects (one from our side's `NoteDoc::new`, one from the
            // remote's) that Automerge reconciles via actor-id tiebreak —
            // flipping a coin on whose text we see. Adoption sidesteps
            // that by making the remote the canonical state.
            //
            // If we *do* already have a doc, we attempt to merge. When
            // the two share history, this succeeds. When the two
            // diverged on the same actor (the "duplicate seq" class of
            // error — can happen after a DocStore wipe or a partial
            // persist on either side), merging is undefined and
            // Automerge returns an error. In that case, adopt the
            // remote copy: losing our local edits for this one note is
            // strictly better than bricking sync for every note.
            //
            // Adoption uses OUR actor (`store.our_actor_id()`), not the
            // peer's. The save bytes carry the peer's history under the
            // peer's actor; our subsequent local edits append under our
            // actor. Loading the bytes under our actor preserves the
            // history (Automerge `with_actor` only governs what NEW ops
            // get tagged with) and makes any post-adopt mutation safe
            // even if the doc is held in memory before the next reload.
            let our_actor = store.our_actor_id();
            // Captured from `local` just before a merge-failure adopt
            // discards it, then written to a conflict artifact after the
            // store lock is dropped (below). `None` in every non-lossy path.
            let mut conflict_backup: Option<String> = None;
            // B1 recommit race (CAS): the stored doc's heads at load time. The
            // adopt releases the store lock for backup I/O (below) then re-locks
            // to commit; a concurrent local save (the editor's own save path
            // takes the same lock) can land in that window. At commit we re-read
            // the heads and, if they moved, SKIP adopting this round rather than
            // overwrite the local edit — the live sync loop re-merges from fresh
            // state on the next exchange. `None` = no stored doc at load time (a
            // concurrent create then also shows up as a mismatch).
            let mut snapshot_heads: Option<Vec<automerge::ChangeHash>> = None;
            // Hole A: true when we hold NO prior DocStore record for this path
            // and are adopting the peer's copy wholesale. On first pair —
            // especially SAF/Android, where `scan_roost` is skipped — an
            // untracked file may already sit at this path on disk; the imminent
            // `render_to_roost` overwrite would erase it with no trace. We back
            // it up after the store lock drops (disk I/O off the lock).
            let adopted_fresh = !store.exists(&path);
            let mut doc = if store.exists(&path) {
                let mut local = store.load_or_create(&path)?;
                // Capture the CAS token BEFORE the merge mutates `local`.
                snapshot_heads = Some(local.heads());
                match NoteDoc::load_with_actor(&bytes, our_actor.clone()) {
                    Ok(mut remote) => match local.merge(&mut remote) {
                        Ok(_) => local,
                        Err(e) => {
                            warn!(
                                path = %path,
                                error = %e,
                                "handle_inbound: merge failed (CRDT history divergence) — adopting remote"
                            );
                            // QUARANTINE before discarding local: capture the
                            // local rendered text so the user can recover the
                            // edits this adopt is about to overwrite. Empty
                            // docs yield None (nothing worth saving). The
                            // actual disk write happens after `drop(store)`
                            // so we don't hold the store lock across an FS op.
                            conflict_backup = local.render().ok().filter(|t| !t.trim().is_empty());
                            // Re-loading the same bytes can also fail on
                            // corruption that only surfaces during the
                            // second walk. Skip the note rather than crash
                            // the peer task.
                            match NoteDoc::load_with_actor(&bytes, our_actor.clone()) {
                                Ok(d) => d,
                                Err(e2) => {
                                    warn!(
                                        path = %path,
                                        error = %e2,
                                        "handle_inbound: FullDoc adopt also failed — dropping this note from this peer for now"
                                    );
                                    // SYN-I5/B8: previously silent. Surface like
                                    // the parse-fail sibling below so a note we
                                    // couldn't adopt isn't shown as synced.
                                    let _ = ctx.events.send(SyncEvent::NoteSyncFailed {
                                        path: path.0.clone(),
                                        reason: "incoming copy couldn't be read on retry (corrupt)"
                                            .into(),
                                    });
                                    return Ok(false);
                                }
                            }
                        }
                    },
                    Err(e) => {
                        warn!(
                            path = %path,
                            error = %e,
                            "handle_inbound: FullDoc from peer didn't parse — keeping local, dropping this frame"
                        );
                        // H-6: covers a schema too new to load (C-5) as well as
                        // a corrupt/undecodable FullDoc — previously silent.
                        let _ = ctx.events.send(SyncEvent::NoteSyncFailed {
                            path: path.0.clone(),
                            reason: "incoming copy couldn't be read (corrupt or newer schema)"
                                .into(),
                        });
                        return Ok(false);
                    }
                }
            } else {
                match NoteDoc::load_with_actor(&bytes, our_actor) {
                    Ok(d) => d,
                    Err(e) => {
                        warn!(
                            path = %path,
                            error = %e,
                            "handle_inbound: FullDoc into-empty failed — skipping note (peer's stored doc is corrupt)"
                        );
                        let _ = ctx.events.send(SyncEvent::NoteSyncFailed {
                            path: path.0.clone(),
                            reason: "incoming copy couldn't be read (corrupt or newer schema)"
                                .into(),
                        });
                        return Ok(false);
                    }
                }
            };
            // FAIL-CLOSED conflict backups (fix #4): before COMMITTING the adopt
            // (which overwrites the local DocStore doc, then the on-disk file),
            // preserve any local content this adopt would destroy. If a REQUIRED
            // backup can't be written, do NOT adopt — otherwise a failed backup +
            // committed adopt loses the local edits, and because `render_to_roost`
            // only guards EMPTY-over-nonempty, a later retry would overwrite the
            // divergent local file without any backup at all. On backup failure we
            // keep local, surface `NoteSyncFailed`, and let a future round retry
            // the whole adopt (backup-first). Disk I/O must not hold the store
            // lock, and the commit below re-locks, so drop the lock now.
            drop(store);

            // (a) Merge-divergence: local CRDT edits captured just before the
            //     adopt discarded them.
            if let Some(local_text) = conflict_backup.take() {
                match write_conflict_artifact(
                    &ctx.roost,
                    ctx.roost_fs.as_deref(),
                    &path,
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
                            path = %path,
                            "handle_inbound: conflict backup FAILED — NOT adopting remote over divergent local edits; keeping local copy, will retry"
                        );
                        let _ = ctx.events.send(SyncEvent::NoteSyncFailed {
                            path: path.0.clone(),
                            reason: "couldn't back up local edits before adopting remote — kept local copy, will retry".into(),
                        });
                        return Ok(false);
                    }
                }
            }

            // (b) Hole A: a FRESH adoption (we had no DocStore record) is about to
            // render the peer's copy over whatever untracked file sits on disk at
            // this path. That on-disk content was never in any CRDT, so the
            // merge-divergence backup above can't fire. Snapshot it FIRST; a
            // failed snapshot must likewise block the adopt (same fail-closed
            // rule) so a first-pair overwrite (the SAF bootstrap case especially)
            // can't silently erase notes the joining device wrote before pairing.
            // Skipped for tombstone adoptions (trash is already recoverable) and
            // when the on-disk bytes match what we're about to write.
            if adopted_fresh && !doc.is_deleted().unwrap_or(false) {
                if let Ok(incoming) = doc.render() {
                    if let Some(existing) =
                        read_note_on_disk(&ctx.roost, ctx.roost_fs.as_deref(), &path).await
                    {
                        let existing_text = String::from_utf8_lossy(&existing);
                        if !existing_text.trim().is_empty() && existing_text != incoming {
                            match write_conflict_artifact(
                                &ctx.roost,
                                ctx.roost_fs.as_deref(),
                                &path,
                                &existing_text,
                            )
                            .await
                            {
                                ConflictBackup::Saved(conflict) => {
                                    // Surface the recovery copy so the file tree
                                    // refreshes (SAF has no live watcher).
                                    let _ = ctx.events.send(SyncEvent::NoteWritten {
                                        path: conflict.0.clone(),
                                    });
                                }
                                ConflictBackup::NothingToSave => {}
                                ConflictBackup::Failed => {
                                    warn!(
                                        path = %path,
                                        "handle_inbound: Hole-A backup FAILED — NOT adopting over untracked on-disk note; keeping local file, will retry"
                                    );
                                    let _ = ctx.events.send(SyncEvent::NoteSyncFailed {
                                        path: path.0.clone(),
                                        reason: "couldn't back up an existing local note before adopting remote — kept local copy, will retry".into(),
                                    });
                                    return Ok(false);
                                }
                            }
                        }
                    }
                }
            }

            // Backups are safe (or there was nothing to preserve) — COMMIT the
            // adopt now. Same defensive wrap as the SyncMessage path: a save can
            // fail on internal corruption even after a successful load. Don't
            // kill the peer task; the user can manually "Reset sync history" if
            // the bad note is critical.
            {
                let store = ctx.doc_store.lock().await;
                // B1 CAS: if a concurrent local save moved the stored doc's
                // heads while the lock was released for backup I/O, do NOT
                // overwrite it. Skip this round; the sync state for this path is
                // untouched, so the next exchange re-detects the divergence and
                // re-adopts against the fresh local (no data loss).
                let current_heads: Option<Vec<automerge::ChangeHash>> = if store.exists(&path) {
                    Some(store.load_or_create(&path)?.heads())
                } else {
                    None
                };
                if current_heads != snapshot_heads {
                    warn!(
                        path = %path,
                        "handle_inbound: note changed locally during the adopt backup window — skipping this round, will re-merge on the next exchange"
                    );
                    return Ok(false);
                }
                if let Err(e) = store.save(&path, &mut doc) {
                    let msg = e.to_string();
                    warn!(
                        path = %path,
                        error = %msg,
                        "handle_inbound: FullDoc save failed — dropping this note from local store"
                    );
                    let _ = store.remove(&path);
                    ctx.sync.forget(&path);
                    // SYN-I5/B8: surface it (the SyncMessage save-fail twin
                    // already does). `forget` above gives the durable retry
                    // (peer re-offers next exchange); this stops the UI showing
                    // the note as synced when it isn't on disk.
                    let _ = ctx.events.send(SyncEvent::NoteSyncFailed {
                        path: path.0.clone(),
                        reason: "could not save adopted note (disk full or permission denied)"
                            .into(),
                    });
                    return Ok(false);
                }
                info!(
                    peer = %ctx.peer_display_name,
                    path = %path,
                    "sync: adopted FullDoc from peer"
                );
                // Reset our sync state for this path so the next exchange starts
                // from the adopted heads, not the stale pre-conflict heads that
                // would otherwise keep re-triggering the error.
                ctx.sync.forget(&path);
            }

            // Adopted doc may itself be a tombstone (peer deleted while we
            // had no copy) — reconcile handles both render and trash.
            if reconcile_doc_to_roost(ctx, &path, &mut doc).await? {
                request_missing_blobs(transport, ctx, &mut doc).await?;
            }
            ctx.dirty.insert(path);
        }
        Message::FullDocChunk {
            path,
            chunk_index,
            chunk_count,
            bytes,
        } => {
            if super::policy::vet_remote_rel(&path).is_err() {
                warn!(peer = %ctx.peer_display_name, path = %path, "handle_inbound: rejecting unsafe FullDocChunk path");
                return Ok(false);
            }
            // Validate the claimed shape before allocating anything (C-4). A
            // hostile peer must not be able to exhaust memory via a bogus
            // chunk_count or an endless stream of gap-filling chunks.
            if chunk_count == 0 || chunk_count > MAX_FULLDOC_CHUNKS {
                warn!(peer = %ctx.peer_display_name, path = %path, chunk_count, "handle_inbound: rejecting FullDocChunk with implausible chunk_count");
                ctx.full_doc_buffers.remove(&path);
                return Ok(false);
            }
            // Evict stalled/abandoned transfers before anything else (C-4), so
            // first-chunk-only floods can't permanently jam the bounded slots.
            let now = Instant::now();
            ctx.full_doc_buffers
                .retain(|_, b| now.duration_since(b.last_activity) < FULLDOC_BUFFER_IDLE_TTL);
            // Aggregate-bytes ceiling across ALL in-flight transfers (C-4): the
            // per-buffer cap bounds one transfer, this bounds the sum so a batch
            // of concurrent transfers can't collectively OOM a phone. Computed
            // before the mutable buffer borrow; `existing_total` already counts
            // this path's current bytes, so + incoming = the post-append total.
            let existing_total: usize = ctx.full_doc_buffers.values().map(|b| b.bytes.len()).sum();
            if fulldoc_total_exceeds(existing_total, bytes.len()) {
                warn!(peer = %ctx.peer_display_name, path = %path, existing_total, incoming = bytes.len(), "handle_inbound: chunked FullDoc transfers exceed aggregate buffer ceiling — dropping");
                ctx.full_doc_buffers.remove(&path);
                return Ok(false);
            }
            // Bound the NUMBER of concurrent in-flight transfers, not just each
            // buffer's size (C-4). A new-path transfer is refused once we're at
            // the cap; an already-in-progress path is always allowed to
            // continue so a flood can't evict a legitimate transfer.
            if !ctx.full_doc_buffers.contains_key(&path)
                && ctx.full_doc_buffers.len() >= MAX_CONCURRENT_FULLDOC_BUFFERS
            {
                warn!(peer = %ctx.peer_display_name, path = %path, buffers = ctx.full_doc_buffers.len(), "handle_inbound: too many concurrent chunked FullDoc transfers — dropping new one");
                return Ok(false);
            }
            let buf = ctx
                .full_doc_buffers
                .entry(path.clone())
                .or_insert_with(|| FullDocBuffer {
                    chunk_count,
                    next_index: 0,
                    bytes: Vec::new(),
                    last_activity: now,
                });
            // Reset on any inconsistency (count changed mid-stream, or an
            // out-of-order/duplicate index) — a fresh sequence must restart.
            if buf.chunk_count != chunk_count || buf.next_index != chunk_index {
                if chunk_index == 0 {
                    *buf = FullDocBuffer {
                        chunk_count,
                        next_index: 0,
                        bytes: Vec::new(),
                        last_activity: now,
                    };
                } else {
                    warn!(peer = %ctx.peer_display_name, path = %path, expected = buf.next_index, got = chunk_index, "handle_inbound: out-of-order FullDocChunk — dropping partial transfer");
                    ctx.full_doc_buffers.remove(&path);
                    return Ok(false);
                }
            }
            if buf.bytes.len().saturating_add(bytes.len()) > MAX_FULLDOC_BYTES {
                warn!(peer = %ctx.peer_display_name, path = %path, "handle_inbound: chunked FullDoc exceeds max reassembled size — dropping");
                ctx.full_doc_buffers.remove(&path);
                return Ok(false);
            }
            buf.bytes.extend_from_slice(&bytes);
            buf.last_activity = now;
            buf.next_index += 1;
            if buf.next_index < buf.chunk_count {
                return Ok(false); // more chunks to come
            }
            // Final chunk: reassembled. Adopt exactly as a one-shot FullDoc by
            // re-dispatching through the proven adopt path (boxed because
            // handle_inbound is async and this is a self-call).
            let full = ctx
                .full_doc_buffers
                .remove(&path)
                .map(|b| b.bytes)
                .unwrap_or_default();
            info!(peer = %ctx.peer_display_name, path = %path, bytes = full.len(), "sync: reassembled chunked FullDoc — adopting");
            return Box::pin(handle_inbound(
                transport,
                ctx,
                Message::FullDoc { path, bytes: full },
            ))
            .await;
        }
        Message::NoteDelete { path, at_millis } => {
            // Backward-compat ONLY. A pre-stage-2 peer still sends this
            // destructive frame. New code never emits it — deletes now ride
            // the CRDT `deleted` flag. We translate the frame into a CRDT
            // tombstone (mark the doc deleted, keep it) instead of destroying
            // the doc, so we converge with stage-2 peers and the tombstone
            // keeps propagating. `reconcile_doc_to_roost` trashes the file
            // and emits NoteDeleted; `dirty` re-propagates the tombstone.
            //
            // SYN-B1 (audit follow-up): this is the one legacy message arm
            // that had NOT been vetted — SyncMessage/FullDoc/FullDocChunk all
            // gate on vet_remote_rel before touching the DocStore; this arm
            // went straight to load_or_create + trash. A paired peer could
            // target `.git/config` / `.corvux/vault-id.bin` / any non-markdown
            // path. Same skip-don't-disconnect treatment as the other arms.
            if super::policy::vet_remote_rel(&path).is_err() {
                warn!(peer = %ctx.peer_display_name, path = %path, "handle_inbound: rejecting unsafe NoteDelete path");
                return Ok(false);
            }
            ctx.poisoned_paths.remove(&path);
            ctx.recovery_attempts.remove(&path);
            // Serialize the tombstone commit + disk trash against this note's
            // other mutators.
            let _note_guard = ctx.note_locks.lock(&path).await;
            let store = ctx.doc_store.lock().await;
            let mut doc = store.load_or_create(&path)?;
            // The legacy peer's clock is untrusted (M-1). A raw `at_millis` of 0
            // makes the tombstone un-reapable; a value far in the past makes it
            // reap on the next sweep and resurrect the note; a future value
            // never reaps. Clamp anything outside a sane window to our local
            // receive time before stamping the tombstone.
            let safe_at = sanitize_tombstone_millis(at_millis, super::util::now_millis());
            doc.mark_deleted(safe_at)?;
            store.save(&path, &mut doc)?;
            drop(store);
            reconcile_doc_to_roost(ctx, &path, &mut doc).await?;
            ctx.dirty.insert(path);
        }
        Message::Ping => {
            transport.send(&Message::Pong).await?;
        }
        Message::Pong => {}
        Message::Bye => {
            info!(peer = %ctx.peer_display_name, "peer said Bye");
            let _ = transport.close().await;
            return Ok(true);
        }
        // Handshake messages are illegal post-handshake.
        Message::Hello { .. }
        | Message::PairingChallenge { .. }
        | Message::PairingProof { .. }
        | Message::PairingOk { .. }
        | Message::PairingFailed { .. } => {
            return Err(crate::sync::error::ProtocolError::UnexpectedMessage {
                state: "Sync",
                actual: "handshake-message",
            }
            .into());
        }
        // Should never reach here: `EncryptedTransport::recv` unwraps
        // envelopes before they hit the inbound handler. If we DO see one,
        // the peer double-wrapped or the encryption layer was bypassed —
        // either way it's a protocol violation.
        Message::EncryptedEnvelope { .. } => {
            return Err(crate::sync::error::ProtocolError::UnexpectedMessage {
                state: "Sync",
                actual: "EncryptedEnvelope (double-wrap?)",
            }
            .into());
        }
        // ── Attachment blob sync (PROTOCOL_VERSION 4) ───────────────
        // Peer wants an attachment we may have. Read it (RoostFs enforces
        // path-scope), enforce the cap, and reply — or send the typed
        // negative so the requester can stop asking (C-2).
        Message::RequestBlob { rel } => {
            let reply = match read_blob_for_send(ctx, &rel).await {
                Ok(bytes) => Message::Blob { rel, bytes },
                Err(reason) => {
                    // DI-12: if OUR OWN attachment exceeds the sync cap, surface
                    // AttachmentTooLarge on THIS (owner) device too. Previously
                    // only the requesting peer emitted it, so the person who
                    // pasted the oversized image never learned it wouldn't sync.
                    if matches!(reason, BlobUnavailableReason::OverCap) {
                        let _ = ctx
                            .events
                            .send(SyncEvent::AttachmentTooLarge { rel: rel.clone() });
                    }
                    Message::BlobUnavailable { rel, reason }
                }
            };
            transport.send(&reply).await?;
        }
        // Peer sent an attachment we asked for. Re-validate path + cap
        // (never trust the peer's framing), write it via the backend
        // (SAF-safe), and tell the UI so the embed can re-render.
        Message::Blob { rel, bytes } => {
            // SYN-B9: only accept a blob we actually requested (an in-flight
            // slot), and vet path + cap + content-address before writing — so a
            // peer can't fill the roost with unsolicited blobs or overwrite a
            // content-addressed attachment with forged bytes.
            let was_requested = ctx.blob_inflight.remove(&rel).is_some();
            if !was_requested {
                warn!(rel = %rel, "Blob: rejected unsolicited attachment (no matching request)");
            } else if let Err(reject) = super::policy::vet_remote_blob_rel(&rel, &bytes) {
                warn!(rel = %rel, ?reject, "Blob: rejected hostile path / over-cap / content mismatch");
            } else {
                match write_blob(ctx, &rel, &bytes).await {
                    Ok(()) => {
                        let _ = ctx
                            .events
                            .send(SyncEvent::AttachmentWritten { rel: rel.clone() });
                        info!(rel = %rel, bytes = bytes.len(), "sync: wrote attachment from peer");
                    }
                    Err(e) => warn!(rel = %rel, error = %e, "Blob: write failed"),
                }
            }
            // A slot freed — send the next queued request (C-7 drain).
            pump_blob_requests(transport, ctx).await?;
        }
        // Peer can't give us this blob. Mark terminal so we don't loop
        // re-requesting it; a later edit to the referencing note clears it
        // by virtue of a fresh reconcile re-evaluating refs.
        Message::BlobUnavailable { rel, reason } => {
            ctx.blob_inflight.remove(&rel);
            ctx.blob_unavailable.insert(rel.clone());
            debug!(rel = %rel, ?reason, "sync: peer can't provide attachment");
            // Over-cap is permanent (the file can't fit a sync frame) — tell
            // the UI so the embed shows "too large" instead of a forever
            // "not synced yet". NotFound/Rejected are transient (source
            // offline / not-its-copy) and stay generic.
            if matches!(reason, BlobUnavailableReason::OverCap) {
                let _ = ctx
                    .events
                    .send(SyncEvent::AttachmentTooLarge { rel: rel.clone() });
            }
            // A slot freed — send the next queued request.
            pump_blob_requests(transport, ctx).await?;
        }
    }
    Ok(false)
}

/// Process one local change from the watcher: read the markdown from disk,
/// apply it to the CRDT, mark the note dirty so the outbound flush picks
/// it up.
async fn handle_local(ctx: &mut SyncCtx, change: LocalChange) -> SyncResult<()> {
    match change {
        // Single-writer: the engine already applied this modification to the
        // shared docstore before fan-out (see `SyncEngine::apply_local_change`).
        // The per-peer task is pure transport now — just mark the path dirty
        // so `flush_outbound` generates the sync message from the
        // already-current doc. We deliberately do NOT re-apply here: the
        // engine only fans a change out when its central apply reported a
        // real change, and re-applying would (because apply_markdown
        // short-circuits unchanged content) sometimes fail to mark dirty —
        // silently stalling live sync to connected peers.
        LocalChange::Modified { path, .. } | LocalChange::ModifiedWithContent { path, .. } => {
            ctx.dirty.insert(path);
        }
        LocalChange::Removed { path } => {
            // CRDT tombstone, NOT destructive removal. Set `deleted=true` in
            // the doc and let it propagate through the normal Automerge sync
            // path (so peers converge and the delete reaches offline devices
            // via the queue's FullDoc). We deliberately do NOT `forget` the
            // sync state — the tombstone must keep syncing — and we no longer
            // emit a `NoteDelete` frame. The markdown file itself was already
            // trashed by the delete entry point (or removed externally, which
            // is what triggered this watcher event), so there's nothing to
            // trash here on the local side.
            let _note_guard = ctx.note_locks.lock(&path).await;
            let store = ctx.doc_store.lock().await;
            let mut doc = store.load_or_create(&path)?;
            // Idempotent: when the delete entry point (`mark_note_deleted`)
            // already tombstoned the doc, skip the redundant CRDT op and just
            // re-propagate. Only the watcher-driven external-delete path, where
            // nothing wrote the tombstone yet, actually marks here.
            if !doc.is_deleted()? {
                doc.mark_deleted(super::util::now_millis())?;
                store.save(&path, &mut doc)?;
            }
            drop(store);
            ctx.dirty.insert(path);
        }
        LocalChange::RemovedPrefix { .. } => {
            // Prefix removals (external folder deletes) are expanded into a
            // tombstone per nested note centrally in the engine's
            // watcher-intake loop (`mark_note_deleted` for each), BEFORE
            // fan-out — a peer task never receives one. Handled here only for
            // match exhaustiveness; the central path owns the docstore sweep.
        }
    }
    Ok(())
}

/// Drain pending sync messages: for every dirty path, ask Automerge for
/// the next message and send it. Loops until no path has pending output.
async fn flush_outbound<T: Transport>(transport: &mut T, ctx: &mut SyncCtx) -> SyncResult<()> {
    // Copy dirty set so we can mutate while iterating.
    let paths: Vec<NotePath> = ctx.dirty.drain().collect();
    for path in paths {
        let store = ctx.doc_store.lock().await;
        let mut doc = store.load_or_create(&path)?;
        let maybe = ctx.sync.generate(&path, &mut doc)?;
        drop(store);
        if let Some(bytes) = maybe {
            transport
                .send(&Message::SyncMessage {
                    path: path.clone(),
                    bytes,
                })
                .await?;
            // Stay dirty: the peer's reply may prompt further messages
            // until the protocol quiesces.
            ctx.dirty.insert(path);
        }
    }
    Ok(())
}

/// Reconcile a doc's state to disk after a merge/apply, and emit the matching
/// UI event. This is the single convergence point for the delete-vs-edit fix:
/// every device that merges a doc whose `deleted` flag is `true` deterministically
/// trashes the markdown here, so all devices end in the same disk state.
///
/// - `deleted == true`  → trash the file (if present) and emit `NoteDeleted`.
/// - `deleted == false` → render the text and emit `NoteWritten` (if disk changed).
///
/// Returns `Ok(true)` iff the note's markdown actually changed on disk —
/// the caller uses that to decide whether to re-scan the note for missing
/// attachments (no write → refs unchanged → nothing to fetch).
async fn reconcile_doc_to_roost(
    ctx: &SyncCtx,
    path: &NotePath,
    doc: &mut NoteDoc,
) -> SyncResult<bool> {
    if doc.is_deleted()? {
        match trash_in_roost(&ctx.roost, ctx.roost_fs.as_deref(), path).await {
            Ok(true) => {
                let _ = ctx.events.send(SyncEvent::NoteDeleted {
                    path: path.0.clone(),
                });
            }
            // AlreadyGone: disk already agrees with the tombstone. Silent — a
            // convergent re-delete must not spam NoteSyncFailed.
            Ok(false) => {}
            // SYN-I5/B8: the CRDT says deleted but the file survives (read-only
            // roost, permission, I/O). Surface it AND enroll for durable retry
            // — the supervisor re-attempts the trash each tick until it lands
            // (the live-path equivalent of the queue's retain→re-pull). Without
            // the retry this would be a permanent tombstone↔file divergence the
            // watcher could resurrect on the next edit.
            Err(e) => {
                let _ = ctx.events.send(SyncEvent::NoteSyncFailed {
                    path: path.0.clone(),
                    reason: format!("could not trash deleted note: {e}"),
                });
                if let Err(e2) = ctx.pending_trash.lock().await.insert(path) {
                    warn!(path = %path, error = %e2, "reconcile_doc_to_roost: pending_trash insert failed");
                }
            }
        }
        return Ok(false);
    }
    match render_to_roost(&ctx.roost, ctx.roost_fs.as_deref(), path, doc).await? {
        RenderOutcome::Wrote => {
            let _ = ctx.events.send(SyncEvent::NoteWritten {
                path: path.0.clone(),
            });
            // Only a fresh write warrants chasing this note's attachments.
            Ok(true)
        }
        RenderOutcome::Unchanged => Ok(false),
        // H-6: both a hard write failure and a data-loss-guard skip mean the
        // note isn't on disk as the CRDT says it should be — surface it so it
        // isn't falsely shown as synced.
        RenderOutcome::Skipped { reason } | RenderOutcome::Failed { reason } => {
            let _ = ctx.events.send(SyncEvent::NoteSyncFailed {
                path: path.0.clone(),
                reason,
            });
            Ok(false)
        }
    }
}

/// How many blob requests may be outstanding to a peer at once. Acts as a
/// flow-control window: a note with more missing attachments than this parks
/// the rest in `blob_pending` and `pump_blob_requests` refills the window as
/// each `Blob` / `BlobUnavailable` reply lands — so even a 200-image note
/// drains fully (C-7) while never queuing more than this many multi-MB
/// responses on the peer at a time.
const MAX_BLOB_INFLIGHT: usize = 8;

/// SYN-M8: deadline on a `RequestBlob` → `Blob`/`BlobUnavailable` round-trip.
/// A peer that silently drops the request (crash, bug, hostile) must not hold
/// the in-flight slot forever — the ping-tick sweep reclaims a request older
/// than this back to `blob_pending` for re-request. Generous so a genuinely
/// slow transfer of a large blob over a poor mobile link isn't falsely retried
/// (8 MiB in 120s ≈ 70 KB/s — slower than any usable link).
const BLOB_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

/// True iff `rel` is a roost-relative attachment path under `.attachments/`
/// with no traversal. Defense-in-depth on top of `RoostFs::validate_rel`:
/// we never *issue* a request or *write* for anything outside `.attachments/`.
pub(super) fn is_attachment_rel(rel: &str) -> bool {
    let norm = rel.replace('\\', "/");
    let norm = norm.trim_start_matches("./");
    norm.starts_with(".attachments/")
        && norm.len() > ".attachments/".len()
        && !norm.split('/').any(|s| s == "..")
}

/// Minimal percent-decoder so a markdown ref like
/// `.attachments/my%20pic.webp` resolves to the on-disk `my pic.webp`.
/// Our own pasted filenames never need this, but user-imported ones can.
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let hi = (b[i + 1] as char).to_digit(16);
            let lo = (b[i + 2] as char).to_digit(16);
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Extract deduped `.attachments/…` refs from rendered markdown. Handles
/// both `![alt](.attachments/x.webp)` and `![[.attachments/x.webp]]`.
/// Anchors on the `.attachments/` prefix and reads to the first terminator,
/// so a malicious note can't make us request a traversal/absolute path
/// (the `RoostFs` boundary is the real guard; this just avoids the request).
fn parse_attachment_refs(md: &str) -> Vec<String> {
    const MARK: &str = ".attachments/";
    let mut out: Vec<String> = Vec::new();
    let mut from = 0;
    while let Some(idx) = md[from..].find(MARK) {
        let start = from + idx;
        let mut end = md.len();
        for (i, c) in md[start..].char_indices() {
            if matches!(
                c,
                ')' | ']' | '"' | '\'' | '|' | '#' | '\n' | '\r' | '\t' | ' ' | '<' | '>'
            ) {
                end = start + i;
                break;
            }
        }
        from = end.max(start + MARK.len());
        let decoded = percent_decode(&md[start..end]);
        if is_attachment_rel(&decoded) && !out.contains(&decoded) {
            out.push(decoded);
        }
    }
    out
}

/// Does this attachment already exist on disk? Cheap existence check via the
/// backend (or `std::fs` fallback when no backend is installed).
async fn blob_exists(ctx: &SyncCtx, rel: &str) -> bool {
    resolve_fs(&ctx.roost, ctx.roost_fs.as_deref())
        .as_fs()
        .exists(rel)
        .await
        .unwrap_or(false)
}

/// After a note's markdown lands on disk, queue any attachments it
/// references that we don't have yet, then pump the request window. Dedups
/// against in-flight, terminal, and already-pending sets; only queues paths
/// genuinely missing on disk. Never fetches for a note whose `render()`
/// failed.
async fn request_missing_blobs<T: Transport>(
    transport: &mut T,
    ctx: &mut SyncCtx,
    doc: &mut NoteDoc,
) -> SyncResult<()> {
    let md = match doc.render() {
        Ok(t) => t,
        Err(_) => return Ok(()),
    };
    for rel in parse_attachment_refs(&md) {
        if ctx.blob_inflight.contains_key(&rel)
            || ctx.blob_unavailable.contains(&rel)
            || ctx.blob_pending.contains(&rel)
        {
            continue;
        }
        if blob_exists(ctx, &rel).await {
            continue;
        }
        ctx.blob_pending.insert(rel);
    }
    pump_blob_requests(transport, ctx).await
}

/// Number of notes processed per backfill tick. Bounds per-tick CPU (each is
/// an Automerge load + render) so the scan stays a low-priority trickle the
/// select! loop interleaves with real traffic, never a blocking spike.
const BLOB_SCAN_BATCH: usize = 16;

/// Process one batch of the on-connect backfill: pop notes off `scan_queue`,
/// render each, and request any referenced attachment missing on disk. Drives
/// the "missing blob, but note text never changes" fix without blocking the
/// sync loop — the per-write trigger only fires on text changes, so a note
/// that synced earlier (text identical now) whose attachment we still lack
/// would otherwise be stuck. Best-effort: a note that won't load/render is
/// skipped, not fatal.
///
/// Cost is O(local notes) renders spread across ticks — fine at launch scale;
/// large roosts should move to a synced path→hash manifest (iroh-blobs Phase
/// B note) so reconnects don't re-render everything.
async fn scan_blob_batch<T: Transport>(transport: &mut T, ctx: &mut SyncCtx) -> SyncResult<()> {
    for _ in 0..BLOB_SCAN_BATCH {
        let Some(path) = ctx.scan_queue.pop() else {
            break;
        };
        let mut doc = {
            let store = ctx.doc_store.lock().await;
            match store.load_or_create(&path) {
                Ok(d) => d,
                Err(_) => continue,
            }
        };
        request_missing_blobs(transport, ctx, &mut doc).await?;
    }
    Ok(())
}

/// Send `RequestBlob`s from `blob_pending` until the in-flight window is
/// full. Called after queuing new refs and again each time a reply frees a
/// slot, so a note with many attachments drains over successive replies
/// without ever exceeding `MAX_BLOB_INFLIGHT` outstanding responses.
/// SYN-M8: move any blob request older than `BLOB_REQUEST_TIMEOUT` from
/// `blob_inflight` back to `blob_pending`. Returns `true` if any were reclaimed
/// (the caller then re-pumps to re-issue them). Split out from the ping-tick
/// arm so the deadline logic is unit-testable without a transport.
fn sweep_timed_out_blob_requests(ctx: &mut SyncCtx, now: Instant) -> bool {
    let timed_out: Vec<String> = ctx
        .blob_inflight
        .iter()
        .filter(|(_, sent)| now.duration_since(**sent) > BLOB_REQUEST_TIMEOUT)
        .map(|(rel, _)| rel.clone())
        .collect();
    if timed_out.is_empty() {
        return false;
    }
    for rel in timed_out {
        ctx.blob_inflight.remove(&rel);
        ctx.blob_pending.insert(rel);
    }
    true
}

async fn pump_blob_requests<T: Transport>(transport: &mut T, ctx: &mut SyncCtx) -> SyncResult<()> {
    while ctx.blob_inflight.len() < MAX_BLOB_INFLIGHT {
        // Take one pending ref (any order — set has no priority).
        let Some(rel) = ctx.blob_pending.iter().next().cloned() else {
            break;
        };
        ctx.blob_pending.remove(&rel);
        // A ref could have been satisfied or marked terminal since queuing.
        if ctx.blob_inflight.contains_key(&rel) || ctx.blob_unavailable.contains(&rel) {
            continue;
        }
        ctx.blob_inflight.insert(rel.clone(), Instant::now());
        transport.send(&Message::RequestBlob { rel }).await?;
    }
    Ok(())
}

/// Read an attachment to send to a requesting peer. Enforces the
/// `.attachments/` scope and the sync cap; maps a missing file / bad path to
/// the typed negative reason so the requester stops asking.
async fn read_blob_for_send(ctx: &SyncCtx, rel: &str) -> Result<Vec<u8>, BlobUnavailableReason> {
    if !is_attachment_rel(rel) {
        return Err(BlobUnavailableReason::Rejected);
    }
    // One guarded read (resolve_fs → backend or on-demand NativeFs): a symlink
    // under `.attachments/` (or a symlinked parent) can no longer exfiltrate an
    // out-of-roost file to a requesting peer — safe_resolve rejects the escape.
    let fsh = resolve_fs(&ctx.roost, ctx.roost_fs.as_deref());
    let bytes = match fsh.as_fs().read(rel).await {
        Ok(b) => b,
        Err(crate::roostfs::RoostError::NotFound(_)) => {
            return Err(BlobUnavailableReason::NotFound)
        }
        Err(_) => return Err(BlobUnavailableReason::Rejected),
    };
    if bytes.len() as u64 > SYNC_BLOB_MAX_BYTES {
        return Err(BlobUnavailableReason::OverCap);
    }
    Ok(bytes)
}

/// Persist a received attachment via the backend (SAF-safe) or `std::fs`
/// fallback. Caller has already validated path + cap; this re-guards the
/// path and surfaces a string error (mirrors `render_to_roost`'s
/// log-and-continue style — no `RoostError → SyncError` conversion exists).
async fn write_blob(ctx: &SyncCtx, rel: &str, bytes: &[u8]) -> Result<(), String> {
    if !is_attachment_rel(rel) {
        return Err("path outside .attachments/".into());
    }
    // One guarded write (resolve_fs → backend or on-demand NativeFs). The
    // backend's write_atomic + safe_resolve reject a final-component or parent
    // symlink that escapes the roost — no raw std::fs, no overwrite-outside.
    let fsh = resolve_fs(&ctx.roost, ctx.roost_fs.as_deref());
    let fs = fsh.as_fs();
    if let Err(e) = fs.mkdir(".attachments").await {
        if !matches!(e, crate::roostfs::RoostError::AlreadyExists(_)) {
            return Err(format!("mkdir .attachments: {e}"));
        }
    }
    fs.write_atomic(rel, bytes)
        .await
        .map_err(|e| format!("write_atomic: {e}"))?;
    Ok(())
}

/// Move a note's markdown to trash (recoverable), going through the RoostFs
/// backend when installed and falling back to `std::fs` otherwise. Returns
/// `true` only when a file was actually present and removed — so callers fire
/// `NoteDeleted` on a real disk transition, not on every redundant sync of an
/// already-trashed tombstone.
/// Trash a note's markdown (recoverable) when adopting a tombstone on the LIVE
/// receive path. Three outcomes, deliberately distinguished (SYN-I5, B8) so a
/// genuine delete failure isn't swallowed as "done":
/// - `Ok(true)`  — a file was present and removed → caller fires `NoteDeleted`.
/// - `Ok(false)` — `NotFound`, disk already agrees with the tombstone → no
///   event (a convergent re-delete must not spam `NoteSyncFailed`).
/// - `Err(e)`    — the delete genuinely failed (permissions, read-only roost,
///   I/O). The DocStore is tombstoned but the file survives, so the desktop
///   watcher would resurrect it — caller MUST surface `NoteSyncFailed`.
///
/// This is the single trash implementation shared by BOTH receive paths:
/// `SyncEngine::trash_note` (queue) delegates here, so the 3-state contract and
/// the symlink-safe `resolve_fs` routing can't drift between live and queue.
pub(super) async fn trash_in_roost(
    roost: &std::path::Path,
    roost_fs: Option<&dyn RoostFs>,
    note: &NotePath,
) -> SyncResult<bool> {
    // SYN-B1 defense in depth (twin of render_to_roost's check below): the
    // frame handlers already vet every remote note path, but this is the
    // actual arbitrary-DELETE write site — refuse a policy-rejected path here
    // too so no present-or-future caller can trash a `.git/*`, `.corvux/*`, or
    // non-markdown file. A local-origin note (mark_note_deleted, the queue
    // path's trash_note) is always a scanned `.md` outside hidden dirs, so
    // this never false-rejects a legitimate delete.
    if super::policy::vet_remote_rel(note).is_err() {
        warn!(path = %note, "trash_in_roost: refusing to trash a policy-rejected path");
        return Err(crate::sync::error::SyncError::Io(std::io::Error::other(
            "path rejected by remote write policy",
        )));
    }
    // One guarded delete (resolve_fs → backend or on-demand NativeFs). The
    // backend's trash routes through safe_resolve, so a symlinked parent can't
    // make us delete a file outside the roost.
    match resolve_fs(roost, roost_fs).as_fs().trash(&note.0).await {
        Ok(()) => Ok(true),
        Err(crate::roostfs::RoostError::NotFound(_)) => Ok(false),
        Err(e) => {
            warn!(path = %note, error = %e, "trash_in_roost: trash failed");
            Err(crate::sync::error::SyncError::Io(std::io::Error::other(
                e.to_string(),
            )))
        }
    }
}

/// Write the rendered CRDT text back to disk so the user sees the updated
/// note in their editor.
///
/// When a `RoostFs` backend is installed (normal case — both desktop
/// `NativeFs` and Android `SafFs` are live after `open_roost`), we write
/// through it: `mkdir` for any missing parent directory, then
/// `write_atomic`. This is the only path that works on Android SAF
/// content:// roots — `std::fs` returns EROFS there.
///
/// If no backend is installed (e.g. sync enabled before a roost was
/// opened — shouldn't happen but we don't want to crash), we fall back
/// to `std::fs` so desktop still works out of the box.
/// Returns `Ok(true)` iff the roost on disk actually changed. Short-circuit
/// no-ops return `Ok(false)` so the caller can skip firing UI events for
/// writes that didn't move any bytes.
/// Outcome of rendering a received note to the roost. Distinguishes the cases
/// the caller reacts to differently (H-6): a genuine write failure and a
/// data-loss-guard skip both need surfacing to the user, whereas `Unchanged`
/// (on-disk already matched) is a silent no-op and only `Wrote` warrants a
/// blob-request pass.
pub(super) enum RenderOutcome {
    /// New content was written to disk.
    Wrote,
    /// On-disk content already matched the render — nothing to do.
    Unchanged,
    /// Deliberately not written by a data-loss guard; the note stays in the
    /// DocStore but disk and CRDT now disagree.
    Skipped { reason: String },
    /// A write was attempted and failed (disk full, permission, reserved
    /// filename, unmappable path).
    Failed { reason: String },
}

/// Read a note's current on-disk bytes through the same backend
/// `render_to_roost` writes through — `RoostFs` when installed (the only path
/// that works for SAF `content://` roots), else `std::fs`. `None` when the file
/// is absent or unreadable. Used by the Hole-A backup: know what we're about to
/// overwrite before a fresh adoption clobbers it.
pub(super) async fn read_note_on_disk(
    roost: &std::path::Path,
    roost_fs: Option<&dyn RoostFs>,
    note: &NotePath,
) -> Option<Vec<u8>> {
    resolve_fs(roost, roost_fs).as_fs().read(&note.0).await.ok()
}

/// H-symlink (root fix): every fallback filesystem op now routes through a
/// `RoostFs` — the installed backend when present, else an on-demand `NativeFs`
/// over the roost. `NativeFs` is the SAME hardened backend the production
/// desktop path uses: its `safe_resolve` canonicalizes every path component
/// (rejecting a symlink — including the FINAL one — that escapes the roost) and
/// `write_atomic` guards its temp sibling too (AUDIT-076/076b). This replaces
/// the earlier per-site, parent-only `std::fs` guards (which followed a
/// final-component symlink on a direct write, and were missing entirely from
/// several sites): there is no longer ANY raw `std::fs` path on the sync write/
/// read/delete surface, so no unguarded symlink escape can exist. `NativeFs`
/// only ever backs the fallback on desktop/tests — on Android `SafFs` is always
/// installed (and `std::fs` can't touch a SAF `content://` root anyway).
///
/// Held in a `MaybeNative` so the on-demand `NativeFs` outlives the borrow.
pub(super) enum MaybeNative<'a> {
    Borrowed(&'a dyn RoostFs),
    Owned(NativeFs),
}

impl MaybeNative<'_> {
    pub(super) fn as_fs(&self) -> &dyn RoostFs {
        match self {
            MaybeNative::Borrowed(f) => *f,
            MaybeNative::Owned(n) => n,
        }
    }
}

/// Resolve the optional backend to a concrete `RoostFs` (see `MaybeNative`).
pub(super) fn resolve_fs<'a>(
    roost: &std::path::Path,
    roost_fs: Option<&'a dyn RoostFs>,
) -> MaybeNative<'a> {
    match roost_fs {
        Some(fs) => MaybeNative::Borrowed(fs),
        None => MaybeNative::Owned(NativeFs::new(roost)),
    }
}

pub(super) async fn render_to_roost(
    roost: &std::path::Path,
    roost_fs: Option<&dyn RoostFs>,
    note: &NotePath,
    doc: &mut NoteDoc,
) -> SyncResult<RenderOutcome> {
    // SYN-B1 defense in depth: the frame handlers already vet every remote note
    // path, but this is the actual arbitrary-content write site — refuse a
    // policy-rejected path here too so no present-or-future caller can render a
    // `.git/*`, `.corvux/*`, or non-markdown file. A local-origin note is always
    // a scanned `.md` outside hidden dirs, so this never false-rejects.
    if super::policy::vet_remote_rel(note).is_err() {
        return Ok(RenderOutcome::Skipped {
            reason: "path rejected by remote write policy".into(),
        });
    }
    let text = doc.render()?;

    // H-symlink root fix: one guarded path. `resolve_fs` yields the installed
    // backend or an on-demand `NativeFs`, so the write always goes through
    // `safe_resolve` (no raw `std::fs`, no symlink escape) — the std::fs
    // fallback that used to live below is gone.
    let fsh = resolve_fs(roost, roost_fs);
    let fs = fsh.as_fs();
    {
        // Roost-relative path — the backend rejects absolute / traversal /
        // symlink-escape paths at its boundary, so we pass the NotePath
        // straight through.
        let rel = &note.0;

        // Short-circuit when current on-disk content matches. Avoids
        // triggering the watcher (desktop) and needless SAF writes
        // (Android — each write crosses a JNI boundary).
        if let Ok(existing) = fs.read(rel).await {
            if existing == text.as_bytes() {
                return Ok(RenderOutcome::Unchanged);
            }
            // DATA-LOSS SAFETY GUARD (narrowed).
            // Fire ONLY on the true divergence signature: an EMPTY render
            // about to blank a non-empty file, where the doc's own md_hash
            // says it should NOT be empty. That's the "two text objects from
            // independent NoteDoc::new merged, actor-id tiebreak picked the
            // empty sibling" bug.
            //
            // The OLD check refused whenever `md_hash != sha256(render())`,
            // which is ALWAYS true after a healthy concurrent merge (the LWW
            // md_hash register holds one side's pre-merge hash, never the hash
            // of the merged union) — so it silently refused every merged write
            // and the next save then diffed the merged CRDT against the
            // stale-on-disk text and deleted the other device's edit. Allowing
            // non-empty merged text through fixes that; a legitimate "user
            // cleared the note" (md_hash == sha256("")) is still allowed.
            if text.is_empty()
                && !existing.is_empty()
                && doc.md_hash().ok() != Some(crate::sync::crdt::sha256_text(""))
            {
                warn!(
                    path = %rel,
                    existing_bytes = existing.len(),
                    "render_to_roost: refusing empty render over non-empty file (CRDT-divergence artifact)"
                );
                return Ok(RenderOutcome::Skipped {
                    reason: "refused to overwrite this note with an empty version \
                             (possible sync divergence)"
                        .into(),
                });
            }
        }

        // Ensure the parent directory exists. RoostFs::mkdir is
        // idempotent and returns AlreadyExists for the happy path —
        // swallow that variant, propagate real errors.
        if let Some(slash) = rel.rfind('/') {
            let parent_rel = &rel[..slash];
            if !parent_rel.is_empty() {
                if let Err(e) = fs.mkdir(parent_rel).await {
                    use crate::roostfs::RoostError;
                    if !matches!(e, RoostError::AlreadyExists(_)) {
                        warn!(
                            path = parent_rel,
                            error = %e,
                            "render_to_roost: mkdir failed — continuing, write may still succeed"
                        );
                    }
                }
            }
        }

        if let Err(e) = fs.write_atomic(rel, text.as_bytes()).await {
            warn!(
                path = rel,
                error = %e,
                "render_to_roost: write failed — note lives in DocStore but not on disk"
            );
            return Ok(RenderOutcome::Failed {
                reason: "could not write to disk (disk full or permission denied)".into(),
            });
        }
        Ok(RenderOutcome::Wrote)
    }
}

/// Roost-relative path for a conflict-recovery artifact saved next to
/// `note` before a remote doc is adopted over a divergent local copy.
/// `folder/note.md` → `folder/note.conflict-<hash>.md`, where `<hash>` is a
/// short content hash of the local text (M-6).
///
/// Content-addressing (not the old wall-clock millis) fixes two hazards: a
/// permanently-corrupt peer that churns recovery every cycle now overwrites
/// ONE artifact instead of spawning an unbounded pile of `conflict-<millis>`
/// files; and two devices whose clocks differ can no longer write *different*
/// local texts to the *same* `conflict-<millis>` path (distinct text → distinct
/// hash → distinct file, so they don't merge into a corrupted third text).
pub(super) fn conflict_note_path(note: &NotePath, content_hash: &str) -> NotePath {
    let raw = note.0.as_str();
    let stem = raw
        .strip_suffix(".markdown")
        .or_else(|| raw.strip_suffix(".md"))
        .unwrap_or(raw);
    NotePath(format!("{stem}.conflict-{content_hash}.md"))
}

/// Content-address suffix (first 128 bits / 32 hex of SHA-256 of `text`) for a
/// conflict artifact. SYN-M9: this was 32 bits (`[..4]`), with a comment
/// claiming "collisions are irrelevant." They are NOT: two DIFFERENT conflicted
/// local texts colliding in 32 bits (birthday bound ~65K distinct texts) map to
/// the SAME `conflict-<hash>.md`, so the second write CLOBBERS the first —
/// silently losing a user's conflicted edits, the exact data this artifact
/// exists to preserve. 128 bits pushes the birthday bound to 2^64, collision-
/// free in practice. Identical text still shares one filename (idempotent) —
/// that reuse is intended; distinct-text collision is the bug this closes.
fn conflict_content_hash(text: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(text.as_bytes());
    hex::encode(&digest[..16])
}

/// Outcome of trying to save a pre-adopt conflict backup.
///
/// Fix #4: the old `Option<NotePath>` collapsed TWO very different cases into
/// `None` — "there was nothing worth saving" (safe to proceed) and "the write
/// FAILED" (the local text still lives only at the primary path). Only the
/// second must stop the adopt. Distinguishing them lets the caller be genuinely
/// fail-closed: proceed on `NothingToSave`/`Saved`, abort on `Failed`.
pub(super) enum ConflictBackup {
    /// The local text was empty/whitespace — nothing to preserve. Safe to adopt.
    NothingToSave,
    /// The local text was saved to this artifact. Safe to adopt.
    Saved(NotePath),
    /// The backup write FAILED — the local edits are NOT recoverable from a
    /// separate artifact, so the only copy is still at the primary path.
    /// The caller MUST NOT overwrite it: abort the adopt and retry later.
    Failed,
}

/// Save `text` to a recoverable conflict artifact BEFORE the caller adopts
/// a remote `FullDoc` over a divergent/corrupt local doc — so the user's
/// local edits are never silently discarded by the adopt. This is the
/// "never lose text" guarantee behind the merge-failure recovery path.
///
/// Fix #4 (fail-closed): distinguishes "nothing to save" from "write failed"
/// via [`ConflictBackup`] so the caller can refuse to adopt when the backup
/// couldn't be written (rather than the old best-effort behavior that let a
/// failed backup fall through to a silent overwrite). An empty/whitespace-only
/// local doc yields `NothingToSave`; a successful write yields `Saved`; every
/// I/O failure (mkdir excepted — best-effort, the write still decides) yields
/// `Failed`.
///
/// Writes through the `RoostFs` backend when installed (the only path that
/// works on Android SAF) and falls back to `std::fs` otherwise — mirroring
/// `render_to_roost`. The artifact is an ordinary markdown file, so on
/// desktop the watcher picks it up and it syncs like any other note — a
/// welcome side effect: the recovery copy lands on every device.
pub(super) async fn write_conflict_artifact(
    roost: &std::path::Path,
    roost_fs: Option<&dyn RoostFs>,
    note: &NotePath,
    text: &str,
) -> ConflictBackup {
    if text.trim().is_empty() {
        return ConflictBackup::NothingToSave;
    }
    // M-6: content-addressed suffix — identical local text reuses one artifact
    // (idempotent, no pile-up), distinct text gets a distinct file.
    let conflict = conflict_note_path(note, &conflict_content_hash(text));

    // One guarded write (resolve_fs → backend or on-demand NativeFs). The
    // artifact goes through safe_resolve + write_atomic, so a final-component or
    // parent symlink can't redirect it outside the roost.
    let fsh = resolve_fs(roost, roost_fs);
    let fs = fsh.as_fs();
    let rel = &conflict.0;
    if let Some(slash) = rel.rfind('/') {
        let parent = &rel[..slash];
        if !parent.is_empty() {
            if let Err(e) = fs.mkdir(parent).await {
                use crate::roostfs::RoostError;
                if !matches!(e, RoostError::AlreadyExists(_)) {
                    warn!(path = parent, error = %e, "write_conflict_artifact: mkdir failed");
                }
            }
        }
    }
    match fs.write_atomic(rel, text.as_bytes()).await {
        Ok(()) => {
            warn!(path = %rel, "sync: saved local edits to conflict artifact before adopting remote");
            ConflictBackup::Saved(conflict)
        }
        Err(e) => {
            warn!(path = %rel, error = %e, "write_conflict_artifact: write failed — local edits NOT recoverable from a separate artifact");
            ConflictBackup::Failed
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::crypto::pairing::Pin;
    use crate::sync::transport::mock::MockTransport;
    use tempfile::tempdir;

    fn dev(n: u8) -> DeviceId {
        DeviceId([n; 16])
    }

    fn pin() -> Pin {
        Pin::parse("424242").unwrap()
    }

    /// M-7 regression guard. `handle_inbound` routes a failed `store.save`
    /// two ways: CRDT-corruption signatures trip the recovery breaker and
    /// request a fresh copy; everything else (transient local I/O like
    /// disk-full / permission-denied) must be treated as non-corruption so it
    /// hits the isolate-this-note-keep-the-connection branch instead of
    /// tearing down the whole peer. If a future error string starts matching
    /// `is_crdt_corruption` by accident, a disk-full would wrongly drop the
    /// note's local copy — this test pins the classification.
    #[test]
    fn transient_io_errors_are_not_classified_as_corruption() {
        // Corruption signatures → true (route to breaker/RequestFullDoc).
        for corrupt in [
            "duplicate seq 7 found for actor abc",
            "failed to load document",
            "changes out of order",
            "error inflating document chunk ops",
        ] {
            assert!(
                is_crdt_corruption(corrupt),
                "should be corruption: {corrupt}"
            );
        }
        // Transient local I/O → false (route to isolate-keep-note, keep peer).
        for io in [
            "No space left on device (os error 28)",
            "Permission denied (os error 13)",
            "Input/output error (os error 5)",
            "Read-only file system (os error 30)",
        ] {
            assert!(!is_crdt_corruption(io), "should NOT be corruption: {io}");
        }
    }

    /// M-1: a legacy peer's `at_millis` is untrusted and must be clamped.
    #[test]
    fn tombstone_millis_clamped_to_trusted_window() {
        let now = 1_700_000_000_000u64;
        // 0 (would be un-reapable) → local now.
        assert_eq!(sanitize_tombstone_millis(0, now), now);
        // Future beyond skew (would never reap) → local now.
        assert_eq!(sanitize_tombstone_millis(now + 3_600_000, now), now);
        // Implausibly old (would reap immediately → resurrection) → local now.
        assert_eq!(sanitize_tombstone_millis(1, now), now);
        // A plausible recent delete is preserved verbatim.
        let recent = now - 60_000;
        assert_eq!(sanitize_tombstone_millis(recent, now), recent);
        // Small forward skew (within 5 min) is tolerated, not clamped.
        let small_skew = now + 60_000;
        assert_eq!(sanitize_tombstone_millis(small_skew, now), small_skew);
    }

    // ── Attachment blob sync (Phase 5) ──────────────────────────────
    //
    // C-1 (blobs never enter the 30-day store-and-forward queue) is
    // enforced *by construction*, not asserted here: `handle_inbound` has
    // no access to `SyncEngine` and therefore none to the outbox, and the
    // outbox API (`enqueue(device, &NotePath, ts)`) is statically typed to
    // note paths — a blob has no `NotePath`. Blob frames are sent inline
    // via `transport.send`, the same as `Pong`. There is no code path that
    // could enqueue one.

    /// SyncCtx over a fresh temp roost. No RoostFs backend → blob I/O takes
    /// the `std::fs` fallback rooted at the temp dir.
    async fn blob_ctx() -> (
        tempfile::TempDir,
        SyncCtx,
        tokio::sync::broadcast::Receiver<SyncEvent>,
    ) {
        let roost = tempdir().unwrap();
        let store = Arc::new(Mutex::new(DocStore::new(
            roost.path().to_path_buf(),
            dev(1),
        )));
        store.lock().await.init().unwrap();
        let (events_tx, events_rx) = tokio::sync::broadcast::channel::<SyncEvent>(16);
        let ctx = SyncCtx {
            roost: roost.path().to_path_buf(),
            roost_fs: None,
            doc_store: store,
            note_locks: Arc::new(NoteLocks::new()),
            pending_trash: Arc::new(Mutex::new(PendingTrashStore::in_memory())),
            sync: PeerSyncState::new(),
            dirty: HashSet::new(),
            peer_device_id: dev(2),
            peer_display_name: "peer".into(),
            events: events_tx,
            recovery_attempts: HashMap::new(),
            poisoned_paths: HashSet::new(),
            blob_inflight: HashMap::new(),
            blob_unavailable: HashSet::new(),
            blob_pending: HashSet::new(),
            scan_queue: Vec::new(),
            full_doc_buffers: HashMap::new(),
            seed_done: false,
        };
        (roost, ctx, events_rx)
    }

    fn write_attachment(roost: &std::path::Path, name: &str, bytes: &[u8]) {
        let dir = roost.join(".attachments");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(name), bytes).unwrap();
    }

    #[test]
    fn safe_note_path_rejects_escapes_and_accepts_normal() {
        for ok in [
            "a.md",
            "notes/a.md",
            "deep/nested/dir/file.md",
            ".attachments/x.webp",
        ] {
            assert!(
                is_safe_note_path(&NotePath(ok.into())),
                "should accept {ok:?}"
            );
        }
        for bad in [
            "",             // empty
            "/etc/passwd",  // absolute
            "..",           // pure traversal
            "../secret.md", // parent escape
            "a/../../b.md", // mid-path escape
            "a/./b.md",     // cur-dir component
            "a//b.md",      // empty component
            "a/",           // trailing slash → empty component
            "a\\b.md",      // backslash separator (Windows traversal)
            "x\0y.md",      // NUL
        ] {
            assert!(
                !is_safe_note_path(&NotePath(bad.into())),
                "should reject {bad:?}"
            );
        }
    }

    /// DI-1 regression: `render_to_roost` must WRITE non-empty text even when
    /// the doc's `md_hash` disagrees with the rendered text (the post-merge
    /// state), and must still write a legitimate clear-to-empty. The old guard
    /// refused any hash-mismatch, which silently dropped merged edits.
    #[tokio::test]
    async fn render_to_roost_writes_nonempty_and_legit_clear() {
        let roost = tempdir().unwrap();
        let note = NotePath("n.md".into());
        let target = roost.path().join("n.md");

        // Non-empty content writes.
        let mut doc = NoteDoc::new(dev(1)).unwrap();
        doc.apply_markdown("hello world").unwrap();
        let outcome = render_to_roost(roost.path(), None, &note, &mut doc)
            .await
            .unwrap();
        assert!(
            matches!(outcome, RenderOutcome::Wrote),
            "non-empty render must write"
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "hello world");

        // A legitimate clear (md_hash == sha256("")) over a non-empty file
        // writes the empty content — it is NOT a divergence artifact.
        doc.apply_markdown("").unwrap();
        let outcome = render_to_roost(roost.path(), None, &note, &mut doc)
            .await
            .unwrap();
        assert!(
            matches!(outcome, RenderOutcome::Wrote),
            "legit clear-to-empty must write"
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "");
    }

    /// H-symlink: the std::fs fallback must NOT follow an in-roost symlink that
    /// escapes the roost. `from_note_path` only does lexical checks (no `..`,
    /// not absolute), so `<roost>/link/n.md` where `link -> <outside>` passes
    /// the lexical gate; the canonicalization guard must then refuse the write
    /// so a paired peer can't plant a note outside the roost via a pre-existing
    /// symlink. Unix-only (symlink semantics).
    #[cfg(unix)]
    #[tokio::test]
    async fn render_to_roost_fallback_refuses_symlink_escape() {
        let roost = tempdir().unwrap();
        let outside = tempdir().unwrap();
        // <roost>/link -> <outside>
        std::os::unix::fs::symlink(outside.path(), roost.path().join("link")).unwrap();

        let note = NotePath("link/n.md".into());
        let mut doc = NoteDoc::new(dev(1)).unwrap();
        doc.apply_markdown("escaped content").unwrap();

        let outcome = render_to_roost(roost.path(), None, &note, &mut doc)
            .await
            .unwrap();
        assert!(
            matches!(outcome, RenderOutcome::Failed { .. }),
            "a write whose resolved path escapes the roost via a symlink must be refused"
        );
        assert!(
            !outside.path().join("n.md").exists(),
            "nothing must be written outside the roost"
        );
    }

    /// A full peer control channel must make `send_local` return `ChannelFull`
    /// IMMEDIATELY, never block — a wedged/slow peer can't be allowed to stall
    /// the file-save path (which calls this under the engine lock).
    #[tokio::test]
    async fn send_local_is_non_blocking_when_channel_full() {
        // Capacity 1, and we deliberately never drain `_rx`.
        let (tx, _rx) = tokio::sync::mpsc::channel::<PeerControl>(1);
        let handle = PeerHandle {
            device_id: dev(9),
            display_name: "wedged".into(),
            queue_pair_key: [0u8; 32],
            peer_vault_id: [0u8; 16],
            control: tx,
            generation: 0,
            peer_reconnect: None,
            resumption_token: [0u8; 32],
        };
        // Fill the single buffered slot.
        handle
            .send_local(LocalChange::Removed {
                path: NotePath("a.md".into()),
            })
            .unwrap();
        // The next send must fail fast rather than await capacity.
        let err = handle
            .send_local(LocalChange::Removed {
                path: NotePath("b.md".into()),
            })
            .unwrap_err();
        assert!(
            matches!(err, SyncError::Engine(EngineError::ChannelFull)),
            "expected ChannelFull, got {err:?}"
        );
    }

    /// Non-blocking drain of any `RequestBlob`s currently queued on `b`.
    async fn drain_request_blobs(b: &mut MockTransport) -> Vec<String> {
        let mut out = Vec::new();
        while let Ok(Ok(msg)) =
            tokio::time::timeout(std::time::Duration::from_millis(10), b.recv()).await
        {
            if let Message::RequestBlob { rel } = msg {
                out.push(rel);
            }
        }
        out
    }

    #[tokio::test]
    async fn request_blob_sends_existing_attachment() {
        let (roost, mut ctx, _rx) = blob_ctx().await;
        write_attachment(roost.path(), "paste-1.webp", b"PNGDATA");
        let (mut a, mut b) = MockTransport::pair();
        handle_inbound(
            &mut a,
            &mut ctx,
            Message::RequestBlob {
                rel: ".attachments/paste-1.webp".into(),
            },
        )
        .await
        .unwrap();
        match b.recv().await.unwrap() {
            Message::Blob { rel, bytes } => {
                assert_eq!(rel, ".attachments/paste-1.webp");
                assert_eq!(bytes, b"PNGDATA");
            }
            m => panic!("expected Blob, got {m:?}"),
        }
    }

    #[tokio::test]
    async fn request_blob_missing_returns_not_found() {
        let (_roost, mut ctx, _rx) = blob_ctx().await;
        let (mut a, mut b) = MockTransport::pair();
        handle_inbound(
            &mut a,
            &mut ctx,
            Message::RequestBlob {
                rel: ".attachments/nope.webp".into(),
            },
        )
        .await
        .unwrap();
        match b.recv().await.unwrap() {
            Message::BlobUnavailable { reason, .. } => {
                assert_eq!(reason, BlobUnavailableReason::NotFound)
            }
            m => panic!("expected BlobUnavailable(NotFound), got {m:?}"),
        }
    }

    #[tokio::test]
    async fn request_blob_traversal_is_rejected() {
        let (_roost, mut ctx, _rx) = blob_ctx().await;
        let (mut a, mut b) = MockTransport::pair();
        handle_inbound(
            &mut a,
            &mut ctx,
            Message::RequestBlob {
                rel: ".attachments/../secret".into(),
            },
        )
        .await
        .unwrap();
        match b.recv().await.unwrap() {
            Message::BlobUnavailable { reason, .. } => {
                assert_eq!(reason, BlobUnavailableReason::Rejected)
            }
            m => panic!("expected BlobUnavailable(Rejected), got {m:?}"),
        }
    }

    #[tokio::test]
    async fn request_blob_over_cap_returns_over_cap() {
        let (roost, mut ctx, _rx) = blob_ctx().await;
        write_attachment(
            roost.path(),
            "huge.webp",
            &vec![0u8; (SYNC_BLOB_MAX_BYTES as usize) + 1],
        );
        let (mut a, mut b) = MockTransport::pair();
        handle_inbound(
            &mut a,
            &mut ctx,
            Message::RequestBlob {
                rel: ".attachments/huge.webp".into(),
            },
        )
        .await
        .unwrap();
        match b.recv().await.unwrap() {
            Message::BlobUnavailable { reason, .. } => {
                assert_eq!(reason, BlobUnavailableReason::OverCap)
            }
            m => panic!("expected BlobUnavailable(OverCap), got {m:?}"),
        }
    }

    #[tokio::test]
    async fn received_blob_is_written_and_emits_event() {
        let (roost, mut ctx, mut rx) = blob_ctx().await;
        ctx.blob_inflight
            .insert(".attachments/in.webp".into(), Instant::now());
        let (mut a, _b) = MockTransport::pair();
        handle_inbound(
            &mut a,
            &mut ctx,
            Message::Blob {
                rel: ".attachments/in.webp".into(),
                bytes: b"BYTES".to_vec(),
            },
        )
        .await
        .unwrap();
        let written = std::fs::read(roost.path().join(".attachments/in.webp")).unwrap();
        assert_eq!(written, b"BYTES");
        assert!(
            !ctx.blob_inflight.contains_key(".attachments/in.webp"),
            "in-flight entry cleared on arrival"
        );
        match rx.try_recv().unwrap() {
            SyncEvent::AttachmentWritten { rel } => assert_eq!(rel, ".attachments/in.webp"),
            e => panic!("expected AttachmentWritten, got {e:?}"),
        }
    }

    #[tokio::test]
    async fn received_blob_over_cap_or_bad_path_is_not_written() {
        let (roost, mut ctx, _rx) = blob_ctx().await;
        let (mut a, _b) = MockTransport::pair();
        // Over cap → dropped, no write.
        handle_inbound(
            &mut a,
            &mut ctx,
            Message::Blob {
                rel: ".attachments/big.webp".into(),
                bytes: vec![0u8; (SYNC_BLOB_MAX_BYTES as usize) + 1],
            },
        )
        .await
        .unwrap();
        assert!(!roost.path().join(".attachments/big.webp").exists());
        // Traversal path → rejected before write.
        handle_inbound(
            &mut a,
            &mut ctx,
            Message::Blob {
                rel: ".attachments/../escape.webp".into(),
                bytes: b"x".to_vec(),
            },
        )
        .await
        .unwrap();
        assert!(!roost.path().join("escape.webp").exists());
    }

    #[tokio::test]
    async fn blob_unavailable_marks_path_terminal() {
        let (_roost, mut ctx, _rx) = blob_ctx().await;
        ctx.blob_inflight
            .insert(".attachments/x.webp".into(), Instant::now());
        let (mut a, _b) = MockTransport::pair();
        handle_inbound(
            &mut a,
            &mut ctx,
            Message::BlobUnavailable {
                rel: ".attachments/x.webp".into(),
                reason: BlobUnavailableReason::NotFound,
            },
        )
        .await
        .unwrap();
        assert!(!ctx.blob_inflight.contains_key(".attachments/x.webp"));
        assert!(
            ctx.blob_unavailable.contains(".attachments/x.webp"),
            "terminal so we don't re-request (C-2)"
        );
    }

    #[tokio::test]
    async fn over_cap_unavailable_emits_too_large_but_not_found_does_not() {
        let (_roost, mut ctx, mut rx) = blob_ctx().await;
        let (mut a, _b) = MockTransport::pair();
        // OverCap → permanent → UI signal.
        handle_inbound(
            &mut a,
            &mut ctx,
            Message::BlobUnavailable {
                rel: ".attachments/huge.gif".into(),
                reason: BlobUnavailableReason::OverCap,
            },
        )
        .await
        .unwrap();
        match rx.try_recv().unwrap() {
            SyncEvent::AttachmentTooLarge { rel } => assert_eq!(rel, ".attachments/huge.gif"),
            e => panic!("expected AttachmentTooLarge, got {e:?}"),
        }
        // NotFound is transient → no permanent "too large" signal.
        handle_inbound(
            &mut a,
            &mut ctx,
            Message::BlobUnavailable {
                rel: ".attachments/elsewhere.webp".into(),
                reason: BlobUnavailableReason::NotFound,
            },
        )
        .await
        .unwrap();
        assert!(
            rx.try_recv().is_err(),
            "NotFound must not emit AttachmentTooLarge"
        );
    }

    #[tokio::test]
    async fn request_missing_blobs_dedups_and_skips_present() {
        let (roost, mut ctx, _rx) = blob_ctx().await;
        write_attachment(roost.path(), "present.webp", b"P");
        let mut doc = NoteDoc::new(dev(1)).unwrap();
        doc.apply_markdown("![](.attachments/present.webp) and ![](.attachments/missing.webp)")
            .unwrap();
        let (mut a, mut b) = MockTransport::pair();

        request_missing_blobs(&mut a, &mut ctx, &mut doc)
            .await
            .unwrap();
        // Only the missing one is requested; the present one is skipped.
        match b.recv().await.unwrap() {
            Message::RequestBlob { rel } => assert_eq!(rel, ".attachments/missing.webp"),
            m => panic!("expected RequestBlob(missing), got {m:?}"),
        }
        assert_eq!(ctx.blob_inflight.len(), 1, "present blob not requested");
        assert!(ctx.blob_inflight.contains_key(".attachments/missing.webp"));

        // Second pass: already in-flight → no new request (C-3 / C-7 dedup).
        request_missing_blobs(&mut a, &mut ctx, &mut doc)
            .await
            .unwrap();
        let second = tokio::time::timeout(std::time::Duration::from_millis(50), b.recv()).await;
        assert!(second.is_err(), "no duplicate request while in-flight");
    }

    #[tokio::test]
    async fn many_missing_blobs_drain_through_the_window() {
        // A note with more missing attachments than MAX_BLOB_INFLIGHT must
        // still fetch ALL of them: the window caps concurrency, and each
        // reply pumps the next pending request until none remain (C-7).
        let (_roost, mut ctx, _rx) = blob_ctx().await;
        const N: usize = 12;
        // Compile-time guard: the test only exercises the window if there
        // are more refs than slots.
        const _: () = assert!(N > MAX_BLOB_INFLIGHT);
        let md: String = (0..N)
            .map(|i| format!("![](.attachments/m{i}.webp)"))
            .collect::<Vec<_>>()
            .join(" ");
        let mut doc = NoteDoc::new(dev(1)).unwrap();
        doc.apply_markdown(&md).unwrap();
        let (mut a, mut b) = MockTransport::pair();

        request_missing_blobs(&mut a, &mut ctx, &mut doc)
            .await
            .unwrap();
        // First pass fills the window; the rest park in pending.
        assert_eq!(ctx.blob_inflight.len(), MAX_BLOB_INFLIGHT);
        assert_eq!(ctx.blob_pending.len(), N - MAX_BLOB_INFLIGHT);

        // Reply to each request with a tiny Blob; every reply should pump the
        // next pending one until all N are requested.
        let mut requested: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut to_reply: Vec<String> = Vec::new();
        for rel in drain_request_blobs(&mut b).await {
            requested.insert(rel.clone());
            to_reply.push(rel);
        }
        assert_eq!(
            requested.len(),
            MAX_BLOB_INFLIGHT,
            "initial window requested"
        );

        while let Some(rel) = to_reply.pop() {
            handle_inbound(
                &mut a,
                &mut ctx,
                Message::Blob {
                    rel,
                    bytes: b"x".to_vec(),
                },
            )
            .await
            .unwrap();
            for rel in drain_request_blobs(&mut b).await {
                if requested.insert(rel.clone()) {
                    to_reply.push(rel);
                }
            }
        }

        assert_eq!(
            requested.len(),
            N,
            "every missing blob was eventually requested"
        );
        assert!(ctx.blob_pending.is_empty(), "pending fully drained");
        assert!(ctx.blob_inflight.is_empty(), "all replies accounted for");
    }

    #[tokio::test]
    async fn connect_scan_requests_missing_blob_for_unchanged_note() {
        // The retry-gap fix: a note already in the store references an
        // attachment we lack, and its text won't change this session. The
        // per-write trigger would never fire, so the on-connect scan must
        // request it.
        let (_roost, mut ctx, _rx) = blob_ctx().await;
        let path = NotePath("note.md".into());
        {
            let store = ctx.doc_store.lock().await;
            let mut doc = NoteDoc::new(dev(1)).unwrap();
            doc.apply_markdown("see ![](.attachments/orphan.webp)")
                .unwrap();
            store.save(&path, &mut doc).unwrap();
        }
        // Mirror what run_sync_loop does at connect, then drain a batch.
        ctx.scan_queue = ctx.doc_store.lock().await.list().unwrap();
        let (mut a, mut b) = MockTransport::pair();
        scan_blob_batch(&mut a, &mut ctx).await.unwrap();
        match b.recv().await.unwrap() {
            Message::RequestBlob { rel } => assert_eq!(rel, ".attachments/orphan.webp"),
            m => panic!("expected RequestBlob from backfill scan, got {m:?}"),
        }
        assert!(ctx.blob_inflight.contains_key(".attachments/orphan.webp"));
        assert!(ctx.scan_queue.is_empty(), "single note drained from queue");
    }

    /// Two peers exchange handshakes + an initial NoteList over a mock
    /// transport and both end up with the same note content after sync.
    #[tokio::test]
    async fn two_peers_converge_via_mock_transport() {
        let (a_side, b_side) = MockTransport::pair();

        let a_roost = tempdir().unwrap();
        let b_roost = tempdir().unwrap();
        let a_store = Arc::new(Mutex::new(DocStore::new(
            a_roost.path().to_path_buf(),
            dev(1),
        )));
        let b_store = Arc::new(Mutex::new(DocStore::new(
            b_roost.path().to_path_buf(),
            dev(2),
        )));
        a_store.lock().await.init().unwrap();
        b_store.lock().await.init().unwrap();

        // Seed A with one note so B has to fetch it via FullDoc.
        {
            let store = a_store.lock().await;
            let mut doc = NoteDoc::new(dev(1)).unwrap();
            doc.apply_markdown("hello from A").unwrap();
            store.save(&NotePath("note.md".into()), &mut doc).unwrap();
        }

        let a_role = Role::Client {
            handshake: ClientHandshake::new(dev(1), "A".into(), "roost".into(), [1u8; 16], pin()),
        };
        let b_role = Role::Server {
            handshake: ServerHandshake::new(dev(2), "B".into(), "roost".into(), [2u8; 16], pin()),
        };

        let a_roost_p = a_roost.path().to_path_buf();
        let b_roost_p = b_roost.path().to_path_buf();
        let a_store_c = a_store.clone();
        let b_store_c = b_store.clone();
        let (a_events, _) = tokio::sync::broadcast::channel(16);
        let (b_events, _) = tokio::sync::broadcast::channel(16);

        let ((_a_h, a_task), (_b_h, b_task)) = tokio::join!(
            async {
                spawn(
                    a_side,
                    a_role,
                    a_roost_p,
                    None,
                    a_store_c,
                    Arc::new(NoteLocks::new()),
                    Arc::new(Mutex::new(PendingTrashStore::in_memory())),
                    a_events,
                    tokio_util::task::TaskTracker::new(),
                    tokio_util::sync::CancellationToken::new(),
                )
                .await
                .unwrap()
            },
            async {
                spawn(
                    b_side,
                    b_role,
                    b_roost_p,
                    None,
                    b_store_c,
                    Arc::new(NoteLocks::new()),
                    Arc::new(Mutex::new(PendingTrashStore::in_memory())),
                    b_events,
                    tokio_util::task::TaskTracker::new(),
                    tokio_util::sync::CancellationToken::new(),
                )
                .await
                .unwrap()
            },
        );

        // Poll for convergence instead of a fixed sleep (a fixed wait flakes
        // under parallel load): break as soon as B receives A's note, up to a
        // generous ceiling.
        let note = NotePath("note.md".into());
        for _ in 0..600 {
            let got = b_store
                .lock()
                .await
                .load_or_create(&note)
                .ok()
                .and_then(|d| d.render().ok());
            if got.as_deref() == Some("hello from A") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        // Shut both actors down so the task handles can resolve.
        _a_h.shutdown().await;
        _b_h.shutdown().await;
        let _ = tokio::time::timeout(Duration::from_secs(2), a_task).await;
        let _ = tokio::time::timeout(Duration::from_secs(2), b_task).await;

        // B should now have the note.
        let b_got = b_store
            .lock()
            .await
            .load_or_create(&note)
            .unwrap()
            .render()
            .unwrap();
        assert_eq!(b_got, "hello from A");
    }

    /// A local delete on one side propagates as a CRDT tombstone (the
    /// `deleted` flag synced via Automerge — NOT a destructive removal). The
    /// receiving side KEEPS the doc as a tombstone, marks it deleted, trashes
    /// the file on disk, and emits `NoteDeleted`.
    #[tokio::test]
    async fn note_delete_tombstone_propagates() {
        let (a_side, b_side) = MockTransport::pair();
        let a_roost = tempdir().unwrap();
        let b_roost = tempdir().unwrap();
        let a_store = Arc::new(Mutex::new(DocStore::new(
            a_roost.path().to_path_buf(),
            dev(1),
        )));
        let b_store = Arc::new(Mutex::new(DocStore::new(
            b_roost.path().to_path_buf(),
            dev(2),
        )));
        a_store.lock().await.init().unwrap();
        b_store.lock().await.init().unwrap();

        // Pre-populate both sides with the same note so the delete has
        // somewhere to land on B. Write the markdown to disk too (no RoostFs
        // backend here → std::fs path) so B has a real file to trash.
        let path = NotePath("doomed.md".into());
        for store in [&a_store, &b_store] {
            let s = store.lock().await;
            let mut doc = NoteDoc::new(dev(1)).unwrap();
            doc.apply_markdown("delete me").unwrap();
            s.save(&path, &mut doc).unwrap();
        }
        std::fs::write(a_roost.path().join("doomed.md"), "delete me").unwrap();
        std::fs::write(b_roost.path().join("doomed.md"), "delete me").unwrap();

        let a_role = Role::Client {
            handshake: ClientHandshake::new(dev(1), "A".into(), "v".into(), [1u8; 16], pin()),
        };
        let b_role = Role::Server {
            handshake: ServerHandshake::new(dev(2), "B".into(), "v".into(), [2u8; 16], pin()),
        };
        let (a_events_tx, _a_events_rx) = tokio::sync::broadcast::channel::<SyncEvent>(16);
        let (b_events_tx, mut b_events_rx) = tokio::sync::broadcast::channel::<SyncEvent>(16);

        let ((a_h, a_task), (b_h, b_task)) = tokio::join!(
            async {
                spawn(
                    a_side,
                    a_role,
                    a_roost.path().to_path_buf(),
                    None,
                    a_store.clone(),
                    Arc::new(NoteLocks::new()),
                    Arc::new(Mutex::new(PendingTrashStore::in_memory())),
                    a_events_tx,
                    tokio_util::task::TaskTracker::new(),
                    tokio_util::sync::CancellationToken::new(),
                )
                .await
                .unwrap()
            },
            async {
                spawn(
                    b_side,
                    b_role,
                    b_roost.path().to_path_buf(),
                    None,
                    b_store.clone(),
                    Arc::new(NoteLocks::new()),
                    Arc::new(Mutex::new(PendingTrashStore::in_memory())),
                    b_events_tx,
                    tokio_util::task::TaskTracker::new(),
                    tokio_util::sync::CancellationToken::new(),
                )
                .await
                .unwrap()
            },
        );

        // Initial NoteList exchange settles.
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Emit a local delete on A. Triggers `LocalChange::Removed` →
        // mark_deleted in A's doc → Automerge sync of the `deleted` flag →
        // B's `reconcile_doc_to_roost` trashes the file + emits NoteDeleted.
        a_h.send_local(LocalChange::Removed { path: path.clone() })
            .unwrap();

        // Wait for B to receive + apply.
        let mut deleted_event = false;
        for _ in 0..30 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            while let Ok(ev) = b_events_rx.try_recv() {
                if matches!(ev, SyncEvent::NoteDeleted { .. }) {
                    deleted_event = true;
                }
            }
            if deleted_event {
                break;
            }
        }
        assert!(deleted_event, "B should have emitted NoteDeleted");
        // The doc is KEPT as a tombstone (not removed) so the delete keeps
        // propagating; it just reports deleted.
        assert!(
            b_store.lock().await.exists(&path),
            "B keeps the tombstone doc (delete is a CRDT flag, not a removal)"
        );
        let b_doc = b_store.lock().await.load_or_create(&path).unwrap();
        assert!(
            b_doc.is_deleted().unwrap(),
            "B's doc must be marked deleted"
        );
        // The markdown file is trashed on disk.
        assert!(
            !b_roost.path().join("doomed.md").exists(),
            "B's markdown file should be trashed"
        );

        a_h.shutdown().await;
        b_h.shutdown().await;
        let _ = tokio::time::timeout(Duration::from_secs(2), a_task).await;
        let _ = tokio::time::timeout(Duration::from_secs(2), b_task).await;
    }

    /// Headline regression: A deletes a note while B concurrently edits the
    /// SAME note (both offline), then they sync. delete-wins-but-recoverable
    /// means BOTH converge to deleted==true, both retain B's edited text in
    /// the CRDT, and neither resurrects the file on disk.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn delete_then_concurrent_edit_converges() {
        let (a_side, b_side) = MockTransport::pair();
        let a_roost = tempdir().unwrap();
        let b_roost = tempdir().unwrap();
        let a_store = Arc::new(Mutex::new(DocStore::new(
            a_roost.path().to_path_buf(),
            dev(1),
        )));
        let b_store = Arc::new(Mutex::new(DocStore::new(
            b_roost.path().to_path_buf(),
            dev(2),
        )));
        a_store.lock().await.init().unwrap();
        b_store.lock().await.init().unwrap();

        // Both sides start from identical shared history (same actor + same
        // ops → same change hashes), so they're already converged before the
        // concurrent delete/edit.
        let path = NotePath("conflict.md".into());
        for store in [&a_store, &b_store] {
            let s = store.lock().await;
            let mut doc = NoteDoc::new(dev(9)).unwrap();
            doc.apply_markdown("original").unwrap();
            s.save(&path, &mut doc).unwrap();
        }
        std::fs::write(a_roost.path().join("conflict.md"), "original").unwrap();
        std::fs::write(b_roost.path().join("conflict.md"), "original").unwrap();

        let a_role = Role::Client {
            handshake: ClientHandshake::new(dev(1), "A".into(), "v".into(), [1u8; 16], pin()),
        };
        let b_role = Role::Server {
            handshake: ServerHandshake::new(dev(2), "B".into(), "v".into(), [2u8; 16], pin()),
        };
        let (a_events_tx, _a_rx) = tokio::sync::broadcast::channel::<SyncEvent>(32);
        let (b_events_tx, _b_rx) = tokio::sync::broadcast::channel::<SyncEvent>(32);

        let ((a_h, a_task), (b_h, b_task)) = tokio::join!(
            async {
                spawn(
                    a_side,
                    a_role,
                    a_roost.path().to_path_buf(),
                    None,
                    a_store.clone(),
                    Arc::new(NoteLocks::new()),
                    Arc::new(Mutex::new(PendingTrashStore::in_memory())),
                    a_events_tx,
                    tokio_util::task::TaskTracker::new(),
                    tokio_util::sync::CancellationToken::new(),
                )
                .await
                .unwrap()
            },
            async {
                spawn(
                    b_side,
                    b_role,
                    b_roost.path().to_path_buf(),
                    None,
                    b_store.clone(),
                    Arc::new(NoteLocks::new()),
                    Arc::new(Mutex::new(PendingTrashStore::in_memory())),
                    b_events_tx,
                    tokio_util::task::TaskTracker::new(),
                    tokio_util::sync::CancellationToken::new(),
                )
                .await
                .unwrap()
            },
        );

        tokio::time::sleep(Duration::from_millis(150)).await;

        // Concurrent: A deletes, B edits the same note. The real delete entry
        // point trashes the file on disk *before* notifying the engine, so we
        // mirror that here (handle_local only tombstones the CRDT, it doesn't
        // touch the file).
        std::fs::remove_file(a_roost.path().join("conflict.md")).unwrap();
        tokio::join!(
            async {
                a_h.send_local(LocalChange::Removed { path: path.clone() })
                    .unwrap();
            },
            async {
                // Mirror `SyncEngine::apply_local_change`: in production the
                // engine applies an edit to the shared docstore *centrally*
                // before fanning out to peer tasks, which are pure transport
                // for `Modified*` (see `handle_local` — it only marks dirty,
                // unlike `Removed` which still tombstones). This test drives
                // the peer handle directly, so it must do that central apply
                // itself; otherwise B's edit never lands in the CRDT, the peer
                // flushes an empty sync, and A converges to bare "original" —
                // masking the delete-vs-edit preservation this test guards.
                {
                    let store = b_store.lock().await;
                    let mut doc = store.load_or_create(&path).unwrap();
                    doc.apply_markdown("original + B edit").unwrap();
                    store.save(&path, &mut doc).unwrap();
                }
                b_h.send_local(LocalChange::ModifiedWithContent {
                    path: path.clone(),
                    content: "original + B edit".into(),
                })
                .unwrap();
            },
        );

        // Poll for convergence instead of a fixed quiesce sleep. The delete
        // and the edit are concurrent and cross a real async transport, so a
        // fixed `sleep` flakes under variable CI load (the cross-peer merge of
        // B's edit can land after the window). We assert *that* it converges,
        // not how fast — so wait up to a generous deadline for A to reflect
        // both the tombstone and B's edit, then proceed. A genuine
        // non-convergence still fails the assertions below after the timeout.
        // Generous ceiling: the loop breaks as soon as convergence is observed,
        // so a large deadline only widens the starved-CI failure window, never
        // the happy path. 5s flaked under full-suite parallel load.
        let deadline = Duration::from_secs(30);
        let started = tokio::time::Instant::now();
        loop {
            // Poll on the FULL set the assertions below check — BOTH sides
            // tombstoned AND converged on B's text. Polling only A's state
            // breaks early while B is still merging, and the B-side asserts
            // then flake under load.
            let side_converged = |doc: &crate::sync::crdt::NoteDoc| {
                doc.is_deleted().unwrap_or(false)
                    && doc
                        .render()
                        .map(|t| t == "original + B edit")
                        .unwrap_or(false)
            };
            let converged = {
                let a_ok = {
                    let doc = a_store.lock().await.load_or_create(&path).unwrap();
                    side_converged(&doc)
                };
                let b_ok = {
                    let doc = b_store.lock().await.load_or_create(&path).unwrap();
                    side_converged(&doc)
                };
                a_ok && b_ok
            };
            if converged || started.elapsed() >= deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }

        a_h.shutdown().await;
        b_h.shutdown().await;
        let _ = tokio::time::timeout(Duration::from_secs(2), a_task).await;
        let _ = tokio::time::timeout(Duration::from_secs(2), b_task).await;

        let a_doc = a_store.lock().await.load_or_create(&path).unwrap();
        let b_doc = b_store.lock().await.load_or_create(&path).unwrap();

        // Convergence: both deleted.
        assert!(a_doc.is_deleted().unwrap(), "A converges to deleted");
        assert!(
            b_doc.is_deleted().unwrap(),
            "B converges to deleted (delete wins over the concurrent edit)"
        );
        // Text converges identically and preserves B's edit (recoverable).
        assert_eq!(
            a_doc.render().unwrap(),
            b_doc.render().unwrap(),
            "both sides converge to identical text"
        );
        assert_eq!(
            a_doc.render().unwrap(),
            "original + B edit",
            "B's edit is preserved in the CRDT (recoverable via restore)"
        );
        // Neither side has the file live on disk.
        assert!(
            !a_roost.path().join("conflict.md").exists(),
            "A's file stays trashed"
        );
        assert!(
            !b_roost.path().join("conflict.md").exists(),
            "B's file is trashed — the edit must not resurrect it"
        );
    }

    /// Backward-compat: an inbound legacy `NoteDelete` frame from a pre-stage-2
    /// peer must mark the doc as a tombstone (kept, deleted==true), NOT destroy
    /// it — so we converge with stage-2 peers.
    #[tokio::test]
    async fn old_notedelete_inbound_marks_tombstone() {
        let a_roost = tempdir().unwrap();
        let store = Arc::new(Mutex::new(DocStore::new(
            a_roost.path().to_path_buf(),
            dev(1),
        )));
        store.lock().await.init().unwrap();
        let path = NotePath("legacy.md".into());
        {
            let s = store.lock().await;
            let mut doc = NoteDoc::new(dev(1)).unwrap();
            doc.apply_markdown("legacy body").unwrap();
            s.save(&path, &mut doc).unwrap();
        }
        std::fs::write(a_roost.path().join("legacy.md"), "legacy body").unwrap();

        let (events_tx, _rx) = tokio::sync::broadcast::channel::<SyncEvent>(16);
        let mut ctx = SyncCtx {
            roost: a_roost.path().to_path_buf(),
            roost_fs: None,
            doc_store: store.clone(),
            note_locks: Arc::new(NoteLocks::new()),
            pending_trash: Arc::new(Mutex::new(PendingTrashStore::in_memory())),
            sync: PeerSyncState::new(),
            dirty: HashSet::new(),
            peer_device_id: dev(2),
            peer_display_name: "old-peer".into(),
            events: events_tx,
            recovery_attempts: HashMap::new(),
            poisoned_paths: HashSet::new(),
            blob_inflight: HashMap::new(),
            blob_unavailable: HashSet::new(),
            blob_pending: HashSet::new(),
            scan_queue: Vec::new(),
            full_doc_buffers: HashMap::new(),
            seed_done: false,
        };

        // A plausible recent delete time (within the trusted window) is
        // honored verbatim; absurd values are clamped — see the dedicated
        // `tombstone_millis_clamped_to_trusted_window` unit test (M-1).
        let recent = crate::sync::engine::util::now_millis() - 60_000;
        let mut transport = MockTransport::pair().0;
        handle_inbound(
            &mut transport,
            &mut ctx,
            Message::NoteDelete {
                path: path.clone(),
                at_millis: recent,
            },
        )
        .await
        .unwrap();

        // Doc kept as a tombstone, marked deleted, file trashed.
        let doc = store.lock().await.load_or_create(&path).unwrap();
        assert!(
            store.lock().await.exists(&path),
            "legacy NoteDelete must keep the doc as a tombstone, not destroy it"
        );
        assert!(doc.is_deleted().unwrap(), "doc must be marked deleted");
        assert_eq!(
            doc.deleted_at().unwrap(),
            recent,
            "a plausible at_millis is honored"
        );
        assert!(
            !a_roost.path().join("legacy.md").exists(),
            "file should be trashed"
        );
    }

    /// SYN-B1 regression: a legacy `NoteDelete` naming a policy-rejected path
    /// (here, `.git/config` — a dot-prefixed component) must be REFUSED before
    /// it ever reaches the DocStore or disk. Before this fix, the NoteDelete
    /// arm was the one message type that skipped `vet_remote_rel` entirely —
    /// a paired peer could tombstone-and-trash an arbitrary roost-relative
    /// path. `handle_inbound` must return `Ok(false)` (skip, not disconnect)
    /// and touch neither the DocStore nor disk.
    #[tokio::test]
    async fn notedelete_with_unsafe_path_is_rejected_not_trashed() {
        let roost = tempdir().unwrap();
        let store = Arc::new(Mutex::new(DocStore::new(
            roost.path().to_path_buf(),
            dev(1),
        )));
        store.lock().await.init().unwrap();
        // A real file at the targeted path, so a wrongful trash would be
        // observable (the assertion below proves it survives).
        std::fs::create_dir_all(roost.path().join(".git")).unwrap();
        std::fs::write(roost.path().join(".git/config"), b"[core]\n").unwrap();
        let evil_path = NotePath(".git/config".into());

        let (events_tx, _rx) = tokio::sync::broadcast::channel::<SyncEvent>(16);
        let mut ctx = SyncCtx {
            roost: roost.path().to_path_buf(),
            roost_fs: None,
            doc_store: store.clone(),
            note_locks: Arc::new(NoteLocks::new()),
            pending_trash: Arc::new(Mutex::new(PendingTrashStore::in_memory())),
            sync: PeerSyncState::new(),
            dirty: HashSet::new(),
            peer_device_id: dev(2),
            peer_display_name: "attacker".into(),
            events: events_tx,
            recovery_attempts: HashMap::new(),
            poisoned_paths: HashSet::new(),
            blob_inflight: HashMap::new(),
            blob_unavailable: HashSet::new(),
            blob_pending: HashSet::new(),
            scan_queue: Vec::new(),
            full_doc_buffers: HashMap::new(),
            seed_done: false,
        };

        let mut transport = MockTransport::pair().0;
        let result = handle_inbound(
            &mut transport,
            &mut ctx,
            Message::NoteDelete {
                path: evil_path.clone(),
                at_millis: crate::sync::engine::util::now_millis(),
            },
        )
        .await
        .unwrap();

        assert!(
            !result,
            "a policy-rejected NoteDelete must skip (Ok(false)), not tear down the session"
        );
        assert!(
            !store.lock().await.exists(&evil_path),
            "the DocStore must never record a tombstone for a policy-rejected path"
        );
        assert!(
            roost.path().join(".git/config").exists(),
            ".git/config must survive untouched — the delete must never reach disk"
        );
    }

    /// SYN-B1 defense-in-depth: `trash_in_roost` itself must refuse a
    /// policy-rejected path, independent of any frame-handler gate — mirroring
    /// `render_to_roost`'s existing write-site check. Proves the fix holds even
    /// if a future caller forgets to vet first.
    #[tokio::test]
    async fn trash_in_roost_refuses_policy_rejected_path() {
        let roost = tempdir().unwrap();
        std::fs::create_dir_all(roost.path().join(".corvux")).unwrap();
        std::fs::write(roost.path().join(".corvux/vault-id.bin"), [0u8; 16]).unwrap();

        let err = trash_in_roost(roost.path(), None, &NotePath(".corvux/vault-id.bin".into()))
            .await
            .unwrap_err();
        let _ = err; // Err variant is all we assert — content is incidental.

        assert!(
            roost.path().join(".corvux/vault-id.bin").exists(),
            "a policy-rejected path must never be trashed, even calling trash_in_roost directly"
        );
    }

    /// End-to-end faithful reproduction of the user's scenario: BOTH devices
    /// go offline, make divergent changes (A deletes some notes, B edits one of
    /// the same notes + edits an unrelated note), THEN both come online and
    /// sync. This mutates each DocStore *while disconnected* (no transport),
    /// then spins up the peer tasks to model "coming back online" — a truer
    /// reproduction than driving `send_local` over a live link.
    ///
    /// Expected end state on BOTH devices:
    ///   - `deleted.md`  → deleted everywhere (A deleted it, B didn't touch it).
    ///   - `both.md`     → deleted everywhere; B's concurrent edit is preserved
    ///                     in the CRDT but the file stays trashed (delete wins).
    ///   - `b-only.md`   → alive everywhere with B's edit (no delete involved).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn both_offline_delete_and_edit_then_reconnect_converges() {
        let a_roost = tempdir().unwrap();
        let b_roost = tempdir().unwrap();
        let a_store = Arc::new(Mutex::new(DocStore::new(
            a_roost.path().to_path_buf(),
            dev(1),
        )));
        let b_store = Arc::new(Mutex::new(DocStore::new(
            b_roost.path().to_path_buf(),
            dev(2),
        )));
        a_store.lock().await.init().unwrap();
        b_store.lock().await.init().unwrap();

        let deleted = NotePath("deleted.md".into());
        let both = NotePath("both.md".into());
        let b_only = NotePath("b-only.md".into());

        // ── Shared starting point ──────────────────────────────────────────
        // Seed all three notes identically on both sides (same actor + same
        // ops → identical change hashes → genuinely shared history), and write
        // them to disk on both roosts.
        let seed = [
            (&deleted, "delete me"),
            (&both, "shared body"),
            (&b_only, "untouched"),
        ];
        for store in [&a_store, &b_store] {
            let s = store.lock().await;
            for (path, body) in seed {
                let mut doc = NoteDoc::new(dev(9)).unwrap();
                doc.apply_markdown(body).unwrap();
                s.save(path, &mut doc).unwrap();
            }
        }
        for roost in [&a_roost, &b_roost] {
            std::fs::write(roost.path().join("deleted.md"), "delete me").unwrap();
            std::fs::write(roost.path().join("both.md"), "shared body").unwrap();
            std::fs::write(roost.path().join("b-only.md"), "untouched").unwrap();
        }

        // ── OFFLINE divergence (no transport up) ────────────────────────────
        // A deletes `deleted.md` and `both.md`: tombstone the CRDT + trash the
        // files (mirrors the delete command's disk-trash-then-tombstone order).
        {
            let s = a_store.lock().await;
            for path in [&deleted, &both] {
                let mut doc = s.load_or_create(path).unwrap();
                doc.mark_deleted(1_000).unwrap();
                s.save(path, &mut doc).unwrap();
            }
        }
        std::fs::remove_file(a_roost.path().join("deleted.md")).unwrap();
        std::fs::remove_file(a_roost.path().join("both.md")).unwrap();

        // B (still offline) edits `both.md` (concurrent with A's delete) and
        // `b-only.md` (no conflict), updating both CRDT and disk.
        {
            let s = b_store.lock().await;
            let mut d = s.load_or_create(&both).unwrap();
            d.apply_markdown("shared body + B edit").unwrap();
            s.save(&both, &mut d).unwrap();
            let mut d2 = s.load_or_create(&b_only).unwrap();
            d2.apply_markdown("untouched + B edit").unwrap();
            s.save(&b_only, &mut d2).unwrap();
        }
        std::fs::write(b_roost.path().join("both.md"), "shared body + B edit").unwrap();
        std::fs::write(b_roost.path().join("b-only.md"), "untouched + B edit").unwrap();

        // ── COME ONLINE: spin up both peer tasks and let them reconcile ─────
        let (a_side, b_side) = MockTransport::pair();
        let a_role = Role::Client {
            handshake: ClientHandshake::new(dev(1), "A".into(), "v".into(), [1u8; 16], pin()),
        };
        let b_role = Role::Server {
            handshake: ServerHandshake::new(dev(2), "B".into(), "v".into(), [2u8; 16], pin()),
        };
        let (a_events_tx, _a_rx) = tokio::sync::broadcast::channel::<SyncEvent>(64);
        let (b_events_tx, _b_rx) = tokio::sync::broadcast::channel::<SyncEvent>(64);

        let ((a_h, a_task), (b_h, b_task)) = tokio::join!(
            async {
                spawn(
                    a_side,
                    a_role,
                    a_roost.path().to_path_buf(),
                    None,
                    a_store.clone(),
                    Arc::new(NoteLocks::new()),
                    Arc::new(Mutex::new(PendingTrashStore::in_memory())),
                    a_events_tx,
                    tokio_util::task::TaskTracker::new(),
                    tokio_util::sync::CancellationToken::new(),
                )
                .await
                .unwrap()
            },
            async {
                spawn(
                    b_side,
                    b_role,
                    b_roost.path().to_path_buf(),
                    None,
                    b_store.clone(),
                    Arc::new(NoteLocks::new()),
                    Arc::new(Mutex::new(PendingTrashStore::in_memory())),
                    b_events_tx,
                    tokio_util::task::TaskTracker::new(),
                    tokio_util::sync::CancellationToken::new(),
                )
                .await
                .unwrap()
            },
        );

        // Poll for convergence instead of a fixed sleep (a fixed wait flakes
        // under parallel load). Wait on the FULL set the assertions below
        // check — every note, BOTH sides — or an early break leaves some note
        // unconverged and the asserts flake. Breaks as soon as it all lands.
        let deleted_flag = |s: &crate::sync::store::doc_store::DocStore, n: &NotePath| {
            s.load_or_create(n).ok().and_then(|d| d.is_deleted().ok())
        };
        let text_of = |s: &crate::sync::store::doc_store::DocStore, n: &NotePath| {
            s.load_or_create(n).ok().and_then(|d| d.render().ok())
        };
        for _ in 0..600 {
            let converged = {
                let a = a_store.lock().await;
                let b = b_store.lock().await;
                // deleted.md: tombstoned both sides.
                deleted_flag(&a, &deleted) == Some(true)
                    && deleted_flag(&b, &deleted) == Some(true)
                    // both.md: tombstoned both sides, text = B's edit.
                    && deleted_flag(&a, &both) == Some(true)
                    && deleted_flag(&b, &both) == Some(true)
                    && text_of(&a, &both).as_deref() == Some("shared body + B edit")
                    && text_of(&b, &both).as_deref() == Some("shared body + B edit")
                    // b-only.md: alive both sides, B's edit synced.
                    && deleted_flag(&a, &b_only) == Some(false)
                    && deleted_flag(&b, &b_only) == Some(false)
                    && text_of(&a, &b_only).as_deref() == Some("untouched + B edit")
                    && text_of(&b, &b_only).as_deref() == Some("untouched + B edit")
            };
            if converged {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        a_h.shutdown().await;
        b_h.shutdown().await;
        let _ = tokio::time::timeout(Duration::from_secs(2), a_task).await;
        let _ = tokio::time::timeout(Duration::from_secs(2), b_task).await;

        // ── Assert convergence on every note, both devices ──────────────────
        let a = a_store.lock().await;
        let b = b_store.lock().await;

        // deleted.md: deleted on both, file gone on both.
        assert!(a.load_or_create(&deleted).unwrap().is_deleted().unwrap());
        assert!(b.load_or_create(&deleted).unwrap().is_deleted().unwrap());
        assert!(!a_roost.path().join("deleted.md").exists());
        assert!(!b_roost.path().join("deleted.md").exists());

        // both.md: delete wins over the concurrent edit; deleted on both, file
        // trashed on both, but B's edit is preserved in the CRDT (recoverable).
        let a_both = a.load_or_create(&both).unwrap();
        let b_both = b.load_or_create(&both).unwrap();
        assert!(a_both.is_deleted().unwrap(), "both.md deleted on A");
        assert!(
            b_both.is_deleted().unwrap(),
            "both.md deleted on B (delete wins)"
        );
        assert_eq!(
            a_both.render().unwrap(),
            b_both.render().unwrap(),
            "both.md text converges identically"
        );
        assert_eq!(
            a_both.render().unwrap(),
            "shared body + B edit",
            "B's edit preserved in the CRDT despite the delete"
        );
        assert!(
            !a_roost.path().join("both.md").exists(),
            "both.md trashed on A"
        );
        assert!(
            !b_roost.path().join("both.md").exists(),
            "both.md trashed on B — the edit must not resurrect it"
        );

        // b-only.md: no delete; alive on both with B's edit synced over.
        let a_bonly = a.load_or_create(&b_only).unwrap();
        let b_bonly = b.load_or_create(&b_only).unwrap();
        assert!(!a_bonly.is_deleted().unwrap(), "b-only.md stays alive on A");
        assert!(!b_bonly.is_deleted().unwrap(), "b-only.md stays alive on B");
        assert_eq!(
            a_bonly.render().unwrap(),
            "untouched + B edit",
            "B's edit synced to A"
        );
        assert_eq!(b_bonly.render().unwrap(), "untouched + B edit");
        assert!(
            a_roost.path().join("b-only.md").exists(),
            "b-only.md rendered to disk on A"
        );
    }

    #[test]
    fn crdt_corruption_classifier_matches_known_failure_strings() {
        // The classifier decides whether a per-note error is "this doc's
        // bytes are unsalvageable" (recover the single note) vs a transient
        // problem (leave it alone). Getting this wrong in either direction
        // is a reliability bug: false negatives let a poison note thrash the
        // whole session; false positives needlessly nuke healthy notes.
        //
        // Positive cases — the empirically-observed Automerge/DocStore
        // strings documented on `is_crdt_corruption`. Wrapped in extra text
        // because the real errors carry context around the marker substring.
        for s in [
            "duplicate seq 7 for actor abcd",
            "automerge: failed to load document chunk",
            "changes out of order: missing dependency",
            "error inflating compressed save",
        ] {
            assert!(is_crdt_corruption(s), "should be classified corrupt: {s:?}");
        }
        // Negative cases — ordinary transient/IO errors must NOT trip
        // per-note recovery.
        for s in [
            "connection reset by peer",
            "deadline has elapsed",
            "permission denied (os error 13)",
            "",
        ] {
            assert!(
                !is_crdt_corruption(s),
                "should NOT be classified corrupt: {s:?}"
            );
        }
    }

    #[test]
    fn recovery_breaker_trips_after_threshold_and_isolates_notes() {
        // The breaker is the safety net behind "one bad note must not kill
        // the session": a note that keeps failing recovery within the window
        // is poisoned after RECOVERY_MAX_ATTEMPTS, so we stop retrying it
        // instead of looping. A separate note must be tracked independently —
        // poisoning one note can't strand its neighbors.
        let mut attempts: HashMap<NotePath, RecoveryWindow> = HashMap::new();
        let mut poisoned: HashSet<NotePath> = HashSet::new();
        let flaky = NotePath("flaky.md".into());

        // The first RECOVERY_MAX_ATTEMPTS - 1 attempts accumulate but don't
        // trip, and the note stays un-poisoned.
        for i in 1..RECOVERY_MAX_ATTEMPTS {
            assert!(
                !trip_recovery_breaker(&mut attempts, &mut poisoned, &flaky),
                "attempt {i} must not trip the breaker yet"
            );
            assert!(!poisoned.contains(&flaky), "not poisoned before threshold");
        }
        // The threshold-th attempt trips and poisons the path.
        assert!(
            trip_recovery_breaker(&mut attempts, &mut poisoned, &flaky),
            "attempt {RECOVERY_MAX_ATTEMPTS} must trip the breaker"
        );
        assert!(poisoned.contains(&flaky), "tripped path must be poisoned");

        // A different note is accounted independently — a single failure on
        // it neither trips nor poisons.
        let healthy = NotePath("healthy.md".into());
        assert!(!trip_recovery_breaker(
            &mut attempts,
            &mut poisoned,
            &healthy
        ));
        assert!(
            !poisoned.contains(&healthy),
            "an unrelated note must not be poisoned by the flaky one"
        );
    }

    #[test]
    fn conflict_note_path_appends_content_hash_suffix() {
        assert_eq!(
            conflict_note_path(&NotePath("folder/note.md".into()), "deadbeef").0,
            "folder/note.conflict-deadbeef.md"
        );
        // `.markdown` is stripped too, and a path with no extension is left
        // intact apart from the suffix.
        assert_eq!(
            conflict_note_path(&NotePath("a.markdown".into()), "abcd1234").0,
            "a.conflict-abcd1234.md"
        );
        assert_eq!(
            conflict_note_path(&NotePath("noext".into()), "00").0,
            "noext.conflict-00.md"
        );
    }

    #[test]
    fn conflict_content_hash_is_stable_and_distinct() {
        // M-6: identical text → identical suffix (idempotent artifact);
        // different text → different suffix (no clock-skew collision).
        assert_eq!(
            conflict_content_hash("same text"),
            conflict_content_hash("same text")
        );
        assert_ne!(
            conflict_content_hash("text A"),
            conflict_content_hash("text B")
        );
    }

    #[tokio::test]
    async fn write_conflict_artifact_saves_local_text_to_disk() {
        let roost = tempdir().unwrap();
        let note = NotePath("sub/note.md".into());
        let saved = match write_conflict_artifact(
            roost.path(),
            None, // std::fs fallback
            &note,
            "IMPORTANT LOCAL EDITS\n",
        )
        .await
        {
            ConflictBackup::Saved(p) => p,
            ConflictBackup::NothingToSave => panic!("non-empty text must not be NothingToSave"),
            ConflictBackup::Failed => panic!("non-empty text must produce a Saved artifact"),
        };
        assert!(
            saved.0.starts_with("sub/note.conflict-") && saved.0.ends_with(".md"),
            "unexpected conflict path: {}",
            saved.0
        );
        let body = std::fs::read_to_string(roost.path().join(&saved.0)).unwrap();
        assert_eq!(
            body, "IMPORTANT LOCAL EDITS\n",
            "the about-to-be-lost local text must be recoverable verbatim"
        );
        // M-6: writing the SAME text again is idempotent — same file, no pile-up.
        let again =
            match write_conflict_artifact(roost.path(), None, &note, "IMPORTANT LOCAL EDITS\n")
                .await
            {
                ConflictBackup::Saved(p) => p,
                _ => panic!("expected Saved"),
            };
        assert_eq!(again.0, saved.0, "identical text must reuse one artifact");
    }

    #[tokio::test]
    async fn write_conflict_artifact_skips_empty_text() {
        let roost = tempdir().unwrap();
        // Fix #4: whitespace-only local doc is "nothing to save" — a DISTINCT
        // outcome from a write failure, so the adopt is free to proceed.
        assert!(
            matches!(
                write_conflict_artifact(roost.path(), None, &NotePath("x.md".into()), "   \n")
                    .await,
                ConflictBackup::NothingToSave
            ),
            "whitespace-only local doc must be NothingToSave, not Failed"
        );
    }

    /// Fix #4: a genuine WRITE FAILURE must surface as `Failed` (not collapse
    /// into the same value as "nothing to save"), so the caller can refuse to
    /// adopt and keep the local copy. Forced by occupying the artifact's PARENT
    /// path with a file: `create_dir_all` on a path whose parent is a file fails,
    /// so the artifact can't be written.
    #[tokio::test]
    async fn write_conflict_artifact_reports_failure_when_write_fails() {
        let roost = tempdir().unwrap();
        // The note lives under "sub/", so the artifact's parent dir is
        // "<roost>/sub". Put a FILE there so create_dir_all("<roost>/sub") fails.
        std::fs::write(roost.path().join("sub"), b"i am a file, not a dir").unwrap();
        let note = NotePath("sub/note.md".into());
        assert!(
            matches!(
                write_conflict_artifact(roost.path(), None, &note, "IMPORTANT LOCAL EDITS\n").await,
                ConflictBackup::Failed
            ),
            "a real write failure must be reported as Failed so the caller can fail-closed"
        );
    }

    /// Headline #4 regression: when an inbound `FullDoc` fails to merge into a
    /// divergent local doc (the "duplicate seq" class), the engine adopts the
    /// remote copy — but the local edits it overwrites MUST be saved to a
    /// recoverable conflict artifact first, never silently discarded.
    #[tokio::test]
    async fn full_doc_merge_failure_quarantines_local_before_adopt() {
        let roost = tempdir().unwrap();
        let store = Arc::new(Mutex::new(DocStore::new(
            roost.path().to_path_buf(),
            dev(1),
        )));
        store.lock().await.init().unwrap();
        let path = NotePath("conflict.md".into());

        // Local doc with unsynced edits, created under actor dev(7).
        let actor = dev(7).as_actor_id();
        {
            let s = store.lock().await;
            let mut local = NoteDoc::new_with_actor(actor.clone()).unwrap();
            local.apply_markdown("IMPORTANT LOCAL EDITS\n").unwrap();
            s.save(&path, &mut local).unwrap();
        }
        // Remote doc: SAME actor, independent history → merge fails with the
        // duplicate-seq error, forcing the adopt path.
        let remote_bytes = {
            let mut remote = NoteDoc::new_with_actor(actor).unwrap();
            remote.apply_markdown("remote replacement\n").unwrap();
            remote.save()
        };

        let (events_tx, _rx) = tokio::sync::broadcast::channel::<SyncEvent>(16);
        let mut ctx = SyncCtx {
            roost: roost.path().to_path_buf(),
            roost_fs: None,
            doc_store: store.clone(),
            note_locks: Arc::new(NoteLocks::new()),
            pending_trash: Arc::new(Mutex::new(PendingTrashStore::in_memory())),
            sync: PeerSyncState::new(),
            dirty: HashSet::new(),
            peer_device_id: dev(2),
            peer_display_name: "peer".into(),
            events: events_tx,
            recovery_attempts: HashMap::new(),
            poisoned_paths: HashSet::new(),
            blob_inflight: HashMap::new(),
            blob_unavailable: HashSet::new(),
            blob_pending: HashSet::new(),
            scan_queue: Vec::new(),
            full_doc_buffers: HashMap::new(),
            seed_done: false,
        };

        let mut transport = MockTransport::pair().0;
        handle_inbound(
            &mut transport,
            &mut ctx,
            Message::FullDoc {
                path: path.clone(),
                bytes: remote_bytes,
            },
        )
        .await
        .unwrap();

        // Remote was adopted as the canonical state...
        let adopted = store
            .lock()
            .await
            .load_or_create(&path)
            .unwrap()
            .render()
            .unwrap();
        assert_eq!(
            adopted, "remote replacement\n",
            "remote should be adopted after a merge failure"
        );

        // ...but the local edits were quarantined to a conflict artifact.
        let conflict = std::fs::read_dir(roost.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .find(|n| n.starts_with("conflict.conflict-") && n.ends_with(".md"))
            .expect("a conflict artifact must be written before the adopt");
        let body = std::fs::read_to_string(roost.path().join(&conflict)).unwrap();
        assert_eq!(
            body, "IMPORTANT LOCAL EDITS\n",
            "the overwritten local edits must be recoverable verbatim"
        );
    }

    /// Fix #4 (fail-closed): when an inbound `FullDoc` fails to merge into a
    /// divergent local doc AND the conflict backup can't be written, the engine
    /// must NOT adopt the remote (which would overwrite the local edits with no
    /// recoverable copy). It keeps the local doc and emits `NoteSyncFailed` so
    /// the round retries later. This is the whole point of distinguishing
    /// "nothing to save" from "write failed".
    #[tokio::test]
    async fn full_doc_merge_failure_does_not_adopt_when_backup_fails() {
        let roost = tempdir().unwrap();
        let store = Arc::new(Mutex::new(DocStore::new(
            roost.path().to_path_buf(),
            dev(1),
        )));
        store.lock().await.init().unwrap();
        // Note under "sub/" so its conflict artifact would land at
        // "<roost>/sub/note.conflict-*.md". Occupy "<roost>/sub" with a FILE so
        // the artifact's `create_dir_all` fails → backup write fails.
        let path = NotePath("sub/note.md".into());

        let actor = dev(7).as_actor_id();
        {
            let s = store.lock().await;
            let mut local = NoteDoc::new_with_actor(actor.clone()).unwrap();
            local.apply_markdown("IMPORTANT LOCAL EDITS\n").unwrap();
            s.save(&path, &mut local).unwrap();
        }
        // Block the conflict-artifact parent dir (DocStore state lives under
        // "<roost>/docs/…", so this file doesn't disturb the CRDT store).
        std::fs::write(roost.path().join("sub"), b"not a dir").unwrap();

        let remote_bytes = {
            let mut remote = NoteDoc::new_with_actor(actor).unwrap();
            remote.apply_markdown("remote replacement\n").unwrap();
            remote.save()
        };

        let (events_tx, mut rx) = tokio::sync::broadcast::channel::<SyncEvent>(16);
        let mut ctx = SyncCtx {
            roost: roost.path().to_path_buf(),
            roost_fs: None,
            doc_store: store.clone(),
            note_locks: Arc::new(NoteLocks::new()),
            pending_trash: Arc::new(Mutex::new(PendingTrashStore::in_memory())),
            sync: PeerSyncState::new(),
            dirty: HashSet::new(),
            peer_device_id: dev(2),
            peer_display_name: "peer".into(),
            events: events_tx,
            recovery_attempts: HashMap::new(),
            poisoned_paths: HashSet::new(),
            blob_inflight: HashMap::new(),
            blob_unavailable: HashSet::new(),
            blob_pending: HashSet::new(),
            scan_queue: Vec::new(),
            full_doc_buffers: HashMap::new(),
            seed_done: false,
        };

        let mut transport = MockTransport::pair().0;
        handle_inbound(
            &mut transport,
            &mut ctx,
            Message::FullDoc {
                path: path.clone(),
                bytes: remote_bytes,
            },
        )
        .await
        .unwrap();

        // The local doc MUST survive — the adopt was refused because the backup
        // couldn't be written.
        let kept = store
            .lock()
            .await
            .load_or_create(&path)
            .unwrap()
            .render()
            .unwrap();
        assert_eq!(
            kept, "IMPORTANT LOCAL EDITS\n",
            "a failed backup must block the adopt and keep the local copy"
        );

        // And the failure is surfaced (not falsely green).
        let mut saw_failed = false;
        while let Ok(ev) = rx.try_recv() {
            if let SyncEvent::NoteSyncFailed { path: p, .. } = ev {
                if p == path.0 {
                    saw_failed = true;
                }
            }
        }
        assert!(
            saw_failed,
            "a blocked adopt must emit NoteSyncFailed so the note isn't shown as synced"
        );
    }

    /// H-6: a note that can't be synced must emit `NoteSyncFailed` instead of
    /// silently returning `Ok(false)` with a falsely-green indicator. Drives
    /// the corrupt-FullDoc-into-empty path.
    #[tokio::test]
    async fn corrupt_full_doc_emits_note_sync_failed() {
        let roost = tempdir().unwrap();
        let store = Arc::new(Mutex::new(DocStore::new(
            roost.path().to_path_buf(),
            dev(1),
        )));
        store.lock().await.init().unwrap();
        let path = NotePath("broken.md".into());

        let (events_tx, mut rx) = tokio::sync::broadcast::channel::<SyncEvent>(16);
        let mut ctx = SyncCtx {
            roost: roost.path().to_path_buf(),
            roost_fs: None,
            doc_store: store.clone(),
            note_locks: Arc::new(NoteLocks::new()),
            pending_trash: Arc::new(Mutex::new(PendingTrashStore::in_memory())),
            sync: PeerSyncState::new(),
            dirty: HashSet::new(),
            peer_device_id: dev(2),
            peer_display_name: "peer".into(),
            events: events_tx,
            recovery_attempts: HashMap::new(),
            poisoned_paths: HashSet::new(),
            blob_inflight: HashMap::new(),
            blob_unavailable: HashSet::new(),
            blob_pending: HashSet::new(),
            scan_queue: Vec::new(),
            full_doc_buffers: HashMap::new(),
            seed_done: false,
        };

        let mut transport = MockTransport::pair().0;
        handle_inbound(
            &mut transport,
            &mut ctx,
            Message::FullDoc {
                path: path.clone(),
                bytes: vec![0xff, 0x00, 0x01, 0x02], // not a valid Automerge save
            },
        )
        .await
        .unwrap();

        let mut saw = false;
        while let Ok(ev) = rx.try_recv() {
            if let SyncEvent::NoteSyncFailed { path: p, .. } = ev {
                if p == "broken.md" {
                    saw = true;
                }
            }
        }
        assert!(saw, "a corrupt inbound FullDoc must emit NoteSyncFailed");
    }

    /// H-6: a render-to-disk failure (here: the note's parent folder can't be
    /// created because a FILE already occupies that path) must emit
    /// `NoteSyncFailed`, not silently succeed. This is the disk-full / reserved
    /// -name / permission class the round-2 audit flagged as still-silent.
    #[tokio::test]
    async fn render_write_failure_emits_note_sync_failed() {
        let roost = tempdir().unwrap();
        // Occupy "dir" with a regular file so create_dir_all(roost/dir) fails
        // when we try to write "dir/note.md".
        std::fs::write(roost.path().join("dir"), b"i am a file, not a folder").unwrap();

        let store = Arc::new(Mutex::new(DocStore::new(
            roost.path().to_path_buf(),
            dev(1),
        )));
        store.lock().await.init().unwrap();
        let path = NotePath("dir/note.md".into());

        // A valid Automerge save with real content so render reaches the write.
        let save = {
            let mut d = NoteDoc::new(dev(9)).unwrap();
            d.apply_markdown("content that must reach disk").unwrap();
            d.save()
        };

        let (events_tx, mut rx) = tokio::sync::broadcast::channel::<SyncEvent>(16);
        let mut ctx = SyncCtx {
            roost: roost.path().to_path_buf(),
            roost_fs: None,
            doc_store: store.clone(),
            note_locks: Arc::new(NoteLocks::new()),
            pending_trash: Arc::new(Mutex::new(PendingTrashStore::in_memory())),
            sync: PeerSyncState::new(),
            dirty: HashSet::new(),
            peer_device_id: dev(2),
            peer_display_name: "peer".into(),
            events: events_tx,
            recovery_attempts: HashMap::new(),
            poisoned_paths: HashSet::new(),
            blob_inflight: HashMap::new(),
            blob_unavailable: HashSet::new(),
            blob_pending: HashSet::new(),
            scan_queue: Vec::new(),
            full_doc_buffers: HashMap::new(),
            seed_done: false,
        };

        let mut transport = MockTransport::pair().0;
        handle_inbound(
            &mut transport,
            &mut ctx,
            Message::FullDoc {
                path: path.clone(),
                bytes: save,
            },
        )
        .await
        .unwrap();

        let mut failed = false;
        let mut wrongly_written = false;
        while let Ok(ev) = rx.try_recv() {
            match ev {
                SyncEvent::NoteSyncFailed { path: p, .. } if p == "dir/note.md" => failed = true,
                SyncEvent::NoteWritten { path: p } if p == "dir/note.md" => wrongly_written = true,
                _ => {}
            }
        }
        assert!(failed, "a render-to-disk failure must emit NoteSyncFailed");
        assert!(
            !wrongly_written,
            "a failed write must not also emit NoteWritten"
        );
    }

    /// SYN-B8: a tombstone whose disk-trash genuinely FAILS (here: `.corvux-trash`
    /// is occupied by a regular file, so the backend can't create the trash dir)
    /// must emit `NoteSyncFailed`, NOT `NoteDeleted` — otherwise the DocStore
    /// says deleted while the `.md` survives, the desktop watcher re-ingests it,
    /// and the delete is silently resurrected + re-propagated. Before the fix the
    /// delete branch returned bare `false` and emitted nothing at all.
    #[tokio::test]
    async fn tombstone_trash_failure_emits_note_sync_failed_not_deleted() {
        let roost = tempdir().unwrap();
        // The note exists on disk (so trash is attempted, not a NotFound no-op).
        std::fs::write(roost.path().join("gone.md"), b"soon to be deleted").unwrap();
        // Occupy `.corvux-trash` with a FILE so the backend's trash-dir create
        // fails → `trash` returns a genuine (non-NotFound) error.
        std::fs::write(roost.path().join(".corvux-trash"), b"not a dir").unwrap();

        let store = Arc::new(Mutex::new(DocStore::new(
            roost.path().to_path_buf(),
            dev(1),
        )));
        store.lock().await.init().unwrap();
        let path = NotePath("gone.md".into());

        // A tombstoned FullDoc (deleted flag set) — reconcile takes the trash arm.
        let save = {
            let mut d = NoteDoc::new(dev(9)).unwrap();
            d.apply_markdown("soon to be deleted").unwrap();
            d.mark_deleted(1).unwrap();
            d.save()
        };

        let (events_tx, mut rx) = tokio::sync::broadcast::channel::<SyncEvent>(16);
        let mut ctx = SyncCtx {
            roost: roost.path().to_path_buf(),
            roost_fs: None,
            doc_store: store.clone(),
            note_locks: Arc::new(NoteLocks::new()),
            pending_trash: Arc::new(Mutex::new(PendingTrashStore::in_memory())),
            sync: PeerSyncState::new(),
            dirty: HashSet::new(),
            peer_device_id: dev(2),
            peer_display_name: "peer".into(),
            events: events_tx,
            recovery_attempts: HashMap::new(),
            poisoned_paths: HashSet::new(),
            blob_inflight: HashMap::new(),
            blob_unavailable: HashSet::new(),
            blob_pending: HashSet::new(),
            scan_queue: Vec::new(),
            full_doc_buffers: HashMap::new(),
            seed_done: false,
        };

        let mut transport = MockTransport::pair().0;
        handle_inbound(
            &mut transport,
            &mut ctx,
            Message::FullDoc {
                path: path.clone(),
                bytes: save,
            },
        )
        .await
        .unwrap();

        let mut failed = false;
        let mut wrongly_deleted = false;
        while let Ok(ev) = rx.try_recv() {
            match ev {
                SyncEvent::NoteSyncFailed { path: p, .. } if p == "gone.md" => failed = true,
                SyncEvent::NoteDeleted { path: p } if p == "gone.md" => wrongly_deleted = true,
                _ => {}
            }
        }
        assert!(
            failed,
            "a genuine trash failure must emit NoteSyncFailed (SYN-B8)"
        );
        assert!(
            !wrongly_deleted,
            "a failed trash must NOT emit NoteDeleted — the file is still on disk"
        );
    }

    /// Hole A: a first-pair FullDoc adoption for a path we hold NO DocStore
    /// record for must NOT silently overwrite an untracked file already on disk
    /// at that path. The pre-pair local content must survive in a conflict
    /// artifact (the SAF-bootstrap data-loss case). Exercises the std::fs
    /// fallback (roost_fs = None), which shares the backup logic.
    #[tokio::test]
    async fn fresh_adopt_backs_up_untracked_ondisk_file() {
        let roost = tempdir().unwrap();
        // A note the joining device wrote before ever pairing — untracked.
        std::fs::write(roost.path().join("note.md"), b"LOCAL PHONE CONTENT").unwrap();

        let store = Arc::new(Mutex::new(DocStore::new(
            roost.path().to_path_buf(),
            dev(1),
        )));
        store.lock().await.init().unwrap();
        let path = NotePath("note.md".into());

        // The peer's copy of the SAME path, with DIFFERENT content.
        let remote_save = {
            let mut d = NoteDoc::new(dev(9)).unwrap();
            d.apply_markdown("REMOTE DESKTOP CONTENT").unwrap();
            d.save()
        };

        let mut ctx = bootstrap_ctx(roost.path(), store.clone());
        let mut transport = MockTransport::pair().0;
        handle_inbound(
            &mut transport,
            &mut ctx,
            Message::FullDoc {
                path,
                bytes: remote_save,
            },
        )
        .await
        .unwrap();

        // Adopted: the on-disk note now holds the remote content.
        assert_eq!(
            std::fs::read_to_string(roost.path().join("note.md")).unwrap(),
            "REMOTE DESKTOP CONTENT",
            "fresh adoption must write the peer's copy"
        );

        // The pre-pair local content must be recoverable from a conflict artifact.
        let mut backups: Vec<String> = std::fs::read_dir(roost.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("note.conflict-") && n.ends_with(".md"))
            .collect();
        assert_eq!(
            backups.len(),
            1,
            "exactly one conflict artifact must preserve the overwritten local note"
        );
        let body = std::fs::read_to_string(roost.path().join(backups.pop().unwrap())).unwrap();
        assert_eq!(
            body, "LOCAL PHONE CONTENT",
            "the pre-pair local content must survive the overwrite verbatim"
        );
    }

    /// Hole A: a fresh adoption whose on-disk file is byte-identical to the
    /// incoming copy must NOT scatter a needless conflict artifact.
    #[tokio::test]
    async fn fresh_adopt_no_backup_when_ondisk_matches() {
        let roost = tempdir().unwrap();
        std::fs::write(roost.path().join("same.md"), b"IDENTICAL").unwrap();
        let store = Arc::new(Mutex::new(DocStore::new(
            roost.path().to_path_buf(),
            dev(1),
        )));
        store.lock().await.init().unwrap();

        let remote_save = {
            let mut d = NoteDoc::new(dev(9)).unwrap();
            d.apply_markdown("IDENTICAL").unwrap();
            d.save()
        };
        let mut ctx = bootstrap_ctx(roost.path(), store.clone());
        let mut transport = MockTransport::pair().0;
        handle_inbound(
            &mut transport,
            &mut ctx,
            Message::FullDoc {
                path: NotePath("same.md".into()),
                bytes: remote_save,
            },
        )
        .await
        .unwrap();

        let has_conflict = std::fs::read_dir(roost.path()).unwrap().any(|e| {
            e.map(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("same.conflict-")
            })
            .unwrap_or(false)
        });
        assert!(
            !has_conflict,
            "identical on-disk content must not produce a conflict artifact"
        );
    }

    /// Hole B: on a scan-skipped (`content://`) roost, seed a note the joining
    /// device has on disk that the peer does NOT have, while leaving a note the
    /// peer already has for the divergence-free FullDoc-adopt path. The
    /// `content://` gate keys off the roost STRING; a `NativeFs` over a temp dir
    /// does the real I/O.
    #[tokio::test]
    async fn seed_disk_only_notes_seeds_peer_absent_and_skips_shared() {
        let roost_dir = tempdir().unwrap();
        std::fs::write(roost_dir.path().join("phone-only.md"), b"only on the phone").unwrap();
        std::fs::write(roost_dir.path().join("shared.md"), b"exists on both").unwrap();

        let state = tempdir().unwrap();
        let store = Arc::new(Mutex::new(DocStore::new(
            state.path().to_path_buf(),
            dev(1),
        )));
        store.lock().await.init().unwrap();

        let fs = Arc::new(crate::roostfs::native::NativeFs::new(roost_dir.path()));
        let mut ctx = bootstrap_ctx(roost_dir.path(), store.clone());
        ctx.roost = std::path::PathBuf::from("content://test-roost");
        ctx.roost_fs = Some(fs);

        let peer_paths: HashSet<NotePath> = [NotePath("shared.md".into())].into_iter().collect();
        let ours: HashSet<NotePath> = HashSet::new();

        let seeded = seed_disk_only_notes(&ctx, &peer_paths, &ours).await;
        assert_eq!(seeded, 1, "only the peer-absent note must be seeded");

        let store = store.lock().await;
        assert!(
            store.exists(&NotePath("phone-only.md".into())),
            "a note only this device has must be seeded so it can sync out"
        );
        assert!(
            !store.exists(&NotePath("shared.md".into())),
            "a note the peer already has must NOT be locally seeded (adopt path handles it)"
        );
        let doc = store
            .load_or_create(&NotePath("phone-only.md".into()))
            .unwrap();
        assert_eq!(doc.render().unwrap(), "only on the phone");
    }

    /// Hole B: seeding is a no-op off a scan-skipped roost — desktop
    /// `scan_roost` already ingested everything, so the NoteList path must not
    /// double-seed on a native roost.
    #[tokio::test]
    async fn seed_disk_only_notes_is_noop_on_non_saf_roost() {
        let roost_dir = tempdir().unwrap();
        std::fs::write(roost_dir.path().join("n.md"), b"x").unwrap();
        let store = Arc::new(Mutex::new(DocStore::new(
            roost_dir.path().to_path_buf(),
            dev(1),
        )));
        store.lock().await.init().unwrap();
        let ctx = bootstrap_ctx(roost_dir.path(), store.clone()); // roost is a real path, not content://
        let seeded = seed_disk_only_notes(&ctx, &HashSet::new(), &HashSet::new()).await;
        assert_eq!(seeded, 0, "no seeding on a non-scan-skipped roost");
    }

    fn bootstrap_ctx(roost: &std::path::Path, store: Arc<Mutex<DocStore>>) -> SyncCtx {
        let (events_tx, _rx) = tokio::sync::broadcast::channel::<SyncEvent>(16);
        SyncCtx {
            roost: roost.to_path_buf(),
            roost_fs: None,
            doc_store: store,
            note_locks: Arc::new(NoteLocks::new()),
            pending_trash: Arc::new(Mutex::new(PendingTrashStore::in_memory())),
            sync: PeerSyncState::new(),
            dirty: HashSet::new(),
            peer_device_id: dev(2),
            peer_display_name: "peer".into(),
            events: events_tx,
            recovery_attempts: HashMap::new(),
            poisoned_paths: HashSet::new(),
            blob_inflight: HashMap::new(),
            blob_unavailable: HashSet::new(),
            blob_pending: HashSet::new(),
            scan_queue: Vec::new(),
            full_doc_buffers: HashMap::new(),
            seed_done: false,
        }
    }

    /// SYN-M8: a blob request with no reply past the deadline is reclaimed from
    /// inflight back to pending (so it re-issues and the slot frees); a request
    /// still within the deadline is left in flight.
    #[tokio::test]
    async fn timed_out_blob_request_is_reclaimed_to_pending() {
        let roost = tempdir().unwrap();
        let store = Arc::new(Mutex::new(DocStore::new(
            roost.path().to_path_buf(),
            dev(1),
        )));
        let mut ctx = bootstrap_ctx(roost.path(), store);

        let sent_at = Instant::now();
        ctx.blob_inflight
            .insert(".attachments/stuck.webp".into(), sent_at);

        // Sweep with a clock past the deadline → reclaimed to pending.
        let past_deadline = sent_at + BLOB_REQUEST_TIMEOUT + Duration::from_secs(1);
        assert!(
            sweep_timed_out_blob_requests(&mut ctx, past_deadline),
            "a request past the deadline must be reclaimed"
        );
        assert!(!ctx.blob_inflight.contains_key(".attachments/stuck.webp"));
        assert!(
            ctx.blob_pending.contains(".attachments/stuck.webp"),
            "reclaimed request must go to pending for re-issue"
        );

        // A fresh request (swept at its own send time) is left alone.
        let fresh_at = Instant::now();
        ctx.blob_inflight
            .insert(".attachments/fresh.webp".into(), fresh_at);
        assert!(
            !sweep_timed_out_blob_requests(&mut ctx, fresh_at),
            "a request within the deadline must not be reclaimed"
        );
        assert!(ctx.blob_inflight.contains_key(".attachments/fresh.webp"));
    }

    /// SYN-M9: the conflict-artifact content hash is 128-bit (32 hex) and
    /// distinguishes different texts — so two DIFFERENT conflicted local edits
    /// never collide onto one `conflict-<hash>.md` and clobber each other.
    /// Identical text still shares a filename (intended idempotent reuse).
    #[test]
    fn conflict_content_hash_is_128_bit_and_distinguishes_text() {
        let a = conflict_content_hash("IMPORTANT local edits A\n");
        let a_again = conflict_content_hash("IMPORTANT local edits A\n");
        let b = conflict_content_hash("DIFFERENT local edits B\n");
        assert_eq!(a.len(), 32, "128-bit hash must be 32 hex chars");
        assert_eq!(a, a_again, "identical text → same suffix (idempotent)");
        assert_ne!(a, b, "distinct text → distinct suffix (no clobber)");
    }

    /// C-4: a note delivered as a chunked FullDoc sequence must reassemble and
    /// adopt to identical content — the whole point of the cliff fix. We split
    /// a real save with a tiny chunk size (the wire chunk size only affects
    /// how it's split, not correctness).
    #[tokio::test]
    async fn chunked_full_doc_reassembles_and_adopts() {
        let roost = tempdir().unwrap();
        let store = Arc::new(Mutex::new(DocStore::new(
            roost.path().to_path_buf(),
            dev(1),
        )));
        store.lock().await.init().unwrap();
        let path = NotePath("big.md".into());

        // Build a real doc snapshot, then split it into 4 pieces by hand.
        let save = {
            let mut d = NoteDoc::new(dev(9)).unwrap();
            d.apply_markdown("chunked content that survives reassembly")
                .unwrap();
            d.save()
        };
        let piece = save.len().div_ceil(4);
        let chunks: Vec<&[u8]> = save.chunks(piece.max(1)).collect();
        let count = chunks.len() as u32;

        let mut ctx = bootstrap_ctx(roost.path(), store.clone());
        let mut transport = MockTransport::pair().0;
        for (i, ch) in chunks.iter().enumerate() {
            handle_inbound(
                &mut transport,
                &mut ctx,
                Message::FullDocChunk {
                    path: path.clone(),
                    chunk_index: i as u32,
                    chunk_count: count,
                    bytes: ch.to_vec(),
                },
            )
            .await
            .unwrap();
        }

        assert_eq!(
            store
                .lock()
                .await
                .load_or_create(&path)
                .unwrap()
                .render()
                .unwrap(),
            "chunked content that survives reassembly",
            "chunked FullDoc must adopt to identical content"
        );
        assert!(
            ctx.full_doc_buffers.is_empty(),
            "reassembly buffer must be cleared after adoption"
        );
    }

    /// C-4: an out-of-order chunk drops the partial transfer instead of
    /// corrupting the buffer.
    #[tokio::test]
    async fn out_of_order_chunk_drops_partial_transfer() {
        let roost = tempdir().unwrap();
        let store = Arc::new(Mutex::new(DocStore::new(
            roost.path().to_path_buf(),
            dev(1),
        )));
        store.lock().await.init().unwrap();
        let path = NotePath("x.md".into());
        let mut ctx = bootstrap_ctx(roost.path(), store.clone());
        let mut transport = MockTransport::pair().0;

        // chunk_index 0 then 2 (skipping 1) → the second is out of order.
        for idx in [0u32, 2u32] {
            handle_inbound(
                &mut transport,
                &mut ctx,
                Message::FullDocChunk {
                    path: path.clone(),
                    chunk_index: idx,
                    chunk_count: 3,
                    bytes: vec![0u8; 4],
                },
            )
            .await
            .unwrap();
        }
        assert!(
            ctx.full_doc_buffers.is_empty(),
            "an out-of-order chunk must drop the partial transfer"
        );
        assert!(
            !store.lock().await.exists(&path),
            "nothing should be adopted"
        );
    }

    /// C-4: a peer that opens more distinct-path chunked transfers than the
    /// concurrency cap — each first-chunk-only, never completed — must have the
    /// surplus transfers refused, so aggregate reassembly memory stays bounded
    /// even though every individual buffer is under its own size cap.
    #[tokio::test]
    async fn concurrent_fulldoc_transfers_are_capped() {
        let roost = tempdir().unwrap();
        let store = Arc::new(Mutex::new(DocStore::new(
            roost.path().to_path_buf(),
            dev(1),
        )));
        store.lock().await.init().unwrap();
        let mut ctx = bootstrap_ctx(roost.path(), store.clone());
        let mut transport = MockTransport::pair().0;

        // Open MAX + 8 distinct transfers, each sending only chunk 0 of a
        // 2-chunk sequence (so none ever completes).
        let attempts = MAX_CONCURRENT_FULLDOC_BUFFERS + 8;
        for i in 0..attempts {
            handle_inbound(
                &mut transport,
                &mut ctx,
                Message::FullDocChunk {
                    path: NotePath(format!("flood/{i}.md")),
                    chunk_index: 0,
                    chunk_count: 2,
                    bytes: vec![0u8; 16],
                },
            )
            .await
            .unwrap();
        }
        assert_eq!(
            ctx.full_doc_buffers.len(),
            MAX_CONCURRENT_FULLDOC_BUFFERS,
            "surplus concurrent transfers beyond the cap must be refused"
        );

        // An already-in-progress path is still allowed to continue (not evicted
        // by the flood): feeding chunk 1 to an existing buffer completes it.
        let existing = NotePath("flood/0.md".into());
        assert!(ctx.full_doc_buffers.contains_key(&existing));
    }

    /// C-4: the aggregate-bytes ceiling boundary — tested via the pure helper so
    /// we don't have to allocate gigabytes to hit the real cap.
    #[test]
    fn fulldoc_total_ceiling_boundary() {
        assert!(!fulldoc_total_exceeds(0, MAX_FULLDOC_TOTAL_BYTES));
        assert!(!fulldoc_total_exceeds(MAX_FULLDOC_TOTAL_BYTES, 0));
        assert!(fulldoc_total_exceeds(MAX_FULLDOC_TOTAL_BYTES, 1));
        assert!(fulldoc_total_exceeds(
            MAX_FULLDOC_TOTAL_BYTES / 2 + 1,
            MAX_FULLDOC_TOTAL_BYTES / 2
        ));
        // Saturating: an absurd incoming can't wrap around to look "under".
        assert!(fulldoc_total_exceeds(1, usize::MAX));
    }

    /// C-4: a stalled transfer (no chunk within the idle TTL) is evicted when
    /// the next chunk arrives, freeing its slot so a flood can't jam slots
    /// permanently. We backdate the buffer's last_activity to simulate the gap.
    #[tokio::test]
    async fn idle_fulldoc_buffer_is_evicted() {
        let roost = tempdir().unwrap();
        let store = Arc::new(Mutex::new(DocStore::new(
            roost.path().to_path_buf(),
            dev(1),
        )));
        store.lock().await.init().unwrap();
        let mut ctx = bootstrap_ctx(roost.path(), store.clone());
        let mut transport = MockTransport::pair().0;

        // Open a transfer (chunk 0 of 2), then backdate it well past the TTL.
        let stale = NotePath("stale.md".into());
        handle_inbound(
            &mut transport,
            &mut ctx,
            Message::FullDocChunk {
                path: stale.clone(),
                chunk_index: 0,
                chunk_count: 2,
                bytes: vec![0u8; 8],
            },
        )
        .await
        .unwrap();
        assert!(ctx.full_doc_buffers.contains_key(&stale));
        ctx.full_doc_buffers.get_mut(&stale).unwrap().last_activity =
            Instant::now() - (FULLDOC_BUFFER_IDLE_TTL + Duration::from_secs(1));

        // A chunk for a DIFFERENT note triggers the idle sweep, evicting stale.
        handle_inbound(
            &mut transport,
            &mut ctx,
            Message::FullDocChunk {
                path: NotePath("fresh.md".into()),
                chunk_index: 0,
                chunk_count: 2,
                bytes: vec![0u8; 8],
            },
        )
        .await
        .unwrap();
        assert!(
            !ctx.full_doc_buffers.contains_key(&stale),
            "a transfer idle past the TTL must be evicted"
        );
        assert!(
            ctx.full_doc_buffers
                .contains_key(&NotePath("fresh.md".into())),
            "the fresh transfer stays"
        );
    }
}

#[cfg(test)]
mod blob_helpers_tests {
    use super::{is_attachment_rel, parse_attachment_refs, percent_decode};

    #[test]
    fn is_attachment_rel_accepts_only_attachments_dir() {
        assert!(is_attachment_rel(".attachments/paste-1.webp"));
        assert!(is_attachment_rel("./.attachments/x.png"));
        assert!(!is_attachment_rel("notes/ideas.md"));
        assert!(!is_attachment_rel(".attachments/")); // no filename
        assert!(!is_attachment_rel(".attachments/../secret"));
        assert!(!is_attachment_rel("../.attachments/x.png"));
    }

    #[test]
    fn parse_refs_markdown_and_wikilink() {
        let md = "text ![alt](.attachments/paste-1.webp) more\n\
                  and ![[.attachments/diagram.png]] and audio \
                  ![rec](.attachments/recording-9.webm)";
        let refs = parse_attachment_refs(md);
        assert!(refs.contains(&".attachments/paste-1.webp".to_string()));
        assert!(refs.contains(&".attachments/diagram.png".to_string()));
        assert!(refs.contains(&".attachments/recording-9.webm".to_string()));
        assert_eq!(refs.len(), 3);
    }

    #[test]
    fn parse_refs_dedups_same_attachment() {
        let md = "![a](.attachments/x.webp) ... ![b](.attachments/x.webp)";
        let refs = parse_attachment_refs(md);
        assert_eq!(refs, vec![".attachments/x.webp".to_string()]);
    }

    #[test]
    fn parse_refs_ignores_non_attachment_and_traversal() {
        let md = "![](images/foo.png) ![](.attachments/../etc/passwd) plain text";
        let refs = parse_attachment_refs(md);
        assert!(refs.is_empty());
    }

    #[test]
    fn parse_refs_percent_decodes_spaces() {
        let md = "![](.attachments/my%20pic.webp)";
        let refs = parse_attachment_refs(md);
        assert_eq!(refs, vec![".attachments/my pic.webp".to_string()]);
    }

    #[test]
    fn percent_decode_handles_trailing_percent() {
        assert_eq!(percent_decode("a%"), "a%");
        assert_eq!(percent_decode("a%2"), "a%2");
        assert_eq!(percent_decode("a%20b"), "a b");
    }
}
