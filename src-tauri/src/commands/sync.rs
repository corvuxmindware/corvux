//! Tauri IPC surface for the sync subsystem.
//!
//! The frontend never touches `sync::SyncEngine` directly — it calls these
//! commands, which thin-wrap the engine methods and translate errors to the
//! string form Tauri's bridge expects.
//!
//! # State model
//!
//! A single [`SyncEngineState`] is managed by Tauri. It holds an
//! `Option<SyncEngine>` because the engine isn't ready until a roost is
//! opened and the user activates sync — during that time, commands return
//! a "not initialized" error rather than a stub.
//!
//! # Error form
//!
//! Returned errors are plain `String`s. This is the Tauri convention and
//! keeps the frontend free of Rust-error-shape coupling. We pre-format the
//! `thiserror`-derived message so log messages and UI strings match.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use tauri::{AppHandle, Manager, State};
use tauri_specta::Event;
use tokio::sync::{mpsc, Mutex};
use tracing::info;

use crate::sync::crypto::fingerprint::Fingerprint;
use crate::sync::crypto::pairing::{PairingSecret, Pin};
use crate::sync::discovery::{DiscoveredPeer, Discovery, DiscoveryEvent};
use crate::sync::engine::{SyncEngine, SyncEvent, SyncStatus};
use crate::sync::identity::{DeviceId, Identity};
use crate::sync::transport::tcp_tls::TlsIdentity;

/// Wire shape of `SyncEvent` sent to the frontend via `sync://event`.
/// DeviceIds are hex strings (not raw byte arrays) so the UI can display
/// and compare them without conversion.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, specta::Type)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SyncEventPayload {
    ListenerBound {
        port: u16,
    },
    /// iroh endpoint bound — frontend renders the QR code from `node_id`.
    /// Hex-encoded so the wire format stays string-only across the Tauri
    /// bridge (matches how `device_id` is rendered in other variants).
    IrohListenerBound {
        node_id: String,
    },
    PeerConnected {
        device_id: String,
        display_name: String,
    },
    PeerDisconnected {
        device_id: String,
    },
    PairingFailed {
        reason: String,
    },
    ScanProgress {
        #[specta(type = u32)]
        processed: usize,
        #[specta(type = u32)]
        total: usize,
    },
    ScanComplete {
        #[specta(type = u32)]
        count: usize,
    },
    NoteWritten {
        path: String,
    },
    /// A requested attachment blob arrived from a peer and is on disk.
    /// `rel` is the roost-relative `.attachments/...` path; the editor
    /// re-decorates that embed without reloading the note.
    AttachmentWritten {
        rel: String,
    },
    /// A peer reported `rel` exceeds the sync cap — it won't arrive via sync.
    /// The editor shows an honest "too large to sync" notice for it.
    AttachmentTooLarge {
        rel: String,
    },
    NoteDeleted {
        path: String,
    },
    /// A specific note could not be synced/persisted and the engine couldn't
    /// silently recover it (H-6). The UI surfaces a per-note "didn't sync"
    /// marker for `path`; `reason` is a short user-facing category.
    NoteSyncFailed {
        path: String,
        reason: String,
    },
    PeerStatusChanged {
        device_id: String,
        state: crate::sync::engine::PeerConnectionState,
    },
    /// Durable outbox pending-count changed. `total_pending` is the number
    /// of (peer, note) deliveries still awaiting retry.
    OutboxChanged {
        total_pending: u32,
    },
    Stopped,
}

/// Shape of an mDNS-discovered peer sent to the frontend via
/// `sync://discovered`. Keeps the Tauri bridge free of Rust-flavored
/// types.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, specta::Type)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DiscoveryEventPayload {
    Found {
        device_id: String,
        display_name: String,
        roost_name: String,
        addr: String,
        port: u16,
    },
    Lost {
        device_id: String,
    },
}

impl From<&DiscoveredPeer> for DiscoveryEventPayload {
    fn from(p: &DiscoveredPeer) -> Self {
        DiscoveryEventPayload::Found {
            device_id: p.device_id.as_hex(),
            display_name: p.display_name.clone(),
            roost_name: p.roost_name.clone(),
            addr: p.addr.clone(),
            port: p.port,
        }
    }
}

impl From<SyncEvent> for SyncEventPayload {
    fn from(ev: SyncEvent) -> Self {
        match ev {
            SyncEvent::ListenerBound { port } => Self::ListenerBound { port },
            SyncEvent::IrohListenerBound { node_id } => Self::IrohListenerBound {
                node_id: node_id.to_string(),
            },
            SyncEvent::PeerConnected {
                device_id,
                display_name,
            } => Self::PeerConnected {
                device_id: device_id.as_hex(),
                display_name,
            },
            SyncEvent::PeerDisconnected { device_id } => Self::PeerDisconnected {
                device_id: device_id.as_hex(),
            },
            SyncEvent::PairingFailed { reason } => Self::PairingFailed { reason },
            SyncEvent::ScanProgress { processed, total } => Self::ScanProgress { processed, total },
            SyncEvent::ScanComplete { count } => Self::ScanComplete { count },
            SyncEvent::NoteWritten { path } => Self::NoteWritten { path },
            SyncEvent::AttachmentWritten { rel } => Self::AttachmentWritten { rel },
            SyncEvent::AttachmentTooLarge { rel } => Self::AttachmentTooLarge { rel },
            SyncEvent::NoteDeleted { path } => Self::NoteDeleted { path },
            SyncEvent::NoteSyncFailed { path, reason } => Self::NoteSyncFailed { path, reason },
            SyncEvent::PeerStatusChanged { device_id, state } => Self::PeerStatusChanged {
                device_id: device_id.as_hex(),
                state,
            },
            SyncEvent::OutboxChanged { total_pending } => Self::OutboxChanged { total_pending },
            SyncEvent::Stopped => Self::Stopped,
        }
    }
}

/// On-disk sync preferences. Lets us auto-resume hosting on next launch
/// without making the user re-enter settings every time. Written whenever
/// the user successfully starts hosting; flipped to `enabled = false` on
/// explicit `sync_stop` (not on window close — we WANT window close to
/// leave `enabled = true` so next launch resumes).
///
/// Stored as JSON at `app_data/sync/prefs.json`. Contains a PIN, so it
/// stays in app-private storage only. The PIN itself is a 6-digit pairing
/// artifact; the real trust root is the pinned TLS fingerprint in
/// `peer_store`.
/// Per-roost hosting config, so switching roosts doesn't lose each roost's
/// pairing PINs. Snapshotted into `SyncPrefs.per_roost` on every save and
/// restored by `enable_prefs`.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
struct RoostHosting {
    #[serde(default)]
    hosting_pin: String,
    #[serde(default)]
    iroh_hosting_pin: String,
    #[serde(default)]
    hosting_port: u16,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct SyncPrefs {
    #[serde(default)]
    enabled: bool,
    #[serde(default)]
    roost_path: String,
    #[serde(default)]
    device_name: String,
    #[serde(default)]
    hosting_port: u16,
    #[serde(default)]
    hosting_pin: String,
    /// PIN the user generated on the iroh "Show pairing code" panel.
    /// Persisted so `sync_try_autostart` can re-bind the iroh listener
    /// with the same PIN — without it, paired peers would dial in but
    /// fail the HMAC handshake on every restart, forcing a fresh QR
    /// pair flow even though the trust relationship survives.
    #[serde(default)]
    iroh_hosting_pin: String,
    /// Optional store-and-forward queue endpoint, e.g.
    /// `https://queue.meetcorvux.com`. When set + `strict_p2p_only` is
    /// false, the engine installs a `QueueClient` so local writes fan
    /// out to offline peers via the queue, and a periodic task pulls
    /// pending payloads addressed to this device.
    #[serde(default)]
    queue_endpoint: String,
    /// When true, the engine refuses to install a queue client even if
    /// `queue_endpoint` is set — strict P2P-only mode for users who
    /// don't want any Corvux-operated server in their sync path.
    #[serde(default)]
    strict_p2p_only: bool,
    /// Power-user escape hatch for self-hosted queue servers. A host (no
    /// scheme, no path) the user has explicitly confirmed twice via the
    /// `sync_pin_custom_queue_host` IPC. Cleared whenever the queue
    /// endpoint is set back to the default-allowlist host. Without this,
    /// only `meetcorvux.com` (and subdomains) are accepted by
    /// `sync_set_queue_endpoint`.
    #[serde(default)]
    pinned_queue_host: String,
    /// Per-roost hosting config keyed by normalized roost path. Lets a user
    /// who syncs multiple roosts keep each roost's pairing PINs — switching
    /// roosts no longer wipes the previous roost's PINs (which forced a
    /// re-pair on return). Snapshotted on every save; restored by enable_prefs.
    #[serde(default)]
    per_roost: std::collections::HashMap<String, RoostHosting>,
}

impl Default for SyncPrefs {
    /// Locked Decision #6 (re-reversed 2026-07-11): sync is **strict-P2P by
    /// default** — a fresh install talks only directly between the user's own
    /// devices (LAN + NAT-punched direct), with the vendor-operated relay and
    /// store-and-forward queue OFF until the user opts in. This `Default` is
    /// what a BRAND-NEW install gets (no prefs file → `load_sync_prefs` returns
    /// it). MIGRATION-SAFE: the field carries a bare `#[serde(default)]`, which
    /// deserializes an ABSENT field via `bool::default()` (= `false`), NOT this
    /// struct default — so an EXISTING user whose stored prefs predate the flip
    /// keeps relay/queue exactly as it was, rather than silently losing working
    /// sync on upgrade. Only genuinely new installs (and unreadable/corrupt
    /// prefs, which fall back to the safe default) start strict.
    fn default() -> Self {
        Self {
            enabled: false,
            roost_path: String::new(),
            device_name: String::new(),
            hosting_port: 0,
            hosting_pin: String::new(),
            iroh_hosting_pin: String::new(),
            queue_endpoint: String::new(),
            strict_p2p_only: true,
            pinned_queue_host: String::new(),
            per_roost: std::collections::HashMap::new(),
        }
    }
}

// Q-03: queue endpoint URL validation lives in
// `commands::sync_endpoint`. Re-imported here so the existing call
// sites (`sync_set_queue_endpoint` below) read identically.
use super::sync_endpoint::validate_queue_endpoint;

#[cfg(test)]
mod strict_p2p_default_tests {
    //! Locked Decision #6 re-reversal (2026-07-11): strict-P2P by default for
    //! NEW installs, without silently flipping EXISTING users on upgrade.
    use super::*;

    #[test]
    fn new_install_defaults_to_strict_p2p() {
        // No prefs file → `load_sync_prefs` returns `SyncPrefs::default()`.
        assert!(
            SyncPrefs::default().strict_p2p_only,
            "a brand-new install must default to strict P2P (relay/queue off)"
        );
    }

    #[test]
    fn existing_prefs_without_the_field_stay_non_strict() {
        // An existing user's stored prefs predate the flag (field absent). The
        // bare `#[serde(default)]` must deserialize it as `false` — preserving
        // their working relay/queue — NOT inherit the new struct default.
        let prefs: SyncPrefs = serde_json::from_str("{}").unwrap();
        assert!(
            !prefs.strict_p2p_only,
            "an existing prefs file missing the field must NOT be flipped to strict on upgrade"
        );
    }

