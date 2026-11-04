//! Sync engine — the public orchestrator.
//!
//! Owns the long-lived state for a running sync subsystem: identity, TLS
//! material, stores, and the map of live peer connections. One [`SyncEngine`]
//! instance per active roost; Tauri holds it inside a `State` and the
//! frontend drives it through the commands in `commands::sync`.
//!
//! Sibling modules:
//! - [`super::state`] — `PeerConnectionState`, `SyncEvent`, `SyncStatus`,
//!   `ConnectedPeer`, `KnownPeer`. Pure data shapes.
//! - [`super::accept`] — TCP accept loop + per-incoming handshake task.
//! - [`super::util`] — stateless helpers (time, roost-name decoding,
//!   peer-handle clone-by-channel).
//! - [`super::peer`] — per-connection state machine (handshake → sync loop).
//! - [`super::watcher`] — filesystem watcher emitting `LocalChange`s.
//!
//! # Lifecycle
//!
//! ```text
//!   SyncEngine::new(roost, identity, tls, name)
//!          │
//!          ▼
//!   start_listening(port)  ──────────────▶  accept loop (background task)
//!          │
//!          ▼
//!   connect_peer(addr, pin, pinned)  ─────▶  outbound handshake → per-peer task
//!                                       ◀─── event bus (status, failures)
//!          │
//!          ▼
//!   stop()  ─── broadcasts shutdown to every peer task + listener task
//! ```
//!
//! # Re-pairing vs first-pairing
//!
//! The transport accepts a `pinned: Option<Fingerprint>`:
//! - `None` → TOFU. Used on first connection; after pairing succeeds, the
//!   observed fingerprint is persisted in `PeerStore`.
//! - `Some(fp)` → subsequent reconnects verify against the stored pin and
//!   refuse on mismatch (key rotation or MitM).
//!
//! The PIN handshake runs on every connection in the MVP — the stored pin
//! protects only against MitM, not against replaying a pairing session.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{broadcast, Mutex, RwLock};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tracing::{debug, info, warn};

use crate::roostfs::RoostFs;
use crate::sync::crypto::fingerprint::Fingerprint;
use crate::sync::crypto::pairing::Pin;
use crate::sync::error::{SyncError, SyncResult};
use crate::sync::identity::{DeviceId, Identity};
use crate::sync::queue::client::QueueClient;
use crate::sync::store::doc_store::DocStore;
use crate::sync::store::peer_store::PeerStore;
use crate::sync::transport::tcp_tls::TlsIdentity;
use iroh::{Endpoint, EndpointId as NodeId};

use super::peer::PeerHandle;
use super::state::{ConnectedPeer, KnownPeer, PeerConnectionState, SyncEvent, SyncStatus};
use super::util::{clone_handle, now_millis, roost_folder_name};
use super::watcher::{LocalChange, RoostWatcher};

/// Handle to a running sync engine. Cloneable (internally `Arc`ed).
#[derive(Clone)]
pub struct SyncEngine {
    pub(super) inner: Arc<SyncEngineInner>,
}

/// Whether the engine should report `running`. Extracted as a pure function so
/// the "a live iroh listener counts as running" rule has a hermetic regression
/// test — a fresh host that only tapped "pair a device" binds an iroh listener
/// but has no TCP port and no peers yet, and previously reported "stopped".
fn compute_running(
    has_tcp_listener: bool,
    iroh_listening: bool,
    has_connected_peers: bool,
    has_known_peers: bool,
) -> bool {
    has_tcp_listener || iroh_listening || has_connected_peers || has_known_peers
}

pub(super) struct SyncEngineInner {
    pub(super) roost: PathBuf,
    /// Last path segment of `roost`. Sent in the handshake so peers can
    /// refuse pairings between mismatched roosts (e.g. one side opened
    /// "MyBrain" and the other opened "Work").
    pub(super) roost_name: String,
    /// AUDIT-015: 16 random bytes generated once per roost, exchanged in
    /// the handshake's `Hello` so two unrelated "MyBrain" roosts can't
    /// pair-and-weld CRDT histories. Read from (or written to)
    /// `<roost>/.corvux/vault-id.bin` on engine construction.
    pub(super) vault_id: [u8; 16],
    /// Backend-agnostic roost filesystem. On desktop this is `NativeFs`
    /// wrapping `tokio::fs`; on Android it's `SafFs` wrapping the SAF
    /// `ContentResolver`. `None` when sync is started without an open
    /// roost backend (rare — `sync_init` fills it in from `RoostFsState`).
    /// `peer.rs` uses this for rendering received notes back to disk so
    /// writes succeed on content:// URIs.
    pub(super) roost_fs: Option<Arc<dyn RoostFs>>,
    pub(super) identity: Arc<Identity>,
    pub(super) tls_identity: Arc<TlsIdentity>,
    pub(super) device_name: String,

    pub(super) doc_store: Arc<Mutex<DocStore>>,
    pub(super) peer_store: Arc<Mutex<PeerStore>>,

    /// Per-note serialization. Held OUTERMOST (before `doc_store`) across the
    /// whole "commit DocStore → reconcile disk" span at every mutation entry
    /// point — local save/delete, live adopt (shared with each peer task via
    /// `SyncCtx`), queue adopt, and startup scan/recover — so a stale disk
    /// render can't clobber a concurrent local write (then be re-ingested by
    /// the watcher as CRDT data loss). See `note_locks::NoteLocks`.
    pub(super) note_locks: Arc<super::note_locks::NoteLocks>,

    /// Durable client outbox. Records `(peer, note)` delivery intents that
    /// failed to reach an offline peer through the queue, so they're retried
    /// on a timer / at startup instead of waiting for the next local edit.
    /// Persisted to `<state_dir>/outbox.bin`. Empty / inert in strict-P2P
    /// mode (no queue client).
    pub(super) outbox: Arc<Mutex<crate::sync::store::outbox_store::OutboxStore>>,

    /// Live peer connections indexed by DeviceId.
    pub(super) peers: Arc<RwLock<HashMap<DeviceId, PeerHandle>>>,

    /// SYN-B8 (durable as of the 2026-07-13 audit follow-up): notes whose
    /// tombstone is committed to the DocStore but whose on-disk trash FAILED
    /// (read-only roost, permission, I/O). The receive paths
    /// (`reconcile_doc_to_roost`) and the local delete (`mark_note_deleted`)
    /// enroll the path here instead of leaving a silent tombstone↔file
    /// divergence; the reconnect supervisor drains it by re-attempting the
    /// trash each tick until it succeeds (`NoteDeleted`) or the file is gone.
    /// This is the live-path equivalent of the queue path's retain-message →
    /// re-pull → re-attempt-trash — the tombstone STANDS (per the scan-skip
    /// policy: a present file never auto-un-deletes; only an explicit Restore
    /// does), so retrying the trash pursues the CRDT's delete intent rather
    /// than resurrecting the note.
    ///
    /// Backed by `PendingTrashStore` (`pending_trash.bin`, same atomic-
    /// rewrite-on-mutation discipline as `OutboxStore`), so an enrollment
    /// survives a restart — previously in-memory-only, which meant a note
    /// whose trash failed right before the app closed was silently forgotten:
    /// the startup scan does NOT re-enroll (it can't tell "trash failed" from
    /// "user recreated the file", and auto-trashing the latter would be data
    /// loss — scan deliberately skips tombstoned-present notes for exactly
    /// this reason), so nothing else would ever retry it. Persisting the
    /// enrollment itself is safe BECAUSE `retry_pending_trashes` always
    /// re-checks `is_deleted()` under the note lock before trashing,
    /// regardless of whether the entry came from this session or a reload —
    /// a restored note is abandoned, never wrongly trashed.
    pub(super) pending_trash:
        Arc<Mutex<crate::sync::store::pending_trash_store::PendingTrashStore>>,

    /// Monotonic counter handing a unique `generation` to every
    /// `register_peer` call. The peer task's async cleanup compares its
    /// generation against the one currently in `peers` so a superseded
    /// (reconnected) entry is never torn down by its predecessor's exit.
    pub(super) peer_gen: std::sync::atomic::AtomicU64,

    /// Coalesced, engine-owned set of `(peer → note paths)` whose *live*
    /// delivery was dropped because the peer's control channel was momentarily
    /// full (a wedged/slow but still-connected peer — `send_local` fast-fails
    /// rather than block the editor). Unlike [`outbox`](Self::outbox) this is
    /// NOT queue-gated: it works in the default strict-P2P / no-queue config,
    /// where the outbox is inert. The reconnect supervisor drains it every tick
    /// by re-poking the peer's control channel (`PeerControl::MarkDirty`), so a
    /// dropped change converges within one supervisor tick instead of stalling
    /// until the next edit or reconnect. No payload travels — `flush_outbound`
    /// regenerates from the authoritative DocStore. In-memory only: a process
    /// restart forces reconnect, whose `NoteList` exchange re-dirties shared
    /// paths anyway, and `register_peer` clears a device's entries on redial.
    pub(super) live_pending: Mutex<HashMap<DeviceId, HashSet<crate::sync::protocol::NotePath>>>,

    /// Set once by `stop()`. In-flight dial tasks (spawned before stop and
    /// still unwinding) check this before lazily re-binding the endpoint or
    /// registering a peer — without it, a redial that completes just after a
    /// roost switch resurrects the OLD roost's engine (invisible to the UI)
    /// and keeps writing synced notes into the old roost until process exit.
    /// One-way: a fresh roost gets a fresh engine (fresh `inner`, flag false).
    pub(super) stopped: std::sync::atomic::AtomicBool,

    /// Set from `prefs.strict_p2p_only` at init. When true, the iroh endpoint
    /// binds with `RelayMode::Disabled` so no Corvux/Number0 relay ever carries
    /// this device's traffic — the strict-P2P promise. (Direct/LAN + NAT
    /// hole-punch only; cross-NAT reachability that needs a relay won't work,
    /// which is the intended trade for strict mode.)
    pub(super) relay_disabled: std::sync::atomic::AtomicBool,

    /// Per-peer dial lifecycle. Written by the engine's dial paths
    /// (manual `connect_peer`, `try_reconnect_all`) and the accept
    /// loop when an inbound peer reaches `PeerConnected`. Exposed via
    /// `SyncStatus.known_peers[i].connection_state` and
    /// `SyncEvent::PeerStatusChanged`. In-memory only — on restart every
    /// peer re-starts at `Disconnected`.
    pub(super) peer_states: Arc<RwLock<HashMap<DeviceId, PeerConnectionState>>>,

    /// Per-peer reconnect bookkeeping for the engine-owned supervisor:
    /// exponential backoff (`attempts` / `next_retry_at`), last failure
    /// reason, and last successful-connect time. Updated centrally in
    /// `set_peer_state` so both inbound and outbound transitions feed it.
    /// In-memory only. Drives the supervisor's "dial due peers" decision
    /// and the richer per-peer status fields.
    pub(super) reconnect: Arc<RwLock<HashMap<DeviceId, ReconnectState>>>,

    /// Engine-owned reconnect supervisor task. Periodically redials
    /// disconnected known peers whose backoff has elapsed — so sync
    /// convergence survives the webview being suspended/killed (mobile)
    /// and no longer depends on a frontend retry loop. Aborted on `stop()`.
    pub(super) reconnect_task: Mutex<Option<JoinHandle<()>>>,

    /// Set by `start_listening`; cleared by `stop`.
    pub(super) listen_state: Mutex<Option<ListenState>>,

    /// Set by `start_listening_iroh`; cleared by `stop`. Held alongside the
    /// TCP `listen_state` during the iroh migration so existing LAN-only
    /// pairings keep working while we validate the iroh path. Removing
    /// the TCP path is a follow-up once iroh is proven across desktop +
    /// mobile + cellular in the beta cohort.
    pub(super) iroh_listen_state: Mutex<Option<IrohListenState>>,

    /// LAN address hints fed by our own mDNS discovery: iroh `EndpointId`
    /// (NodeId) to observed direct UDP addresses. Attached to the iroh
    /// endpoint at bind time via `Endpoint::builder(..).address_lookup(..)`
    /// (additive alongside the N0 preset's own pkarr/relay lookup -- see
    /// `transport::iroh::build_endpoint`) so the outbound dialer can
    /// resolve a same-Wi-Fi peer without depending on stale ticket addrs
    /// or the internet pkarr record. iroh 1.0.1 has no LAN/mDNS discovery
    /// of its own, which is the gap this closes.
    ///
    /// Address resolution only. Trusting a LAN-advertised address here
    /// carries the same risk profile as iroh own discovery services --
    /// discovery only ever tells you WHERE to dial, never WHO you are
    /// talking to. Authentication is still gated by the pinned TLS
    /// fingerprint (legacy transport) and, on iroh, the NodeId-
    /// authenticated QUIC handshake plus the app-layer PIN/resumption-
    /// token proof (`peer::run_handshake`) -- a spoofed LAN advertisement
    /// can at best make us dial the wrong socket, never pass as a peer it
    /// is not.
    pub(super) lan_lookup: iroh::address_lookup::MemoryLookup,

    /// Optional store-and-forward queue client. When set, local writes
    /// to known-but-currently-disconnected peers fan out as encrypted
    /// queue pushes; on demand, the engine pulls + decrypts + applies
    /// pending messages addressed to this device's NodeId. `None` means
    /// strict P2P-only mode — the queue is a configurable add-on, not a
    /// hard dependency. Wired via `SyncEngine::set_queue_client`.
    pub(super) queue_client: tokio::sync::RwLock<Option<Arc<QueueClient>>>,

    /// Background task that periodically drains pending queue messages.
    /// Spun up alongside the queue client; aborted when the client is
    /// removed (or replaced). Shutdown also aborts via `stop()`.
    pub(super) queue_pull_task: Mutex<Option<JoinHandle<()>>>,

    /// Background task that periodically re-attempts due outbox entries.
    /// Lifecycle mirrors `queue_pull_task` — spun up with the queue client,
    /// aborted when it's removed and on `stop()`.
    pub(super) outbox_drain_task: Mutex<Option<JoinHandle<()>>>,

    /// Drop to kill the watcher.
    pub(super) watcher: Mutex<Option<RoostWatcher>>,

    /// Sender side of the watcher→engine change pipe. The engine fan-outs
    /// each change to every connected peer.
    pub(super) watcher_task: Mutex<Option<JoinHandle<()>>>,

    pub(super) events: broadcast::Sender<SyncEvent>,

    /// AUDIT-077: shared across every accepted handshake on this engine.
    /// Holds the exponential-backoff counter for failed pairings so that
    /// reconnecting can't reset it. Spans both the TCP+TLS and iroh
    /// accept paths.
    pub(super) pairing_rate_limiter:
        std::sync::Arc<std::sync::Mutex<crate::sync::crypto::pairing::KeyedPairingRateLimiter>>,

    /// SRX brute-force fix: engine-shared first-pair enrollment window. Opened
    /// by the IPC pairing commands when the user is actively pairing; the accept
    /// paths inject it into every `ServerHandshake` so inbound FIRST-PAIR
    /// handshakes are refused outside the window and a source-independent global
    /// cap trips inside it (defeating fresh-NodeId PIN spraying). Reconnects are
    /// never gated by this.
    pub(super) enrollment_window: crate::sync::crypto::pairing::SharedEnrollmentWindow,

    /// SRX layer 3: the one-time high-entropy pairing secret minted for the
    /// current "Show code" session (`Some`), or `None` when not actively
    /// QR-pairing. It is embedded in the pairing ticket AND folded into the
    /// first-pair proof/session key by the accept path, so ticket pairing can't
    /// be brute-forced — a caller that didn't obtain the ticket derives a
    /// different key and its proof fails. Single source of truth for both the
    /// ticket (`iroh_pairing_ticket`) and the handshake (accept path).
    pub(super) pairing_secret: Arc<RwLock<Option<crate::sync::crypto::pairing::PairingSecret>>>,

    /// SYN-I4 structured lifecycle. `tracker` owns EVERY task the engine
    /// spawns (via `spawn_tracked`); `cancel` is the root cancellation token
    /// whose child tokens each task selects on. `stop()` = `cancel.cancel()` +
    /// `tracker.close()` + `tracker.wait()`, so no engine task can outlive
    /// `stop()`. A fresh roost builds a fresh engine (fresh `inner`), so a
    /// closed tracker is never reused — `stop()` is terminal per instance.
    pub(super) tracker: TaskTracker,
    pub(super) cancel: CancellationToken,

    /// SYN-B6 lifecycle serialization. Held by `stop()` across cancel+drain
    /// and by every bring-up method (`start_listening`, `start_listening_iroh`,
    /// `start_watcher`, `start_reconnect_supervisor`, `set_queue_client`) so a
    /// bring-up racing a stop can't install a task into a draining engine.
    /// Lock order: command-layer `State` locks → this → engine field locks.
    /// Engine methods never reach back into command `State`, so no inversion.
    pub(super) lifecycle: Mutex<()>,
}

/// Per-peer reconnect backoff + outcome bookkeeping. See
/// [`SyncEngineInner::reconnect`].
#[derive(Default, Clone)]
pub(super) struct ReconnectState {
    /// Consecutive failed reconnect attempts (resets to 0 on success).
    pub(super) attempts: u32,
    /// Unix millis before which the supervisor should not redial this
    /// peer. 0 = due now.
    pub(super) next_retry_at_millis: u64,
    /// Most recent failure reason (cleared on success).
    pub(super) last_error: Option<String>,
    /// Unix millis of the last successful connection this run.
    pub(super) last_success_at_millis: Option<u64>,
}

