//! UI/event types crossing the engine boundary. Kept separate from the
//! `SyncEngine` impl so handler code in `commands::sync` and the frontend
//! can pull just the shapes without dragging in the engine's transitive
//! deps (transport, stores, peer tasks).

use crate::sync::identity::DeviceId;
use iroh::EndpointId as NodeId;

/// Per-peer connection lifecycle. In-memory only — on app restart every
/// known peer starts `Disconnected`, transitions to `Dialing` when the
/// autoresume path fires, then `Handshaking` / `Connected` or
/// `Failed { reason }`. Surfaced to the UI as a colored dot + error
/// tooltip so the user can see what's happening instead of a silent
/// "nothing's happening".
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, specta::Type)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PeerConnectionState {
    Disconnected,
    /// TCP dial in progress.
    Dialing,
    /// TCP up, PIN/fingerprint handshake running.
    Handshaking,
    Connected,
    /// Most recent dial attempt failed. `reason` is user-facing: shown
    /// in a tooltip. `at_millis` lets the UI decay old failures so a
    /// working reconnect clears the red dot.
    Failed {
        reason: String,
        #[specta(type = u32)]
        at_millis: u64,
    },
}

/// One-shot status event emitted to the frontend. Keeps the UI layer from
/// having to poll for peer state changes.
#[derive(Debug, Clone)]
pub enum SyncEvent {
    ListenerBound {
        port: u16,
    },
    /// Emitted when the iroh endpoint binds. Carries our `NodeId` so the
    /// frontend can render the QR code that the joining device scans.
    /// Distinct from `ListenerBound` because the iroh path doesn't have a
    /// human-meaningful "port" (it multiplexes everything onto a UDP socket
    /// and uses NodeId addressing).
    IrohListenerBound {
        node_id: NodeId,
    },
    PeerConnected {
        device_id: DeviceId,
        display_name: String,
    },
    PeerDisconnected {
        device_id: DeviceId,
    },
    #[allow(dead_code)] // surfaced through tracing today; reserved for the
    // pairing-flow toast banner once it lands
    PairingFailed {
        reason: String,
    },
    /// Emitted during a roost rescan. Lets the UI show a progress bar /
    /// "ingested N of M notes".
    ScanProgress {
        processed: usize,
        total: usize,
    },
    /// Emitted once scanning finishes. Carries the final count so the UI
    /// can log "scan complete, N notes in store".
    ScanComplete {
        count: usize,
    },
    /// A remote peer's edits were just written to the roost on disk.
    /// The UI should refresh its file tree — on Android SAF roosts the
    /// filesystem watcher is a no-op, so without this signal the sidebar
    /// would only pick up new files on window reopen.
    NoteWritten {
        path: String,
    },
    /// A remote peer just sent an attachment (image/audio blob) that we'd
    /// requested, and it's now on disk under `.attachments/`. The editor
    /// should re-decorate any embed for this path so a "not synced yet"
    /// placeholder flips to the real image — WITHOUT reloading the note
    /// (the markdown didn't change; reloading would drop the mobile
    /// keyboard). `rel` is the roost-relative attachment path.
    AttachmentWritten {
        rel: String,
    },
    /// A peer reported that a referenced attachment exceeds the sync cap
    /// (>8 MiB) and therefore will never arrive via sync. The editor shows
    /// an honest "too large to sync" notice instead of a perpetual "not
    /// synced yet". `rel` is the roost-relative attachment path.
    AttachmentTooLarge {
        rel: String,
    },
    /// A remote peer's `NoteDelete` tombstone was just applied — the
    /// file has been moved to trash (or removed where trash isn't
    /// available) and the docstore entry is gone. UI should drop the
    /// note from the file tree and close any open editor for it.
    NoteDeleted {
        path: String,
    },
    /// A specific note failed to sync/persist and the engine could not
    /// silently recover it (H-6). Covers the previously-invisible cases: a
    /// FullDoc that exceeds the frame cap (C-4), a save I/O failure (M-7), a
    /// parse/decode failure, a schema too new to load (C-5), a render-to-disk
    /// failure (L-1 on Windows), or a path collision (C-1). Lets the UI show a
    /// per-note "didn't sync" marker instead of a falsely-green indicator.
    /// `reason` is a short, user-facing category string.
    NoteSyncFailed {
        path: String,
        reason: String,
    },
    /// A known peer's dial state changed (dialing → handshaking →
    /// connected / failed). Drives the per-peer status dot in Settings.
    PeerStatusChanged {
        device_id: DeviceId,
        state: PeerConnectionState,
    },
    /// The durable outbox's pending count changed (a change was queued for
    /// retry, or one drained successfully). Lets the UI show a "syncing /
    /// N changes waiting" indicator without polling.
    OutboxChanged {
        total_pending: u32,
    },
    Stopped,
}