    #[test]
    fn explicit_strict_setting_is_honored_both_ways() {
        let on: SyncPrefs = serde_json::from_str(r#"{"strict_p2p_only":true}"#).unwrap();
        let off: SyncPrefs = serde_json::from_str(r#"{"strict_p2p_only":false}"#).unwrap();
        assert!(on.strict_p2p_only);
        assert!(
            !off.strict_p2p_only,
            "an explicit opt-in to relay/queue must stick"
        );
    }
}

#[cfg(test)]
mod resolve_pin_tests {
    //! AUDIT-003b: cover the policy that decides whether a tcp_tls dial
    //! is allowed. The previous behaviour (caller passes None →
    //! `PinnedVerifier::new(None)` → accept any cert) is now provably
    //! unreachable: the policy returns `Err` for every "no known pin"
    //! shape, so the rustls verifier never sees a no-pin construction.

    use super::*;
    use crate::sync::store::peer_store::PeerRecord;

    fn fake_peer(addr: Option<&str>, fingerprint: [u8; 32]) -> PeerRecord {
        let mut r = PeerRecord::new(
            crate::sync::identity::DeviceId([1u8; 16]),
            fingerprint,
            "fake".into(),
        );
        r.last_known_addr = addr.map(String::from);
        r
    }

    fn addr() -> SocketAddr {
        "192.168.1.5:7337".parse().unwrap()
    }

    #[test]
    fn unknown_addr_is_rejected_with_pointer_to_iroh() {
        let peers: Vec<PeerRecord> = vec![];
        let err = lookup_pinned_fp_for_addr(&peers, addr()).unwrap_err();
        assert!(err.contains("no paired peer"), "got: {err}");
        assert!(err.contains("sync_connect_iroh"), "got: {err}");
    }

    #[test]
    fn known_addr_returns_stored_fingerprint() {
        let mut fp = [0u8; 32];
        fp[0] = 0xAB;
        let peers = vec![fake_peer(Some("192.168.1.5:7337"), fp)];
        let got = lookup_pinned_fp_for_addr(&peers, addr()).unwrap();
        assert_eq!(got.0, fp);
    }

    #[test]
    fn iroh_only_peer_zero_fingerprint_is_rejected() {
        // Peers paired via iroh have a zeroed `fingerprint` in
        // peer_store — the tcp_tls layer never observed their cert.
        // Dialing them via tcp_tls would have to TOFU-accept, which
        // is exactly the vulnerability we're closing.
        let peers = vec![fake_peer(Some("192.168.1.5:7337"), [0u8; 32])];
        let err = lookup_pinned_fp_for_addr(&peers, addr()).unwrap_err();
        assert!(err.contains("iroh-paired"), "got: {err}");
        assert!(err.contains("sync_connect_iroh"), "got: {err}");
    }

    #[test]
    fn addr_mismatch_falls_through_to_unknown() {
        // Peer is paired but at a different addr — we won't dial them
        // here. (mDNS will rediscover the real addr and try again.)
        let mut fp = [0u8; 32];
        fp[0] = 0xCD;
        let peers = vec![fake_peer(Some("10.0.0.99:7337"), fp)];
        let err = lookup_pinned_fp_for_addr(&peers, addr()).unwrap_err();
        assert!(err.contains("no paired peer"), "got: {err}");
    }

    #[test]
    fn peer_with_no_known_addr_is_invisible_to_lookup() {
        // last_known_addr=None means "never dialed" — those peers can't
        // be auto-resolved by addr.
        let mut fp = [0u8; 32];
        fp[0] = 0xEF;
        let peers = vec![fake_peer(None, fp)];
        let err = lookup_pinned_fp_for_addr(&peers, addr()).unwrap_err();
        assert!(err.contains("no paired peer"), "got: {err}");
    }

    #[test]
    fn multiple_peers_returns_the_one_matching_addr() {
        // Realistic store: a few paired devices, only one at the
        // dialed addr. Lookup must pick the right one.
        let mut fp_a = [0u8; 32];
        fp_a[0] = 0xAA;
        let mut fp_b = [0u8; 32];
        fp_b[0] = 0xBB;
        let mut fp_c = [0u8; 32];
        fp_c[0] = 0xCC;
        let peers = vec![
            fake_peer(Some("10.0.0.1:7337"), fp_a),
            fake_peer(Some("192.168.1.5:7337"), fp_b),
            fake_peer(Some("172.16.0.10:7337"), fp_c),
        ];
        let got = lookup_pinned_fp_for_addr(&peers, addr()).unwrap();
        assert_eq!(got.0, fp_b);
    }

    #[test]
    fn same_sync_roost_normalizes_separators_and_trailing_slash() {
        // A roost path that round-trips through different representations
        // (forward vs back slash, trailing slash) must still compare equal —
        // otherwise resume silently fails when the reopen path drifts.
        assert!(same_sync_roost(
            "W:/notes/Brain-Dump",
            "W:\\notes\\Brain-Dump"
        ));
        assert!(same_sync_roost("/home/me/roost/", "/home/me/roost"));
        assert!(same_sync_roost("W:/a/b", "W:/a/b"));
        // Genuinely different roosts must not collide.
        assert!(!same_sync_roost("W:/a/Brain-Dump", "W:/a/test-roost"));
    }

    #[test]
    fn enable_prefs_same_roost_preserves_hosting_config() {
        // Re-enabling the SAME roost keeps its hosting PIN/port/queue config
        // so a paired roost doesn't lose its pairing just by toggling enable.
        let existing = SyncPrefs {
            enabled: false,
            roost_path: "W:/v/Brain-Dump".into(),
            device_name: "Old name".into(),
            hosting_port: 7337,
            hosting_pin: "424242".into(),
            iroh_hosting_pin: "131313".into(),
            queue_endpoint: "https://queue.meetcorvux.com".into(),
            strict_p2p_only: true,
            pinned_queue_host: String::new(),
            per_roost: Default::default(),
        };
        let prefs = enable_prefs(existing, "W:\\v\\Brain-Dump", "New name");
        assert!(prefs.enabled, "enabling sets enabled=true");
        assert_eq!(prefs.device_name, "New name", "device name is updated");
        assert_eq!(prefs.hosting_pin, "424242", "same-roost keeps hosting PIN");
        assert_eq!(
            prefs.iroh_hosting_pin, "131313",
            "same-roost keeps iroh PIN"
        );
        assert!(prefs.strict_p2p_only, "same-roost keeps queue prefs");
    }

    #[test]
    fn enable_prefs_different_roost_starts_fresh() {
        // Global prefs track one roost; enabling a DIFFERENT roost must not
        // carry the prior roost's hosting PIN over to it.
        let existing = SyncPrefs {
            enabled: true,
            roost_path: "W:/v/Brain-Dump".into(),
            hosting_pin: "424242".into(),
            iroh_hosting_pin: "131313".into(),
            ..Default::default()
        };
        let prefs = enable_prefs(existing, "W:/v/test-roost", "Dev");
        assert!(prefs.enabled);
        assert_eq!(prefs.roost_path, "W:/v/test-roost");
        assert_eq!(prefs.hosting_pin, "", "different roost drops the old PIN");
        assert_eq!(
            prefs.iroh_hosting_pin, "",
            "different roost drops the old iroh PIN"
        );
    }

    #[test]
    fn enable_prefs_restores_per_roost_pins_on_switch_back() {
        // Sync roost A (its PIN gets recorded in per_roost, which is what
        // save_sync_prefs snapshots).
        let mut a = enable_prefs(SyncPrefs::default(), "W:/roots/A", "dev");
        a.iroh_hosting_pin = "111111".into();
        a.per_roost.insert(
            normalize_roost_key("W:/roots/A"),
            RoostHosting {
                iroh_hosting_pin: "111111".into(),
                ..Default::default()
            },
        );
        // Switch to roost B (never synced): fresh PIN, but the map is retained.
        let b = enable_prefs(a, "W:/roots/B", "dev");
        assert_eq!(b.iroh_hosting_pin, "", "a never-synced roost starts fresh");
        assert!(
            b.per_roost.contains_key(&normalize_roost_key("W:/roots/A")),
            "per-roost map is carried across the switch"
        );
        // Switch back to A: its PIN is RESTORED, not wiped (the PT-20 fix).
        let back = enable_prefs(b, "W:/roots/A", "dev");
        assert_eq!(
            back.iroh_hosting_pin, "111111",
            "switching back to a previously-synced roost restores its PIN"
        );
    }

    #[test]
    fn should_resume_only_for_enabled_matching_roost() {
        let base = SyncPrefs {
            enabled: true,
            roost_path: "W:/v/Brain-Dump".into(),
            ..Default::default()
        };
        // Happy path: enabled + same roost (slash-normalized) resumes.
        assert!(should_resume(&base, "W:\\v\\Brain-Dump"));
        // Regression for the reported bug: enable was clicked but never
        // persisted enabled=true → must NOT resume (and we now DO persist it,
        // so this guards the gate, not the persistence).
        let disabled = SyncPrefs {
            enabled: false,
            ..base.clone()
        };
        assert!(!should_resume(&disabled, "W:/v/Brain-Dump"));
        // Enabled, but a different roost is open → don't resume.
        assert!(!should_resume(&base, "W:/v/test-roost"));
    }

    #[test]
    fn roost_fs_matches_path_detects_cross_roost_switch() {
        use crate::roostfs::native::NativeFs;
        use tempfile::TempDir;
        // A backend mounted on roost A must be accepted for A and REJECTED for a
        // different roost B — the exact predicate that stops sync_init_inner
        // from building an engine whose roost_path and RoostFs disagree after a
        // fire-and-forget roost switch (cross-roost data bleed).
        let a = TempDir::new().expect("tempdir a");
        let b = TempDir::new().expect("tempdir b");
        let fs_a = NativeFs::new(a.path());
        assert!(
            roost_fs_matches_path(&fs_a, &a.path().to_string_lossy()),
            "backend for A must match roost A"
        );
        assert!(
            !roost_fs_matches_path(&fs_a, &b.path().to_string_lossy()),
            "backend for A must be rejected for roost B (switch race)"
        );
    }
}

/// Roost-switch race guard (`sync_init_inner` aborts if `switch_generation`
/// moved between its entry snapshot and the pre-install re-check under the
/// engine lock). The `AppHandle`-driven end-to-end path isn't reachable from
/// a unit test, so this locks in just the counter mechanics the guard relies
/// on: a fresh state starts at 0, and every `stop_sync_for_roost_switch` bump
/// (`fetch_add`) is visible to a later `load` — i.e. no roost switch can be
/// silently lost between the snapshot and the re-check.
#[cfg(test)]
mod switch_generation_tests {
    use super::*;
    use std::sync::atomic::Ordering;

    #[test]
    fn fresh_state_starts_at_zero() {
        let state = SyncEngineState::default();
        assert_eq!(state.switch_generation.load(Ordering::SeqCst), 0);
    }

    /// The legacy TCP-TLS autostart listener must stay OFF — it's a dead,
    /// unauthenticatable path that could only reject inbound with PairingClosed.
    /// This locks that so reviving it is a deliberate, test-breaking change.
    #[test]
    fn legacy_tcp_listener_autostart_is_disabled() {
        assert!(
            !should_autostart_legacy_tcp_listener(),
            "the legacy TCP-TLS listener must not be launched on autostart"
        );
    }

    #[test]
    fn fetch_add_increments_are_observed_by_a_later_load() {
        let state = SyncEngineState::default();
        // Mirrors `stop_sync_for_roost_switch`'s unconditional bump.
        state.switch_generation.fetch_add(1, Ordering::SeqCst);
        assert_eq!(
            state.switch_generation.load(Ordering::SeqCst),
            1,
            "a single switch must advance the generation by exactly one"
        );
        state.switch_generation.fetch_add(1, Ordering::SeqCst);
        state.switch_generation.fetch_add(1, Ordering::SeqCst);
        assert_eq!(
            state.switch_generation.load(Ordering::SeqCst),
            3,
            "each switch bumps the generation, so a stale `gen_at_start` \
             snapshot can never equal the post-switch value"
        );
    }
}

fn sync_prefs_path(app: &AppHandle) -> Result<PathBuf, String> {
    let app_data = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("resolving app data dir: {e}"))?;
    Ok(app_data.join("sync").join("prefs.json"))
}

fn load_sync_prefs(app: &AppHandle) -> SyncPrefs {
    let path = match sync_prefs_path(app) {
        Ok(p) => p,
        Err(_) => return SyncPrefs::default(),
    };
    let Ok(bytes) = std::fs::read(&path) else {
        return SyncPrefs::default();
    };
    serde_json::from_slice::<SyncPrefs>(&bytes).unwrap_or_default()
}

fn save_sync_prefs(app: &AppHandle, prefs: &SyncPrefs) -> Result<(), String> {
    let path = sync_prefs_path(app)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("creating sync dir: {e}"))?;
    }
    // Snapshot the current roost's hosting config into the per-roost map
    // before persisting, so switching roosts (which resets the top-level
    // fields via enable_prefs) can restore it instead of forcing a re-pair.
    let mut prefs = prefs.clone();
    if !prefs.roost_path.is_empty() {
        let key = normalize_roost_key(&prefs.roost_path);
        prefs.per_roost.insert(
            key,
            RoostHosting {
                hosting_pin: prefs.hosting_pin.clone(),
                iroh_hosting_pin: prefs.iroh_hosting_pin.clone(),
                hosting_port: prefs.hosting_port,
            },
        );
    }
    let bytes = serde_json::to_vec_pretty(&prefs).map_err(|e| format!("serializing prefs: {e}"))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &bytes).map_err(|e| format!("writing prefs: {e}"))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("renaming prefs: {e}"))?;
    Ok(())
}