/// Per-peer reconnect backoff schedule (millis), indexed by consecutive
/// failures. Capped at 60s (was 5 min): pairings are one-directional, so
/// when the LISTENING side restarts, only the dialer can restore the link —
/// and a dialer parked at a 5-minute cap reads as "sync is broken" to a
/// user watching both devices. A dial attempt is a single QUIC handshake,
/// so the worst-case cost of the lower cap is one tiny packet exchange a
/// minute per unreachable peer.
/// L-2: after this many consecutive failed dials the reconnect supervisor
/// stops auto-retrying a peer (backoff caps at 60s, so otherwise a dead/removed
/// peer is dialed every minute forever). `force_reconnect_all` (app foreground
/// / manual Reconnect) resets `attempts`, re-arming a peer that comes back.
/// L-2: consecutive-failure cutoff. Paired with the escalating backoff below,
/// 40 attempts spans ~2.7 h before the supervisor stops auto-dialing a peer
/// (`force_reconnect_all` on app foreground / manual Reconnect re-arms it).
/// The old value (20, all under a 60 s cap) gave up after only ~18 min — long
/// enough to abandon a peer whose WiFi was merely down for an hour. This is
/// tuned to ride out a realistic transient outage while still eventually
/// releasing a genuinely dead/removed peer.
pub(super) const MAX_RECONNECT_ATTEMPTS: u32 = 40;

/// L-2: whether the reconnect supervisor should skip auto-dialing a peer that
/// already has a reconnect entry this tick — either it has hit the
/// consecutive-failure cutoff, or its backoff window hasn't elapsed.
pub(super) fn should_skip_reconnect(attempts: u32, next_retry_at_millis: u64, now: u64) -> bool {
    attempts >= MAX_RECONNECT_ATTEMPTS || next_retry_at_millis > now
}

pub(super) fn reconnect_backoff_ms(attempts: u32) -> u64 {
    match attempts {
        0 | 1 => 15_000, // 15s
        2 => 30_000,     // 30s
        3..=8 => 60_000, // 1m for the first few minutes
        // Escalate to a 5m cap for a sustained outage so we stop hammering the
        // relay every minute for hours, while still riding the outage out to
        // the MAX_RECONNECT_ATTEMPTS cutoff (~2.7 h total). See L-2.
        _ => 300_000, // 5m cap
    }
}

/// `reconnect_backoff_ms` with ±25% random jitter applied. Kept separate so
/// the base schedule stays deterministic (and unit-testable), while the value
/// actually written to `next_retry_at_millis` is spread out. Without this, N
/// devices that drop at the same instant (router reboot, or a single
/// `force_reconnect_all` on app resume) all share an attempt count and retry
/// on the identical tick — a self-inflicted thundering herd against the paid
/// relay. OsRng is already a sync-crate dependency (crypto envelope). See M-5.
pub(super) fn jittered_backoff_ms(attempts: u32) -> u64 {
    use rand::RngCore;
    let base = reconnect_backoff_ms(attempts);
    let spread = base / 4;
    if spread == 0 {
        return base;
    }
    let mut buf = [0u8; 8];
    rand::rngs::OsRng.fill_bytes(&mut buf);
    let delta = u64::from_le_bytes(buf) % (spread * 2 + 1);
    base.saturating_sub(spread).saturating_add(delta)
}

pub(super) struct ListenState {
    pub(super) port: u16,
    pub(super) task: JoinHandle<()>,
}

/// State held while the iroh endpoint is bound and accepting inbound
/// connections. The `Endpoint` itself is `Clone` (cheap Arc inside) so
/// outbound dials reuse the same socket without re-binding.
pub(super) struct IrohListenState {
    pub(super) endpoint: Endpoint,
    pub(super) node_id: NodeId,
    pub(super) incoming_pin: Arc<RwLock<Pin>>,
    pub(super) task: JoinHandle<()>,
}

impl SyncEngine {
    pub fn new(
        roost: PathBuf,
        state_dir: PathBuf,
        roost_fs: Option<Arc<dyn RoostFs>>,
        identity: Arc<Identity>,
        tls_identity: Arc<TlsIdentity>,
        device_name: String,
    ) -> SyncResult<Self> {
        let device_id = identity.device_id();
        // DocStore + PeerStore both live in the *app-private* state dir, not
        // the roost. This is the only path that's guaranteed writable under
        // all Tauri target OSes (notably Android SAF roosts return EROFS
        // for std::fs writes).
        info!(state_dir = %state_dir.display(), roost = %roost.display(), "SyncEngine::new: preparing state_dir");
        std::fs::create_dir_all(&state_dir).map_err(|e| {
            warn!(dir = %state_dir.display(), error = %e, "SyncEngine::new: create_dir_all state_dir failed");
            SyncError::Io(e)
        })?;
        info!("SyncEngine::new: DocStore::new + init");
        // C-1: probe the roost's filesystem for case-sensitivity too, not just
        // the state dir — the `.md` source of truth lives in the roost and it
        // can be a different volume. The probe self-guards against a
        // non-writable roost (SAF/EROFS) by falling back to case-sensitive.
        let doc_store =
            DocStore::new(state_dir.clone(), device_id).with_roost_probe_dir(roost.clone());
        doc_store.init().map_err(|e| {
            warn!(dir = %state_dir.display(), error = %e, "SyncEngine::new: DocStore init failed");
            e
        })?;
        info!("SyncEngine::new: PeerStore::load");
        let peer_store = PeerStore::load(&state_dir).map_err(|e| {
            warn!(dir = %state_dir.display(), error = %e, "SyncEngine::new: PeerStore load failed");
            e
        })?;
        info!("SyncEngine::new: OutboxStore::load");
        let outbox =
            crate::sync::store::outbox_store::OutboxStore::load(&state_dir).map_err(|e| {
                warn!(dir = %state_dir.display(), error = %e, "SyncEngine::new: OutboxStore load failed");
                e
            })?;
        info!("SyncEngine::new: PendingTrashStore::load");
        let pending_trash = crate::sync::store::pending_trash_store::PendingTrashStore::load(
            &state_dir,
        )
        .map_err(|e| {
            warn!(dir = %state_dir.display(), error = %e, "SyncEngine::new: PendingTrashStore load failed");
            e
        })?;
        let (events, _) = broadcast::channel(64);
        let roost_name = roost_folder_name(&roost);
        // AUDIT-015: ensure the roost carries a stable id before any peer
        // can see our Hello. A read/write failure here is non-fatal — we
        // fall back to a derived id keyed by the roost path so the rest
        // of sync still works; only the "moves with the roost" property
        // is lost.
        let vault_id = match crate::sync::vault_id::ensure_vault_id_sync(&roost) {
            Ok(id) => id,
            Err(e) => {
                warn!(
                    error = %e,
                    "could not persist vault-id.bin; deriving a fallback id from the roost path"
                );
                use sha2::Digest;
                let mut hasher = sha2::Sha256::new();
                hasher.update(b"corvux-vault-id-fallback-v1");
                hasher.update(roost.to_string_lossy().as_bytes());
                let digest = hasher.finalize();
                let mut id = [0u8; 16];
                id.copy_from_slice(&digest[..16]);
                id
            }
        };
        Ok(Self {
            inner: Arc::new(SyncEngineInner {
                roost,
                roost_name,
                vault_id,
                roost_fs,
                identity,
                tls_identity,
                device_name,
                doc_store: Arc::new(Mutex::new(doc_store)),
                peer_store: Arc::new(Mutex::new(peer_store)),
                note_locks: Arc::new(super::note_locks::NoteLocks::new()),
                outbox: Arc::new(Mutex::new(outbox)),
                peers: Arc::new(RwLock::new(HashMap::new())),
                pending_trash: Arc::new(Mutex::new(pending_trash)),
                live_pending: Mutex::new(HashMap::new()),
                peer_gen: std::sync::atomic::AtomicU64::new(0),
                stopped: std::sync::atomic::AtomicBool::new(false),
                relay_disabled: std::sync::atomic::AtomicBool::new(false),
                peer_states: Arc::new(RwLock::new(HashMap::new())),
                reconnect: Arc::new(RwLock::new(HashMap::new())),
                reconnect_task: Mutex::new(None),
                listen_state: Mutex::new(None),
                iroh_listen_state: Mutex::new(None),
                lan_lookup: iroh::address_lookup::MemoryLookup::new(),
                queue_client: tokio::sync::RwLock::new(None),
                queue_pull_task: Mutex::new(None),
                outbox_drain_task: Mutex::new(None),
                watcher: Mutex::new(None),
                watcher_task: Mutex::new(None),
                events,
                pairing_rate_limiter: std::sync::Arc::new(std::sync::Mutex::new(
                    crate::sync::crypto::pairing::KeyedPairingRateLimiter::new(),
                )),
                enrollment_window: std::sync::Arc::new(std::sync::Mutex::new(
                    crate::sync::crypto::pairing::EnrollmentWindow::new(),
                )),
                pairing_secret: Arc::new(RwLock::new(None)),
                tracker: TaskTracker::new(),
                cancel: CancellationToken::new(),
                lifecycle: Mutex::new(()),
            }),
        })
    }