/// Current engine state snapshot for UI display. Fields are primitive /
/// serde-friendly so they can cross the Tauri bridge without conversion.
#[derive(Debug, Clone, serde::Serialize, specta::Type)]
pub struct SyncStatus {
    pub running: bool,
    pub listen_port: Option<u16>,
    pub our_device_id: String,
    pub our_fingerprint_words: String,
    pub connected_peers: Vec<ConnectedPeer>,
    pub known_peers: Vec<KnownPeer>,
    /// Whether the store-and-forward queue client is installed (the
    /// "queue active / off" indicator). Live-only / strict-P2P when false.
    pub queue_active: bool,
    /// Total undelivered outbound changes across all peers (durable outbox
    /// depth). A non-zero value means "changes are waiting to reach a peer".
    #[specta(type = u32)]
    pub total_pending_outbound: u32,
    /// PT-1: relay reachability. `None` = relay disabled (strict-P2P) or no
    /// endpoint bound yet; `Some(true)` = connected to a home relay;
    /// `Some(false)` = a relay is configured but currently unreachable — the
    /// "relay down" signal (cross-network reconnect is degraded, LAN still
    /// works). Surfaces the connectivity problem that used to be silent log spam.
    pub relay_connected: Option<bool>,
}

#[derive(Debug, Clone, serde::Serialize, specta::Type)]
pub struct ConnectedPeer {
    pub device_id: String,
    pub display_name: String,
}

#[derive(Debug, Clone, serde::Serialize, specta::Type)]
pub struct KnownPeer {
    pub device_id: String,
    pub display_name: String,
    #[specta(type = u32)]
    pub paired_at_millis: u64,
    #[specta(type = u32)]
    pub last_seen_millis: u64,
    /// Current connection state for this peer. The frontend renders
    /// this as a colored dot next to the peer's name; `Failed` sets
    /// the tooltip to the reason so the user can tell "can't reach"
    /// from "bad PIN" from "fingerprint mismatch".
    pub connection_state: PeerConnectionState,
    /// Whether we have enough info (addr + PIN) to redial without UI.
    /// Purely cosmetic for the UI — lets it surface a "Reconnect now"
    /// button only on peers that can actually be reconnected.
    pub can_auto_reconnect: bool,
    /// Local changes durably queued for this peer that haven't been
    /// delivered yet (the client outbox depth for this recipient).
    #[specta(type = u32)]
    pub pending_outbound: u32,
    /// Unix millis of the last successful *connection* to this peer this run
    /// (handshake completed) — NOT a convergence/queue-drain marker. `None`
    /// until first connect. Drives a "connected X ago" label; deliberately
    /// not called "synced" because pending changes may still be in flight
    /// (see `pending_outbound`).
    #[specta(type = Option<u32>)]
    pub last_connected_millis: Option<u64>,
    /// Reason the most recent reconnect attempt failed, if it did. Cleared
    /// on the next success. Surfaced as the status-dot tooltip.
    pub last_error: Option<String>,
    /// Unix millis when the engine's reconnect supervisor will next try
    /// this peer (while disconnected + backing off). `None` when connected
    /// or no retry is scheduled. Lets the UI show "retrying in Ns".
    #[specta(type = Option<u32>)]
    pub next_retry_millis: Option<u64>,
}