/// Forward every `SyncEvent` from the engine's broadcast channel to the
/// frontend as a `sync://event` Tauri event. The task exits when the engine
/// is dropped (Sender closed), so no explicit shutdown is needed.
fn spawn_event_bridge(app: AppHandle, engine: &SyncEngine) {
    use tokio::sync::broadcast::error::RecvError;
    let mut rx = engine.events();
    tauri::async_runtime::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(ev) => {
                    let payload: SyncEventPayload = ev.into();
                    if let Err(e) = crate::events::SyncStreamEvent(payload).emit(&app) {
                        tracing::warn!(error = %e, "sync: failed to emit event to frontend");
                    }
                }
                Err(RecvError::Lagged(n)) => {
                    // DEFERRED (#6c): the buffer overflowed and dropped `n`
                    // events, so incremental UI state can be stale until the
                    // next event. The fix is to emit a coalesced resync signal
                    // (new SyncEventPayload variant) the frontend reacts to — but
                    // that adds a wire variant, which requires regenerating the
                    // checked-in bindings.ts, and gen_bindings can't run on the
                    // Windows dev box (STATUS_ENTRYPOINT_NOT_FOUND). Deferred
                    // until it can be regenerated + verified on Linux/CI.
                    tracing::warn!(dropped = n, "sync: event bridge lagged");
                }
                Err(RecvError::Closed) => break,
            }
        }
    });
}

/// Tauri state: lazily-initialized engine + discovery.
///
/// `engine` and `discovery` are both `Option`s because neither is ready
/// until the user opens a roost and enables sync. They are torn down
/// together via `sync_stop` so there's never a stale mDNS announcement
/// pointing at a dead engine.
#[derive(Default)]
pub struct SyncEngineState {
    pub engine: Mutex<Option<SyncEngine>>,
    pub discovery: Mutex<Option<Discovery>>,
    /// Task that watches the bound iroh endpoint's direct-address changes
    /// (`Endpoint::watch_addr`) and re-broadcasts fresh addrs over mDNS, so
    /// the LAN fast path survives iroh discovering/changing its addresses
    /// after the one-shot listen-time announce. Holds an `Endpoint` clone
    /// (which pins the endpoint), so it's aborted explicitly on stop rather
    /// than relying on the watcher stream ending.
    pub reannounce_task: Mutex<Option<tauri::async_runtime::JoinHandle<()>>>,
    /// Monotonic counter bumped on every roost switch (`stop_sync_for_roost_switch`).
    /// A cheap early-abort: `sync_init_inner` snapshots it at entry and re-checks
    /// it at commit, so a switch that bumped it after the snapshot is caught even
    /// when the (rare) failed switch leaves NO backend mounted to compare against.
    /// It is NOT sufficient on its own — a single bump can't cover an init that
    /// snapshotted *mid-switch*, which is why the authoritative gate is
    /// `lifecycle` below re-validating the live backend.
    pub switch_generation: std::sync::atomic::AtomicU64,
    /// Serializes the roost-switch teardown (stop engine + swap `RoostFs`
    /// backend) against the engine-install commit in `sync_init_inner`.
    ///
    /// The switch (in `open_roost_impl`) holds this across BOTH tearing the old
    /// engine down AND swapping the backend. `sync_init_inner` holds it only
    /// across the short commit — re-read the live backend, re-check the
    /// generation, install — NOT the long async engine build. That makes the two
    /// mutually exclusive: an init can only commit when no switch is
    /// mid-transition, and it re-validates the *live* backend under this lock, so
    /// a stale engine can never be installed for a roost the user already left.
    /// This closes the window the generation counter alone can't (an init that
    /// snapshotted the generation mid-switch, old backend still mounted).
    ///
    /// Lock order is always init → lifecycle → (engine | RoostFsState) on both
    /// paths, and neither path holds engine and RoostFsState simultaneously
    /// (`RoostFsState::current` returns an owned `Arc` and drops its guard before
    /// the engine lock), so there is no inversion.
    pub lifecycle: Mutex<()>,
    /// Fix #5: serializes the WHOLE `sync_init_inner` body so two concurrent
    /// inits can't both build an engine, scan/write the same store, and start
    /// mDNS discovery before either commits. Held from entry (before the
    /// idempotence check, so a same-roost double-fire returns the first engine's
    /// status instead of building a second) through the commit. Lock order is
    /// `init → lifecycle → (engine | RoostFsState)`; the roost-switch path uses
    /// the same order and calls the `_init_locked` shutdown helper.
    pub init: Mutex<()>,
}

/// Acquire the Android `WifiManager.MulticastLock` for the duration of
/// the sync session. Without it Android filters incoming multicast
/// packets and our mDNS browser sees zero peers.
///
/// Stub on desktop / iOS — those targets don't filter multicast and
/// the plugin's mobile binding isn't registered there. Errors are
/// logged but never propagated; mDNS works degraded (manual IP) if the
/// lock fails.
#[cfg(target_os = "android")]
fn discover_acquire_lock(app: &AppHandle) {
    use tauri_plugin_multicast_lock::MulticastLockExt;
    if let Err(e) = app.multicast_lock().acquire() {
        tracing::warn!(error = %e, "sync: MulticastLock acquire failed; mDNS may see no peers");
    }
}
#[cfg(not(target_os = "android"))]
fn discover_acquire_lock(_app: &AppHandle) {}

/// Release the Android MulticastLock. Mirror of [`discover_acquire_lock`].
#[cfg(target_os = "android")]
fn discover_release_lock(app: &AppHandle) {
    use tauri_plugin_multicast_lock::MulticastLockExt;
    if let Err(e) = app.multicast_lock().release() {
        tracing::warn!(error = %e, "sync: MulticastLock release failed");
    }
}
#[cfg(not(target_os = "android"))]
fn discover_release_lock(_app: &AppHandle) {}

/// Spawn (replacing any prior) the mDNS re-announce watcher. iroh discovers
/// its own direct addresses asynchronously *after* bind, and they change when
/// the device moves networks — so the one-shot announce at listen time can be
/// empty or go stale, leaving the LAN fast path dark. This task watches the
/// endpoint's address via `Endpoint::watch_addr` and re-announces the fresh
/// `iroh_addrs` over mDNS whenever the set changes (skipping no-op churn).
///
/// The task holds an `Endpoint` clone, which keeps the endpoint alive, so the
/// watcher stream never self-terminates — it is aborted explicitly on stop
/// (`sync_stop` / `sync_shutdown_without_disabling`).
async fn spawn_reannounce_task(
    state: &SyncEngineState,
    endpoint: iroh::Endpoint,
    discovery: Discovery,
    display_name: String,
    roost_name: String,
    port: u16,
) {
    let handle = tauri::async_runtime::spawn(async move {
        use futures_util::StreamExt;
        use iroh::Watcher as _;
        let node_id = endpoint.id().to_string();
        let mut last: Option<std::collections::BTreeSet<std::net::SocketAddr>> = None;
        let mut stream = endpoint.watch_addr().stream();
        while let Some(addr) = stream.next().await {
            let addrs: Vec<std::net::SocketAddr> = addr.ip_addrs().copied().collect();
            let set: std::collections::BTreeSet<std::net::SocketAddr> =
                addrs.iter().copied().collect();
            // Skip re-registering identical data -- avoids churning mdns-sd.
            if last.as_ref() == Some(&set) {
                continue;
            }
            if let Err(e) = discovery
                .announce(&display_name, &roost_name, port, &node_id, &addrs)
                .await
            {
                tracing::debug!(error = %e, "sync: mDNS re-announce on addr change failed");
            }
            last = Some(set);
        }
    });
    if let Some(prev) = state.reannounce_task.lock().await.replace(handle) {
        prev.abort();
    }
}

/// Bridge `DiscoveryEvent`s from the mDNS browser to the frontend as
/// `sync://discovered` Tauri events, AND feed any iroh identity the peer
/// advertised into the engine's LAN `MemoryLookup` so the outbound iroh
/// dialer can resolve it without depending on stale ticket addrs or
/// internet pkarr discovery (iroh 1.0.1 has no LAN/mDNS discovery of its
/// own -- see `SyncEngine::feed_lan_endpoint`). Spawned once per
/// `sync_init` call.
fn spawn_discovery_bridge(
    app: AppHandle,
    engine: SyncEngine,
    mut rx: mpsc::UnboundedReceiver<DiscoveryEvent>,
) {
    tauri::async_runtime::spawn(async move {
        while let Some(ev) = rx.recv().await {
            let payload = match &ev {
                DiscoveryEvent::Found(peer) => {
                    // Best-effort: an older peer, or one whose iroh listener
                    // had not bound yet at announce time, simply carries no
                    // hint here -- it still shows up in the UI via the event
                    // below, just without the LAN-fed iroh fast path.
                    if let Some(node_id_str) = peer.node_id.as_deref() {
                        if !peer.iroh_addrs.is_empty() {
                            match node_id_str.parse::<iroh::EndpointId>() {
                                Ok(node_id) => {
                                    // Gate the feed: only trust a LAN advertisement's
                                    // (NodeId -> addr) hint for a peer we actually
                                    // paired with AND whose advertised device_id is
                                    // welded to the NodeId (AUDIT-081). This bounds
                                    // the address lookup to real pairings and removes
                                    // the rogue-advertiser injection / reflection
                                    // surface. Address resolution only -- auth is
                                    // unchanged (NodeId QUIC handshake + PIN/token).
                                    if !engine.is_paired(peer.device_id).await {
                                        tracing::debug!(
                                            "sync: discovered peer is not paired -- skipping LAN address feed"
                                        );
                                    } else if crate::sync::transport::iroh::verify_node_device_binding(
                                        peer.device_id,
                                        node_id,
                                    )
                                    .is_err()
                                    {
                                        tracing::debug!(
                                            "sync: discovered peer device_id/node_id binding mismatch -- skipping LAN address feed"
                                        );
                                    } else {
                                        engine.feed_lan_endpoint(
                                            node_id,
                                            peer.iroh_addrs.iter().copied(),
                                        );
                                    }
                                }
                                Err(e) => {
                                    tracing::debug!(error = %e, "sync: discovered peer's node_id did not parse -- skipping LAN address feed");
                                }
                            }
                        }
                    }
                    DiscoveryEventPayload::from(peer)
                }
                DiscoveryEvent::Lost { device_id } => DiscoveryEventPayload::Lost {
                    device_id: device_id.as_hex(),
                },
            };
            if let Err(e) = crate::events::SyncDiscoveredEvent(payload).emit(&app) {
                tracing::warn!(error = %e, "sync: failed to emit discovery event");
            }
        }
    });
}

/// iroh identity + direct UDP addresses to fold into the mDNS TXT record
/// via `Discovery::announce`, or empty values when the iroh listener has
/// not bound yet at announce time (e.g. this call races
/// `start_listening_iroh`, or the user has not started iroh hosting this
/// session). `Discovery::announce` treats an empty `node_id` as "omit the
/// iroh TXT keys entirely", so older/still-starting peers degrade to the
/// pre-iroh-LAN-lookup behaviour rather than advertising bogus data.
async fn iroh_announce_fields(engine: &SyncEngine) -> (String, Vec<SocketAddr>) {
    match engine.iroh_endpoint_addr().await {
        Some((node_id, addrs)) => (node_id.to_string(), addrs),
        None => (String::new(), Vec::new()),
    }
}

/// Install default TLS crypto provider once per process. `rustls` 0.23
/// demands this before any TLS operation; idempotent after first call.
fn install_crypto_provider() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// True when the installed RoostFs backend belongs to the roost we were asked
/// to init sync for. Guards the cross-roost-switch race in `sync_init_inner`:
/// the frontend startup sequence is fire-and-forget, so a stale sequence can
/// call autostart for roost A after the user switched to B, by which point
/// `roost_fs_state.current()` is B's backend. Building an engine with a
/// (path=A, backend=B) mismatch would write A's incoming peer notes through B
/// — cross-roost data bleed. Reuses `same_roost_root` so path normalization
/// (trailing slash, separators, host-specific case folding) matches roost open.
pub(crate) fn roost_fs_matches_path(fs: &dyn crate::roostfs::RoostFs, roost_path: &str) -> bool {
    crate::commands::roost::same_roost_root(&fs.root_id(), std::path::Path::new(roost_path))
}