    /// SYN-I4: spawn a task OWNED by the engine's `TaskTracker`, so `stop()`
    /// drains it. The task body receives a child of the engine cancel token —
    /// long-lived loops must `select!` on `token.cancelled()` and exit promptly;
    /// short per-op tasks can ignore it (the tracker still waits for them) but
    /// should honor it where a long await (sleep, network) would otherwise
    /// delay shutdown. This is the ONLY sanctioned spawn primitive on the sync
    /// surface — the ratchet (`sync_invariants::syn_i4_*`) fails a raw
    /// `tokio::spawn` in engine-owned files.
    pub(super) fn spawn_tracked<F>(&self, f: impl FnOnce(CancellationToken) -> F) -> JoinHandle<()>
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        let token = self.inner.cancel.child_token();
        self.inner.tracker.spawn(f(token))
    }

    /// Engine TaskTracker clone — for `peer::spawn`, which spawns the per-peer
    /// sync loop on it so `stop()` drains the loop (SYN-I4).
    pub(super) fn tracker_clone(&self) -> TaskTracker {
        self.inner.tracker.clone()
    }

    /// A fresh child of the engine cancel token — for `peer::spawn` and any
    /// other caller that spawns onto the tracker outside `spawn_tracked`.
    pub(super) fn child_cancel_token(&self) -> CancellationToken {
        self.inner.cancel.child_token()
    }

    /// Shared handle to the SYN-B8 pending-trash retry set — threaded into
    /// `peer::spawn`'s `SyncCtx` so the live receive path can enroll a failed
    /// tombstone-trash for the supervisor to re-attempt.
    pub(super) fn pending_trash_clone(
        &self,
    ) -> Arc<Mutex<crate::sync::store::pending_trash_store::PendingTrashStore>> {
        self.inner.pending_trash.clone()
    }

    /// AUDIT-077: handle to the engine-scoped pairing rate limiter. The
    /// accept paths thread this into every `ServerHandshake` so the
    /// brute-force backoff counter survives across reconnects.
    pub(super) fn pairing_rate_limiter(
        &self,
    ) -> std::sync::Arc<std::sync::Mutex<crate::sync::crypto::pairing::KeyedPairingRateLimiter>>
    {
        self.inner.pairing_rate_limiter.clone()
    }

    /// SRX brute-force fix: handle to the engine-scoped first-pair enrollment
    /// window. The accept paths inject this into every `ServerHandshake`.
    pub(super) fn enrollment_window(&self) -> crate::sync::crypto::pairing::SharedEnrollmentWindow {
        self.inner.enrollment_window.clone()
    }

    /// SRX brute-force fix: open the first-pair enrollment window for the
    /// standard duration. Called by the IPC pairing commands when the user takes
    /// an explicit pairing action ("Show code" / scan+PIN) so inbound first-pair
    /// handshakes are only admitted while the user is actually pairing.
    pub fn open_enrollment_window(&self) {
        if let Ok(mut w) = self.inner.enrollment_window.lock() {
            w.open(crate::sync::crypto::pairing::ENROLLMENT_WINDOW);
        }
    }

    /// SRX brute-force fix: close the first-pair enrollment window (e.g. on
    /// engine stop). Reconnects are unaffected — they authenticate by token.
    pub fn close_enrollment_window(&self) {
        if let Ok(mut w) = self.inner.enrollment_window.lock() {
            w.close();
        }
    }

    /// SRX layer 3: set (or clear) the one-time pairing secret for the current
    /// "Show code" session. Stored as the single source of truth for both the
    /// ticket and the accept-path handshake. Called by `sync_listen_iroh`.
    pub async fn set_pairing_secret(
        &self,
        secret: Option<crate::sync::crypto::pairing::PairingSecret>,
    ) {
        *self.inner.pairing_secret.write().await = secret;
    }

    /// SRX layer 3: the current session's pairing secret handle, injected by the
    /// accept path into each `ServerHandshake` so first-pair proofs are bound to
    /// the secret carried in the ticket.
    pub(super) fn pairing_secret_handle(
        &self,
    ) -> Arc<RwLock<Option<crate::sync::crypto::pairing::PairingSecret>>> {
        self.inner.pairing_secret.clone()
    }

    /// Roost folder name used in the sync handshake. Exposed so UI / mDNS
    /// can surface it ("Nearby: Raj's Phone — roost MyBrain").
    pub fn roost_name(&self) -> &str {
        &self.inner.roost_name
    }

    /// AUDIT-015: stable 16-byte identity for this roost. Sent in the
    /// handshake `Hello`; persistent across engine restarts via
    /// `<roost>/.corvux/vault-id.bin`.
    pub fn vault_id(&self) -> [u8; 16] {
        self.inner.vault_id
    }

    /// Full roost path (filesystem or `content://` URI on Android). Used
    /// by the auto-resume flow to key saved prefs against the roost the
    /// engine was actually started with.
    pub fn roost_path(&self) -> String {
        self.inner.roost.to_string_lossy().to_string()
    }

    /// Clone of the engine LAN address-lookup table (see
    /// `SyncEngineInner::lan_lookup`). Cheap -- `MemoryLookup` is
    /// `Arc`-backed internally. Passed to `transport::iroh::build_endpoint`
    /// at bind time so the iroh endpoint actually consults it. Production
    /// callers reach the field directly via `self.inner.lan_lookup` (see
    /// dial.rs); this accessor is kept public for status/diagnostics call
    /// sites and so tests do not need `pub(super)` visibility into
    /// `SyncEngineInner`.
    #[allow(dead_code)] // exercised by tests today; see doc comment above
    pub fn lan_lookup(&self) -> iroh::address_lookup::MemoryLookup {
        self.inner.lan_lookup.clone()
    }

    /// Feed a LAN-observed peer iroh identity + direct addresses into the
    /// engine `MemoryLookup` so the outbound iroh dialer can resolve it.
    /// Called from the mDNS discovery bridge
    /// (`commands::sync::spawn_discovery_bridge`) for a paired peer whose
    /// advertised NodeId is welded to its device_id.
    ///
    /// Uses `set_endpoint_info` (REPLACE), not `add_endpoint_info` (augment):
    /// each mDNS sighting carries the peer's *current* full LAN address set,
    /// so replacing keeps the entry fresh and stops stale addresses from
    /// piling up across the peer's IP changes -- no separate prune-on-`Lost`
    /// needed. Address resolution only -- see the doc comment on
    /// `SyncEngineInner::lan_lookup` for why trusting a LAN-advertised
    /// address is acceptable (matches iroh own discovery threat model).
    pub fn feed_lan_endpoint(
        &self,
        node_id: iroh::EndpointId,
        addrs: impl IntoIterator<Item = std::net::SocketAddr>,
    ) {
        let mut node_addr = iroh::EndpointAddr::new(node_id);
        for addr in addrs {
            node_addr = node_addr.with_ip_addr(addr);
        }
        // REPLACE, not augment, so a peer's stale addresses don't linger.
        let _ = self.inner.lan_lookup.set_endpoint_info(node_addr);
    }

    /// True when `device_id` is a peer we've actually paired with (present
    /// in `peer_store`). Used by the mDNS discovery bridge to gate the LAN
    /// address feed: we only trust a LAN advertisement's `(NodeId -> addr)`
    /// hint for peers we chose to pair with, so a rogue device on the Wi-Fi
    /// cannot inject arbitrary entries into the address lookup (bounds the
    /// table to our pairing count and removes the reflection surface). This
    /// is address-resolution gating only -- auth is still the NodeId QUIC
    /// handshake + PIN/token proof regardless.
    pub async fn is_paired(&self, device_id: DeviceId) -> bool {
        self.inner.peer_store.lock().await.get(device_id).is_some()
    }

    /// Clone of the currently-bound iroh `Endpoint`, or `None` if the iroh
    /// listener isn't up. Used by the mDNS re-announce task (`commands::sync`)
    /// to watch this endpoint's direct-address changes via
    /// `Endpoint::watch_addr` and re-broadcast the fresh addrs over mDNS, so
    /// the LAN fast path keeps working after the endpoint discovers or changes
    /// its addresses post-bind (the one-shot announce would otherwise go
    /// stale). `Endpoint` is cheap to clone (Arc inside).
    pub async fn iroh_endpoint(&self) -> Option<Endpoint> {
        self.inner
            .iroh_listen_state
            .lock()
            .await
            .as_ref()
            .map(|s| s.endpoint.clone())
    }

    /// Current docstore text for a note, or `None` if the note isn't in the
    /// store. Lets the command layer assert that a local write actually
    /// reached sync without reaching into the engine's private docstore.
    /// Test-only: the production sync path never reads back through here.
    #[cfg(test)]
    pub(crate) async fn rendered_note_text(
        &self,
        path: &crate::sync::protocol::NotePath,
    ) -> Option<String> {
        let store = self.inner.doc_store.lock().await;
        if !store.exists(path) {
            return None;
        }
        store.load_or_create(path).ok()?.render().ok()
    }

    /// Human-readable device name chosen at init time. Shown in the peer's
    /// Sync UI and included in mDNS TXT records.
    pub fn display_name(&self) -> &str {
        &self.inner.device_name
    }

    pub fn events(&self) -> broadcast::Receiver<SyncEvent> {
        self.inner.events.subscribe()
    }

    /// Device name as supplied to `SyncEngine::new`. Sibling-visible so
    /// `accept.rs` can include it in the handshake without reaching into
    /// the private `inner` field.
    pub(super) fn device_name(&self) -> &str {
        &self.inner.device_name
    }

    /// Owned `PathBuf` of the roost root. `accept.rs` clones this when
    /// spawning a per-peer task.
    pub(super) fn roost_path_buf(&self) -> PathBuf {
        self.inner.roost.clone()
    }

    /// Clone of the active roost filesystem backend, if any.
    pub(super) fn roost_fs_clone(&self) -> Option<Arc<dyn RoostFs>> {
        self.inner.roost_fs.clone()
    }

    /// Shared `DocStore` handle. Cheap clone (Arc).
    pub(super) fn doc_store_clone(&self) -> Arc<Mutex<DocStore>> {
        self.inner.doc_store.clone()
    }

    /// Shared per-note lock table. Cheap clone (Arc). Handed to every per-peer
    /// task so a live adopt serializes against local saves / queue adopts for
    /// the same note.
    pub(super) fn note_locks_clone(&self) -> Arc<super::note_locks::NoteLocks> {
        self.inner.note_locks.clone()
    }

    /// Sender side of the engine's event broadcast channel. Sibling tasks
    /// emit `SyncEvent`s through this so they reach the frontend listener.
    pub(super) fn events_sender(&self) -> broadcast::Sender<SyncEvent> {
        self.inner.events.clone()
    }

    /// Lock the peer store for the duration of the returned guard. Used by
    /// `accept.rs` (sibling) and `commands::sync` (Tauri layer) to upsert
    /// or amend peer records — e.g., remembering an iroh ticket on
    /// successful pair so the autoresume path can redial without UI.
    pub async fn peer_store_lock(&self) -> tokio::sync::MutexGuard<'_, PeerStore> {
        self.inner.peer_store.lock().await
    }

    /// Send a `SyncEvent` on the engine's broadcast channel. Failures are
    /// swallowed — a dropped channel just means no one's listening, which
    /// is fine for fire-and-forget status updates.
    pub(super) fn send_event(&self, ev: SyncEvent) {
        let _ = self.inner.events.send(ev);
    }

    /// Record a new dial-state for `device` and emit
    /// `PeerStatusChanged`. Always goes through this helper so the
    /// map and the event bus never disagree.
    pub(super) async fn set_peer_state(&self, device: DeviceId, state: PeerConnectionState) {
        // Reconnect bookkeeping: this is the single chokepoint every
        // connection transition (inbound accept + outbound dial) flows
        // through, so backoff/last-error/last-success are tracked here for
        // both directions rather than scattered across the dial paths.
        //
        // Read the previous state BEFORE taking the reconnect lock — status()
        // acquires peer_states then reconnect, so grabbing them in the other
        // order here would risk an ABBA deadlock.
        let was_connected = matches!(
            self.inner.peer_states.read().await.get(&device),
            Some(PeerConnectionState::Connected)
        );
        // L-2: set when this failure is the one that reaches the consecutive-
        // failure cutoff, so we can surface a distinct "gave up" reason exactly
        // once instead of silently abandoning the peer behind an ordinary
        // Failed dot.
        let mut hit_cutoff = false;
        {
            let mut rc = self.inner.reconnect.write().await;
            let entry = rc.entry(device).or_default();
            match &state {
                PeerConnectionState::Connected => {
                    entry.attempts = 0;
                    entry.next_retry_at_millis = 0;
                    entry.last_error = None;
                    entry.last_success_at_millis = Some(now_millis());
                }
                PeerConnectionState::Failed { reason, at_millis } => {
                    // A drop of an ESTABLISHED connection restarts backoff
                    // from the fast end; only consecutive failed *dials*
                    // escalate it. Otherwise a peer that goes away and comes
                    // back (desktop restart, phone roaming networks) inherits
                    // whatever backoff accrued before the last success and
                    // waits out the cap instead of retrying promptly.
                    entry.attempts = if was_connected {
                        1
                    } else {
                        entry.attempts.saturating_add(1)
                    };
                    entry.last_error = Some(reason.clone());
                    entry.next_retry_at_millis =
                        at_millis.saturating_add(jittered_backoff_ms(entry.attempts));
                    // Exactly one failure lands on the cutoff (the supervisor
                    // stops dialing afterward, so attempts can't climb past it
                    // without an intervening reset), so this fires once.
                    hit_cutoff = entry.attempts == MAX_RECONNECT_ATTEMPTS;
                }
                // Dialing / Handshaking / Disconnected don't change backoff.
                _ => {}
            }
        }
        // On the cutoff, replace the emitted reason with an explicit "exhausted"
        // message. The supervisor will now stop auto-dialing until the app is
        // foregrounded or the user taps Reconnect (both reset `attempts`).
        let emit_state = if hit_cutoff {
            if let PeerConnectionState::Failed { at_millis, .. } = &state {
                let at_millis = *at_millis;
                warn!(
                    device = ?device,
                    "reconnect: consecutive-failure cutoff reached — pausing auto-dial for this peer"
                );
                PeerConnectionState::Failed {
                    reason: "reconnect attempts exhausted — will retry when the app is \
                             foregrounded or you tap Reconnect"
                        .into(),
                    at_millis,
                }
            } else {
                state
            }
        } else {
            state
        };
        {
            let mut map = self.inner.peer_states.write().await;
            map.insert(device, emit_state.clone());
        }
        let _ = self.inner.events.send(SyncEvent::PeerStatusChanged {
            device_id: device,
            state: emit_state,
        });
    }

    /// Disconnect and forget a peer. Removes the stored pin + pairing
    /// record; the next reconnect will need a fresh PIN.
    pub async fn unpair(&self, device: DeviceId) -> SyncResult<()> {
        if let Some(handle) = self.inner.peers.write().await.remove(&device) {
            handle.shutdown().await;
        }
        // H-3: before we drop the peer record (and its NodeId), tell the queue
        // server to purge any ciphertext we've pushed for this peer. Without
        // this, messages we queued sit on the server for up to the 30-day TTL
        // and the unpaired device — which still holds the symmetric queue key —
        // could pull and decrypt them. Best-effort: a removed device can still
        // read anything it already pulled, and a queue-less (strict-P2P) setup
        // has nothing to purge, so a failure here never blocks the unpair.
        let peer_node_id = self
            .inner
            .peer_store
            .lock()
            .await
            .get(device)
            .and_then(|rec| rec.peer_node_id);
        if let (Some(recipient), Some(client)) =
            (peer_node_id, self.inner.queue_client.read().await.clone())
        {
            match client.revoke(recipient).await {
                Ok(purged) => {
                    if purged > 0 {
                        info!(purged, "unpair: purged queued messages for revoked peer");
                    }
                }
                Err(e) => warn!(error = %e, "unpair: queue revoke failed (best-effort)"),
            }
        }
        self.inner.peer_store.lock().await.remove(device)?;
        // Purge the in-memory dial bookkeeping too. Without this the unpaired
        // peer lingers as a ghost in `peer_states`/`reconnect` — surfacing a
        // stale dot and retry countdown in the UI, and getting swept up by
        // `force_reconnect_all` on the next foreground.
        self.inner.peer_states.write().await.remove(&device);
        self.inner.reconnect.write().await.remove(&device);
        // Drop any queued deliveries for this peer — without its queue key
        // they could never deliver, so they'd linger as ghost pending work.
        let removed = self
            .inner
            .outbox
            .lock()
            .await
            .remove_peer(device)
            .unwrap_or(0);
        if removed > 0 {
            self.emit_outbox_status().await;
        }
        Ok(())
    }

    /// Snapshot of current state. Cheap — intended for UI polling.
    pub async fn status(&self) -> SyncStatus {
        let listen_port = self
            .inner
            .listen_state
            .lock()
            .await
            .as_ref()
            .map(|s| s.port);
        let peers_guard = self.inner.peers.read().await;
        let connected_ids: std::collections::HashSet<DeviceId> =
            peers_guard.keys().copied().collect();
        let connected: Vec<ConnectedPeer> = peers_guard
            .values()
            .map(|h| ConnectedPeer {
                device_id: h.device_id.as_hex(),
                display_name: h.display_name.clone(),
            })
            .collect();
        drop(peers_guard);
        let states = self.inner.peer_states.read().await;
        let reconnect = self.inner.reconnect.read().await;
        let pending_by_peer: std::collections::HashMap<[u8; 16], u32> = self
            .inner
            .outbox
            .lock()
            .await
            .pending_by_peer()
            .into_iter()
            .collect();
        let total_pending_outbound: u32 = pending_by_peer.values().sum();
        let queue_active = self.inner.queue_client.read().await.is_some();
        let peer_records = self.inner.peer_store.lock().await.list();
        let known: Vec<KnownPeer> = peer_records
            .into_iter()
            .map(|r| {
                let dev = DeviceId(r.device_id);
                let is_connected = connected_ids.contains(&dev);
                let connection_state = if is_connected {
                    PeerConnectionState::Connected
                } else {
                    states
                        .get(&dev)
                        .cloned()
                        .unwrap_or(PeerConnectionState::Disconnected)
                };
                let can_auto_reconnect = r.last_known_addr.is_some() && r.last_known_pin.is_some();
                let rc = reconnect.get(&dev);
                // Only surface a future retry time while actually disconnected
                // and backing off — a connected peer has nothing scheduled.
                let next_retry_millis = rc.and_then(|s| {
                    if !is_connected && s.next_retry_at_millis > 0 {
                        Some(s.next_retry_at_millis)
                    } else {
                        None
                    }
                });
                KnownPeer {
                    device_id: dev.as_hex(),
                    display_name: r.display_name,
                    paired_at_millis: r.paired_at_millis,
                    last_seen_millis: r.last_seen_millis,
                    connection_state,
                    can_auto_reconnect,
                    pending_outbound: pending_by_peer.get(&r.device_id).copied().unwrap_or(0),
                    last_connected_millis: rc.and_then(|s| s.last_success_at_millis),
                    last_error: rc.and_then(|s| s.last_error.clone()),
                    next_retry_millis,
                }
            })
            .collect();
        // Lock the iroh listener state once: it feeds BOTH `running` and
        // `relay_connected`.
        let iroh_guard = self.inner.iroh_listen_state.lock().await;
        let iroh_listening = iroh_guard.is_some();
        // Engine is "running" if we have a TCP listener OR a live iroh listener
        // OR at least one connected peer OR any known paired peer (autodial is
        // eligible). Dialer-only devices never bind a listener but are running.
        // A fresh host that only tapped "pair a device" binds an iroh listener
        // (no TCP port, no peers yet) — without the iroh term it wrongly
        // reported "stopped" while its pairing endpoint was live.
        let running = compute_running(
            listen_port.is_some(),
            iroh_listening,
            !connected_ids.is_empty(),
            !known.is_empty(),
        );
        // PT-1: relay reachability. None when strict-P2P (relay disabled) or no
        // endpoint is bound; otherwise whether any home relay is connected
        // (iroh 1.0 exposes home_relay_status() on the Endpoint).
        let relay_connected = if self
            .inner
            .relay_disabled
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            None
        } else {
            iroh_guard.as_ref().map(|s| {
                use iroh::Watcher as _;
                s.endpoint
                    .home_relay_status()
                    .get()
                    .iter()
                    .any(|r| r.is_connected())
            })
        };
        drop(iroh_guard);
        SyncStatus {
            running,
            listen_port,
            our_device_id: self.identity().device_id().as_hex(),
            our_fingerprint_words: self.inner.tls_identity.fingerprint.as_words(),
            connected_peers: connected,
            known_peers: known,
            queue_active,
            total_pending_outbound,
            relay_connected,
        }
    }

    /// Graceful shutdown: cancel + drain every engine task, tear down all
    /// peers, close the listeners. Leaves the stores on disk intact.
    ///
    /// SYN-I4: `stop()` is the single drain point. It (1) serializes against
    /// bring-up under `lifecycle`, (2) cancels the root token so every
    /// `spawn_tracked` task's `select!` fires, (3) closes+`wait`s the
    /// `TaskTracker` so NO engine task outlives this call. Terminal per
    /// instance — a fresh roost builds a fresh engine with a fresh tracker.
    pub async fn stop(&self) -> SyncResult<()> {
        // Signal live peers to close gracefully (send Bye) BEFORE cancelling —
        // a cancelled peer loop would skip the polite shutdown handshake.
        // `shutdown()` only ENQUEUES the Bye (it doesn't join the loop), and we
        // cancel right after, so a peer still mid-Bye may have its loop dropped
        // and the app-level Bye truncated. That's fine: the iroh QUIC
        // `endpoint.close()` below sends an `ApplicationClose` the peer sees as
        // a clean disconnect, and no live TCP peers exist in the shipped config.
        // Joining the loop to guarantee the Bye would reintroduce a stop() hang
        // on a wedged peer — not worth it.
        let handles: Vec<PeerHandle> = {
            let mut peers = self.inner.peers.write().await;
            peers.drain().map(|(_, h)| h).collect()
        };
        for h in handles {
            h.shutdown().await;
        }

        // SYN-B6: the (set-stopped + cancel) pair is made atomic w.r.t. every
        // bring-up method's (check-stopped + install) by `lifecycle`. Held ONLY
        // across this instant, non-awaiting critical section — deliberately NOT
        // across the drain below. A tracked task that lazily calls a bring-up
        // (dial → `ensure_iroh_endpoint` → `start_listening_iroh`) while we are
        // in `tracker.wait()` must be able to take `lifecycle`, observe
        // `stopped`, and no-op — holding `lifecycle` across `wait()` would
        // deadlock that task against us. `child_token()` derived after this
        // `cancel()` is born already-cancelled, so any post-cancel `spawn_*`
        // self-terminates immediately.
        {
            let _life = self.inner.lifecycle.lock().await;
            self.inner
                .stopped
                .store(true, std::sync::atomic::Ordering::SeqCst);
            // SYN-I4: cancel every child token. Long-lived loops `select!` on it
            // and exit; per-op tasks with long awaits (redial, outbox push,
            // handshake) drop their sleep/network wait.
            self.inner.cancel.cancel();
        }
        // SRX: a stopped engine is not actively pairing — close the first-pair
        // enrollment window so a lingering open window can't admit an inbound
        // first-pair if the engine is somehow re-driven mid-teardown.
        self.close_enrollment_window();

        // Close the QUIC endpoint so the iroh accept loop's `select!` wakes and
        // exits; the TCP accept loop exits on the cancel token above. Take the
        // listener state either way (its task is tracker-owned; the `.abort()`
        // is belt-and-suspenders liveness for any loop not yet cancel-aware).
        if let Some(state) = self.inner.listen_state.lock().await.take() {
            state.task.abort();
        }
        if let Some(state) = self.inner.iroh_listen_state.lock().await.take() {
            state.task.abort();
            // Doesn't wait — we don't want shutdown to block on a slow relay.
            state.endpoint.close().await;
        }

        // Abort the stored long-lived handles as belt-and-suspenders (immediate
        // teardown even if a loop isn't yet cancel-aware) and clear the slots.
        if let Some(task) = self.inner.watcher_task.lock().await.take() {
            task.abort();
        }
        if let Some(task) = self.inner.reconnect_task.lock().await.take() {
            task.abort();
        }
        if let Some(task) = self.inner.queue_pull_task.lock().await.take() {
            task.abort();
        }
        if let Some(task) = self.inner.outbox_drain_task.lock().await.take() {
            task.abort();
        }
        *self.inner.queue_client.write().await = None;
        *self.inner.watcher.lock().await = None;

        // Drain EVERY tracked task with NO lock held — a task that needs a field
        // lock (or `lifecycle`) to unwind would otherwise deadlock against us.
        self.inner.tracker.close();
        self.inner.tracker.wait().await;

        let _ = self.inner.events.send(SyncEvent::Stopped);
        Ok(())
    }

    /// Delay before a watcher-observed removal is confirmed as a real delete
    /// and written into the CRDT as a tombstone.
    ///
    /// Filesystem watchers routinely surface a path as *momentarily gone*
    /// during events that are NOT user deletes: an editor's atomic
    /// `write-temp → rename` save, our own `render_to_roost` rewrite churn, a
    /// `scan_roost` / `recover_missing_files` pass rewriting files on startup,
    /// and — on iOS — the volume re-scan that fires when the app returns from
    /// suspension. Turning any of those directly into `mark_note_deleted`
    /// tombstones a *live* note and, because a tombstone is a CRDT change,
    /// propagates the delete to every paired device (observed: a note
    /// vanishing on BOTH the phone and the desktop after the phone was
    /// reopened mid-sync).
    ///
    /// So a watcher-originated removal is *provisional*: we wait this long,
    /// then re-check that the file is genuinely still gone before tombstoning.
    /// Intentional deletes (the `notify_local_remove` IPC / user "Delete"
    /// action) bypass this entirely and tombstone immediately — the guard is
    /// only for events the engine did not itself originate.
    const REMOVAL_VERIFY_DELAY: std::time::Duration = std::time::Duration::from_millis(1200);

    /// Confirm a watcher-observed removal and tombstone the note **only if the
    /// file is genuinely still missing**. Fail-safe by construction:
    /// - file reappeared (transient churn / our own rewrite) → skip, no delete;
    /// - existence probe errored → skip, no delete (never delete on doubt);
    /// - file really gone → tombstone (the genuine external-delete path).
    ///
    /// Split out from the timing wrapper so the decision is unit-testable
    /// without sleeping.
    pub(super) async fn verify_then_tombstone(&self, note: crate::sync::protocol::NotePath) {
        // A removal scheduled just before a roost switch / shutdown must not
        // fire into a torn-down engine and tombstone into the previous roost's
        // docstore. Match the existing stopped-latch guard used on the dial
        // paths and bail.
        if self.is_stopped() {
            return;
        }
        match self.note_exists_on_disk(&note).await {
            Ok(true) => {
                info!(
                    note = %note,
                    "watcher removal not confirmed — file present after verify delay; skipping tombstone (transient FS event, not a real delete)"
                );
            }
            Ok(false) => {
                self.mark_note_deleted(note).await;
            }
            Err(e) => {
                warn!(
                    note = %note,
                    error = %e,
                    "watcher removal verify: existence check failed — skipping tombstone (fail-safe: never delete on uncertainty)"
                );
            }
        }
    }

    /// Schedule a [`Self::verify_then_tombstone`] after [`Self::REMOVAL_VERIFY_DELAY`].
    /// Spawns so the watcher-intake loop keeps draining. The delayed existence
    /// re-check is self-cancelling: if a `Modified` for the same path lands in
    /// the window and rewrites the file, the probe simply finds it present and
    /// no tombstone is written — no explicit cancellation bookkeeping needed.
    fn schedule_verified_tombstone(&self, note: crate::sync::protocol::NotePath) {
        let engine = self.clone();
        // SYN-I4: tracked + cancel-aware. A pending verify-delay must not
        // outlive `stop()` (it would fire `verify_then_tombstone` into a
        // torn-down engine, or hold `tracker.wait()` for the full 1.2s delay).
        self.spawn_tracked(move |token| async move {
            tokio::select! {
                _ = token.cancelled() => {} // engine stopping — drop the pending tombstone
                _ = tokio::time::sleep(Self::REMOVAL_VERIFY_DELAY) => {
                    engine.verify_then_tombstone(note).await;
                }
            }
        });
    }

    /// Start the roost watcher. Local markdown edits get fanned-out to
    /// every currently-connected peer. Idempotent.
    pub async fn start_watcher(&self) -> SyncResult<()> {
        // SYN-B6: serialize against stop() — don't install a watcher + scan
        // task into an engine that is being torn down.
        let _life = self.inner.lifecycle.lock().await;
        if self.is_stopped() {
            return Ok(());
        }
        let mut guard = self.inner.watcher.lock().await;
        if guard.is_some() {
            return Ok(());
        }
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<LocalChange>();

        // Soft-start: on Android SAF roosts (`content://...`) the native
        // `notify` crate can't install a recursive watch — its inotify call
        // returns ENOENT because there's no real filesystem path to watch.
        // That's non-fatal for sync: we can still use `scan_roost` + manual
        // "Rescan" to push the current state to peers, and remote edits
        // still flow in via Automerge. Log a warning and continue with no
        // live watcher instead of refusing to start the engine.
        match RoostWatcher::start(self.inner.roost.clone(), tx) {
            Ok(watcher) => {
                *guard = Some(watcher);
                info!(roost = %self.inner.roost.display(), "watcher: started");
            }
            Err(e) => {
                warn!(roost = %self.inner.roost.display(), error = %e, "watcher: unavailable on this roost (likely SAF/content URI) — falling back to manual Rescan only");
            }
        }
        drop(guard);

        // Seed the DocStore with every markdown file that exists on disk
        // right now. Without this, the watcher only sees *future* changes
        // and `announce_notes` sends an empty list to freshly-paired peers
        // — they have nothing to request, so pre-existing notes never
        // bootstrap onto a new device.
        //
        // Run the scan in the background so `sync_init` can return fast.
        // For a 200-note roost this scan takes a couple seconds; we don't
        // want the UI to freeze while it runs. Progress + completion flow
        // through the `SyncEvent` broadcast channel so the frontend can
        // render a banner.
        //
        // After the scan, also run `recover_missing_files` so any docs the
        // store knows about that don't exist on disk get rendered back. On
        // SAF roosts `scan_roost` is intentionally a no-op (CRDT divergence
        // protection), but that means a peer-adopted FullDoc never lands
        // on the user's filesystem until they manually open the note —
        // recovery covers that gap.
        let engine = self.clone();
        // SYN-I4/B7: the startup bootstrap was the cited detached-task leak — a
        // heavy scan+recover that `stop()` couldn't cancel, so a roost switch
        // raced it (scanning/writing into a torn-down engine). Now tracked and
        // cancel-aware at each phase boundary: a cancel drops the in-flight
        // phase (idempotent — re-runs on next start) and skips the rest.
        self.spawn_tracked(move |token| async move {
            tokio::select! {
                _ = token.cancelled() => return,
                r = engine.scan_roost() => {
                    if let Err(e) = r {
                        warn!(error = %e, "initial roost scan failed — edits will still sync, but existing notes may not bootstrap on a new peer");
                    }
                }
            }
            if token.is_cancelled() {
                return;
            }
            // Cancel-wrap recover too (not just a phase-boundary check) so a
            // stop()/roost-switch mid-recover on a large roost doesn't stall
            // `tracker.wait()` for a full recover pass. Idempotent — a dropped
            // recover re-runs on next start.
            tokio::select! {
                _ = token.cancelled() => return,
                r = engine.recover_missing_files() => match r {
                    Ok(0) => {}
                    Ok(n) => info!(
                        restored = n,
                        "recover_missing_files: rendered docstore-only notes back to disk"
                    ),
                    Err(e) => {
                        warn!(error = %e, "recover_missing_files failed — roost and docstore may be out of sync")
                    }
                }
            }
            if token.is_cancelled() {
                return;
            }
            // Reap note tombstones past the 30-day TTL so deleted-note CRDT
            // docs don't accumulate forever. Bounded/quick — the pre-check above
            // is enough; no need to cancel-wrap.
            engine.reap_old_tombstones().await;
        });

        let peers = self.inner.peers.clone();
        let events = self.inner.events.clone();
        let engine = self.clone();
        // SYN-I4: tracked + cancel-aware. Exits on `stop()` (cancel) OR when
        // the watcher's sender is dropped (rx closed). Stored handle kept for
        // belt-and-suspenders abort in `stop()`.
        let task = self.spawn_tracked(move |token| async move {
            loop {
                let change = tokio::select! {
                    _ = token.cancelled() => break,
                    c = rx.recv() => match c {
                        Some(c) => c,
                        None => break, // watcher sender dropped
                    },
                };
                // An external removal (Finder/Obsidian/agent shell deleting a
                // file) must become a CRDT tombstone, not a destructive drop.
                // Route it through the same chokepoint as the IPC delete so
                // the docstore tombstone is written, live peers are notified,
                // and offline peers get it via the queue.
                if let LocalChange::Removed { path } = &change {
                    // Watcher-originated: provisional. A transient "file gone"
                    // event (rename churn, our own rewrite, iOS resume rescan)
                    // must not become a propagated tombstone — verify the file
                    // is really gone after a short delay first.
                    engine.schedule_verified_tombstone(path.clone());
                    continue;
                }
                // External folder delete (Finder/Explorer/agent shell removing
                // a directory): the OS may fire only this single prefix event
                // and never the per-file removes. Expand it against the
                // docstore — the files are already gone from disk, so the
                // docstore is the only record of what was inside — and route
                // each nested note through the same delete chokepoint.
                if let LocalChange::RemovedPrefix { prefix } = &change {
                    let under = {
                        let store = engine.inner.doc_store.lock().await;
                        match store.list() {
                            Ok(all) => paths_under_prefix(all, prefix),
                            Err(e) => {
                                warn!(error = %e, prefix = %prefix, "watcher: docstore list failed for prefix removal — nested notes may not tombstone");
                                Vec::new()
                            }
                        }
                    };
                    if !under.is_empty() {
                        info!(prefix = %prefix, count = under.len(), "watcher: folder removed — tombstoning nested notes");
                    }
                    for note in under {
                        // Same provisional treatment as a single-file removal:
                        // a transient prefix event (e.g. iOS resume) must not
                        // tombstone every note under it. Each note is
                        // re-verified independently after the delay.
                        engine.schedule_verified_tombstone(note);
                    }
                    continue;
                }
                // Single-writer: persist the edit to the docstore HERE,
                // before fan-out. Previously each connected peer's
                // `handle_local` applied it, which meant an edit made with
                // zero peers connected never reached the docstore (so the
                // queue push would have shipped stale content) and the
                // offline queue path was never invoked from the watcher at
                // all. Centralizing the write makes the docstore
                // authoritative on every edit — the foundation the durable
                // outbox regenerates payloads from.
                let note_path = match &change {
                    LocalChange::Modified { path, .. }
                    | LocalChange::ModifiedWithContent { path, .. } => path.clone(),
                    // Removal variants were handled + `continue`d above.
                    LocalChange::Removed { .. } | LocalChange::RemovedPrefix { .. } => continue,
                };
                match engine.apply_local_change(&change).await {
                    Ok(true) => {}
                    // Unchanged content (apply_markdown short-circuited) —
                    // nothing to propagate. Matches the old per-peer
                    // short-circuit behaviour.
                    Ok(false) => continue,
                    Err(e) => {
                        warn!(note = %note_path, error = %e, "watcher: docstore apply failed");
                        continue;
                    }
                }

                // Tell every connected peer about the change so it propagates.
                let snapshot: Vec<PeerHandle> =
                    peers.read().await.values().map(clone_handle).collect();
                for handle in snapshot {
                    if let Err(e) = handle.send_local(change.clone()) {
                        debug!(peer = %handle.display_name, error = %e, "peer channel closed/full, queueing for retry");
                        // Live-drop coverage. Record BOTH intents: the outbox
                        // (offline / queue path, no-op without a queue client)
                        // and the always-on live-pending set (redelivered over
                        // the control channel by the reconnect supervisor even
                        // in strict-P2P / no-queue mode).
                        engine
                            .enqueue_outbox_for_peer(handle.device_id, note_path.clone())
                            .await;
                        engine
                            .record_live_pending(handle.device_id, note_path.clone())
                            .await;
                    }
                }

                // Push to every known-but-offline peer via the durable
                // outbox (the watcher path never did this before).
                engine.queue_push_to_offline_peers(note_path.clone()).await;

                // Surface modifications to the frontend via `SyncEvent::NoteWritten`.
                // Without this, an external edit (vim / Obsidian / agent shell
                // dropping a `- [ ]` line into a markdown file) never reaches the
                // graph indexer, so derived state — backlinks, tags, the Tasks
                // pane — drifts behind the actual disk content. The frontend's
                // existing `sync://event` listener picks up `NoteWritten` and
                // calls `graphStore.indexNote(path, content)` for both local
                // watcher-driven and peer-sync-driven writes. Phase 2.1 slice 7.
                let path_str = match &change {
                    LocalChange::Modified { abs, .. } => Some(abs.to_string_lossy().into_owned()),
                    LocalChange::ModifiedWithContent { path, .. } => Some(path.0.clone()),
                    // Both removal kinds are handled+`continue`d above; arms
                    // kept for exhaustiveness.
                    LocalChange::Removed { .. } | LocalChange::RemovedPrefix { .. } => None,
                };
                if let Some(p) = path_str {
                    let _ = events.send(SyncEvent::NoteWritten { path: p });
                }
            }
        });
        *self.inner.watcher_task.lock().await = Some(task);
        Ok(())
    }

    pub fn identity(&self) -> &Identity {
        &self.inner.identity
    }

    #[allow(dead_code)] // exposed for future re-pairing UI; not wired today
    pub fn our_fingerprint(&self) -> Fingerprint {
        self.inner.tls_identity.fingerprint
    }

    /// True once `stop()` has run. Dial/accept paths check this before doing
    /// work that could resurrect a torn-down engine (see `stopped`).
    pub(super) fn is_stopped(&self) -> bool {
        self.inner.stopped.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Set strict-P2P (no relay) mode. Call before the iroh listener binds so
    /// `start_listening_iroh` picks the right `RelayMode`.
    pub fn set_relay_disabled(&self, disabled: bool) {
        self.inner
            .relay_disabled
            .store(disabled, std::sync::atomic::Ordering::SeqCst);
    }

    pub(super) async fn register_peer(
        &self,
        mut handle: PeerHandle,
        task: tokio::task::JoinHandle<SyncResult<()>>,
    ) {
        // A dial that completed just after stop() (e.g. roost switch) must not
        // re-insert a peer into a torn-down engine — that would keep syncing
        // into the old roost. Drop the fresh connection instead.
        if self.is_stopped() {
            handle.shutdown().await;
            task.abort();
            return;
        }
        let device = handle.device_id;
        // Stamp this registration with a unique generation before it goes
        // into the map. The cleanup task below uses it to tell "I'm still
        // the current connection" from "a reconnect replaced me".
        let generation = self
            .inner
            .peer_gen
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        handle.generation = generation;
        let display_name = handle.display_name.clone();
        self.inner.peers.write().await.insert(device, handle);
        // A fresh connection re-syncs shared paths via its `NoteList` exchange,
        // so any live-pending redelivery intents for this device are now
        // redundant. Drop them so they can't accumulate across redials.
        self.inner.live_pending.lock().await.remove(&device);
        // Drive the peer task to completion in the background, then remove
        // its handle from the peers map. Previously we detached and dropped
        // the JoinHandle, which meant a dropped transport left a stale
        // handle in the map — status() kept reporting the peer as live and
        // auto-reconnect never fired. The sync loop itself emits the
        // PeerDisconnected event before returning; this task just owns the
        // map cleanup.
        let peers = Arc::clone(&self.inner.peers);
        let engine = self.clone();
        // SYN-I4: tracked + cancel-aware. On `stop()` (cancel) the peers map is
        // already drained, so skip cleanup and let the tracker drain this task;
        // otherwise await the peer loop's exit and reconcile the map.
        self.spawn_tracked(move |token| async move {
            let err_reason = tokio::select! {
                _ = token.cancelled() => return,
                r = task => match r {
                    Ok(Ok(())) => None,
                    Ok(Err(e)) => {
                        warn!(peer = %display_name, error = %e, "peer task exited with error");
                        Some(e.to_string())
                    }
                    Err(e) => {
                        warn!(peer = %display_name, error = %e, "peer task panicked");
                        Some(format!("panic: {e}"))
                    }
                },
            };
            // Only clean up if we're STILL the connection registered for this
            // device. A reconnect that landed while this task was unwinding
            // will have inserted a newer handle (higher generation); removing
            // it here — or overwriting its Connected state with Failed —
            // would tear down a live session and flap the UI straight back to
            // "disconnected" after a successful reconnect. If superseded, the
            // newer registration owns all cleanup, so we bail untouched.
            {
                let mut guard = peers.write().await;
                match guard.get(&device) {
                    Some(h) if h.generation == generation => {
                        guard.remove(&device);
                    }
                    _ => return,
                }
            }
            let new_state = match err_reason {
                Some(reason) => PeerConnectionState::Failed {
                    reason,
                    at_millis: now_millis(),
                },
                None => PeerConnectionState::Disconnected,
            };
            engine.set_peer_state(device, new_state).await;
        });
    }
}