/// Shared implementation behind `sync_init` and `sync_try_autostart`.
/// Takes the plain types the IPC surface and the autostart path both have,
/// so we don't duplicate the bring-up sequence (identity → TLS → engine →
/// watcher → discovery) in two places.
async fn sync_init_inner(
    app: &AppHandle,
    state: &SyncEngineState,
    roost_fs_state: &crate::commands::files::RoostFsState,
    roost_path: &str,
    device_name: &str,
) -> Result<SyncStatus, String> {
    install_crypto_provider();

    // Fix #5: single-flight the entire init. Two concurrent inits (re-tap
    // "Enable sync", or the fire-and-forget startup sequence double-firing)
    // would otherwise BOTH build an engine, scan+write the same DocStore, and
    // start mDNS discovery before either committed. Holding this across the whole
    // body — including the idempotence check below — makes them serialize: a
    // same-roost double-fire returns the first engine's status without building a
    // second, and a different-roost race runs strictly after the first commits.
    let _init_guard = state.init.lock().await;

    // Idempotence + leak guard: if an engine is already running for THIS roost,
    // return its status instead of building a second one. A repeat init — the
    // user re-taps "Enable sync", or the fire-and-forget startup sequence
    // double-fires — would otherwise construct a whole new engine (watcher,
    // dial supervisor, queue puller) and the install below would drop the old
    // handle WITHOUT calling stop(). Its background tasks hold their own Arc
    // clones and only stop() sets the kill-latch + aborts them, so the old
    // engine would outlive its handle: two engines on one roost, double-dialing
    // the same peers and both writing the sync-state files. The stop-before-
    // install guard at the install site covers the different-roost case.
    {
        let existing = state.engine.lock().await.as_ref().cloned();
        if let Some(existing) = existing {
            if same_sync_roost(&existing.roost_path(), roost_path) {
                tracing::info!(
                    roost = %roost_path,
                    "sync_init: engine already running for this roost — returning current status (no rebuild)"
                );
                return Ok(existing.status().await);
            }
        }
    }

    // Snapshot the roost-switch generation at entry. This init is fire-and-
    // forget with no cancellation, so a switch can land at any point before we
    // install the engine below. We re-read this under the engine lock right
    // before install and abort if it moved. Captured as early as possible so
    // the detection window covers the whole init, not just the tail.
    let gen_at_start = state
        .switch_generation
        .load(std::sync::atomic::Ordering::SeqCst);

    let app_data = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("resolving app data dir: {e}"))?;
    tracing::info!(app_data = %app_data.display(), roost = %roost_path, "sync_init: starting");
    let sync_dir = app_data.join("sync");
    tracing::info!(sync_dir = %sync_dir.display(), "sync_init: creating sync_dir");
    std::fs::create_dir_all(&sync_dir).map_err(|e| {
        tracing::warn!(dir = %sync_dir.display(), error = %e, "sync_init: create_dir_all sync_dir failed");
        format!("creating sync dir at {}: {e}", sync_dir.display())
    })?;

    let identity =
        Arc::new(Identity::load_or_create(&sync_dir).map_err(|e| format!("identity: {e}"))?);
    let tls = Arc::new(load_or_create_tls(&sync_dir).map_err(|e| format!("tls: {e}"))?);

    // Sync state (Automerge docs + paired-peer registry) lives in the
    // app-private data dir keyed by roost path. NOT inside the roost — SAF
    // roosts on Android are read-only via std::fs (EROFS), and even on
    // desktop the Automerge binaries are device-local (should never sync
    // through Syncthing into another device's state tree).
    let state_dir = crate::paths::roost_db_dir(app, roost_path)
        .map_err(|e| format!("state dir: {e}"))?
        .join("sync");
    tracing::info!(state_dir = %state_dir.display(), "sync_init: resolved state_dir");

    // Grab the currently-installed RoostFs backend so received notes are
    // written through the same abstraction the rest of the app uses
    // (NativeFs on desktop, SafFs on Android). `None` if no roost has
    // been opened yet — sync_init should still succeed; writes fall back
    // to std::fs and will warn on SAF.
    let roost_fs = roost_fs_state.current().await;

    // AUDIT: guard against a stale roost-switch race. The frontend startup
    // sequence is fire-and-forget with no cancellation, so if roost A's
    // sequence lands here *after* the user switched to roost B,
    // `roost_fs_state.current()` is now B's backend while `roost_path` still
    // names A. Building the engine with a mismatched (path, backend) pair
    // would write A's incoming peer notes through B's RoostFs — cross-roost
    // data bleed. If a backend is installed and it no longer matches the
    // roost we were asked to init, abort instead of installing a mismatched
    // engine. `roost_fs == None` (no roost opened yet) is left untouched:
    // sync_init still succeeds and writes fall back to std::fs.
    if let Some(fs) = &roost_fs {
        if !roost_fs_matches_path(fs.as_ref(), roost_path) {
            tracing::warn!(
                requested = %roost_path,
                active = %fs.root_id(),
                "sync_init: aborting — active roost backend no longer matches requested roost (switch race)"
            );
            return Err("sync_init aborted: active roost changed".into());
        }
    }

    let engine = SyncEngine::new(
        PathBuf::from(roost_path),
        state_dir.clone(),
        roost_fs,
        identity.clone(),
        tls,
        device_name.to_string(),
    )
    .map_err(|e| {
        tracing::warn!(state_dir = %state_dir.display(), error = %e, "sync_init: SyncEngine::new failed");
        format!("engine init: {e}")
    })?;
    // Honor strict-P2P before any listener binds: no Corvux/Number0 relay in
    // the path. (Discovery still publishes our NodeAddr to the n0 DNS namespace
    // — a smaller, address-only surface — tracked as a separate follow-up.)
    engine.set_relay_disabled(load_sync_prefs(app).strict_p2p_only);
    // Attach the event bridge BEFORE starting the watcher. `start_watcher`
    // kicks off a background `scan_roost` that emits `ScanProgress` /
    // `ScanComplete` events; if the bridge isn't already subscribed,
    // early progress ticks are dropped (broadcast channels don't replay).
    spawn_event_bridge(app.clone(), &engine);
    engine
        .start_watcher()
        .await
        .map_err(|e| format!("watcher: {e}"))?;

    // NOTE (fix #5): LAN discovery (`Discovery::start` + `browse` + the
    // discovery bridge) is started AFTER the engine-install commit below, not
    // here. Starting it pre-commit meant a *losing* init — one that aborts at
    // the commit because the roost switched underneath it — had already opened
    // the mDNS daemon socket and begun browsing, publishing/scanning on behalf
    // of an engine that never got installed. Deferring it past the commit means
    // only the winning init ever touches discovery.

    // Install the queue client if prefs say to. Strict-P2P-only or
    // missing endpoint = no client (engine drops back to live transport
    // only). Periodic pull is spun up by `set_queue_client` itself.
    let prefs = load_sync_prefs(app);
    if !prefs.strict_p2p_only && !prefs.queue_endpoint.is_empty() {
        match crate::sync::queue::client::QueueClient::new(
            &prefs.queue_endpoint,
            engine.identity_arc(),
        ) {
            Ok(c) => engine.set_queue_client(Some(Arc::new(c))).await,
            Err(e) => tracing::warn!(error = %e, "sync_init: queue client construction failed"),
        }
    }

    // Start the engine-owned reconnect supervisor so periodic redial runs
    // in the backend (survives webview suspend/kill on mobile) instead of
    // depending on a frontend setInterval.
    engine.start_reconnect_supervisor().await;

    let status = engine.status().await;
    // Fix #5: assigned the installed engine's handle inside the commit block iff
    // this init wins. Every abort path `return`s before the assignment, so the
    // only code path that reaches the post-commit use below is the winning one.
    let engine_for_discovery: SyncEngine;
    // Atomic install commit. Hold `lifecycle` across the whole check-and-install
    // so no roost switch can interleave: the switch holds this same lock across
    // its teardown + backend swap (see `open_roost_impl`), so while we hold it
    // the mounted backend is guaranteed stable (never mid-transition).
    //
    // Two independent aborts:
    //  - LIVE-BACKEND re-check (authoritative): re-read the currently-mounted
    //    RoostFs and reject if it no longer matches `roost_path`. This catches
    //    the interleaving the generation counter alone can't — an init that
    //    snapshotted the generation *mid-switch* (old backend still mounted at
    //    the early guard) would otherwise install a stale engine after the swap
    //    completed. Under `lifecycle` the backend we read here is the settled
    //    post-switch value, so a mismatch is decisive.
    //  - GENERATION re-check (cheap backstop): catches a switch that bumped the
    //    generation but ended with NO backend mounted (e.g. the new roost failed
    //    to open), where the live-backend check has nothing to compare against.
    //
    // On either, stop the half-built engine and bail rather than install it —
    // installing would write the old roost's incoming peer notes through the new
    // roost's backend (cross-roost bleed) or resurrect an engine the switch just
    // tore down. The early `roost_fs_matches_path` guard still fast-aborts the
    // already-switched case before we waste the engine build.
    {
        let _lifecycle = state.lifecycle.lock().await;
        let live = roost_fs_state.current().await;
        let backend_stale = match &live {
            Some(fs) => !roost_fs_matches_path(fs.as_ref(), roost_path),
            // No roost mounted (never opened, or the switch's open failed). The
            // generation check below covers the failed-switch case; a genuinely
            // no-roost-yet init is legitimately allowed to install.
            None => false,
        };
        let gen_now = state
            .switch_generation
            .load(std::sync::atomic::Ordering::SeqCst);
        if backend_stale || gen_now != gen_at_start {
            drop(_lifecycle);
            tracing::warn!(
                requested = %roost_path,
                backend_stale,
                gen_at_start,
                gen_now,
                "sync_init: aborting — roost switched during init (stale engine not installed)"
            );
            if let Err(e) = engine.stop().await {
                tracing::warn!(error = %e, "sync_init: stopping aborted stale engine failed");
            }
            return Err("sync_init aborted: roost switched during init".into());
        }
        // Stop any engine already installed before replacing it. The early
        // same-roost guard returned already, so a handle here belongs to a
        // different roost (or a racing concurrent init). Dropping it without
        // stop() leaks its background tasks (they hold Arc clones; only stop()
        // sets the kill-latch + aborts them). Take it out, stop it, then install
        // ours — all under `lifecycle`, so no switch can interleave.
        let previous = state.engine.lock().await.take();
        if let Some(old) = previous {
            if let Err(e) = old.stop().await {
                tracing::warn!(error = %e, "sync_init: stopping superseded engine failed");
            }
        }
        // Install under `lifecycle`. The brief engine-lock nests inside; the
        // live-backend read above already dropped its RoostFsState guard, so we
        // never hold engine + RoostFsState at once.
        // Fix #5: keep a handle for the post-commit discovery start below —
        // installing MOVES `engine` into the state.
        engine_for_discovery = engine.clone();
        *state.engine.lock().await = Some(engine);
    }

    // Fix #5: start LAN discovery ONLY now that this init has WON the commit
    // (engine installed, no roost switch interleaved). A losing init returned
    // above without ever opening the mDNS socket. Best-effort — a discovery
    // failure must not fail sync (manual IP pairing still works). Runs outside
    // the `lifecycle` block (so we don't hold it across the daemon-socket open)
    // but still under `_init_guard`, so no concurrent init can interleave.
    {
        let device_id = identity.device_id();
        match Discovery::start(device_id) {
            Ok(discovery) => {
                // Acquire the Android MulticastLock before browsing; without it
                // Android filters incoming multicast and we see zero peers.
                // No-op stub on desktop / iOS.
                discover_acquire_lock(app);
                let (tx, rx) = mpsc::unbounded_channel();
                if let Err(e) = discovery.browse(tx) {
                    tracing::warn!(error = %e, "sync: mdns browse failed to start");
                } else {
                    spawn_discovery_bridge(app.clone(), engine_for_discovery, rx);
                }
                *state.discovery.lock().await = Some(discovery);
            }
            Err(e) => {
                tracing::warn!(error = %e, "sync: mdns unavailable — falling back to manual IP pairing");
            }
        }
    }
    info!(device = %status.our_device_id, "sync engine initialized");
    Ok(status)
}

/// Initialize the engine for an already-opened roost. Loads or generates
/// the device identity + TLS cert, creates the stores, but does NOT start
/// listening. Call `sync_start_listening` to accept inbound connections.
#[tauri::command]
#[specta::specta]
pub async fn sync_init(
    app: tauri::AppHandle,
    state: State<'_, SyncEngineState>,
    roost_fs_state: State<'_, crate::commands::files::RoostFsState>,
    roost_path: String,
    device_name: String,
) -> Result<SyncStatus, String> {
    let status = sync_init_inner(&app, &state, &roost_fs_state, &roost_path, &device_name).await?;
    // Persist enablement so returning to this roost (or relaunching) auto-
    // resumes without the user re-clicking "Enable sync". The engine being up
    // isn't enough — `sync_try_autostart` reads prefs, and switching roosts now
    // stops the engine, so the durable flag is what survives the round-trip.
    let prefs = enable_prefs(load_sync_prefs(&app), &roost_path, &device_name);
    if let Err(e) = save_sync_prefs(&app, &prefs) {
        tracing::warn!(
            error = %e,
            "sync_init: could not persist enable prefs — auto-resume on return/restart may not work"
        );
    }
    Ok(status)
}

/// Whether to bind the legacy TCP-TLS listener on autostart. Always `false`.
///
/// TCP first-pair is unreachable from the UI (pairing is iroh-only) and a TCP
/// reconnect cannot authenticate (the acceptor preloads no resumption tokens,
/// and TLS-without-mTLS gives no authenticated identity to key an enrollment
/// exemption on), so the listener could only ever reject inbound connections
/// with `PairingClosed`. Binding it therefore adds attack surface for zero
/// function. This predicate exists so a future decision to revive TCP is an
/// explicit, test-breaking edit rather than a silent regression. See the
/// TCP-reconnect audit.
fn should_autostart_legacy_tcp_listener() -> bool {
    false
}

/// Best-effort auto-resume after app restart. If the user previously
/// enabled sync on this roost, spin the engine up and start hosting with
/// the saved port + PIN — no UI interaction required. On anything less
/// than a perfect match (different roost, prefs disabled, missing file)
/// we return a non-running `SyncStatus` and the user enables it manually.
#[tauri::command]
#[specta::specta]
pub async fn sync_try_autostart(
    app: tauri::AppHandle,
    state: State<'_, SyncEngineState>,
    roost_fs_state: State<'_, crate::commands::files::RoostFsState>,
    roost_path: String,
) -> Result<SyncStatus, String> {
    // Already running for this roost — nothing to do. If the engine belongs
    // to a previous roost, stop it first so its RoostFs clone releases the
    // old roost lock before this roost tries to resume.
    let existing_roost = {
        let guard = state.engine.lock().await;
        guard.as_ref().map(|engine| engine.roost_path())
    };
    if let Some(existing_roost) = existing_roost {
        if same_sync_roost(&existing_roost, &roost_path) {
            return sync_status(state).await;
        }
        tracing::info!(
            from = %existing_roost,
            to = %roost_path,
            "sync_try_autostart: stopping previous roost engine"
        );
        sync_shutdown_without_disabling(&app, &state).await;
    }

    let prefs = load_sync_prefs(&app);
    if !should_resume(&prefs, &roost_path) {
        return Ok(SyncStatus {
            running: false,
            listen_port: None,
            our_device_id: String::new(),
            our_fingerprint_words: String::new(),
            connected_peers: vec![],
            known_peers: vec![],
            queue_active: false,
            total_pending_outbound: 0,
            relay_connected: None,
        });
    }

    tracing::info!(roost = %roost_path, "sync_try_autostart: resuming previous session");
    let device_name = if prefs.device_name.is_empty() {
        "This device".to_string()
    } else {
        prefs.device_name.clone()
    };
    sync_init_inner(&app, &state, &roost_fs_state, &roost_path, &device_name).await?;

    // If the user previously hosted, bring the listener back up with the
    // saved PIN. Dialer-only devices (paired out but never hosted) have
    // no hosting PIN — we still want to auto-resume the engine + mDNS
    // discovery so the frontend's auto-reconnect loop can redial paired
    // peers; we just skip the listener here.
    // Bring up the iroh listener BEFORE auto-redialing so paired peers
    // dialing in over iroh can reach us, AND so our own outbound
    // `connect_peer_via_iroh` calls have a shared endpoint to use.
    // Skipping this is what made one-device-offline → reconnect require
    // a fresh QR pair: the listener wasn't bound after restart, so the
    // iroh redial path returned `EngineError::NotConnected`.
    if let Some(iroh_pin) = Pin::parse(&prefs.iroh_hosting_pin) {
        let guard = state.engine.lock().await;
        if let Some(engine) = guard.as_ref() {
            if let Err(e) = engine.start_listening_iroh(iroh_pin).await {
                tracing::warn!(error = %e, "sync_try_autostart: iroh listener resume failed");
            }
        }
    } else {
        tracing::debug!("sync_try_autostart: no iroh hosting PIN saved — outbound-only iroh");
    }

    // Kick off active dial for every known peer that has an address +
    // PIN stored in peer_store. The engine handles the state-machine
    // and event emission; this command returns immediately with the
    // current (pre-dial) status — the dial results flow through
    // PeerStatusChanged events. No more "engine is up but sitting idle
    // waiting for mDNS" — reconnection is the backend's default now.
    {
        let guard = state.engine.lock().await;
        if let Some(engine) = guard.as_ref() {
            engine.reconnect_all_known_peers().await;
        }
    }

    // The legacy TCP-TLS listener is a dead path: TCP first-pair is not
    // reachable from the UI (pairing is iroh-only), and a TCP reconnect can't
    // authenticate (no resumption-token preload, and no NodeId weld to key an
    // enrollment exemption on), so an inbound connection could only ever be
    // rejected with `PairingClosed`. Binding it is pure attack surface — an
    // unauthenticated peer reaches the TLS + handshake state machine + the
    // shared pairing rate limiter for zero legitimate function. Skip it. Kept
    // behind an explicit predicate so re-enabling is a deliberate, test-breaking
    // change rather than a silent regression.
    if !should_autostart_legacy_tcp_listener() {
        tracing::info!(
            "sync_try_autostart: legacy TCP-TLS listener disabled (dead path — iroh is the only reachable transport); skipping"
        );
        return sync_status(state).await;
    }
    let pin = match Pin::parse(&prefs.hosting_pin) {
        Some(p) => p,
        None => {
            tracing::info!(
                "sync_try_autostart: no legacy TLS hosting PIN — skipping legacy listener (iroh listener handled above)"
            );
            return sync_status(state).await;
        }
    };
    let port = prefs.hosting_port;
    let bound = {
        let guard = state.engine.lock().await;
        let Some(engine) = guard.as_ref() else {
            return Err("sync engine vanished between init and listen".into());
        };
        match engine.start_listening(port, pin).await {
            Ok(bound) => Some((
                bound,
                engine.roost_name().to_string(),
                engine.display_name().to_string(),
                engine.clone(),
            )),
            Err(e) => {
                tracing::warn!(error = %e, port, "sync_try_autostart: start_listening failed — engine is up, listener isn't");
                None
            }
        }
    };
    if let Some((bound, roost_name, display_name, engine)) = bound {
        let disco = state.discovery.lock().await.as_ref().cloned();
        if let Some(discovery) = disco {
            let (node_id, iroh_addrs) = iroh_announce_fields(&engine).await;
            if let Err(e) = discovery
                .announce(&display_name, &roost_name, bound, &node_id, &iroh_addrs)
                .await
            {
                tracing::warn!(error = %e, "sync_try_autostart: mdns announce failed");
            }
            // Keep the advertised iroh addrs fresh as the endpoint discovers /
            // changes them post-bind (the announce above is one-shot).
            if let Some(ep) = engine.iroh_endpoint().await {
                spawn_reannounce_task(
                    &state,
                    ep,
                    discovery,
                    display_name.clone(),
                    roost_name.clone(),
                    bound,
                )
                .await;
            }
        }
        // Refresh prefs so a port that was chosen by the OS (port=0) gets
        // remembered. Keep enabled=true.
        let _ = save_sync_prefs(
            &app,
            &SyncPrefs {
                enabled: true,
                roost_path: roost_path.clone(),
                device_name: prefs.device_name,
                hosting_port: bound,
                hosting_pin: prefs.hosting_pin,
                iroh_hosting_pin: prefs.iroh_hosting_pin,
                queue_endpoint: prefs.queue_endpoint,
                strict_p2p_only: prefs.strict_p2p_only,
                pinned_queue_host: prefs.pinned_queue_host,
                per_roost: prefs.per_roost,
            },
        );
    }

    sync_status(state).await
}

/// Normalize a roost path for a stable comparison / map key: forward slashes,
/// no trailing slash, lowercased on case-insensitive filesystems.
fn normalize_roost_key(s: &str) -> String {
    let cleaned = s.replace('\\', "/").trim_end_matches('/').to_string();
    if cfg!(any(target_os = "windows", target_os = "macos")) {
        cleaned.to_lowercase()
    } else {
        cleaned
    }
}

fn same_sync_roost(a: &str, b: &str) -> bool {
    let normalize = |s: &str| -> String {
        let cleaned = s.replace('\\', "/").trim_end_matches('/').to_string();
        if cfg!(any(target_os = "windows", target_os = "macos")) {
            cleaned.to_lowercase()
        } else {
            cleaned
        }
    };
    normalize(a) == normalize(b)
}

/// Build the prefs to persist when the user enables sync on a roost (the
/// "Enable sync on this roost" action → `sync_init`). Without this, enabling
/// created a running engine but no durable prefs, so once the engine was
/// stopped (e.g. switching roosts — see `stop_sync_for_roost_switch`) there
/// was nothing to auto-resume and the user had to re-enable on return.
///
/// Re-enabling the *same* roost preserves its hosting config in place.
/// Switching to a *different* roost restores THAT roost's saved hosting config
/// (PINs/port) from the per-roost map if we've synced it before — so a user who
/// syncs two roosts doesn't lose the other's pairing PINs on switch (which
/// forced a re-pair on return). A never-seen roost starts fresh.
fn enable_prefs(existing: SyncPrefs, roost_path: &str, device_name: &str) -> SyncPrefs {
    let mut prefs = if same_sync_roost(&existing.roost_path, roost_path) {
        existing
    } else {
        // Carry the per-roost map across the switch and restore this roost's
        // saved hosting config (empty if never synced).
        let saved = existing
            .per_roost
            .get(&normalize_roost_key(roost_path))
            .cloned()
            .unwrap_or_default();
        SyncPrefs {
            hosting_pin: saved.hosting_pin,
            iroh_hosting_pin: saved.iroh_hosting_pin,
            hosting_port: saved.hosting_port,
            per_roost: existing.per_roost,
            ..SyncPrefs::default()
        }
    };
    prefs.enabled = true;
    prefs.roost_path = roost_path.to_string();
    prefs.device_name = device_name.to_string();
    prefs
}

/// Whether `sync_try_autostart` should resume the engine for `roost_path`.
/// Resume only when sync was enabled AND the recorded roost matches — compared
/// with `same_sync_roost` (path-normalized), NOT an exact string compare,
/// which silently broke resume when the roost path's representation drifted
/// (backslash vs forward slash / drive-letter case) between enable and reopen.
fn should_resume(prefs: &SyncPrefs, roost_path: &str) -> bool {
    prefs.enabled && same_sync_roost(&prefs.roost_path, roost_path)
}