/// Tracked note paths that fall under a removed directory `prefix`. Matching
/// is segment-aware: prefix `projects` matches `projects/a.md` (and the exact
/// `projects` itself, defensively) but NOT the sibling `projects-archive/c.md`.
/// Input order is preserved. Used by the watcher-intake loop to expand an
/// external folder delete into one tombstone per nested note.
fn paths_under_prefix(
    all: Vec<crate::sync::protocol::NotePath>,
    prefix: &crate::sync::protocol::NotePath,
) -> Vec<crate::sync::protocol::NotePath> {
    let base = prefix.0.trim_end_matches('/');
    let with_slash = format!("{base}/");
    all.into_iter()
        .filter(|p| p.0 == base || p.0.starts_with(&with_slash))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::crypto::pairing::Pin;
    use crate::sync::protocol::NotePath;

    #[test]
    fn paths_under_prefix_is_segment_aware() {
        let all = vec![
            NotePath("projects/a.md".into()),
            NotePath("projects/sub/b.md".into()),
            NotePath("projects".into()), // exact match (defensive)
            NotePath("projects-archive/c.md".into()), // sibling — must NOT match
            NotePath("other/d.md".into()),
        ];
        let got = paths_under_prefix(all, &NotePath("projects".into()));
        let strs: Vec<&str> = got.iter().map(|p| p.0.as_str()).collect();
        assert_eq!(
            strs,
            vec!["projects/a.md", "projects/sub/b.md", "projects"],
            "prefix match must be segment-aware and not catch sibling folders"
        );
    }

    use std::net::SocketAddr;
    use tempfile::tempdir;

    fn setup_engine() -> (SyncEngine, tempfile::TempDir, tempfile::TempDir) {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let roost = tempdir().unwrap();
        // Place the actual roost in a fixed-name subdir so both peers in the
        // two-engine test produce the same handshake roost_name. Tempdir
        // basenames are random, which would otherwise trip the
        // roost-name-must-match guard.
        let roost_root = roost.path().join("test-roost");
        std::fs::create_dir_all(&roost_root).unwrap();
        let app_data = tempdir().unwrap();
        let identity = Arc::new(Identity::load_or_create(app_data.path()).unwrap());
        let tls = Arc::new(TlsIdentity::generate().unwrap());
        // Sync state in its own subdir — mirrors what the Tauri command
        // resolves via `paths::roost_db_dir(app, roost).join("sync")`.
        let state_dir = app_data.path().join("sync-state");
        let engine = SyncEngine::new(
            roost_root,
            state_dir,
            None, // tests use the std::fs fallback in render_to_roost
            identity,
            tls,
            "test-device".into(),
        )
        .unwrap();
        (engine, roost, app_data)
    }

    /// Poll until `engine.outbox_pending_total()` reaches `expected`, or panic
    /// after ~5s. Replaces fixed `sleep(400ms)` + assert pairs: the outbox
    /// intent is recorded by a *spawned* delivery task, so a flat sleep raced
    /// under CI load (the task hadn't recorded yet → asserted 0 instead of 1).
    async fn wait_for_outbox_total(engine: &SyncEngine, expected: u32) {
        for _ in 0..600 {
            if engine.outbox_pending_total().await == expected {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        panic!(
            "outbox_pending_total never reached {expected} (last: {})",
            engine.outbox_pending_total().await
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn engine_starts_and_stops() {
        let (engine, _v, _d) = setup_engine();
        let pin = Pin::parse("123456").unwrap();
        let port = engine.start_listening(0, pin).await.unwrap();
        assert!(port > 0);
        let status = engine.status().await;
        assert!(status.running);
        assert_eq!(status.listen_port, Some(port));
        engine.stop().await.unwrap();
        let status = engine.status().await;
        assert!(!status.running);
    }

    /// LAN reconnect fix: feed_lan_endpoint must make the fed EndpointId
    /// resolvable via the engine MemoryLookup -- this is the exact path
    /// spawn_discovery_bridge drives on every mDNS Found event carrying
    /// an iroh node_id, and what build_endpoint address_lookup wiring
    /// depends on for the iroh dialer to actually use it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn feed_lan_endpoint_is_resolvable_via_memory_lookup() {
        let (engine, _v, _d) = setup_engine();
        let node_id = iroh::SecretKey::from_bytes(&[9u8; 32]).public();
        let addr_a: SocketAddr = "192.168.1.20:11223".parse().unwrap();
        let addr_b: SocketAddr = "192.168.1.20:11224".parse().unwrap();

        engine.feed_lan_endpoint(node_id, [addr_a, addr_b]);

        let info = engine
            .lan_lookup()
            .get_endpoint_info(node_id)
            .expect("fed endpoint must be resolvable from the engine MemoryLookup");
        let resolved = info.into_endpoint_addr();
        let ips: std::collections::HashSet<SocketAddr> = resolved.ip_addrs().copied().collect();
        assert!(ips.contains(&addr_a));
        assert!(ips.contains(&addr_b));
    }

    /// The mDNS discovery bridge only feeds PAIRED peers into the LAN
    /// address lookup (the security gate that bounds the table to real
    /// pairings). `is_paired` is that gate's predicate — it must reflect
    /// `peer_store` membership exactly.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn is_paired_reflects_peer_store_membership() {
        let (engine, _v, _d) = setup_engine();
        let dev = DeviceId([0x77; 16]);
        assert!(
            !engine.is_paired(dev).await,
            "an unknown device must not be treated as paired"
        );
        engine
            .inner
            .peer_store
            .lock()
            .await
            .ensure_peer(dev, [0u8; 32], "Test Peer".into())
            .expect("ensure_peer");
        assert!(
            engine.is_paired(dev).await,
            "device must be paired once it is in the peer store"
        );
    }

    /// Single-writer regression: a local write must land in the docstore
    /// even when **no peers are connected**. Before the durable-outbox work
    /// the docstore was only written as a side effect of a connected peer's
    /// `handle_local`, so an offline edit never persisted — and the later
    /// queue push would have shipped stale (or empty) content. This locks in
    /// that `notify_local_write` is now the single writer.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn local_write_persists_to_docstore_with_no_peers() {
        let (a, _v, _d) = setup_engine();
        // No peers connected, no queue client installed.
        a.notify_local_write(
            NotePath("solo.md".into()),
            "written while fully offline".into(),
        )
        .await;

        let store = a.inner.doc_store.lock().await;
        let doc = store.load_or_create(&NotePath("solo.md".into())).unwrap();
        assert_eq!(
            doc.render().unwrap(),
            "written while fully offline",
            "the edit must be in the docstore despite zero connected peers"
        );
    }

    /// L-6: `mark_note_deleted` must honor the roost-switch `stopped` latch,
    /// like the other write paths. A delete dequeued just as the user switches
    /// roosts must NOT write a tombstone into the (now old) DocStore.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mark_note_deleted_bails_when_stopped() {
        let (a, _v, _d) = setup_engine();
        let note = NotePath("keep.md".into());
        a.notify_local_write(note.clone(), "content".into()).await;

        // Simulate a roost switch having latched `stopped`.
        a.inner
            .stopped
            .store(true, std::sync::atomic::Ordering::SeqCst);
        a.mark_note_deleted(note.clone()).await;

        let store = a.inner.doc_store.lock().await;
        let doc = store.load_or_create(&note).unwrap();
        assert!(
            !doc.is_deleted().unwrap(),
            "a delete must not land after the engine is stopped (L-6)"
        );
    }

    /// L-2: reaching the consecutive-failure cutoff must surface a distinct
    /// "exhausted" reason (once) instead of silently abandoning the peer, and
    /// the ordinary failures before it keep their own reason.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reconnect_cutoff_emits_exhausted_reason() {
        let (a, _v, _d) = setup_engine();
        let dev = DeviceId([0x5A; 16]);
        let mut rx = a.inner.events.subscribe();

        // Drive MAX_RECONNECT_ATTEMPTS consecutive dial failures.
        for i in 0..MAX_RECONNECT_ATTEMPTS {
            a.set_peer_state(
                dev,
                PeerConnectionState::Failed {
                    reason: format!("dial failed #{i}"),
                    at_millis: now_millis(),
                },
            )
            .await;
        }

        // Collect the reasons the UI would have seen.
        let mut reasons = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            if let SyncEvent::PeerStatusChanged {
                device_id,
                state: PeerConnectionState::Failed { reason, .. },
            } = ev
            {
                if device_id == dev {
                    reasons.push(reason);
                }
            }
        }
        let exhausted: Vec<_> = reasons
            .iter()
            .filter(|r| r.contains("attempts exhausted"))
            .collect();
        assert_eq!(
            exhausted.len(),
            1,
            "the cutoff must surface exactly one 'exhausted' signal, got {reasons:?}"
        );
        // The failures before the cutoff kept their own reasons.
        assert!(reasons.iter().any(|r| r.contains("dial failed #0")));
    }

    /// A failed offline delivery must persist a durable outbox intent rather
    /// than being dropped. Uses a queue client pointed at a closed port so
    /// every push fails; asserts the (peer, note) intent survives with a
    /// pending count of 1.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn outbox_persists_failed_offline_delivery() {
        use crate::sync::crdt::NoteDoc;
        use crate::sync::queue::client::QueueClient;
        use crate::sync::store::peer_store::PeerRecord;

        let (a, _v, _d) = setup_engine();

        // Register a known peer "B" with queue identity but never connect to
        // it (so it counts as offline for the fan-out).
        let b_device = DeviceId([0xBB; 16]);
        {
            let mut ps = a.inner.peer_store.lock().await;
            ps.upsert(PeerRecord::new(b_device, [0xBB; 32], "phone".into()))
                .unwrap();
            ps.remember_queue_pair(b_device, Some([0xCD; 32]), Some([0xEF; 32]))
                .unwrap();
        }

        // Queue client pointed at a closed port → every push fails fast.
        let dead = Arc::new(QueueClient::new("http://127.0.0.1:1", a.identity_arc()).unwrap());
        a.set_queue_client(Some(dead)).await;

        // Seed the note (what the single-writer path would have done).
        {
            let store = a.inner.doc_store.lock().await;
            let mut doc = NoteDoc::new(a.identity().device_id()).unwrap();
            doc.apply_markdown("offline edit").unwrap();
            store.save(&NotePath("o.md".into()), &mut doc).unwrap();
        }

        // Offline fan-out → write-ahead enqueue, push fails, intent persists.
        a.queue_push_to_offline_peers(NotePath("o.md".into())).await;
        // Delivery is spawned; wait (polling) for it to fail + record the
        // durable intent, rather than a flat sleep that raced under CI load.
        wait_for_outbox_total(&a, 1).await;
        assert_eq!(a.outbox_pending_by_peer().await, vec![(b_device, 1)]);
        a.stop().await.unwrap();
    }

    /// Register an offline peer with queue identity, install a dead queue
    /// client, seed a note, and fan it out so a failed-delivery intent lands
    /// in the outbox. Returns the engine + the peer device id. Shared setup
    /// for the "stale intents get cleared" tests below.
    async fn engine_with_one_stuck_intent() -> (SyncEngine, tempfile::TempDir, DeviceId) {
        use crate::sync::crdt::NoteDoc;
        use crate::sync::queue::client::QueueClient;
        use crate::sync::store::peer_store::PeerRecord;

        let (a, v, _d) = setup_engine();
        let b_device = DeviceId([0xBB; 16]);
        {
            let mut ps = a.inner.peer_store.lock().await;
            ps.upsert(PeerRecord::new(b_device, [0xBB; 32], "phone".into()))
                .unwrap();
            ps.remember_queue_pair(b_device, Some([0xCD; 32]), Some([0xEF; 32]))
                .unwrap();
        }
        let dead = Arc::new(QueueClient::new("http://127.0.0.1:1", a.identity_arc()).unwrap());
        a.set_queue_client(Some(dead)).await;
        {
            let store = a.inner.doc_store.lock().await;
            let mut doc = NoteDoc::new(a.identity().device_id()).unwrap();
            doc.apply_markdown("offline edit").unwrap();
            store.save(&NotePath("o.md".into()), &mut doc).unwrap();
        }
        a.queue_push_to_offline_peers(NotePath("o.md".into())).await;
        wait_for_outbox_total(&a, 1).await; // precondition: one stuck intent
        (a, v, b_device)
    }

    /// Unpairing a peer must drop its queued deliveries — they can never
    /// deliver without the peer's queue key, so they'd be ghost pending work.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn outbox_cleared_on_unpair() {
        let (a, _v, b_device) = engine_with_one_stuck_intent().await;
        a.unpair(b_device).await.unwrap();
        assert_eq!(
            a.outbox_pending_total().await,
            0,
            "unpair must clear the peer's outbox intents"
        );
        a.stop().await.unwrap();
    }

    /// Wiping the docstore must clear the outbox — the canonical docs the
    /// intents would regenerate payloads from are gone.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn outbox_cleared_on_reset_docstore() {
        let (a, _v, _b) = engine_with_one_stuck_intent().await;
        a.reset_docstore().await.unwrap();
        assert_eq!(
            a.outbox_pending_total().await,
            0,
            "reset_docstore must clear stale outbox intents"
        );
        a.stop().await.unwrap();
    }

    #[test]
    fn reconnect_backoff_is_monotonic_and_capped() {
        assert_eq!(reconnect_backoff_ms(0), 15_000);
        assert_eq!(reconnect_backoff_ms(1), 15_000);
        assert!(reconnect_backoff_ms(2) >= reconnect_backoff_ms(1));
        assert!(reconnect_backoff_ms(3) >= reconnect_backoff_ms(2));
        assert!(reconnect_backoff_ms(4) >= reconnect_backoff_ms(3));
        // Fast tier (≤8 attempts, first few minutes) stays at ≤60s so a user
        // watching both devices sees a prompt reconnect after a brief blip.
        assert_eq!(reconnect_backoff_ms(8), 60_000);
        // L-2: sustained outages escalate to a 5m cap — stop hammering the
        // relay every minute for hours while still riding the outage out to
        // the MAX_RECONNECT_ATTEMPTS cutoff.
        assert_eq!(reconnect_backoff_ms(9), 300_000);
        assert_eq!(reconnect_backoff_ms(99), 300_000);
        assert_eq!(reconnect_backoff_ms(1000), 300_000);
    }

    #[test]
    fn reconnect_cutoff_stops_after_max_attempts() {
        let now = 1_000_000u64;
        // Under the cutoff and past the backoff window → attempt.
        assert!(!should_skip_reconnect(0, 0, now));
        assert!(!should_skip_reconnect(
            MAX_RECONNECT_ATTEMPTS - 1,
            now - 1,
            now
        ));
        // Backoff window not elapsed → skip.
        assert!(should_skip_reconnect(1, now + 1, now));
        // At/over the cutoff → skip even when otherwise due (dead peer).
        assert!(should_skip_reconnect(MAX_RECONNECT_ATTEMPTS, 0, now));
        assert!(should_skip_reconnect(MAX_RECONNECT_ATTEMPTS + 5, 0, now));
    }

    #[test]
    fn jittered_backoff_stays_in_band_and_varies() {
        let mut seen = std::collections::HashSet::new();
        for attempts in [0u32, 1, 2, 3, 99] {
            let base = reconnect_backoff_ms(attempts);
            let lo = base - base / 4;
            let hi = base + base / 4;
            for _ in 0..256 {
                let v = jittered_backoff_ms(attempts);
                assert!(
                    (lo..=hi).contains(&v),
                    "jitter out of band for attempts={attempts}: {v} not in {lo}..={hi}"
                );
                seen.insert(v);
            }
        }
        // Deterministic backoff would collapse to 5 distinct values; jitter
        // must produce a genuine spread so simultaneous drops de-synchronise.
        assert!(
            seen.len() > 50,
            "expected jitter to spread values, got {} distinct",
            seen.len()
        );
    }

    /// `set_peer_state` is the single chokepoint that feeds reconnect
    /// bookkeeping for both inbound and outbound transitions: a failure
    /// bumps the backoff + records the error; a success resets it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn set_peer_state_tracks_backoff_and_success() {
        let (a, _v, _d) = setup_engine();
        let dev = DeviceId([0x42; 16]);

        a.set_peer_state(
            dev,
            PeerConnectionState::Failed {
                reason: "boom".into(),
                at_millis: now_millis(),
            },
        )
        .await;
        {
            let rc = a.inner.reconnect.read().await;
            let s = rc.get(&dev).expect("entry created on failure");
            assert_eq!(s.attempts, 1);
            assert!(s.next_retry_at_millis > now_millis(), "backoff scheduled");
            assert_eq!(s.last_error.as_deref(), Some("boom"));
            assert!(s.last_success_at_millis.is_none());
        }

        a.set_peer_state(dev, PeerConnectionState::Connected).await;
        {
            let rc = a.inner.reconnect.read().await;
            let s = rc.get(&dev).unwrap();
            assert_eq!(s.attempts, 0, "success resets attempts");
            assert_eq!(
                s.next_retry_at_millis, 0,
                "no retry scheduled when connected"
            );
            assert!(s.last_error.is_none(), "error cleared on success");
            assert!(s.last_success_at_millis.is_some());
        }
        a.stop().await.unwrap();
    }

    /// Status exposes the queue-active flag, the outbox depth, and the
    /// per-peer reconnect fields (#4 richer status).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn status_surfaces_queue_outbox_and_reconnect_fields() {
        use crate::sync::store::peer_store::PeerRecord;
        let (a, _v, _d) = setup_engine();

        // No queue client + empty outbox.
        let st = a.status().await;
        assert!(!st.queue_active);
        assert_eq!(st.total_pending_outbound, 0);

        // A known peer whose last reconnect failed surfaces last_error +
        // a future next_retry, and (no outbox) zero pending / never synced.
        let dev = DeviceId([0x55; 16]);
        a.inner
            .peer_store
            .lock()
            .await
            .upsert(PeerRecord::new(dev, [0x55; 32], "phone".into()))
            .unwrap();
        a.set_peer_state(
            dev,
            PeerConnectionState::Failed {
                reason: "unreachable".into(),
                at_millis: now_millis(),
            },
        )
        .await;

        let st = a.status().await;
        let kp = st
            .known_peers
            .iter()
            .find(|k| k.device_id == dev.as_hex())
            .expect("known peer present");
        assert_eq!(kp.last_error.as_deref(), Some("unreachable"));
        assert!(kp.next_retry_millis.is_some(), "retry time surfaced");
        assert_eq!(kp.pending_outbound, 0);
        assert!(kp.last_connected_millis.is_none());
        a.stop().await.unwrap();
    }

    /// Long-offline semantics: a tombstone aged past the 30-day queue TTL
    /// but inside the 180-day retention window must SURVIVE the reap — so a
    /// device returning after, say, 60 days still learns of the delete via
    /// `NoteList` + Automerge merge rather than resurrecting the note. Only
    /// past the retention window is it reaped. Regression guard for the
    /// "offline 45 days → deletes resurrect" surprise.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tombstone_survives_within_retention_reaped_past_it() {
        use crate::sync::crdt::NoteDoc;
        let (a, _v, _d) = setup_engine();
        let now = now_millis();
        let day = 24 * 60 * 60 * 1000u64;
        let dev = a.identity().device_id();
        {
            let store = a.inner.doc_store.lock().await;
            // Deleted 60 days ago — older than the queue TTL, well inside
            // the 180-day retention.
            let mut recent = NoteDoc::new(dev).unwrap();
            recent.apply_markdown("recently deleted").unwrap();
            recent.mark_deleted(now - 60 * day).unwrap();
            store
                .save(&NotePath("recent.md".into()), &mut recent)
                .unwrap();
            // Deleted 200 days ago — past retention.
            let mut ancient = NoteDoc::new(dev).unwrap();
            ancient.apply_markdown("ancient").unwrap();
            ancient.mark_deleted(now - 200 * day).unwrap();
            store
                .save(&NotePath("ancient.md".into()), &mut ancient)
                .unwrap();
        }

        a.reap_old_tombstones().await;

        let store = a.inner.doc_store.lock().await;
        assert!(
            store.exists(&NotePath("recent.md".into())),
            "60-day tombstone must survive (within 180-day retention)"
        );
        assert!(
            !store.exists(&NotePath("ancient.md".into())),
            "200-day tombstone must be reaped (past retention)"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn two_engines_pair_and_sync_over_tcp() {
        // Full end-to-end: spin up two engines on loopback, pair them, seed
        // one with a note, confirm the other receives it via FullDoc.
        let (a, a_roost, _a_app) = setup_engine();
        let (b, b_roost, _b_app) = setup_engine();

        let pin = Pin::parse("424242").unwrap();
        let b_port = b.start_listening(0, pin.clone()).await.unwrap();
        // SRX: B is the accepting side for this FIRST pair — open its enrollment
        // window (the "user is actively pairing" signal the IPC command sets in
        // production) so the first-pair handshake is admitted.
        b.open_enrollment_window();

        // Seed A's store with a note.
        {
            use crate::sync::crdt::NoteDoc;
            use crate::sync::protocol::NotePath;
            let a_store = a.inner.doc_store.clone();
            let store = a_store.lock().await;
            let mut doc = NoteDoc::new(a.identity().device_id()).unwrap();
            doc.apply_markdown("payload").unwrap();
            store.save(&NotePath("x.md".into()), &mut doc).unwrap();
        }

        // A connects to B. AUDIT-003b: the dial requires a pinned
        // fingerprint at the type level — no TOFU path anymore. In
        // the prior version this test passed `None`, which is exactly
        // the codepath the audit closed (it routed through
        // `PinnedVerifier::new(None)` and accepted any cert). Today
        // the test seeds B's actual fingerprint to mirror what
        // `peer_store` would carry on every reconnect after a real
        // first-pair (which now happens through iroh).
        let addr: SocketAddr = format!("127.0.0.1:{b_port}").parse().unwrap();
        let b_fp = b.our_fingerprint();
        let _peer_id = a.connect_peer(addr, pin, b_fp).await.unwrap();

        // Poll (up to ~5s) for B to receive + render the note, rather than a
        // single fixed sleep. The full pair → NoteList → RequestFullDoc →
        // FullDoc → render round-trip over real loopback TCP can exceed a flat
        // 500ms under CI load, which made this assert flaky (saw empty content
        // in CI). Polling passes as soon as sync completes and only fails if
        // it genuinely never arrives.
        use crate::sync::protocol::NotePath;
        let b_store = b.inner.doc_store.clone();
        let mut rendered = String::new();
        for _ in 0..600 {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            let store = b_store.lock().await;
            if let Ok(doc) = store.load_or_create(&NotePath("x.md".into())) {
                if let Ok(text) = doc.render() {
                    if text == "payload" {
                        rendered = text;
                        break;
                    }
                }
            }
        }
        assert_eq!(rendered, "payload", "B did not receive the note within ~5s");

        // Clean up so the tokio runtime can reclaim tasks.
        a.stop().await.unwrap();
        b.stop().await.unwrap();
        // Tempdirs persist for the duration of the test; leaving them.
        drop(a_roost);
        drop(b_roost);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn scan_roost_adopts_markdown_and_skips_the_rest() {
        // Bootstrapping pre-existing notes into sync is the desktop path
        // (`roost_fs = None` → std::fs walk). It must adopt real markdown and
        // ignore non-markdown + hidden dirs, or peers would either miss notes
        // or sync junk like `.obsidian/` config.
        let (engine, roost, _app) = setup_engine();
        let root = roost.path().join("test-roost");
        std::fs::write(root.join("alpha.md"), "alpha body").unwrap();
        std::fs::write(root.join("beta.md"), "beta body").unwrap();
        std::fs::write(root.join("notes.txt"), "not markdown").unwrap();
        std::fs::create_dir_all(root.join(".obsidian")).unwrap();
        std::fs::write(root.join(".obsidian").join("app.md"), "hidden").unwrap();

        let count = engine.scan_roost().await.unwrap();
        assert_eq!(count, 2, "only the two top-level .md files adopt");

        let store = engine.inner.doc_store.lock().await;
        assert_eq!(store.list().unwrap().len(), 2);
        assert!(store.exists(&NotePath("alpha.md".into())));
        assert!(store.exists(&NotePath("beta.md".into())));
        assert!(
            !store.exists(&NotePath("notes.txt".into())),
            "non-markdown must not be adopted"
        );
        assert!(
            !store.exists(&NotePath(".obsidian/app.md".into())),
            "files under a hidden dir must not be adopted"
        );
        assert_eq!(
            store
                .load_or_create(&NotePath("alpha.md".into()))
                .unwrap()
                .render()
                .unwrap(),
            "alpha body"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn scan_roost_does_not_resurrect_a_tombstoned_note() {
        // Data-loss guard: a note the user deleted (tombstoned in the
        // docstore) whose markdown reappears on disk — restored from backup,
        // recreated externally, synced back by another tool — must STAY
        // deleted through a bulk scan. Un-delete is an explicit Restore only.
        use crate::sync::crdt::NoteDoc;
        let (engine, roost, _app) = setup_engine();
        let root = roost.path().join("test-roost");
        let path = NotePath("deleted.md".into());

        {
            let store = engine.inner.doc_store.lock().await;
            let mut doc = NoteDoc::new(engine.identity().device_id()).unwrap();
            doc.apply_markdown("once had content").unwrap();
            doc.mark_deleted(now_millis()).unwrap();
            store.save(&path, &mut doc).unwrap();
        }
        std::fs::write(root.join("deleted.md"), "came back from disk").unwrap();

        engine.scan_roost().await.unwrap();

        let still_deleted = engine
            .inner
            .doc_store
            .lock()
            .await
            .load_or_create(&path)
            .unwrap()
            .is_deleted()
            .unwrap();
        assert!(
            still_deleted,
            "scan must not un-delete a tombstoned note that reappears on disk"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reset_docstore_wipes_all_docs() {
        // The user-facing recovery path for a corrupted-merge docstore.
        // After reset, the store is empty so the next peer connect re-adopts
        // a single shared history per note. Verify it actually wipes.
        use crate::sync::crdt::NoteDoc;
        let (engine, _roost, _app) = setup_engine();
        let path = NotePath("seed.md".into());
        {
            let store = engine.inner.doc_store.lock().await;
            let mut doc = NoteDoc::new(engine.identity().device_id()).unwrap();
            doc.apply_markdown("seed content").unwrap();
            store.save(&path, &mut doc).unwrap();
            assert!(store.exists(&path), "doc present before reset");
        }

        engine.reset_docstore().await.unwrap();

        let store = engine.inner.doc_store.lock().await;
        assert!(!store.exists(&path), "reset must remove the seeded doc");
        assert!(
            store.list().unwrap().is_empty(),
            "no docs remain after reset"
        );
    }

    /// SYN-B5: `reset_docstore` must not wipe the store while a per-note
    /// mutation is mid commit+disk. It takes the global mutation barrier, so it
    /// blocks until every in-flight note guard drops. Regression: before the
    /// barrier, reset could `wipe_all()` under `doc_store` while a mutator that
    /// had released `doc_store` (to do disk I/O) was about to re-commit into a
    /// store reset believed empty.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reset_docstore_blocks_until_mutators_drain() {
        let (engine, _roost, _app) = setup_engine();
        let note = NotePath("held.md".into());

        // Simulate a mutation in flight: hold the note guard (which now carries
        // the barrier read) the way every commit+disk critical section does.
        let held = engine.inner.note_locks.lock(&note).await;

        // A reset launched now must NOT complete while the guard is held.
        let engine2 = engine.clone();
        let reset = tokio::spawn(async move { engine2.reset_docstore().await });
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert!(
            !reset.is_finished(),
            "reset_docstore wiped the store while a note mutation was in flight"
        );

        // Dropping the guard lets reset proceed and finish.
        drop(held);
        tokio::time::timeout(std::time::Duration::from_secs(2), reset)
            .await
            .expect("reset_docstore should finish once the mutation drains")
            .expect("join")
            .expect("reset_docstore ok");
    }

    /// SYN-B8 durable retry: a committed tombstone whose disk-trash keeps
    /// failing (read-only roost) stays enrolled in `pending_trash` and is
    /// re-attempted each supervisor tick; once the roost becomes writable the
    /// retry trashes the file, emits `NoteDeleted`, and drops the enrollment.
    /// This is the live-path equivalent of the queue's retain→re-pull; without
    /// it a failed live trash was a permanent tombstone↔file divergence.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn retry_pending_trashes_converges_when_roost_becomes_writable() {
        use crate::sync::crdt::NoteDoc;
        let (engine, _roost, _app) = setup_engine();
        let roost_dir = engine.inner.roost.clone();
        let note = NotePath("gone.md".into());

        // Commit a tombstone in the DocStore + leave the file on disk.
        {
            let store = engine.inner.doc_store.lock().await;
            let mut doc = NoteDoc::new(engine.identity().device_id()).unwrap();
            doc.apply_markdown("soon gone").unwrap();
            doc.mark_deleted(1).unwrap();
            store.save(&note, &mut doc).unwrap();
        }
        std::fs::write(roost_dir.join("gone.md"), b"soon gone").unwrap();
        // Occupy `.corvux-trash` with a FILE so the backend can't create the
        // trash dir → `trash` errors (genuine, non-NotFound).
        std::fs::write(roost_dir.join(".corvux-trash"), b"not a dir").unwrap();

        // Enroll (as a failed live/local trash would) and retry — still failing.
        engine
            .inner
            .pending_trash
            .lock()
            .await
            .insert(&note)
            .unwrap();
        engine.retry_pending_trashes().await;
        assert!(
            engine.inner.pending_trash.lock().await.contains(&note),
            "a still-failing trash must stay enrolled for the next tick"
        );
        assert!(
            roost_dir.join("gone.md").exists(),
            "file must still be present while trash keeps failing"
        );

        // Roost becomes writable — a later tick converges.
        std::fs::remove_file(roost_dir.join(".corvux-trash")).unwrap();
        engine.retry_pending_trashes().await;
        assert!(
            engine.inner.pending_trash.lock().await.is_empty(),
            "a successful retry must drop the note from pending_trash"
        );
        assert!(
            !roost_dir.join("gone.md").exists(),
            "the tombstoned note's file must be trashed once the roost is writable"
        );
    }

    /// SYN-B8: the retry must NOT trash a note that was RESTORED (un-deleted)
    /// after enrollment — an explicit Restore drops the tombstone, and the
    /// recreated/kept file is legitimate. The retry drops such a note silently.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn retry_pending_trashes_abandons_restored_note() {
        use crate::sync::crdt::NoteDoc;
        let (engine, _roost, _app) = setup_engine();
        let roost_dir = engine.inner.roost.clone();
        let note = NotePath("kept.md".into());

        // A NON-deleted doc (as if the user Restored it) + the file on disk.
        {
            let store = engine.inner.doc_store.lock().await;
            let mut doc = NoteDoc::new(engine.identity().device_id()).unwrap();
            doc.apply_markdown("i live").unwrap();
            store.save(&note, &mut doc).unwrap();
        }
        std::fs::write(roost_dir.join("kept.md"), b"i live").unwrap();

        // Stale enrollment from an earlier failed trash, before the Restore.
        engine
            .inner
            .pending_trash
            .lock()
            .await
            .insert(&note)
            .unwrap();
        engine.retry_pending_trashes().await;

        assert!(
            engine.inner.pending_trash.lock().await.is_empty(),
            "a restored note must be dropped from the retry set"
        );
        assert!(
            roost_dir.join("kept.md").exists(),
            "the retry must NOT trash a restored (un-deleted) note's file"
        );
    }

    /// SYN-B8 (audit follow-up, 2026-07-13): the whole point of persisting
    /// `pending_trash` — a failed-trash enrollment MUST survive an app
    /// restart, not just a live retry loop within one process. Simulates a
    /// restart by building a SECOND `SyncEngine` against the SAME state_dir
    /// (the app_data tempdir is not dropped between the two engines) and
    /// proving the fresh engine both sees the enrollment on load AND
    /// successfully retries the trash once the roost is writable. Before this
    /// fix `pending_trash` was in-memory only, so engine #2 would start with
    /// an empty set and this note would never be retried — a permanent
    /// tombstone↔file divergence.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pending_trash_survives_engine_restart() {
        use crate::sync::crdt::NoteDoc;
        let roost = tempdir().unwrap();
        let roost_root = roost.path().join("test-roost");
        std::fs::create_dir_all(&roost_root).unwrap();
        let app_data = tempdir().unwrap();
        let state_dir = app_data.path().join("sync-state");
        let note = NotePath("gone.md".into());

        // "Session 1": trash fails (read-only roost via an occupied trash dir)
        // and the note is enrolled, then the engine is dropped (app closes).
        {
            let identity = Arc::new(Identity::load_or_create(app_data.path()).unwrap());
            let tls = Arc::new(TlsIdentity::generate().unwrap());
            let engine1 = SyncEngine::new(
                roost_root.clone(),
                state_dir.clone(),
                None,
                identity,
                tls,
                "test-device".into(),
            )
            .unwrap();
            {
                let store = engine1.inner.doc_store.lock().await;
                let mut doc = NoteDoc::new(engine1.identity().device_id()).unwrap();
                doc.apply_markdown("soon gone").unwrap();
                doc.mark_deleted(1).unwrap();
                store.save(&note, &mut doc).unwrap();
            }
            std::fs::write(roost_root.join("gone.md"), b"soon gone").unwrap();
            std::fs::write(roost_root.join(".corvux-trash"), b"not a dir").unwrap();
            // Force the first persistence attempt to fail by occupying the
            // atomic temp-file path with a directory. The in-memory intent
            // must stay dirty, and the supervisor tick below must persist it
            // once this obstruction is removed — otherwise engine #2 would
            // lose the enrollment despite the happy-path restart test.
            let pending_tmp = state_dir.join("pending_trash.bin.tmp");
            std::fs::create_dir(&pending_tmp).unwrap();
            assert!(
                engine1
                    .inner
                    .pending_trash
                    .lock()
                    .await
                    .insert(&note)
                    .is_err(),
                "the precondition must exercise a failed persistence attempt"
            );
            std::fs::remove_dir(&pending_tmp).unwrap();
            engine1.retry_pending_trashes().await;
            assert!(
                engine1.inner.pending_trash.lock().await.contains(&note),
                "sanity: still enrolled while the roost is read-only"
            );
            // engine1 drops here — simulates the app closing before the
            // roost ever became writable.
        }

        // Roost becomes writable while the app is closed.
        std::fs::remove_file(roost_root.join(".corvux-trash")).unwrap();

        // "Session 2" (restart): a fresh engine against the SAME state_dir.
        let identity2 = Arc::new(Identity::load_or_create(app_data.path()).unwrap());
        let tls2 = Arc::new(TlsIdentity::generate().unwrap());
        let engine2 = SyncEngine::new(
            roost_root.clone(),
            state_dir,
            None,
            identity2,
            tls2,
            "test-device".into(),
        )
        .unwrap();

        assert!(
            engine2.inner.pending_trash.lock().await.contains(&note),
            "the enrollment must survive across a fresh SyncEngine::new — this \
             is the actual durability property being added"
        );

        engine2.retry_pending_trashes().await;
        assert!(
            engine2.inner.pending_trash.lock().await.is_empty(),
            "the restarted engine must successfully retry and clear the enrollment"
        );
        assert!(
            !roost_root.join("gone.md").exists(),
            "the file must actually be trashed by the post-restart retry"
        );
    }

    /// Build a bare `PeerHandle` for a device. The control channel receiver is
    /// returned so the caller can keep it alive (dropping it would make the
    /// handle's sender error, which we don't want mid-test).
    fn fake_handle(
        device: DeviceId,
        name: &str,
    ) -> (
        PeerHandle,
        tokio::sync::mpsc::Receiver<crate::sync::engine::peer::PeerControl>,
    ) {
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        (
            PeerHandle {
                device_id: device,
                display_name: name.into(),
                queue_pair_key: [0u8; 32],
                peer_vault_id: [0u8; 16],
                control: tx,
                generation: 0,
                peer_reconnect: None,
                resumption_token: [0u8; 32],
            },
            rx,
        )
    }

    /// PR-B regression: a slow/wedged-but-still-connected peer that dropped a
    /// live change (`send_local` → `ChannelFull`) with NO queue client
    /// configured (the default strict-P2P mode) must not lose that change
    /// forever. `record_live_pending` + `drain_live_pending` are the fix:
    /// the drainer re-pokes the peer's control channel with
    /// `PeerControl::MarkDirty` and, once delivery succeeds, clears the
    /// intent so it doesn't linger or get redelivered again.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn drain_live_pending_redelivers_via_mark_dirty_and_clears_entry() {
        use crate::sync::engine::peer::PeerControl;

        let (engine, _v, _d) = setup_engine();
        let device = DeviceId([0x11; 16]);
        let note = NotePath("n.md".into());

        // Default config: no queue client. This fix is specifically about
        // the path where the outbox is inert.
        assert!(
            engine.inner.queue_client.read().await.is_none(),
            "test must exercise the no-queue (default strict-P2P) path"
        );

        // fake_handle's channel has capacity 1 and is currently empty, so
        // the drainer's re-poke has room to land.
        let (handle, mut rx) = fake_handle(device, "peer");
        engine.inner.peers.write().await.insert(device, handle);

        // Simulate a `send_local` that was dropped with `ChannelFull` and no
        // queue client to fall back on — the exact hole this fix closes.
        engine.record_live_pending(device, note.clone()).await;
        assert!(
            engine
                .inner
                .live_pending
                .lock()
                .await
                .get(&device)
                .is_some_and(|set| set.contains(&note)),
            "record_live_pending must record the dropped intent"
        );

        engine.drain_live_pending().await;

        match rx.try_recv() {
            Ok(PeerControl::MarkDirty(p)) => assert_eq!(p, note),
            other => panic!("expected MarkDirty({note:?}) on the control channel, got {other:?}"),
        }
        assert!(
            !engine.inner.live_pending.lock().await.contains_key(&device),
            "a successfully redelivered intent must be cleared, not linger for the next tick"
        );
    }

    /// PR-B regression, failure half: while the peer's control channel is
    /// STILL full (a genuinely wedged peer, not just a momentary blip), the
    /// re-poke must fail without panicking or blocking, and the pending
    /// intent must be LEFT IN PLACE for the next supervisor tick — clearing
    /// it here would silently drop the change exactly like the original bug.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn drain_live_pending_keeps_entry_when_control_channel_is_full() {
        let (engine, _v, _d) = setup_engine();
        let device = DeviceId([0x22; 16]);
        let note = NotePath("n.md".into());

        // fake_handle's control channel has capacity 1. Fill that single
        // slot before registering so the drainer's `send_mark_dirty`
        // re-poke has nowhere to go, and never drain `_rx`.
        let (handle, _rx) = fake_handle(device, "wedged");
        handle
            .send_mark_dirty(NotePath("filler.md".into()))
            .expect("filling the single slot must itself succeed");
        engine.inner.peers.write().await.insert(device, handle);

        engine.record_live_pending(device, note.clone()).await;
        engine.drain_live_pending().await;

        let pending = engine.inner.live_pending.lock().await;
        let set = pending
            .get(&device)
            .expect("entry must remain when the control channel is still full");
        assert!(
            set.contains(&note),
            "the specific note must still be pending for the next drain tick"
        );
    }

    /// PR-B regression: `register_peer` must clear a device's `live_pending`
    /// on reconnect. A fresh connection re-syncs shared paths via its
    /// `NoteList` exchange, so any dropped-live intents are redundant — leaving
    /// them would let the set grow without bound across repeated redials.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn register_peer_clears_live_pending_on_reconnect() {
        let (engine, _v, _d) = setup_engine();
        let device = DeviceId([0x33; 16]);
        let note = NotePath("n.md".into());

        // Seed a pending intent as if a live send had been dropped earlier.
        engine.record_live_pending(device, note).await;
        assert!(
            engine.inner.live_pending.lock().await.contains_key(&device),
            "precondition: the device has a pending intent"
        );

        // A fresh connection registers for the same device. Use a trivially
        // completing task — register_peer clears live_pending synchronously
        // (before spawning its cleanup), so the assertion below is stable.
        let (handle, _rx) = fake_handle(device, "reconnect");
        let task = tokio::spawn(async { Ok::<(), SyncError>(()) });
        engine.register_peer(handle, task).await;

        assert!(
            !engine.inner.live_pending.lock().await.contains_key(&device),
            "register_peer must clear the device's live_pending on reconnect"
        );
    }

    /// PR-D #6a: a live iroh listener alone must count as "running". A fresh
    /// host that only tapped "pair a device" binds an iroh listener but has no
    /// TCP port and no peers yet — it previously reported "stopped".
    #[test]
    fn compute_running_counts_a_live_iroh_listener() {
        // The exact fresh-pairing case: no TCP port, iroh listener up, no peers.
        assert!(compute_running(false, true, false, false));
        // Fully idle → not running.
        assert!(!compute_running(false, false, false, false));
        // Each other term still independently implies running.
        assert!(compute_running(true, false, false, false));
        assert!(compute_running(false, false, true, false));
        assert!(compute_running(false, false, false, true));
    }

    /// PR-D #4: rebinding the iroh listener on a dial-only engine (none bound)
    /// is a clean no-op — it must not panic or error. The live-endpoint rebind
    /// (identity preserved across a real bound endpoint) is covered by the
    /// real-iroh tests in PR-E, which stand up an actual endpoint.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rebind_iroh_listener_is_noop_without_a_bound_listener() {
        let (engine, _v, _d) = setup_engine();
        engine.set_relay_disabled(true);
        assert!(
            engine.rebind_iroh_listener().await.is_ok(),
            "rebind on a dial-only engine (no iroh listener) must be a clean no-op"
        );
    }

    /// Regression: a reconnect that lands while the previous connection's peer
    /// task is still unwinding must survive. Before the generation guard, the
    /// old task's async cleanup ran an unconditional `peers.remove(device)` +
    /// `set_peer_state(Failed)`, which evicted the freshly-registered new
    /// connection and flapped the UI back to "disconnected" right after a
    /// successful reconnect.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reconnect_before_old_task_exits_is_not_clobbered() {
        use crate::sync::error::EngineError;

        let (engine, _v, _d) = setup_engine();
        let device = DeviceId([7u8; 16]);

        // Connection A: its task blocks until we fire `release_a`, then exits
        // with an error (a dropped transport). We keep `_rx_a` alive so the
        // handle's control sender stays valid.
        let (release_a_tx, release_a_rx) = tokio::sync::oneshot::channel::<()>();
        let (handle_a, _rx_a) = fake_handle(device, "A");
        let task_a = tokio::spawn(async move {
            let _ = release_a_rx.await;
            Err::<(), _>(SyncError::from(EngineError::NotConnected))
        });
        engine.register_peer(handle_a, task_a).await;
        engine
            .set_peer_state(device, PeerConnectionState::Connected)
            .await;

        // Connection B for the SAME device lands while A is still "running".
        // Its task never exits during the test, so B's own cleanup won't fire.
        let (handle_b, _rx_b) = fake_handle(device, "B");
        let task_b = tokio::spawn(async move {
            std::future::pending::<()>().await;
            Ok(())
        });
        engine.register_peer(handle_b, task_b).await;
        engine
            .set_peer_state(device, PeerConnectionState::Connected)
            .await;

        // B is the current registration.
        assert_eq!(
            engine
                .inner
                .peers
                .read()
                .await
                .get(&device)
                .map(|h| h.display_name.clone()),
            Some("B".to_string()),
            "B should be the registered handle after reconnect"
        );

        // Let A's task exit. Its cleanup must recognise it was superseded and
        // leave B (and the Connected state) untouched.
        let _ = release_a_tx.send(());
        // Generous window for A's detached cleanup task to run.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        assert_eq!(
            engine
                .inner
                .peers
                .read()
                .await
                .get(&device)
                .map(|h| h.display_name.clone()),
            Some("B".to_string()),
            "old connection's cleanup must not evict the reconnected handle"
        );
        assert!(
            matches!(
                engine.inner.peer_states.read().await.get(&device),
                Some(PeerConnectionState::Connected)
            ),
            "state must stay Connected — the superseded task must not set Failed"
        );
    }

    /// A drop of an ESTABLISHED connection (Connected → Failed) restarts
    /// backoff at the fast end — it must count as attempt 1, not resume the
    /// escalated count from failures before the connection succeeded. Only
    /// consecutive failed *dials* escalate backoff.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn drop_of_connected_peer_restarts_backoff_fast() {
        let (engine, _v, _d) = setup_engine();
        let device = DeviceId([0xAB; 16]);

        // Escalate backoff with repeated failed dials.
        for _ in 0..6 {
            engine
                .set_peer_state(
                    device,
                    PeerConnectionState::Failed {
                        reason: "dial failed".into(),
                        at_millis: now_millis(),
                    },
                )
                .await;
        }
        assert!(
            engine
                .inner
                .reconnect
                .read()
                .await
                .get(&device)
                .unwrap()
                .attempts
                >= 6,
            "consecutive dial failures escalate"
        );

        // Connect, then drop. The drop must schedule a FAST retry (attempt 1
        // ⇒ 15s), not resume from the escalated count.
        engine
            .set_peer_state(device, PeerConnectionState::Connected)
            .await;
        let dropped_at = now_millis();
        engine
            .set_peer_state(
                device,
                PeerConnectionState::Failed {
                    reason: "transport: io: connection lost".into(),
                    at_millis: dropped_at,
                },
            )
            .await;

        let rc = engine.inner.reconnect.read().await;
        let entry = rc.get(&device).unwrap();
        assert_eq!(entry.attempts, 1, "drop counts as first attempt");
        // Scheduled value carries ±25% jitter (M-5), so assert the band around
        // the fast (15s) base rather than an exact tick.
        let base = reconnect_backoff_ms(1);
        let lo = dropped_at + base - base / 4;
        let hi = dropped_at + base + base / 4;
        assert!(
            (lo..=hi).contains(&entry.next_retry_at_millis),
            "retry scheduled within the jittered fast (15s) band: {} not in {}..={}",
            entry.next_retry_at_millis,
            lo,
            hi
        );
    }

    /// `force_reconnect_all` zeroes accrued backoff so a foreground/resume (or
    /// manual retry) dials immediately instead of waiting out a window that
    /// grew while the app was suspended.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn force_reconnect_all_resets_backoff() {
        let (engine, _v, _d) = setup_engine();
        let device = DeviceId([9u8; 16]);
        let far_future = now_millis() + 300_000;
        {
            let mut rc = engine.inner.reconnect.write().await;
            rc.insert(
                device,
                ReconnectState {
                    attempts: 5,
                    next_retry_at_millis: far_future,
                    last_error: Some("boom".into()),
                    ..Default::default()
                },
            );
        }

        engine.force_reconnect_all().await;

        let rc = engine.inner.reconnect.read().await;
        let entry = rc.get(&device).expect("reconnect entry retained");
        assert_eq!(entry.attempts, 0, "attempts reset to 0");
        assert_eq!(
            entry.next_retry_at_millis, 0,
            "next retry cleared so the peer is due now"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stop_marks_engine_stopped() {
        let (engine, _v, _d) = setup_engine();
        assert!(!engine.is_stopped());
        engine.stop().await.unwrap();
        assert!(
            engine.is_stopped(),
            "stop() must latch the stopped flag so in-flight dials bail"
        );
    }

    /// SYN-B7: `stop()` must CANCEL a pending detached task, not wait it out.
    /// A verified-tombstone timer sleeps `REMOVAL_VERIFY_DELAY` (1.2s); if the
    /// task were untracked/uncancelled (the pre-W4 bug), it would either outlive
    /// `stop()` (leak) or force `tracker.wait()` to block the full delay. We
    /// prove BOTH the cancel (stop returns well under the delay) and the drain
    /// (the tracker is empty afterward).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stop_cancels_and_drains_pending_tombstone_task() {
        let (engine, _v, _d) = setup_engine();
        // Schedule a verify-then-tombstone that sleeps 1.2s before acting.
        engine.schedule_verified_tombstone(NotePath("live.md".into()));
        assert!(
            !engine.inner.tracker.is_empty(),
            "the scheduled tombstone task should be tracked"
        );

        let t0 = std::time::Instant::now();
        engine.stop().await.unwrap();
        let elapsed = t0.elapsed();

        assert!(
            elapsed < std::time::Duration::from_millis(1000),
            "stop() took {elapsed:?} — it waited out the tombstone delay instead of \
             cancelling the task (SYN-B7 regression)"
        );
        assert!(
            engine.inner.tracker.is_empty(),
            "stop() must drain every tracked task; tracker not empty after stop()"
        );
    }

    /// SYN-B6: bring-up on an engine that has been stopped must be a no-op —
    /// it must NOT bind a listener or install a background task into an engine
    /// `stop()` has already drained past. Before W4, `start_*` had no
    /// stop-serialization, so an init racing a stop could resurrect a
    /// torn-down engine (a redial completing into the old roost).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bring_up_after_stop_is_noop() {
        let (engine, _v, _d) = setup_engine();
        engine.stop().await.unwrap();
        assert!(engine.is_stopped());

        // Listener bring-up must refuse (Err) and install nothing.
        let pin = Pin::parse("135791").unwrap();
        assert!(
            engine.start_listening_iroh(pin.clone()).await.is_err(),
            "start_listening_iroh bound on a stopped engine"
        );
        assert!(
            engine.inner.iroh_listen_state.lock().await.is_none(),
            "an iroh listener was installed after stop()"
        );
        assert!(
            engine.start_listening(0, pin).await.is_err(),
            "start_listening bound on a stopped engine"
        );
        assert!(engine.inner.listen_state.lock().await.is_none());

        // Void bring-ups must no-op without installing tasks.
        engine.start_watcher().await.unwrap();
        engine.start_reconnect_supervisor().await;
        assert!(engine.inner.watcher.lock().await.is_none());
        assert!(engine.inner.reconnect_task.lock().await.is_none());

        // Nothing was spawned into the (already-drained) tracker.
        assert!(
            engine.inner.tracker.is_empty(),
            "a bring-up installed a task into a stopped engine (SYN-B6 regression)"
        );
    }

    /// L-6: a change dequeued before a roost switch must NOT write to the old
    /// engine's DocStore once it's stopped — otherwise the write lands in the
    /// previous roost's store after the new roost is up (cross-roost
    /// contamination).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn apply_local_change_is_a_noop_after_stop() {
        let (engine, _v, _d) = setup_engine();
        engine.stop().await.unwrap();
        let change = LocalChange::ModifiedWithContent {
            path: NotePath("late.md".into()),
            content: "should not be written".into(),
        };
        let wrote = engine.apply_local_change(&change).await.unwrap();
        assert!(!wrote, "a stopped engine must not apply a local change");
        let notes = engine.inner.doc_store.lock().await.list().unwrap();
        assert!(
            notes.is_empty(),
            "stopped engine must not have written to the DocStore: {notes:?}"
        );
    }

    /// P0 data-loss regression. A watcher `Removed` is frequently a *transient*
    /// filesystem artifact (atomic-rename churn, our own rewrite, an iOS
    /// app-resume volume rescan), NOT a user delete. If the file is still on
    /// disk when we go to confirm, the note must NOT be tombstoned — otherwise
    /// a live note is deleted and, because a tombstone is a CRDT change, the
    /// delete propagates to every paired device (the observed "note vanished on
    /// the phone AND the Mac" bug).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn verify_then_tombstone_skips_when_file_still_present() {
        let (engine, _v, _d) = setup_engine();
        let note = NotePath("keeper.md".into());
        // Live doc in the store AND the file present on disk.
        engine
            .notify_local_write(note.clone(), "important".into())
            .await;
        engine
            .write_recovered_note(&note, "important")
            .await
            .unwrap();
        assert!(
            engine.note_exists_on_disk(&note).await.unwrap(),
            "precondition: the note file is on disk"
        );

        engine.verify_then_tombstone(note.clone()).await;

        let store = engine.inner.doc_store.lock().await;
        let doc = store.load_or_create(&note).unwrap();
        assert!(
            !doc.is_deleted().unwrap(),
            "a live note whose file is still present must NOT be tombstoned by a watcher removal"
        );
    }

    /// The counterpart: a genuine external delete (the file really is gone when
    /// we confirm) must still tombstone, so real deletes keep propagating. The
    /// guard adds at most `REMOVAL_VERIFY_DELAY` of latency, never permanence.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn verify_then_tombstone_deletes_when_file_really_gone() {
        let (engine, roost, _d) = setup_engine();
        let note = NotePath("gone.md".into());
        // Live doc; notify_local_write now commits AND renders (B3), so delete
        // the rendered file to reproduce the genuine external-delete state.
        engine.notify_local_write(note.clone(), "bye".into()).await;
        let _ = std::fs::remove_file(roost.path().join("test-roost").join("gone.md"));
        assert!(
            !engine.note_exists_on_disk(&note).await.unwrap(),
            "precondition: the note file is absent from disk"
        );

        engine.verify_then_tombstone(note.clone()).await;

        let store = engine.inner.doc_store.lock().await;
        let doc = store.load_or_create(&note).unwrap();
        assert!(
            doc.is_deleted().unwrap(),
            "a note whose file is genuinely gone must tombstone (real deletes still propagate)"
        );
    }

    /// `recover_missing_files` must RESTORE a legitimately-empty note (whose
    /// md_hash is that of the empty string) rather than silently dropping it.
    /// The old code `continue`d on ANY empty render, so a cleared note that
    /// lived only in the docstore never came back to disk.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn recover_restores_legitimately_empty_note() {
        let (engine, roost, _d) = setup_engine();
        let note = NotePath("blank.md".into());
        engine.notify_local_write(note.clone(), String::new()).await;
        // notify_local_write now commits AND renders (B3/SYN-I3). Delete the
        // rendered file (the engine's roost is the `test-roost` subdir) to
        // reproduce the "in DocStore but missing from disk" state
        // `recover_missing_files` exists to heal.
        let _ = std::fs::remove_file(roost.path().join("test-roost").join("blank.md"));
        assert!(
            !engine.note_exists_on_disk(&note).await.unwrap(),
            "precondition: empty note removed to simulate missing-from-disk"
        );

        engine.recover_missing_files().await.unwrap();

        assert!(
            engine.note_exists_on_disk(&note).await.unwrap(),
            "a legitimately-empty note must be recovered to disk, not dropped"
        );
    }

    /// B3/SYN-I5: when the local-save locked render fails to write disk (here:
    /// a read-only roost), the failure must SURFACE as `NoteSyncFailed`, not be
    /// swallowed while the DocStore/peers advance — otherwise the note is
    /// falsely shown as synced with disk silently lagging. `render_to_roost`
    /// returns a write failure as `Ok(RenderOutcome::Failed)`, so a naive
    /// `if let Err` would miss it; this guards that the arm is handled.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn local_save_render_failure_surfaces_note_sync_failed() {
        use std::os::unix::fs::PermissionsExt;
        let (engine, roost, _d) = setup_engine();
        let roost_root = roost.path().join("test-roost");
        let mut rx = engine.inner.events.subscribe();

        // Read-only roost → the locked render's write_atomic fails (Failed).
        std::fs::set_permissions(&roost_root, std::fs::Permissions::from_mode(0o555)).unwrap();
        engine
            .notify_local_write(NotePath("stuck.md".into()), "content".into())
            .await;
        // Restore perms so the tempdir can be cleaned up.
        let _ = std::fs::set_permissions(&roost_root, std::fs::Permissions::from_mode(0o755));

        let mut saw_failed = false;
        while let Ok(ev) = rx.try_recv() {
            if matches!(ev, SyncEvent::NoteSyncFailed { .. }) {
                saw_failed = true;
                break;
            }
        }
        assert!(
            saw_failed,
            "a failed locked render must emit NoteSyncFailed, not be swallowed as a clean save"
        );
    }

    /// A device that isn't hosting an iroh listener has no dialable ticket to
    /// offer, so its redials must stay one-directional (offer = None) rather
    /// than panicking or fabricating a bogus ticket. The Some-path (offer
    /// present once hosting) requires a live iroh endpoint, so it's covered by
    /// on-device reconnect verification, not this unit test.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reconnect_offer_is_none_without_a_listener() {
        let (engine, _v, _d) = setup_engine();
        assert!(
            engine.build_own_reconnect_offer().await.is_none(),
            "no iroh listener bound → no reconnect offer (stays one-directional)"
        );
    }

    /// Unpair must purge the in-memory dial bookkeeping, not just the stored
    /// record — otherwise the peer lingers as a ghost dot + retry countdown
    /// and gets re-dialed by force_reconnect_all.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unpair_purges_in_memory_dial_state() {
        use crate::sync::store::peer_store::PeerRecord;
        let (engine, _v, _d) = setup_engine();
        let device = DeviceId([0x33; 16]);
        engine
            .inner
            .peer_store
            .lock()
            .await
            .upsert(PeerRecord::new(device, [0x33; 32], "gone".into()))
            .unwrap();
        engine
            .set_peer_state(
                device,
                PeerConnectionState::Failed {
                    reason: "x".into(),
                    at_millis: now_millis(),
                },
            )
            .await;
        assert!(engine.inner.peer_states.read().await.contains_key(&device));
        assert!(engine.inner.reconnect.read().await.contains_key(&device));

        engine.unpair(device).await.unwrap();

        assert!(
            !engine.inner.peer_states.read().await.contains_key(&device),
            "peer_states entry must be purged on unpair"
        );
        assert!(
            !engine.inner.reconnect.read().await.contains_key(&device),
            "reconnect entry must be purged on unpair"
        );
    }

    // ── PR-E: real-iroh end-to-end tests ────────────────────────────────
    //
    // Everything above drives the engine over `MockTransport` (peer.rs) or
    // real loopback TCP (`two_engines_pair_and_sync_over_tcp`). iroh QUIC is
    // the PRODUCTION transport and had no full-engine coverage at all before
    // this section. These tests bind REAL iroh endpoints — no fakes — but
    // stay hermetic on CI the same way iroh's own doctests do it: every
    // endpoint here binds with `set_relay_disabled(true)` (→
    // `RelayMode::Disabled`) BEFORE the first bind, and peers dial each
    // other using the DIRECT addresses embedded in the pairing ticket
    // (`iroh_pairing_ticket` / `decode_ticket`), never Number0 discovery
    // resolution. Nothing here depends on outbound relay traffic or a
    // discovery round-trip succeeding.
    //
    // Residual (see the test-run report, not fixed here): `build_endpoint`'s
    // own doc comment (`transport/iroh.rs`) notes that Number0 discovery
    // PUBLISH is unconditional regardless of relay mode — `RelayMode::
    // Disabled` only stops the relay from ever carrying *sync* traffic. That
    // publish is fire-and-forget and never blocks `bind()`/`connect()` here
    // (direct addrs are always supplied), so it can't fail these tests, but
    // a fully air-gapped CI runner may still see a harmless background DNS
    // attempt. Also: the direct addresses these binds advertise come from
    // whatever local interface the OS hands back (`build_endpoint` binds
    // `0.0.0.0`/`[::]`, not literally `127.0.0.1`) — same-host dialing to
    // that address is expected to work on a normal CI network namespace,
    // but it isn't a hardcoded loopback literal the way an explicit
    // `bind_addr("127.0.0.1:0")` would be.

    /// Poll `iroh_pairing_ticket` until the bound endpoint has a direct
    /// address to advertise, returning the ticket string. `Endpoint::addr`
    /// documents that local addresses become available "as long as the
    /// endpoint was able to bind" but doesn't promise that's synchronous
    /// with `bind()` returning — a flat sleep here would be exactly the
    /// kind of CI-load-flaky wait the rest of this file avoids with polling.
    async fn wait_for_iroh_ticket(engine: &SyncEngine) -> String {
        for _ in 0..600 {
            if let Ok(ticket) = engine.iroh_pairing_ticket().await {
                if let Ok(addr) = crate::sync::transport::iroh::decode_ticket(&ticket) {
                    if addr.ip_addrs().next().is_some() {
                        return ticket;
                    }
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        panic!("iroh endpoint never advertised a direct address within ~30s");
    }

    /// Gracefully close just ONE peer link (sends `Bye`, drops the QUIC
    /// connection) without touching the engine's listener or `peer_store` —
    /// mirrors a real disconnect (peer closed the app / lost network) while
    /// leaving the stored ticket+PIN in place for a later autoresume. Unlike
    /// `engine.stop()` this is NOT a one-way latch, so the engine can dial
    /// back out again afterward.
    async fn shutdown_peer_link(engine: &SyncEngine, device: DeviceId) {
        let peers = engine.inner.peers.read().await;
        if let Some(handle) = peers.get(&device) {
            handle.shutdown().await;
        }
    }

    async fn wait_for_connected(engine: &SyncEngine, device: DeviceId) {
        let hex = device.as_hex();
        for _ in 0..600 {
            if engine
                .status()
                .await
                .connected_peers
                .iter()
                .any(|p| p.device_id == hex)
            {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        panic!("peer {hex} never reached Connected within ~30s");
    }

    async fn wait_for_disconnected(engine: &SyncEngine, device: DeviceId) {
        let hex = device.as_hex();
        for _ in 0..600 {
            if !engine
                .status()
                .await
                .connected_peers
                .iter()
                .any(|p| p.device_id == hex)
            {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        panic!("peer {hex} never left Connected within ~30s");
    }

    async fn wait_for_note_text(
        engine: &SyncEngine,
        path: &crate::sync::protocol::NotePath,
        expected: &str,
    ) {
        for _ in 0..600 {
            {
                let store = engine.inner.doc_store.lock().await;
                if let Ok(doc) = store.load_or_create(path) {
                    if let Ok(text) = doc.render() {
                        if text == expected {
                            return;
                        }
                    }
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        let store = engine.inner.doc_store.lock().await;
        let got = store
            .load_or_create(path)
            .ok()
            .and_then(|d| d.render().ok())
            .unwrap_or_default();
        panic!("note {path} never converged to {expected:?} within ~30s (last seen: {got:?})");
    }

    /// #6a (PR-D), real-endpoint half: with ONLY a real bound iroh
    /// listener — no TCP listener, no peers, no known peers —
    /// `status().running` must be true. `compute_running_counts_a_live_iroh_
    /// listener` (above) proves the pure-function rule hermetically; this
    /// proves the same rule through the actual `start_listening_iroh` bind
    /// path.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn iroh_only_listener_reports_running_end_to_end() {
        let (engine, _v, _d) = setup_engine();
        engine.set_relay_disabled(true); // hermetic bind — see module note above

        let before = engine.status().await;
        assert!(
            !before.running,
            "a fresh engine with nothing bound must not report running"
        );

        let pin = Pin::parse("246810").unwrap();
        engine.start_listening_iroh(pin).await.unwrap();

        let after = engine.status().await;
        assert!(
            after.running,
            "a bound iroh listener alone (no TCP port, no peers) must count as running"
        );
        assert!(after.listen_port.is_none(), "no TCP listener was bound");
        assert!(after.connected_peers.is_empty());
        assert!(after.known_peers.is_empty());

        engine.stop().await.unwrap();
    }

    /// #4 (PR-D), real-endpoint half: `rebind_iroh_listener` on a LIVE bound
    /// iroh endpoint preserves the device's NodeId (so an outstanding
    /// pairing ticket for it stays valid) and leaves the listener live
    /// afterward. `rebind_iroh_listener_is_noop_without_a_bound_listener`
    /// (above) covers the no-endpoint-bound case and explicitly deferred
    /// this live-endpoint case to "the real-iroh tests in PR-E" — this is
    /// that test.
    ///
    /// Both binds in this test go through `set_relay_disabled(true)` to
    /// stay hermetic (module note above), which means `relay_connected` is
    /// `None` throughout — observing it flip from `Some` to `None` would
    /// need a real relay round-trip and is intentionally out of scope here
    /// (see the test-run report for this gap).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rebind_iroh_listener_preserves_node_id_on_a_live_endpoint() {
        let (engine, _v, _d) = setup_engine();
        engine.set_relay_disabled(true);
        let pin = Pin::parse("112233").unwrap();

        let node_id_before = engine.start_listening_iroh(pin.clone()).await.unwrap();
        wait_for_iroh_ticket(&engine).await;

        let before = engine.status().await;
        assert!(before.running, "a live iroh listener must count as running");
        assert_eq!(
            before.relay_connected, None,
            "relay_connected must be None while strict-P2P is on"
        );

        engine.rebind_iroh_listener().await.unwrap();

        let after = engine.status().await;
        assert!(
            after.running,
            "the listener must be live again immediately after rebind"
        );
        assert_eq!(after.relay_connected, None);

        // Read back the NodeId post-rebind. An already-live listener makes
        // `start_listening_iroh` a cheap PIN-refresh no-op (see its own
        // docs), so this doesn't trigger a SECOND rebind — it just confirms
        // what NodeId the rebound endpoint is answering under.
        let node_id_after = engine.start_listening_iroh(pin).await.unwrap();
        assert_eq!(
            node_id_before, node_id_after,
            "rebind must preserve the device's NodeId so existing pairing tickets stay valid"
        );

        engine.stop().await.unwrap();
    }

    /// #6e-a: two real `SyncEngine`s pair over iroh, disconnect (the peer
    /// link only — both listeners stay up), independently edit the SAME
    /// note while offline, then reconnect and converge. Mirrors
    /// `two_peers_converge_via_mock_transport` and
    /// `both_offline_delete_and_edit_then_reconnect_converges` (both in
    /// peer.rs) but drives the convergence through the real `SyncEngine`
    /// iroh dial/accept path instead of `MockTransport`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn two_engines_converge_over_real_iroh_after_offline_edits() {
        use crate::sync::crdt::NoteDoc;
        use crate::sync::protocol::NotePath;

        let (a, _a_roost, _a_app) = setup_engine();
        let (b, _b_roost, _b_app) = setup_engine();
        a.set_relay_disabled(true);
        b.set_relay_disabled(true);

        let path = NotePath("shared.md".into());
        // Seed the SAME note identically on both sides under a THIRD shared
        // actor, so the pre-pairing state is genuinely shared history (same
        // ops -> same change hash) rather than something the initial
        // connect itself has to reconcile.
        for store in [&a.inner.doc_store, &b.inner.doc_store] {
            let s = store.lock().await;
            let mut doc = NoteDoc::new(DeviceId([0x90; 16])).unwrap();
            doc.apply_markdown("base\n").unwrap();
            s.save(&path, &mut doc).unwrap();
        }

        let pin = Pin::parse("135790").unwrap();
        b.start_listening_iroh(pin.clone()).await.unwrap();
        // SRX: open B's first-pair enrollment window (production sets this via
        // the `sync_listen_iroh` IPC command when the user shows the code).
        b.open_enrollment_window();
        // SRX layer 3: mint a one-time secret on B and set it BEFORE generating
        // the ticket, exactly as `sync_listen_iroh` does — so this test exercises
        // the full end-to-end secret path (mint → embed in ticket → A extracts →
        // folds into the first-pair proof → B verifies), not just PIN-only.
        b.set_pairing_secret(Some(crate::sync::crypto::pairing::PairingSecret::generate()))
            .await;
        let ticket = wait_for_iroh_ticket(&b).await;
        let peer_addr = crate::sync::transport::iroh::decode_ticket(&ticket).unwrap();
        // A extracts the secret from the scanned ticket (as `sync_connect_iroh`).
        let ticket_secret = crate::sync::transport::iroh::decode_ticket_secret(&ticket);
        assert!(
            ticket_secret.is_some(),
            "the ticket must carry the one-time pairing secret B minted"
        );

        let b_device = a
            .connect_peer_via_iroh(peer_addr, pin.clone(), None, None, ticket_secret)
            .await
            .unwrap();
        // Mirror what the IPC layer (`sync_connect_iroh`) persists after a
        // successful dial — without this, A has no stored ticket to redial
        // B with, and the reconnect below would need a fresh QR pair.
        a.inner
            .peer_store
            .lock()
            .await
            .remember_connection(b_device, Some(ticket), Some(pin.as_str().to_string()))
            .unwrap();
        let a_device = a.identity().device_id();

        wait_for_connected(&a, b_device).await;
        wait_for_connected(&b, a_device).await;

        // Disconnect just this peer link. Both engines' listeners stay live.
        shutdown_peer_link(&a, b_device).await;
        wait_for_disconnected(&a, b_device).await;
        wait_for_disconnected(&b, a_device).await;

        // Offline: each side edits the SAME note independently.
        {
            let s = a.inner.doc_store.lock().await;
            let mut doc = s.load_or_create(&path).unwrap();
            doc.apply_markdown("base\nA's offline line\n").unwrap();
            s.save(&path, &mut doc).unwrap();
        }
        {
            let s = b.inner.doc_store.lock().await;
            let mut doc = s.load_or_create(&path).unwrap();
            doc.apply_markdown("base\nB's offline line\n").unwrap();
            s.save(&path, &mut doc).unwrap();
        }

        // Reconnect using ONLY the stored ticket/PIN — no fresh QR/pairing.
        a.force_reconnect_all().await;
        wait_for_connected(&a, b_device).await;
        wait_for_connected(&b, a_device).await;

        // Poll for convergence, then assert neither offline edit was lost.
        let mut a_text = String::new();
        let mut b_text = String::new();
        for _ in 0..600 {
            a_text = a
                .inner
                .doc_store
                .lock()
                .await
                .load_or_create(&path)
                .unwrap()
                .render()
                .unwrap();
            b_text = b
                .inner
                .doc_store
                .lock()
                .await
                .load_or_create(&path)
                .unwrap()
                .render()
                .unwrap();
            if a_text == b_text
                && a_text.contains("A's offline line")
                && a_text.contains("B's offline line")
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert_eq!(
            a_text, b_text,
            "both engines must converge to the same merged text over real iroh"
        );
        assert!(
            a_text.contains("A's offline line"),
            "A's concurrent offline edit must not be lost: {a_text:?}"
        );
        assert!(
            a_text.contains("B's offline line"),
            "B's concurrent offline edit must not be lost: {a_text:?}"
        );

        a.stop().await.unwrap();
        b.stop().await.unwrap();
    }

    /// #6e-b: a burst of local notes bigger than the FullDoc in-flight
    /// window (`MAX_CONCURRENT_FULLDOC_BUFFERS` in peer.rs), with the
    /// connection aborted almost immediately after pairing — well before a
    /// 60-note backfill could possibly finish over such a brief window — to
    /// simulate a connection dropping mid-transfer. Reconnecting on the
    /// stored ticket (no fresh QR/pairing call) must still land every note.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn iroh_reconnect_after_mid_transfer_abort_converges_without_a_fresh_pair() {
        use crate::sync::crdt::NoteDoc;
        use crate::sync::protocol::NotePath;

        const N: usize = 60;

        let (a, _a_roost, _a_app) = setup_engine();
        let (b, _b_roost, _b_app) = setup_engine();
        a.set_relay_disabled(true);
        b.set_relay_disabled(true);

        {
            let store = a.inner.doc_store.lock().await;
            for i in 0..N {
                let mut doc = NoteDoc::new(a.identity().device_id()).unwrap();
                doc.apply_markdown(&format!("burst note #{i}")).unwrap();
                store
                    .save(&NotePath(format!("burst-{i}.md")), &mut doc)
                    .unwrap();
            }
        }

        let pin = Pin::parse("998877").unwrap();
        b.start_listening_iroh(pin.clone()).await.unwrap();
        // SRX: open B's first-pair enrollment window (production sets this via
        // the `sync_listen_iroh` IPC command when the user shows the code).
        b.open_enrollment_window();
        let ticket = wait_for_iroh_ticket(&b).await;
        let peer_addr = crate::sync::transport::iroh::decode_ticket(&ticket).unwrap();

        let b_device = a
            .connect_peer_via_iroh(peer_addr, pin.clone(), None, None, None)
            .await
            .unwrap();
        a.inner
            .peer_store
            .lock()
            .await
            .remember_connection(b_device, Some(ticket), Some(pin.as_str().to_string()))
            .unwrap();

        // Abort almost immediately — the 60-note FullDoc backfill cannot
        // possibly have finished this fast over the brief window.
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        shutdown_peer_link(&a, b_device).await;
        wait_for_disconnected(&a, b_device).await;

        // Reconnect on ONLY the stored ticket/PIN — no fresh QR/pairing.
        a.force_reconnect_all().await;
        wait_for_connected(&a, b_device).await;

        for i in 0..N {
            wait_for_note_text(
                &b,
                &NotePath(format!("burst-{i}.md")),
                &format!("burst note #{i}"),
            )
            .await;
        }

        a.stop().await.unwrap();
        b.stop().await.unwrap();
    }
}