/// Start listening for inbound pairing/sync. `pin` is the 6-digit PIN the
/// user must type into the *other* device to complete pairing. Returns the
/// bound port (useful when `port=0` lets the OS pick one).
#[tauri::command]
#[specta::specta]
pub async fn sync_start_listening(
    app: tauri::AppHandle,
    state: State<'_, SyncEngineState>,
    port: u16,
    pin: String,
) -> Result<u16, String> {
    let guard = state.engine.lock().await;
    let engine = guard.as_ref().ok_or("sync engine not initialized")?;
    let pin_parsed = Pin::parse(&pin).ok_or("invalid PIN — must be 6 digits")?;
    let bound = match engine.start_listening(port, pin_parsed).await {
        Ok(bound) => {
            info!(requested = port, bound, "sync_start_listening ok");
            // SRX brute-force fix: user explicitly started hosting — open the
            // first-pair enrollment window so inbound first-pairs are admitted
            // only while actively pairing.
            engine.open_enrollment_window();
            bound
        }
        Err(e) => {
            let es = e.to_string();
            // Windows error 10048 = WSAEADDRINUSE; Linux is EADDRINUSE.
            // Most often means the user has an older Corvux still running
            // (maybe a dev-mode `corvux.exe` hanging around) that already
            // owns the port. Give them a clear action instead of the raw
            // WinSock text.
            let friendly = if es.contains("os error 10048") || es.contains("address already in use")
            {
                format!(
                    "Port {port} is already in use by another process. \
                     Another Corvux instance is probably running — close it \
                     (Task Manager → corvux.exe) and try again, or pick a \
                     different port."
                )
            } else {
                format!("start listening on 0.0.0.0:{port} failed: {e}")
            };
            tracing::warn!("{friendly}");
            return Err(friendly);
        }
    };

    // mDNS announcement pairs with the listener: peers only see us when
    // we're actually accepting connections. We hold the engine guard long
    // enough to read roost+device names (and any already-bound iroh
    // identity), then drop before touching the discovery lock to avoid
    // nested-lock surprises.
    let roost_name = engine.roost_name().to_string();
    let display_name = engine.display_name().to_string();
    let roost_path = engine.roost_path().to_string();
    let (node_id, iroh_addrs) = iroh_announce_fields(engine).await;
    // Grab the endpoint before dropping the engine guard so the re-announce
    // task can watch it for address changes.
    let iroh_ep = engine.iroh_endpoint().await;
    drop(guard);
    let disco = state.discovery.lock().await.as_ref().cloned();
    if let Some(discovery) = disco {
        if let Err(e) = discovery
            .announce(&display_name, &roost_name, bound, &node_id, &iroh_addrs)
            .await
        {
            tracing::warn!(error = %e, "sync: mdns announce failed — peers won't auto-discover us");
        }
        // Keep the advertised iroh addrs fresh as the endpoint discovers /
        // changes them post-bind (the announce above is one-shot).
        if let Some(ep) = iroh_ep {
            spawn_reannounce_task(
                &state,
                ep,
                discovery,
                display_name.clone(),
                roost_name.clone(),
                bound,
            )
            .await;
        }
    }

    // Persist so the next app launch can auto-resume without user action.
    // Any failure here is non-fatal — user can still retry manually.
    // Preserve any existing iroh_hosting_pin — starting the legacy TLS
    // listener shouldn't clobber the iroh autoresume PIN if the user has
    // also been hosting iroh in the same session.
    let prior = load_sync_prefs(&app);
    if let Err(e) = save_sync_prefs(
        &app,
        &SyncPrefs {
            enabled: true,
            roost_path,
            device_name: display_name,
            hosting_port: bound,
            hosting_pin: pin,
            iroh_hosting_pin: prior.iroh_hosting_pin,
            queue_endpoint: prior.queue_endpoint,
            strict_p2p_only: prior.strict_p2p_only,
            pinned_queue_host: prior.pinned_queue_host,
            per_roost: prior.per_roost,
        },
    ) {
        tracing::warn!(error = %e, "sync_start_listening: could not persist prefs");
    }

    Ok(bound)
}

/// Initiate outbound pairing via the legacy TCP+TLS transport.
///
/// AUDIT-003b: this IPC is now **reconnect-only** for already-paired
/// peers. The caller may pass `pinned_fingerprint_hex` explicitly, OR
/// pass `None` — in which case the backend looks up the fingerprint
/// from `peer_store` by `last_known_addr`. If neither yields a known
/// fingerprint, the dial is rejected with a clear error: tcp_tls's
/// TLS verifier no longer accepts arbitrary certs, and there's no
/// first-pair-via-tcp_tls UX (use `sync_connect_iroh` instead).
///
/// The frontend auto-reconnect loop (`stores/sync.ts` →
/// `tryAutoDial`) passes `null` and relies on the lookup. Manual
/// callers that already have the fingerprint can short-circuit the
/// lookup by passing it directly.
#[tauri::command]
#[specta::specta]
pub async fn sync_connect(
    app: tauri::AppHandle,
    state: State<'_, SyncEngineState>,
    addr: String,
    pin: String,
    pinned_fingerprint_hex: Option<String>,
) -> Result<String, String> {
    let guard = state.engine.lock().await;
    let engine = guard.as_ref().ok_or("sync engine not initialized")?;
    let addr: SocketAddr = addr.parse().map_err(|e| format!("invalid address: {e}"))?;
    let pin = Pin::parse(&pin).ok_or("invalid PIN — must be 6 digits")?;
    let pinned =
        resolve_pinned_fingerprint(engine, addr, pinned_fingerprint_hex.as_deref()).await?;
    let device_id = match engine.connect_peer(addr, pin, pinned).await {
        Ok(id) => {
            info!(%addr, device = %id.as_hex(), "sync_connect ok");
            id
        }
        Err(e) => {
            let msg = format!("connect to {addr} failed: {e}");
            tracing::warn!("{msg}");
            return Err(msg);
        }
    };

    // Persist enabled=true for this roost so the next app launch
    // auto-resumes the engine on a dialer-only device (one that paired
    // out but never hosted). Without this, sync_try_autostart would
    // find prefs.enabled=false and do nothing — the user then has to
    // manually re-open Settings and re-initialize every launch.
    // We preserve any previously-saved hosting PIN/port so a device
    // that both hosts AND dials doesn't lose its listener config.
    let roost_path = engine.roost_path().to_string();
    let display_name = engine.display_name().to_string();
    drop(guard);
    let mut prefs = load_sync_prefs(&app);
    prefs.enabled = true;
    prefs.roost_path = roost_path;
    if prefs.device_name.is_empty() {
        prefs.device_name = display_name;
    }
    if let Err(e) = save_sync_prefs(&app, &prefs) {
        tracing::warn!(error = %e, "sync_connect: could not persist auto-resume prefs");
    }

    Ok(device_id.as_hex())
}

/// Bind the iroh endpoint and return a **pairing ticket** for the QR
/// code. The ticket is a `corvux1:<base64-json>` string that bundles the
/// host's NodeId, current relay URL, and direct addresses — everything
/// the joining device needs to dial without first round-tripping through
/// Iroh's discovery service.
///
/// Idempotent: if the listener is already up, just regenerates the
/// ticket from the live endpoint (its relay URL or direct addrs may
/// have changed since the last call).
/// Result of starting/refreshing the iroh listener for pairing: the QR ticket
/// plus the (sticky) hosting PIN to display alongside it.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, specta::Type)]
pub struct IrohHostingInfo {
    pub ticket: String,
    pub pin: String,
}

#[tauri::command]
#[specta::specta]
pub async fn sync_listen_iroh(
    app: tauri::AppHandle,
    state: State<'_, SyncEngineState>,
    rotate: bool,
) -> Result<IrohHostingInfo, String> {
    // Sticky hosting PIN. The pairing panel used to mint a FRESH random PIN on
    // every "Show code" click and push it here, which rotated the listener's
    // PIN out from under every already-paired device — their stored reconnect
    // PIN no longer matched, so each one silently failed to reconnect (and hit
    // the shared rate limiter on retry). Instead we reuse the persisted PIN
    // (get-or-create), so re-opening the panel shows the SAME code and existing
    // pairings keep working. `rotate: true` is the only way to mint a new PIN
    // (explicit user action, e.g. "the old code leaked").
    let existing = Pin::parse(&load_sync_prefs(&app).iroh_hosting_pin);
    let pin = match (rotate, existing) {
        (false, Some(p)) => p,
        _ => Pin::generate(),
    };
    let pin_str = pin.as_str().to_string();

    let guard = state.engine.lock().await;
    let engine = guard.as_ref().ok_or("sync engine not initialized")?;
    engine
        .start_listening_iroh(pin)
        .await
        .map_err(|e| format!("start iroh listener failed: {e}"))?;
    // SRX brute-force fix: showing the code is an explicit pairing action — open
    // the first-pair enrollment window so an inbound first-pair is admitted only
    // while the user is actively pairing (and only up to the global failure cap).
    engine.open_enrollment_window();
    // SRX layer 3: mint a fresh one-time secret for this "Show code" session and
    // set it BEFORE generating the ticket, so the ticket carries it and the
    // accept path binds it into the first-pair proof. This makes ticket pairing
    // non-brute-forceable: an attacker with only the public NodeId can't forge a
    // first pair. `rotate` gets a fresh secret too (the whole point of rotating).
    engine
        .set_pairing_secret(Some(PairingSecret::generate()))
        .await;
    let ticket = engine
        .iroh_pairing_ticket()
        .await
        .map_err(|e| format!("generate pairing ticket failed: {e}"))?;
    info!(
        ticket_len = ticket.len(),
        rotated = rotate,
        "sync_listen_iroh ok"
    );
    // Persist the iroh hosting PIN so `sync_try_autostart` can re-bind the
    // listener with the SAME PIN on next launch (a peer dialing in proves the
    // PIN in our app-layer handshake).
    let roost_path = engine.roost_path().to_string();
    let display_name = engine.display_name().to_string();
    drop(guard);
    let mut prefs = load_sync_prefs(&app);
    prefs.enabled = true;
    prefs.roost_path = roost_path;
    if prefs.device_name.is_empty() {
        prefs.device_name = display_name;
    }
    prefs.iroh_hosting_pin = pin_str.clone();
    if let Err(e) = save_sync_prefs(&app, &prefs) {
        tracing::warn!(error = %e, "sync_listen_iroh: could not persist iroh hosting prefs");
    }
    Ok(IrohHostingInfo {
        ticket,
        pin: pin_str,
    })
}

/// Connect outbound to a peer over the iroh transport.
///
/// `ticket` is the `corvux1:...` string from the host's QR code (or a
/// bare 64-char NodeId hex as a fallback — discovery resolves the rest).
/// `pin` is the 6-digit pairing PIN the user types after scanning.
#[tauri::command]
#[specta::specta]
pub async fn sync_connect_iroh(
    app: tauri::AppHandle,
    state: State<'_, SyncEngineState>,
    ticket: String,
    pin: String,
) -> Result<String, String> {
    // Clone the engine handle and release the state lock BEFORE dialing.
    // `connect_peer_via_iroh` performs a network dial (bounded by DIAL_TIMEOUT);
    // holding `SyncEngineState.engine` across it would block sync_status and
    // every other sync command for the duration of the dial.
    let engine = {
        let guard = state.engine.lock().await;
        guard.as_ref().ok_or("sync engine not initialized")?.clone()
    };
    // SRX brute-force fix: scanning + entering a PIN is an explicit pairing
    // action, and below we bind OUR OWN listener so the host can dial us back.
    // Open the enrollment window so any first-pair back-dial is admitted while
    // we're actively pairing (reconnects are token-based and never gated).
    engine.open_enrollment_window();
    let pin_parsed = Pin::parse(&pin).ok_or("invalid PIN — must be 6 digits")?;
    let peer = crate::sync::transport::iroh::decode_ticket(&ticket)
        .map_err(|e| format!("invalid pairing code: {e}"))?;

    // Symmetric pairing: bind OUR OWN listener with our sticky hosting PIN and
    // build our dialable ticket, then offer both to the host in the handshake
    // so it can dial US back after a restart (not just us dialing it). Without
    // this, reconnect is one-directional — the QR scanner is the only side that
    // can re-establish. Best-effort: if we can't produce an offer we still pair
    // (falls back to one-directional).
    let my_offer: Option<(String, String)> = {
        let my_pin =
            Pin::parse(&load_sync_prefs(&app).iroh_hosting_pin).unwrap_or_else(Pin::generate);
        let my_pin_str = my_pin.as_str().to_string();
        match engine.start_listening_iroh(my_pin).await {
            Ok(_) => match engine.iroh_pairing_ticket().await {
                Ok(my_ticket) => {
                    // Persist our sticky hosting PIN so redials from the host
                    // keep matching across our restarts.
                    let mut prefs = load_sync_prefs(&app);
                    prefs.iroh_hosting_pin = my_pin_str.clone();
                    let _ = save_sync_prefs(&app, &prefs);
                    Some((my_ticket, my_pin_str))
                }
                Err(e) => {
                    tracing::warn!(error = %e, "sync_connect_iroh: could not build our own ticket — pairing one-directional");
                    None
                }
            },
            Err(e) => {
                tracing::warn!(error = %e, "sync_connect_iroh: could not bind our listener — pairing one-directional");
                None
            }
        }
    };

    // SRX layer 3: pull the one-time secret the host embedded in the ticket (if
    // any). Folded into the first-pair proof so the host accepts us and a peer
    // that only has the public NodeId (never the ticket) can't forge a pair.
    // Absent on a bare-NodeId or legacy ticket → PIN-only fallback.
    let ticket_secret = crate::sync::transport::iroh::decode_ticket_secret(&ticket);
    let device_id = engine
        // First pair → no resumption token yet; authenticate with the PIN
        // (folded with the ticket secret) and derive+store the token on success.
        .connect_peer_via_iroh(peer, pin_parsed, my_offer, None, ticket_secret)
        .await
        .map_err(|e| format!("connect via iroh failed: {e}"))?;
    info!(device = %device_id.as_hex(), "sync_connect_iroh ok");

    // Persist the ticket + PIN against this peer so the engine can dial
    // back without the user re-scanning the QR. Reusing the legacy
    // `last_known_addr` / `last_known_pin` slots — the `corvux1:` prefix
    // on the ticket lets `reconnect_all_known_peers` distinguish iroh
    // tickets from legacy `ip:port` strings.
    if let Err(e) = engine.peer_store_lock().await.remember_connection(
        device_id,
        Some(ticket.clone()),
        Some(pin.clone()),
    ) {
        tracing::warn!(error = %e, "sync_connect_iroh: failed to remember iroh ticket for autoresume");
    }

    // Mirror sync_connect: persist enabled=true so next launch can resume
    // sync on the dialer side too.
    let roost_path = engine.roost_path().to_string();
    let display_name = engine.display_name().to_string();
    let mut prefs = load_sync_prefs(&app);
    prefs.enabled = true;
    prefs.roost_path = roost_path;
    if prefs.device_name.is_empty() {
        prefs.device_name = display_name;
    }
    if let Err(e) = save_sync_prefs(&app, &prefs) {
        tracing::warn!(error = %e, "sync_connect_iroh: could not persist auto-resume prefs");
    }

    Ok(device_id.as_hex())
}

/// Current status snapshot. Safe to poll from the UI.
#[tauri::command]
#[specta::specta]
pub async fn sync_status(state: State<'_, SyncEngineState>) -> Result<SyncStatus, String> {
    let guard = state.engine.lock().await;
    match guard.as_ref() {
        Some(engine) => Ok(engine.status().await),
        None => Ok(SyncStatus {
            running: false,
            listen_port: None,
            our_device_id: String::new(),
            our_fingerprint_words: String::new(),
            connected_peers: vec![],
            known_peers: vec![],
            queue_active: false,
            total_pending_outbound: 0,
            relay_connected: None,
        }),
    }
}

/// Unpair a known peer: drops the live connection and forgets the stored
/// fingerprint pin. Next reconnect will require a fresh PIN.
#[tauri::command]
#[specta::specta]
pub async fn sync_unpair(
    state: State<'_, SyncEngineState>,
    device_id_hex: String,
) -> Result<(), String> {
    let guard = state.engine.lock().await;
    let engine = guard.as_ref().ok_or("sync engine not initialized")?;
    let bytes = hex::decode(&device_id_hex).map_err(|e| format!("invalid device id: {e}"))?;
    if bytes.len() != 16 {
        return Err("device id must be 16 bytes (32 hex chars)".into());
    }
    let mut arr = [0u8; 16];
    arr.copy_from_slice(&bytes);
    engine
        .unpair(DeviceId(arr))
        .await
        .map_err(|e| format!("unpair: {e}"))
}

/// Walk the roost and seed the Automerge DocStore with every pre-existing
/// markdown file, then nudge connected peers to re-announce their
/// `NoteList`. Call this when pairing a fresh peer and the local roost
/// already has notes the peer should learn about. The file watcher only
/// reacts to *changes*, so without a scan, existing notes never bootstrap.
#[tauri::command]
#[specta::specta]
pub async fn sync_rescan_roost(state: State<'_, SyncEngineState>) -> Result<u32, String> {
    let guard = state.engine.lock().await;
    let engine = guard.as_ref().ok_or("sync engine not initialized")?;
    engine
        .scan_roost()
        .await
        .map(|n| n as u32)
        .map_err(|e| format!("rescan: {e}"))
}

/// Wipe this device's locally-persisted Automerge docs. Leaves the
/// roost files themselves untouched; next peer connection rebuilds
/// the CRDT store via FullDoc adoption. The canonical recovery when
/// sync has drifted into the "two independent docs merged into one"
/// state and content starts vanishing on one side.
#[tauri::command]
#[specta::specta]
pub async fn sync_reset_docstore(state: State<'_, SyncEngineState>) -> Result<(), String> {
    let guard = state.engine.lock().await;
    let engine = guard.as_ref().ok_or("sync engine not initialized")?;
    engine
        .reset_docstore()
        .await
        .map_err(|e| format!("reset: {e}"))
}

/// Trigger a silent reconnect attempt for every paired peer that has a
/// saved address + PIN in the peer_store. Dial lifecycle (Dialing →
/// Handshaking → Connected / Failed) surfaces via `sync://event`
/// PeerStatusChanged messages, so the UI updates the status dots in
/// real time. Used by the TopBar "retry" button and the "Reconnect"
/// row in Settings.
#[tauri::command]
#[specta::specta]
pub async fn sync_reconnect_all(state: State<'_, SyncEngineState>) -> Result<(), String> {
    let guard = state.engine.lock().await;
    let engine = guard.as_ref().ok_or("sync engine not initialized")?;
    // force_* resets per-peer backoff first: an explicit reconnect (button
    // tap or app-foreground) should dial now, not wait out a backoff window
    // that accrued while disconnected/suspended.
    engine.force_reconnect_all().await;
    Ok(())
}

/// Stop all sync activity and drop the engine. The identity + TLS cert
/// remain on disk, so the next `sync_init` reuses them. mDNS discovery
/// is shut down in lockstep so we don't advertise a service that no
/// longer exists.
///
/// This is the *explicit user-driven* shutdown path ("Stop sync and
/// forget session" button). It also flips the persisted `enabled` flag
/// to false so the next launch does NOT auto-resume. The window-close
/// hook takes a different path ([`sync_shutdown_without_disabling`]) to
/// preserve auto-resume across restarts.
#[tauri::command]
#[specta::specta]
pub async fn sync_stop(
    app: tauri::AppHandle,
    state: State<'_, SyncEngineState>,
) -> Result<(), String> {
    // SYN-B6: serialize against `sync_init`. Without this, a stop could take the
    // engine out and stop it WHILE an in-flight init (holding only `state.init`)
    // installs a freshly-built, running engine afterwards — the user's "stop"
    // silently undone. Holding `init` (lock order: init → lifecycle → engine)
    // makes stop and init mutually exclusive: whichever the user triggered
    // second runs strictly after the first completes.
    let _init = state.init.lock().await;
    if let Some(t) = state.reannounce_task.lock().await.take() {
        t.abort();
    }
    if let Some(discovery) = state.discovery.lock().await.take() {
        discovery.shutdown().await;
        discover_release_lock(&app);
    }
    let mut guard = state.engine.lock().await;
    if let Some(engine) = guard.take() {
        engine.stop().await.map_err(|e| format!("stop: {e}"))?;
    }
    // User asked to forget — clear the auto-resume flag but keep the rest
    // of prefs so if they re-enable we can pre-fill device name, etc.
    let mut prefs = load_sync_prefs(&app);
    if prefs.enabled {
        prefs.enabled = false;
        if let Err(e) = save_sync_prefs(&app, &prefs) {
            tracing::warn!(error = %e, "sync_stop: could not clear auto-resume flag");
        }
    }
    Ok(())
}

/// Internal shutdown used by the window-close hook. Drops the running
/// engine + discovery cleanly but **does NOT** change the persisted
/// `enabled` flag — the next launch should auto-resume if it was on.
pub async fn sync_shutdown_without_disabling(app: &tauri::AppHandle, state: &SyncEngineState) {
    // SYN-B6: serialize against `sync_init` (twin of `sync_stop`), so a
    // window-close shutdown can't be undone by a concurrently-committing init.
    let _init = state.init.lock().await;
    sync_shutdown_without_disabling_init_locked(app, state).await;
}

/// Shutdown twin for callers that already hold `state.init`.
///
/// The roost transition path takes `init → lifecycle` to match
/// `sync_init_inner`. Calling the public wrapper there would attempt to lock
/// `init` recursively and deadlock, so this leaf contains the actual teardown.
pub(crate) async fn sync_shutdown_without_disabling_init_locked(
    app: &tauri::AppHandle,
    state: &SyncEngineState,
) {
    if let Some(t) = state.reannounce_task.lock().await.take() {
        t.abort();
    }
    if let Some(discovery) = state.discovery.lock().await.take() {
        discovery.shutdown().await;
        discover_release_lock(app);
    }
    if let Some(engine) = state.engine.lock().await.take() {
        if let Err(e) = engine.stop().await {
            tracing::warn!(error = %e, "window-close sync shutdown: engine.stop failed");
        }
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────

/// TLS identity is persisted so the fingerprint stays stable across app
/// restarts — otherwise every launch would invalidate every peer's pin.
/// File format is a minimal postcard envelope.
fn load_or_create_tls(dir: &std::path::Path) -> Result<TlsIdentity, crate::sync::error::SyncError> {
    use serde::{Deserialize, Serialize};
    const FILE: &str = "tls.bin";
    const VERSION: u16 = 1;

    #[derive(Serialize, Deserialize)]
    struct Disk {
        version: u16,
        cert_der: Vec<u8>,
        key_der: Vec<u8>,
    }

    let path = dir.join(FILE);

    // AUDIT-014: tls.bin holds the QUIC TLS **private key** (`key_der`) — a
    // copy lets another local principal impersonate this device's TLS identity
    // to its paired peers. Lock it to the current user exactly like its sibling
    // identity.bin (Unix 0600 / Windows ACL). Idempotent + best-effort, and
    // re-asserted on the load path so files written before this lock shipped
    // get tightened on the next sync start.
    let restrict = |p: &std::path::Path| {
        if let Err(e) = crate::secure_file::restrict_to_current_user(p) {
            tracing::warn!(error = %e, "tls.bin: could not restrict permissions to current user");
        }
    };

    if path.exists() {
        let bytes = std::fs::read(&path)?;
        if let Ok(disk) = postcard::from_bytes::<Disk>(&bytes) {
            if disk.version == VERSION {
                let fingerprint = Fingerprint::of_cert_der(&disk.cert_der);
                restrict(&path);
                return Ok(TlsIdentity {
                    cert_der: disk.cert_der,
                    key_der: disk.key_der,
                    fingerprint,
                });
            }
        }
        // Unknown/legacy file — regenerate below rather than fail.
    }

    let id = TlsIdentity::generate()?;
    let disk = Disk {
        version: VERSION,
        cert_der: id.cert_der.clone(),
        key_der: id.key_der.clone(),
    };
    let bytes = postcard::to_allocvec(&disk)
        .map_err(|e| crate::sync::error::StoreError::Encode(e.to_string()))?;
    let tmp = path.with_extension("bin.tmp");
    std::fs::write(&tmp, &bytes)?;
    std::fs::rename(&tmp, &path)?;
    restrict(&path);
    Ok(id)
}

/// AUDIT-003b: resolve the pinned fingerprint for a tcp_tls dial.
///
/// Order of preference:
/// 1. Caller passed `pinned_hex` explicitly → parse and use that.
/// 2. Fall back to the fingerprint stored in `peer_store` for the
///    peer whose `last_known_addr` matches `addr`.
/// 3. No known peer at that address → reject with a clear error
///    pointing the caller at `sync_connect_iroh` for first-pair.
///
/// Iroh-only peers have a zeroed fingerprint in `peer_store` (the
/// tcp_tls layer never observed a cert for them) — those are treated
/// the same as "no fingerprint" since dialing them via tcp_tls
/// would necessarily MITM-vulnerable. Their reconnect path is
/// `connect_peer_via_iroh`, dispatched by
/// `reconnect_all_known_peers` based on the `corvux1:` addr prefix.
async fn resolve_pinned_fingerprint(
    engine: &crate::sync::engine::SyncEngine,
    addr: SocketAddr,
    pinned_hex: Option<&str>,
) -> Result<Fingerprint, String> {
    if let Some(hex_str) = pinned_hex {
        return parse_fingerprint(hex_str);
    }
    let store = engine.peer_store_lock().await;
    let peers = store.list();
    drop(store);
    lookup_pinned_fp_for_addr(&peers, addr)
}

/// AUDIT-003b: pure lookup over an already-snapshotted peer list. The
/// IO + lock-handling sit in `resolve_pinned_fingerprint`; this
/// function is the policy that's worth pinning with unit tests. Iroh-
/// only peers (zero fingerprint) are treated as "no TLS pin" — they
/// must reconnect via `sync_connect_iroh`, not the addr+pin path.
fn lookup_pinned_fp_for_addr(
    peers: &[crate::sync::store::peer_store::PeerRecord],
    addr: SocketAddr,
) -> Result<Fingerprint, String> {
    let addr_str = addr.to_string();
    let candidate = peers
        .iter()
        .find(|r| r.last_known_addr.as_deref() == Some(addr_str.as_str()));
    match candidate {
        Some(r) if r.fingerprint != [0u8; 32] => Ok(Fingerprint(r.fingerprint)),
        Some(_) => Err(format!(
            "peer at {addr} is iroh-paired and has no TLS fingerprint pinned — \
             use sync_connect_iroh to reconnect (the addr+pin path does not apply)"
        )),
        None => Err(format!(
            "no paired peer found at {addr} — pair via sync_connect_iroh first \
             (the legacy TCP+TLS first-pair flow is removed; AUDIT-003b)"
        )),
    }
}

fn parse_fingerprint(hex_str: &str) -> Result<Fingerprint, String> {
    let cleaned: String = hex_str
        .chars()
        .filter(|c| !c.is_whitespace() && *c != ':')
        .collect();
    let bytes = hex::decode(&cleaned).map_err(|e| format!("invalid fingerprint hex: {e}"))?;
    if bytes.len() != 32 {
        return Err("fingerprint must be 32 bytes (64 hex chars)".into());
    }
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&bytes);
    Ok(Fingerprint(arr))
}

// ── Store-and-forward queue commands ─────────────────────────────────

/// Frontend-facing snapshot of queue settings.
#[derive(Debug, Clone, serde::Serialize, specta::Type)]
pub struct QueueSettings {
    /// Configured queue endpoint URL, or empty string if unset.
    pub endpoint: String,
    /// True iff strict-P2P-only mode is on (queue client will never be
    /// installed even if `endpoint` is set).
    pub strict_p2p_only: bool,
    /// True iff a `QueueClient` is currently installed on the engine.
    pub active: bool,
}

#[tauri::command]
#[specta::specta]
pub async fn sync_get_queue_settings(
    app: tauri::AppHandle,
    state: State<'_, SyncEngineState>,
) -> Result<QueueSettings, String> {
    let prefs = load_sync_prefs(&app);
    let active = {
        let guard = state.engine.lock().await;
        match guard.as_ref() {
            Some(e) => e.queue_client_active().await,
            None => false,
        }
    };
    Ok(QueueSettings {
        endpoint: prefs.queue_endpoint,
        strict_p2p_only: prefs.strict_p2p_only,
        active,
    })
}

/// Set or clear the queue endpoint URL. Pass an empty string to clear.
/// Re-installs (or removes) the engine's `QueueClient` immediately.
///
/// The endpoint is allowlisted to `meetcorvux.com` (and subdomains) plus any
/// host the user has explicitly pinned via `sync_pin_custom_queue_host`.
/// `http://` is refused outright — even though queue payloads are encrypted,
/// captured plaintext traffic still leaks social graph and request metadata.
#[tauri::command]
#[specta::specta]
pub async fn sync_set_queue_endpoint(
    app: tauri::AppHandle,
    state: State<'_, SyncEngineState>,
    endpoint: String,
) -> Result<(), String> {
    let mut prefs = load_sync_prefs(&app);
    let normalized = validate_queue_endpoint(&endpoint, &prefs.pinned_queue_host)?;
    prefs.queue_endpoint = normalized;
    save_sync_prefs(&app, &prefs)?;
    refresh_queue_client(&state, &prefs).await
}

/// Pin a custom queue host (e.g. a self-hosted Corvux queue). The user
/// must type the host twice to confirm — this is a defense against a
/// compromised renderer silently rerouting queue traffic. Pass an empty
/// `host` to clear an existing pin.
#[tauri::command]
#[specta::specta]
pub async fn sync_pin_custom_queue_host(
    app: tauri::AppHandle,
    host: String,
    host_confirm: String,
) -> Result<(), String> {
    let host = host.trim().to_ascii_lowercase();
    let confirm = host_confirm.trim().to_ascii_lowercase();
    if host != confirm {
        return Err("host and confirmation must match exactly".into());
    }
    if !host.is_empty() {
        // Sanity: host should not include a scheme or path. We accept hostnames
        // only (`example.com`, `queue.example.com:8443` allowed).
        if host.contains("://") || host.contains('/') {
            return Err("pinned host must be a bare hostname, no scheme or path".into());
        }
        if host.chars().any(|c| c.is_whitespace()) {
            return Err("pinned host must not contain whitespace".into());
        }
    }
    let mut prefs = load_sync_prefs(&app);
    prefs.pinned_queue_host = host;
    save_sync_prefs(&app, &prefs)?;
    Ok(())
}

/// Toggle strict-P2P-only mode. When true, the engine drops any active
/// queue client and refuses to install one until toggled back off.
#[tauri::command]
#[specta::specta]
pub async fn sync_set_strict_p2p_only(
    app: tauri::AppHandle,
    state: State<'_, SyncEngineState>,
    enabled: bool,
) -> Result<(), String> {
    let mut prefs = load_sync_prefs(&app);
    prefs.strict_p2p_only = enabled;
    save_sync_prefs(&app, &prefs)?;
    // Make the toggle take effect on the LIVE endpoint, not just at next
    // restart. Previously this only dropped the queue client, so a running iroh
    // endpoint kept using the Number0 relay/discovery until the app was
    // relaunched — the UI claimed "direct only" while relay traffic continued.
    // Update `relay_disabled` and rebind the iroh listener so strict-P2P
    // actually stops (and toggling back off resumes) relay use immediately.
    let engine = state.engine.lock().await.as_ref().cloned();
    if let Some(engine) = engine {
        engine.set_relay_disabled(enabled);
        match engine.rebind_iroh_listener().await {
            Ok(()) => {
                // The rebind closed the old endpoint and bound a fresh one. The
                // mDNS re-announce task lives in `SyncEngineState` — a layer the
                // engine method can't reach — so it's still holding and watching
                // the CLOSED old endpoint (its `Endpoint` clone pins it, so the
                // watch stream won't self-terminate). Left alone it would keep
                // re-broadcasting the dead endpoint's addresses — worst exactly
                // here, where the relay was just disabled and mDNS is the only
                // discovery path left. So the task must be dealt with, but ONLY
                // if it existed: we must not START advertising just because of a
                // toggle.
                let had_reannounce = state.reannounce_task.lock().await.is_some();
                if had_reannounce {
                    let disco = state.discovery.lock().await.as_ref().cloned();
                    let ep = engine.iroh_endpoint().await;
                    // Re-point at the new endpoint only when we can announce a
                    // valid record: discovery live, a fresh endpoint bound, and a
                    // real (non-zero) service port. `hosting_port` can be 0 for an
                    // iroh-only session that never started the legacy listener —
                    // re-announcing port 0 would publish a bogus record.
                    // `spawn_reannounce_task` aborts the stale task before it
                    // installs the replacement.
                    match (disco, ep) {
                        (Some(discovery), Some(ep)) if prefs.hosting_port != 0 => {
                            spawn_reannounce_task(
                                &state,
                                ep,
                                discovery,
                                engine.display_name().to_string(),
                                engine.roost_name().to_string(),
                                prefs.hosting_port,
                            )
                            .await;
                        }
                        // Can't re-announce cleanly — at minimum abort the stale
                        // task so it stops watching (and re-broadcasting) the dead
                        // endpoint.
                        _ => {
                            if let Some(t) = state.reannounce_task.lock().await.take() {
                                t.abort();
                            }
                        }
                    }
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "sync_set_strict_p2p_only: iroh listener rebind failed");
            }
        }
    }
    refresh_queue_client(&state, &prefs).await
}

/// Manually drain pending queue messages addressed to this device.
/// Returns the number of messages successfully applied.
#[tauri::command]
#[specta::specta]
pub async fn sync_pull_queue_now(state: State<'_, SyncEngineState>) -> Result<u32, String> {
    let guard = state.engine.lock().await;
    let engine = guard.as_ref().ok_or("sync engine not initialized")?;
    engine
        .try_queue_pull_all()
        .await
        .map(|n| n as u32)
        .map_err(|e| format!("queue pull: {e}"))
}

/// Pending outbound deliveries per peer, plus the total. Drives the
/// "syncing / N changes waiting for <peer>" indicator. Empty when the
/// outbox is drained or sync isn't running.
#[derive(Debug, Clone, serde::Serialize, specta::Type)]
pub struct OutboxStatus {
    pub total_pending: u32,
    pub by_peer: Vec<OutboxPeerPending>,
}

#[derive(Debug, Clone, serde::Serialize, specta::Type)]
pub struct OutboxPeerPending {
    pub device_id: String,
    pub pending: u32,
}

/// Snapshot the durable outbox's pending-delivery counts. Read by the
/// Settings sync panel to surface per-peer queue health.
#[tauri::command]
#[specta::specta]
pub async fn sync_outbox_status(state: State<'_, SyncEngineState>) -> Result<OutboxStatus, String> {
    let guard = state.engine.lock().await;
    let engine = match guard.as_ref() {
        Some(e) => e,
        None => {
            return Ok(OutboxStatus {
                total_pending: 0,
                by_peer: vec![],
            })
        }
    };
    let by_peer: Vec<OutboxPeerPending> = engine
        .outbox_pending_by_peer()
        .await
        .into_iter()
        .map(|(device, pending)| OutboxPeerPending {
            device_id: device.as_hex(),
            pending,
        })
        .collect();
    let total_pending = by_peer.iter().map(|p| p.pending).sum();
    Ok(OutboxStatus {
        total_pending,
        by_peer,
    })
}

/// Force an immediate outbox drain pass (retry every due delivery now).
/// The "Retry pending now" affordance next to the outbox indicator.
#[tauri::command]
#[specta::specta]
pub async fn sync_drain_outbox_now(state: State<'_, SyncEngineState>) -> Result<u32, String> {
    let guard = state.engine.lock().await;
    let engine = guard.as_ref().ok_or("sync engine not initialized")?;
    engine.drain_outbox().await;
    Ok(engine.outbox_pending_total().await)
}

/// Helper: install or remove the engine's `QueueClient` based on prefs.
/// Idempotent — safe to call from any pref-mutating command.
async fn refresh_queue_client(
    state: &State<'_, SyncEngineState>,
    prefs: &SyncPrefs,
) -> Result<(), String> {
    let guard = state.engine.lock().await;
    let engine = match guard.as_ref() {
        Some(e) => e,
        None => return Ok(()), // engine not up yet — install on next sync_init
    };
    if prefs.strict_p2p_only || prefs.queue_endpoint.is_empty() {
        engine.set_queue_client(None).await;
        return Ok(());
    }
    let identity = engine.identity_arc();
    let client = crate::sync::queue::client::QueueClient::new(&prefs.queue_endpoint, identity)
        .map_err(|e| format!("queue client: {e}"))?;
    engine.set_queue_client(Some(Arc::new(client))).await;
    Ok(())
}
