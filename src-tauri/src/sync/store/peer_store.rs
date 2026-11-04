// touch + is_empty exposed for future cleanup pass
#![allow(dead_code)]

//! Known-peer registry.
//!
//! Once a device has been paired (PIN verified, TLS fingerprint observed)
//! we persist a [`PeerRecord`] for it so future connections can skip the
//! PIN ceremony and instead verify via the pinned TLS fingerprint — the
//! TOFU → "pinned" transition standard to this kind of pairing scheme.
//!
//! The registry is a single `peers.bin` file (postcard-encoded). Typical
//! roosts will have ≤5 entries (laptop/desktop/phone/tablet), so full-file
//! rewrites on every update are trivially fine. Atomic via temp+rename.
//!
//! Layout is explicitly versioned so we can migrate without data loss.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::secret_store;
use crate::sync::error::StoreError;
use crate::sync::identity::DeviceId;

use super::peers_file;

/// Indirection for the keychain so tests don't pollute the real OS
/// keychain. Production callers use [`live_secret_backend`]; tests pass
/// an [`InMemorySecretBackend`].
pub trait SecretBackend: Send + Sync {
    fn get(&self, key: &str) -> Option<String>;
    fn set(&self, key: &str, value: &str) -> Result<(), String>;
    fn delete(&self, key: &str) -> Result<(), String>;
}

/// Production backend — talks to [`crate::secret_store`] which is the
/// OS keychain on desktop and an AES-256-GCM-encrypted file on mobile.
pub struct LiveSecretBackend;

impl SecretBackend for LiveSecretBackend {
    fn get(&self, key: &str) -> Option<String> {
        secret_store::get(key).ok().flatten()
    }
    fn set(&self, key: &str, value: &str) -> Result<(), String> {
        secret_store::set(key, value)
    }
    fn delete(&self, key: &str) -> Result<(), String> {
        secret_store::delete(key)
    }
}

/// Convenience: a heap-allocated [`LiveSecretBackend`] for the common
/// case where the caller doesn't need to hold a custom impl.
pub fn live_secret_backend() -> Arc<dyn SecretBackend> {
    Arc::new(LiveSecretBackend)
}

/// File format version. Bump when the on-disk layout changes incompatibly.
/// v1 → v2 (2026-04): added `last_known_addr` + `last_known_pin` so the
/// auto-reconnect path no longer depends on frontend localStorage.
/// v2 → v3 (2026-05): added `peer_node_id` + `queue_pair_key`, the inputs
/// the store-and-forward queue needs to (a) address a recipient by their
/// 32-byte iroh `NodeId` and (b) encrypt payloads to that recipient
/// using a per-pair key that doesn't depend on a live handshake.
/// v3 → v4 (2026-05): moved `last_known_pin` and `queue_pair_key` out of
/// the on-disk envelope and into the OS keychain (or the
/// platform-encrypted secret store on mobile). The envelope now holds
/// only non-secret pairing identity. A backup of the app-data dir no
/// longer leaks PINs or pair keys.
const PEER_FILE_VERSION: u16 = 4;

/// One known peer. Persisted indefinitely unless the user explicitly
/// unpairs; stale peers are fine — connecting to a gone-away device just
/// times out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerRecord {
    /// 16-byte device ID (first half of SHA-256 of the peer's Ed25519
    /// public key). Stable for the lifetime of the peer's keypair.
    pub device_id: [u8; 16],

    /// Pinned TLS SPKI fingerprint (SHA-256 of the DER SubjectPublicKeyInfo).
    /// A mismatch on reconnect means either key rotation or MitM — we
    /// refuse to proceed and force re-pairing.
    pub fingerprint: [u8; 32],

    /// Human-readable name the peer advertised in `Hello`. Shown in the
    /// pairing UI so the user knows which device just connected.
    pub display_name: String,

    /// When the peer was first paired. Unix millis.
    pub paired_at_millis: u64,

    /// Last time we successfully completed a handshake with this peer.
    /// Unix millis. Used only for UI display ordering.
    pub last_seen_millis: u64,

    /// Last observed network address (`ip:port`) we used to reach this
    /// peer. Populated on a successful outbound connect and on handshake
    /// from the server side. Used by the autoresume path to dial without
    /// needing mDNS or a manual IP entry. `None` means "never dialed
    /// yet / inbound-only pair".
    pub last_known_addr: Option<String>,

    /// Last 6-digit PIN the user typed to authenticate this peer. Stored
    /// so automatic reconnect can skip the UI round-trip. Phase 2 will
    /// replace this with a derived session secret so PIN re-entry is
    /// truly never required; today the PIN handshake runs on every
    /// connection and the saved PIN is how "it just reconnects" works.
    pub last_known_pin: Option<String>,

    /// Peer's 32-byte iroh `NodeId` (raw Ed25519 public key). Captured
    /// once we observe the peer over an iroh transport; `None` for legacy
    /// TLS-only pairings. Used by the store-and-forward queue to address
    /// pushes and by the verify path on incoming queue messages.
    pub peer_node_id: Option<[u8; 32]>,

    /// 32-byte symmetric key shared with this peer for queue payloads.
    /// Both ends derive `HKDF(SessionSecret, "corvux-queue-pair-v1")` at
    /// pair time and persist their own copy. Lets either side encrypt
    /// queue messages without an active handshake. `None` until the
    /// next successful pairing — pre-queue pairings can still sync
    /// live, just can't push offline.
    pub queue_pair_key: Option<[u8; 32]>,

    /// AUDIT-015: the roost id this peer advertised at first pair. `None`
    /// for peers paired before the fix landed — first reconnect after
    /// upgrade populates it. Mismatch on a subsequent reconnect is rejected
    /// with `PairingFailure::RoostIdMismatch`.
    pub peer_vault_id: Option<[u8; 16]>,

    /// SEC-2: per-peer resumption token, `HKDF(first-pair SessionSecret,
    /// "corvux-resumption-token-v1")`, derived + stored on both sides at first
    /// pair. When present, reconnect authenticates by proving THIS token
    /// instead of re-proving the human PIN — so a PIN leaked after pairing is
    /// no longer a permanent roost credential. Keychain-backed (not in the
    /// on-disk record). `None` = never completed a v5 pair → fall back to PIN.
    pub resumption_token: Option<[u8; 32]>,
}

impl PeerRecord {
    pub fn new(device_id: DeviceId, fingerprint: [u8; 32], display_name: String) -> Self {
        let now = now_millis();
        Self {
            device_id: device_id.0,
            fingerprint,
            display_name,
            paired_at_millis: now,
            last_seen_millis: now,
            last_known_addr: None,
            last_known_pin: None,
            peer_node_id: None,
            queue_pair_key: None,
            peer_vault_id: None,
            resumption_token: None,
        }
    }

    pub fn device(&self) -> DeviceId {
        DeviceId(self.device_id)
    }
}

/// v1 on-disk shape, kept so we can read pre-upgrade peer files and
/// migrate forward without losing the pairing (fingerprint, display
/// name, pair time). v1 peers get `None` for the new address / PIN
/// fields — first successful reconnect populates them.
#[derive(Debug, Serialize, Deserialize)]
struct PeerRecordV1 {
    device_id: [u8; 16],
    fingerprint: [u8; 32],
    display_name: String,
    paired_at_millis: u64,
    last_seen_millis: u64,
}

impl From<PeerRecordV1> for PeerRecord {
    fn from(v1: PeerRecordV1) -> Self {
        Self {
            device_id: v1.device_id,
            fingerprint: v1.fingerprint,
            display_name: v1.display_name,
            paired_at_millis: v1.paired_at_millis,
            last_seen_millis: v1.last_seen_millis,
            last_known_addr: None,
            last_known_pin: None,
            peer_node_id: None,
            queue_pair_key: None,
            peer_vault_id: None,
            resumption_token: None,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct EnvelopeV1 {
    version: u16,
    peers: Vec<PeerRecordV1>,
}

/// v2 on-disk shape. Reads cleanly from any v2 file written before the
/// queue work; values are lifted into the current `PeerRecord` with
/// `None` for the new queue-related fields.
#[derive(Debug, Serialize, Deserialize)]
struct PeerRecordV2 {
    device_id: [u8; 16],
    fingerprint: [u8; 32],
    display_name: String,
    paired_at_millis: u64,
    last_seen_millis: u64,
    last_known_addr: Option<String>,
    last_known_pin: Option<String>,
}

impl From<PeerRecordV2> for PeerRecord {
    fn from(v2: PeerRecordV2) -> Self {
        Self {
            device_id: v2.device_id,
            fingerprint: v2.fingerprint,
            display_name: v2.display_name,
            paired_at_millis: v2.paired_at_millis,
            last_seen_millis: v2.last_seen_millis,
            last_known_addr: v2.last_known_addr,
            last_known_pin: v2.last_known_pin,
            peer_node_id: None,
            queue_pair_key: None,
            peer_vault_id: None,
            resumption_token: None,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct EnvelopeV2 {
    version: u16,
    peers: Vec<PeerRecordV2>,
}

/// v3 on-disk shape — same layout as the in-memory `PeerRecord` was in
/// v3, with secrets baked in. Read so we can migrate to v4 by extracting
/// the secrets into the keychain and dropping them from disk.
#[derive(Debug, Serialize, Deserialize)]
struct PeerRecordV3 {
    device_id: [u8; 16],
    fingerprint: [u8; 32],
    display_name: String,
    paired_at_millis: u64,
    last_seen_millis: u64,
    last_known_addr: Option<String>,
    last_known_pin: Option<String>,
    peer_node_id: Option<[u8; 32]>,
    queue_pair_key: Option<[u8; 32]>,
}

#[derive(Debug, Serialize, Deserialize)]
struct EnvelopeV3 {
    version: u16,
    peers: Vec<PeerRecordV3>,
}

/// v4 on-disk shape. Holds only non-secret pairing identity — the PIN
/// and queue pair key live in the OS keychain (`secret_store`) keyed on
/// the device id. A backup of `peers.bin` no longer exposes credentials
/// that could be used to talk to a paired peer.
#[derive(Debug, Serialize, Deserialize)]
struct PeerRecordV4 {
    device_id: [u8; 16],
    fingerprint: [u8; 32],
    display_name: String,
    paired_at_millis: u64,
    last_seen_millis: u64,
    last_known_addr: Option<String>,
    peer_node_id: Option<[u8; 32]>,
}

/// On-disk envelope. A top-level struct (rather than bare `Vec<PeerRecordV4>`)
/// so we can include a version header for forward-compatible migrations.
#[derive(Debug, Serialize, Deserialize)]
struct Envelope {
    version: u16,
    peers: Vec<PeerRecordV4>,
}

impl PeerRecordV4 {
    fn from_record(r: &PeerRecord) -> Self {
        Self {
            device_id: r.device_id,
            fingerprint: r.fingerprint,
            display_name: r.display_name.clone(),
            paired_at_millis: r.paired_at_millis,
            last_seen_millis: r.last_seen_millis,
            last_known_addr: r.last_known_addr.clone(),
            peer_node_id: r.peer_node_id,
        }
    }
}

fn record_from_v4(v4: PeerRecordV4, secrets: &dyn SecretBackend) -> PeerRecord {
    let id_hex = hex::encode(v4.device_id);
    let last_known_pin = secrets.get(&secret_store::keys::sync_pin(&id_hex));
    let queue_pair_key = secrets
        .get(&secret_store::keys::sync_queue_key(&id_hex))
        .and_then(|s| {
            let bytes = hex::decode(&s).ok()?;
            <[u8; 32]>::try_from(bytes.as_slice()).ok()
        });
    let peer_vault_id = secrets
        .get(&secret_store::keys::sync_peer_vault_id(&id_hex))
        .and_then(|s| {
            let bytes = hex::decode(&s).ok()?;
            <[u8; 16]>::try_from(bytes.as_slice()).ok()
        });
    let resumption_token = secrets
        .get(&secret_store::keys::sync_resumption_token(&id_hex))
        .and_then(|s| {
            let bytes = hex::decode(&s).ok()?;
            <[u8; 32]>::try_from(bytes.as_slice()).ok()
        });
    PeerRecord {
        device_id: v4.device_id,
        fingerprint: v4.fingerprint,
        display_name: v4.display_name,
        paired_at_millis: v4.paired_at_millis,
        last_seen_millis: v4.last_seen_millis,
        last_known_addr: v4.last_known_addr,
        last_known_pin,
        peer_node_id: v4.peer_node_id,
        queue_pair_key,
        peer_vault_id,
        resumption_token,
    }
}

/// Migrate a v3 record by lifting its secrets into the keychain. Best-
/// effort: a keychain failure leaves the in-memory record populated with
/// the value (so this session still works) and the next flush will retry
/// the keychain write.
fn record_from_v3(v3: PeerRecordV3, secrets: &dyn SecretBackend) -> PeerRecord {
    let id_hex = hex::encode(v3.device_id);
    if let Some(pin) = v3.last_known_pin.as_deref() {
        if let Err(e) = secrets.set(&secret_store::keys::sync_pin(&id_hex), pin) {
            tracing::warn!(error = %e, %id_hex, "peer_store v3→v4: PIN migration to keychain failed");
        }
    }
    if let Some(key) = v3.queue_pair_key.as_ref() {
        if let Err(e) = secrets.set(
            &secret_store::keys::sync_queue_key(&id_hex),
            &hex::encode(key),
        ) {
            tracing::warn!(error = %e, %id_hex, "peer_store v3→v4: queue_pair_key migration to keychain failed");
        }
    }
    PeerRecord {
        device_id: v3.device_id,
        fingerprint: v3.fingerprint,
        display_name: v3.display_name,
        paired_at_millis: v3.paired_at_millis,
        last_seen_millis: v3.last_seen_millis,
        last_known_addr: v3.last_known_addr,
        last_known_pin: v3.last_known_pin,
        peer_node_id: v3.peer_node_id,
        queue_pair_key: v3.queue_pair_key,
        peer_vault_id: None,
        resumption_token: None,
    }
}

/// In-memory peer registry backed by `peers.bin`.
///
/// Loads on construction; every mutation writes the whole file atomically.
/// Volume is tiny (KB, not MB) so this is fine — and it means we never have
/// a half-written registry on disk.
///
/// Secrets (PIN, queue pair key) live in the keychain via [`SecretBackend`],
/// not in the on-disk envelope. The in-memory `PeerRecord` still exposes
/// them for callers that need them; they're hydrated on `load` and
/// persisted on every mutating call.
pub struct PeerStore {
    path: PathBuf,
    peers: BTreeMap<[u8; 16], PeerRecord>,
    secrets: Arc<dyn SecretBackend>,
}

impl PeerStore {
    /// Load the registry from the sync state dir, or return an empty one
    /// if the file doesn't exist yet. `state_dir` is the app-data-backed
    /// per-roost sync directory — not the user's roost.
    ///
    /// Handles migration from v1, v2, and v3 on load:
    /// - v1 / v2: missing fields default to `None`. First successful
    ///   reconnect populates them.
    /// - v3: secrets baked into the on-disk envelope (`last_known_pin`,
    ///   `queue_pair_key`) are lifted into the keychain via the live
    ///   secret backend, then the v4 envelope is written without them.
    pub fn load(state_dir: &std::path::Path) -> Result<Self, StoreError> {
        Self::load_with_secrets(state_dir, live_secret_backend())
    }

    /// Same as [`load`] but with a custom [`SecretBackend`] — used by
    /// tests so the real OS keychain isn't touched.
    pub fn load_with_secrets(
        state_dir: &std::path::Path,
        secrets: Arc<dyn SecretBackend>,
    ) -> Result<Self, StoreError> {
        let path = peers_file(state_dir);
        if !path.exists() {
            return Ok(Self {
                path,
                peers: BTreeMap::new(),
                secrets,
            });
        }
        let bytes = fs::read(&path)?;
        // Peek at just the version so we can dispatch to the right
        // decoder. Postcard is length-tagged per field, so reading just a
        // `u16` off the front is safe and cheap.
        #[derive(Deserialize)]
        struct VersionOnly {
            version: u16,
        }
        let version = postcard::from_bytes::<VersionOnly>(&bytes)
            .map_err(|e| StoreError::Decode(format!("peer file header: {e}")))?
            .version;

        let records: Vec<PeerRecord> = match version {
            4 => {
                let env: Envelope =
                    postcard::from_bytes(&bytes).map_err(|e| StoreError::Decode(e.to_string()))?;
                env.peers
                    .into_iter()
                    .map(|v4| record_from_v4(v4, secrets.as_ref()))
                    .collect()
            }
            3 => {
                let env: EnvelopeV3 = postcard::from_bytes(&bytes)
                    .map_err(|e| StoreError::Decode(format!("peer file v3: {e}")))?;
                env.peers
                    .into_iter()
                    .map(|v3| record_from_v3(v3, secrets.as_ref()))
                    .collect()
            }
            2 => {
                let env: EnvelopeV2 = postcard::from_bytes(&bytes)
                    .map_err(|e| StoreError::Decode(format!("peer file v2: {e}")))?;
                env.peers.into_iter().map(PeerRecord::from).collect()
            }
            1 => {
                let env: EnvelopeV1 = postcard::from_bytes(&bytes)
                    .map_err(|e| StoreError::Decode(format!("peer file v1: {e}")))?;
                env.peers.into_iter().map(PeerRecord::from).collect()
            }
            other => {
                return Err(StoreError::Decode(format!(
                    "peer file version {other} unsupported (expected {PEER_FILE_VERSION}, 3, 2, or 1)"
                )));
            }
        };

        let peers = records.into_iter().map(|p| (p.device_id, p)).collect();
        let store = Self {
            path,
            peers,
            secrets,
        };
        // If we migrated from an older version, rewrite the file as v4
        // immediately so future loads don't go through the migration
        // path AND the on-disk file no longer carries secrets. Non-fatal
        // — next mutation would rewrite anyway.
        if version != PEER_FILE_VERSION {
            if let Err(e) = store.flush() {
                tracing::warn!(
                    from_version = version,
                    to_version = PEER_FILE_VERSION,
                    error = %e,
                    "peer_store: migration flush failed (will retry on next mutation)"
                );
            }
        }
        Ok(store)
    }

    /// Insert or replace a peer. If a record with this device-id already
    /// exists, preserves `paired_at_millis` (first-pairing time is
    /// authoritative) but updates everything else.
    ///
    /// NOTE: replace semantics make this WRONG for the connection paths —
    /// use [`Self::ensure_peer`] there. `upsert` remains for callers that
    /// intentionally construct a full record (tests, migrations).
    pub fn upsert(&mut self, mut record: PeerRecord) -> Result<(), StoreError> {
        if let Some(existing) = self.peers.get(&record.device_id) {
            record.paired_at_millis = existing.paired_at_millis;
        }
        self.peers.insert(record.device_id, record);
        self.flush()
    }

    /// Record that we just completed a handshake with `device` WITHOUT
    /// destroying what we already know about it.
    ///
    /// This replaces `upsert(PeerRecord::new(..))` on the connection paths,
    /// which wholesale-replaced the record — blanking `last_known_addr`,
    /// `last_known_pin`, `peer_node_id`, `queue_pair_key`, `peer_vault_id`
    /// and `resumption_token` on EVERY connection (inbound and outbound),
    /// leaving only whatever that code path individually re-patched
    /// afterwards. Observed on-device as the "reconnected once, grey with
    /// 're-pair needed' on the second relaunch" bug: a successful redial
    /// burned the very ticket it had just dialed with, and an inbound
    /// reconnect without a fresh offer burned the dialer's stored
    /// credentials on the host. It also silently destroyed SEC-2 resumption
    /// tokens on non-first-pair reconnects, degrading reconnect auth back
    /// to the PIN.
    ///
    /// Semantics:
    /// - unknown device → insert a fresh identity record;
    /// - known device → refresh `display_name` + `last_seen_millis`, adopt a
    ///   non-zero `fingerprint` (the TLS path observes a real one; the iroh
    ///   path passes zeroes, which must not clobber a pinned value), and
    ///   PRESERVE every stored credential. The `remember_*` setters stay the
    ///   only paths that change credentials.
    pub fn ensure_peer(
        &mut self,
        device: DeviceId,
        fingerprint: [u8; 32],
        display_name: String,
    ) -> Result<(), StoreError> {
        match self.peers.get_mut(&device.0) {
            Some(existing) => {
                existing.display_name = display_name;
                existing.last_seen_millis = now_millis();
                if fingerprint != [0u8; 32] {
                    existing.fingerprint = fingerprint;
                }
            }
            None => {
                let record = PeerRecord::new(device, fingerprint, display_name);
                self.peers.insert(record.device_id, record);
            }
        }
        self.flush()
    }

    /// Update `last_seen_millis` to now. No-op if the peer is unknown.
    pub fn touch(&mut self, device: DeviceId) -> Result<(), StoreError> {
        if let Some(existing) = self.peers.get_mut(&device.0) {
            existing.last_seen_millis = now_millis();
            self.flush()?;
        }
        Ok(())
    }

    /// Remember how to reach this peer — called after a successful
    /// outbound connect so the next app launch can redial without UI
    /// or mDNS. `addr` is `"ip:port"`, `pin` is the 6-digit PIN the user
    /// typed during pairing (or `None` if the pair was purely inbound).
    /// No-op if the peer isn't in the store yet — caller should upsert
    /// the identity first.
    pub fn remember_connection(
        &mut self,
        device: DeviceId,
        addr: Option<String>,
        pin: Option<String>,
    ) -> Result<(), StoreError> {
        if let Some(existing) = self.peers.get_mut(&device.0) {
            if addr.is_some() {
                existing.last_known_addr = addr;
            }
            if pin.is_some() {
                existing.last_known_pin = pin;
            }
            self.flush()?;
        }
        Ok(())
    }

    /// Persist the queue-related identity for this peer: their iroh
    /// `NodeId` and the per-pair symmetric key both ends derived from
    /// the pairing `SessionSecret`. Called from the engine immediately
    /// after a successful handshake so that future store-and-forward
    /// pushes / pulls can address and encrypt to this peer without a
    /// fresh handshake.
    ///
    /// No-op if the peer isn't in the store yet — caller should upsert
    /// the identity first. Either input may be `None`; the unset field
    /// is left intact (so it's safe to call this multiple times across
    /// re-pairings without losing prior data).
    pub fn remember_queue_pair(
        &mut self,
        device: DeviceId,
        peer_node_id: Option<[u8; 32]>,
        queue_pair_key: Option<[u8; 32]>,
    ) -> Result<(), StoreError> {
        if let Some(existing) = self.peers.get_mut(&device.0) {
            if peer_node_id.is_some() {
                existing.peer_node_id = peer_node_id;
            }
            // B2 (set-once): the per-pair queue key must NEVER rotate after it's
            // first established. It is an HKDF subkey of the per-HANDSHAKE
            // session secret, so re-deriving it every reconnect — as the old
            // UNCONDITIONAL overwrite did — changed it each connection. Any
            // ciphertext already sitting on the queue under the previous key
            // then became permanently undecryptable and was deleted on the next
            // pull (`queue_ops::try_queue_pull_all`), and because the durable
            // outbox drops an intent as soon as the server ACCEPTS it, that lost
            // message could not be regenerated — the release-blocker data-loss
            // path. Writing the key only into an EMPTY slot freezes it at the
            // first-pair value (both ends derive the identical key from the
            // shared first-pair session, so they always agree) and, as a bonus,
            // back-fills legacy pre-v3/pre-SEC-2 peers — whose migrated record
            // carries `None` — exactly once, on their first reconnect. A re-pair
            // routes through `remove()` first, which clears the slot (and the
            // keychain entry), so a fresh key is still installed on re-pair.
            // NOTE (definitive follow-up): the fully rotation-proof design is to
            // stop persisting this at all and derive it on demand from the
            // stored resumption token (stable per pair, always consistent across
            // both ends), once B4 guarantees every peer has a resumption token.
            if queue_pair_key.is_some() && existing.queue_pair_key.is_none() {
                existing.queue_pair_key = queue_pair_key;
            }
            self.flush()?;
        }
        Ok(())
    }

    /// AUDIT-015: pin the peer's roost id at first pair (or update it on
    /// any explicit "user accepted the new roost" flow). Writes through
    /// the keychain so the on-disk envelope doesn't carry it.
    pub fn remember_peer_vault_id(
        &mut self,
        device: DeviceId,
        peer_vault_id: [u8; 16],
    ) -> Result<(), StoreError> {
        if let Some(existing) = self.peers.get_mut(&device.0) {
            existing.peer_vault_id = Some(peer_vault_id);
            let id_hex = hex::encode(device.0);
            if let Err(e) = self.secrets.set(
                &secret_store::keys::sync_peer_vault_id(&id_hex),
                &hex::encode(peer_vault_id),
            ) {
                tracing::warn!(
                    error = %e,
                    %id_hex,
                    "peer_store: vault-id keychain write failed; in-memory value is set and will be retried at next remember"
                );
            }
            self.flush()?;
        }
        Ok(())
    }

    /// SEC-2: persist the per-peer resumption token (keychain-backed). Called
    /// after a FIRST-PAIR handshake, where both sides derive the same token
    /// from the shared session secret. `None` is a no-op — reconnects prove the
    /// existing token and must not overwrite it.
    pub fn remember_resumption_token(
        &mut self,
        device: DeviceId,
        token: Option<[u8; 32]>,
    ) -> Result<(), StoreError> {
        if let (Some(existing), Some(tok)) = (self.peers.get_mut(&device.0), token) {
            existing.resumption_token = Some(tok);
            let id_hex = hex::encode(device.0);
            if let Err(e) = self.secrets.set(
                &secret_store::keys::sync_resumption_token(&id_hex),
                &hex::encode(tok),
            ) {
                tracing::warn!(error = %e, %id_hex, "peer_store: resumption-token keychain write failed");
            }
            self.flush()?;
        }
        Ok(())
    }

    pub fn get(&self, device: DeviceId) -> Option<&PeerRecord> {
        self.peers.get(&device.0)
    }

    pub fn remove(&mut self, device: DeviceId) -> Result<bool, StoreError> {
        let removed = self.peers.remove(&device.0).is_some();
        if removed {
            // Best-effort: clear the keychain entries for this device so
            // an unpair leaves no residual secrets behind. A failure
            // here would leave stale entries that a future re-pair would
            // happily overwrite — never user-visible, just cruft.
            let id_hex = hex::encode(device.0);
            if let Err(e) = self.secrets.delete(&secret_store::keys::sync_pin(&id_hex)) {
                tracing::warn!(error = %e, %id_hex, "peer_store: PIN delete on unpair failed");
            }
            if let Err(e) = self
                .secrets
                .delete(&secret_store::keys::sync_queue_key(&id_hex))
            {
                tracing::warn!(error = %e, %id_hex, "peer_store: queue_key delete on unpair failed");
            }
            if let Err(e) = self
                .secrets
                .delete(&secret_store::keys::sync_peer_vault_id(&id_hex))
            {
                tracing::warn!(error = %e, %id_hex, "peer_store: vault-id delete on unpair failed");
            }
            if let Err(e) = self
                .secrets
                .delete(&secret_store::keys::sync_resumption_token(&id_hex))
            {
                tracing::warn!(error = %e, %id_hex, "peer_store: resumption-token delete on unpair failed");
            }
            self.flush()?;
        }
        Ok(removed)
    }

    pub fn list(&self) -> Vec<PeerRecord> {
        self.peers.values().cloned().collect()
    }

    pub fn is_empty(&self) -> bool {
        self.peers.is_empty()
    }

    fn flush(&self) -> Result<(), StoreError> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        // Persist secrets through the keychain backend BEFORE writing
        // the on-disk envelope. Failures here are logged-and-continued —
        // a stale or missing keychain entry just means the auto-reconnect
        // path will prompt the user once on the next launch, which is
        // strictly better than refusing to write the on-disk pairing.
        for record in self.peers.values() {
            let id_hex = hex::encode(record.device_id);
            if let Some(pin) = record.last_known_pin.as_deref() {
                if let Err(e) = self
                    .secrets
                    .set(&secret_store::keys::sync_pin(&id_hex), pin)
                {
                    tracing::warn!(error = %e, %id_hex, "peer_store: PIN keychain write failed");
                }
            }
            if let Some(key) = record.queue_pair_key.as_ref() {
                if let Err(e) = self.secrets.set(
                    &secret_store::keys::sync_queue_key(&id_hex),
                    &hex::encode(key),
                ) {
                    tracing::warn!(error = %e, %id_hex, "peer_store: queue_pair_key keychain write failed");
                }
            }
            if let Some(tok) = record.resumption_token.as_ref() {
                if let Err(e) = self.secrets.set(
                    &secret_store::keys::sync_resumption_token(&id_hex),
                    &hex::encode(tok),
                ) {
                    tracing::warn!(error = %e, %id_hex, "peer_store: resumption-token keychain write failed");
                }
            }
        }
        let env = Envelope {
            version: PEER_FILE_VERSION,
            peers: self.peers.values().map(PeerRecordV4::from_record).collect(),
        };
        let bytes = postcard::to_allocvec(&env).map_err(|e| StoreError::Encode(e.to_string()))?;
        let tmp = self.path.with_extension("bin.tmp");
        // fsync the temp file BEFORE the rename — see DocStore::save. The
        // peer store holds pairing/resumption state whose loss forces a
        // re-pair; an unsynced write can leave it truncated after power loss.
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

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;
    use tempfile::tempdir;

    /// Test-only secret backend — a `HashMap` instead of the real OS
    /// keychain so test runs don't pollute the user's keychain (and so
    /// CI doesn't need any keychain wiring at all).
    #[derive(Default)]
    struct InMemorySecretBackend {
        inner: Mutex<HashMap<String, String>>,
    }

    impl SecretBackend for InMemorySecretBackend {
        fn get(&self, key: &str) -> Option<String> {
            self.inner.lock().unwrap().get(key).cloned()
        }
        fn set(&self, key: &str, value: &str) -> Result<(), String> {
            self.inner
                .lock()
                .unwrap()
                .insert(key.to_string(), value.to_string());
            Ok(())
        }
        fn delete(&self, key: &str) -> Result<(), String> {
            self.inner.lock().unwrap().remove(key);
            Ok(())
        }
    }

    fn mem_backend() -> Arc<dyn SecretBackend> {
        Arc::new(InMemorySecretBackend::default())
    }

    fn load(path: &std::path::Path, secrets: Arc<dyn SecretBackend>) -> PeerStore {
        PeerStore::load_with_secrets(path, secrets).unwrap()
    }

    fn rec(id: u8, name: &str) -> PeerRecord {
        PeerRecord::new(DeviceId([id; 16]), [id; 32], name.into())
    }

    #[test]
    fn empty_store_when_file_absent() {
        let roost = tempdir().unwrap();
        let store = load(roost.path(), mem_backend());
        assert!(store.is_empty());
    }

    #[test]
    fn upsert_then_reload_roundtrips() {
        let roost = tempdir().unwrap();
        let secrets = mem_backend();
        let mut store = load(roost.path(), secrets.clone());
        store.upsert(rec(1, "laptop")).unwrap();
        store.upsert(rec(2, "phone")).unwrap();

        let reloaded = load(roost.path(), secrets);
        let mut names = reloaded
            .list()
            .into_iter()
            .map(|r| r.display_name)
            .collect::<Vec<_>>();
        names.sort();
        assert_eq!(names, vec!["laptop", "phone"]);
    }

    #[test]
    fn upsert_preserves_paired_at() {
        let roost = tempdir().unwrap();
        let mut store = load(roost.path(), mem_backend());
        let original = rec(1, "laptop");
        let original_paired = original.paired_at_millis;
        store.upsert(original).unwrap();

        // Simulate re-pairing later: different display_name, later timestamp.
        std::thread::sleep(std::time::Duration::from_millis(5));
        let mut updated = rec(1, "laptop-renamed");
        updated.paired_at_millis = now_millis(); // would overwrite if we allowed it
        store.upsert(updated).unwrap();

        let got = store.get(DeviceId([1; 16])).unwrap();
        assert_eq!(got.display_name, "laptop-renamed");
        assert_eq!(
            got.paired_at_millis, original_paired,
            "first-pair time is sticky"
        );
    }

    #[test]
    fn touch_updates_last_seen() {
        let roost = tempdir().unwrap();
        let mut store = load(roost.path(), mem_backend());
        store.upsert(rec(1, "laptop")).unwrap();
        let before = store.get(DeviceId([1; 16])).unwrap().last_seen_millis;

        std::thread::sleep(std::time::Duration::from_millis(5));
        store.touch(DeviceId([1; 16])).unwrap();
        let after = store.get(DeviceId([1; 16])).unwrap().last_seen_millis;
        assert!(after >= before);
    }

    #[test]
    fn remove_deletes() {
        let roost = tempdir().unwrap();
        let secrets = mem_backend();
        let mut store = load(roost.path(), secrets.clone());
        store.upsert(rec(1, "laptop")).unwrap();
        assert!(store.remove(DeviceId([1; 16])).unwrap());
        assert!(!store.remove(DeviceId([1; 16])).unwrap()); // idempotent
        assert!(load(roost.path(), secrets).is_empty());
    }

    #[test]
    fn rejects_wrong_version() {
        let roost = tempdir().unwrap();
        let path = peers_file(roost.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let bogus = Envelope {
            version: 99,
            peers: vec![PeerRecordV4::from_record(&rec(1, "x"))],
        };
        std::fs::write(&path, postcard::to_allocvec(&bogus).unwrap()).unwrap();
        assert!(PeerStore::load_with_secrets(roost.path(), mem_backend()).is_err());
    }

    #[test]
    fn migrates_v1_file_on_load() {
        let roost = tempdir().unwrap();
        let path = peers_file(roost.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let v1 = EnvelopeV1 {
            version: 1,
            peers: vec![PeerRecordV1 {
                device_id: [7; 16],
                fingerprint: [8; 32],
                display_name: "pre-upgrade-phone".into(),
                paired_at_millis: 12345,
                last_seen_millis: 67890,
            }],
        };
        std::fs::write(&path, postcard::to_allocvec(&v1).unwrap()).unwrap();

        let secrets = mem_backend();
        let store = PeerStore::load_with_secrets(roost.path(), secrets.clone())
            .expect("v1 file should migrate");
        let rec = store.get(DeviceId([7; 16])).expect("peer preserved");
        assert_eq!(rec.display_name, "pre-upgrade-phone");
        assert_eq!(rec.paired_at_millis, 12345);
        assert!(rec.last_known_addr.is_none());
        assert!(rec.last_known_pin.is_none());

        // File should have been rewritten as v4 — reloading must parse
        // cleanly as the current version.
        let reloaded = PeerStore::load_with_secrets(roost.path(), secrets).expect("v4 reload");
        assert_eq!(reloaded.list().len(), 1);
    }

    /// THE reconnect-credential regression ("worked once, grey with
    /// 're-pair needed' on the second relaunch"): re-recording a peer's
    /// identity on a reconnect must PRESERVE its stored redial credentials
    /// and resumption token. The old `upsert(PeerRecord::new(..))` on the
    /// connection paths blanked them all.
    #[test]
    fn ensure_peer_preserves_credentials_on_reconnect() {
        let roost = tempdir().unwrap();
        let secrets = mem_backend();
        let mut store = load(roost.path(), secrets.clone());
        // Initial pair: full record with every credential populated.
        store.upsert(rec(1, "laptop")).unwrap();
        store
            .remember_connection(
                DeviceId([1; 16]),
                Some("corvux1:ticketdata".into()),
                Some("123456".into()),
            )
            .unwrap();
        store
            .remember_queue_pair(DeviceId([1; 16]), Some([0xab; 32]), Some([0xcd; 32]))
            .unwrap();
        store
            .remember_peer_vault_id(DeviceId([1; 16]), [0x11; 16])
            .unwrap();
        store
            .remember_resumption_token(DeviceId([1; 16]), Some([0x77; 32]))
            .unwrap();

        // Reconnect: the connection path re-records identity (zeroed
        // fingerprint on the iroh path, possibly a renamed device).
        store
            .ensure_peer(DeviceId([1; 16]), [0u8; 32], "laptop-renamed".into())
            .unwrap();

        let reloaded = load(roost.path(), secrets);
        let got = reloaded.get(DeviceId([1; 16])).unwrap();
        assert_eq!(got.display_name, "laptop-renamed", "identity refreshed");
        assert_eq!(
            got.last_known_addr.as_deref(),
            Some("corvux1:ticketdata"),
            "redial ticket must survive a reconnect"
        );
        assert_eq!(
            got.last_known_pin.as_deref(),
            Some("123456"),
            "redial PIN must survive a reconnect"
        );
        assert_eq!(got.peer_node_id, Some([0xab; 32]), "node id preserved");
        assert_eq!(got.queue_pair_key, Some([0xcd; 32]), "queue key preserved");
        assert_eq!(got.peer_vault_id, Some([0x11; 16]), "vault id preserved");
        assert_eq!(
            got.resumption_token,
            Some([0x77; 32]),
            "SEC-2 resumption token must survive a reconnect"
        );
        assert_eq!(
            got.fingerprint, [1; 32],
            "zeroed fingerprint must not clobber the pinned one"
        );
    }

    /// `ensure_peer` on an unknown device behaves like the old first-pair
    /// insert, and a real (non-zero) fingerprint updates the pinned value.
    #[test]
    fn ensure_peer_inserts_when_absent_and_adopts_real_fingerprint() {
        let roost = tempdir().unwrap();
        let secrets = mem_backend();
        let mut store = load(roost.path(), secrets.clone());
        store
            .ensure_peer(DeviceId([2; 16]), [0u8; 32], "phone".into())
            .unwrap();
        let got = store.get(DeviceId([2; 16])).unwrap();
        assert_eq!(got.display_name, "phone");
        assert!(
            got.last_known_addr.is_none(),
            "fresh identity, no creds yet"
        );
        // A later TLS handshake observes a real fingerprint — adopt it.
        store
            .ensure_peer(DeviceId([2; 16]), [0x42; 32], "phone".into())
            .unwrap();
        let reloaded = load(roost.path(), secrets);
        let got = reloaded.get(DeviceId([2; 16])).unwrap();
        assert_eq!(got.fingerprint, [0x42; 32], "non-zero fingerprint adopted");
    }

    #[test]
    fn remember_connection_persists() {
        let roost = tempdir().unwrap();
        let secrets = mem_backend();
        let mut store = load(roost.path(), secrets.clone());
        store.upsert(rec(1, "laptop")).unwrap();
        store
            .remember_connection(
                DeviceId([1; 16]),
                Some("192.168.1.5:7337".into()),
                Some("123456".into()),
            )
            .unwrap();
        let reloaded = load(roost.path(), secrets);
        let got = reloaded.get(DeviceId([1; 16])).unwrap();
        assert_eq!(got.last_known_addr.as_deref(), Some("192.168.1.5:7337"));
        assert_eq!(got.last_known_pin.as_deref(), Some("123456"));
    }

    #[test]
    fn remember_queue_pair_persists() {
        let roost = tempdir().unwrap();
        let secrets = mem_backend();
        let mut store = load(roost.path(), secrets.clone());
        store.upsert(rec(1, "laptop")).unwrap();
        store
            .remember_queue_pair(DeviceId([1; 16]), Some([0xab; 32]), Some([0xcd; 32]))
            .unwrap();
        let reloaded = load(roost.path(), secrets);
        let got = reloaded.get(DeviceId([1; 16])).unwrap();
        assert_eq!(got.peer_node_id, Some([0xab; 32]));
        assert_eq!(got.queue_pair_key, Some([0xcd; 32]));
    }

    /// B2 (set-once): the per-pair queue key must be written exactly once and
    /// then be immune to overwrite, because it is an HKDF subkey of the
    /// per-handshake session and re-deriving it on every reconnect rotated it —
    /// orphaning (and deleting) any ciphertext already queued under the prior
    /// key. This asserts: (1) an empty slot is filled (first pair / legacy
    /// back-fill), (2) a subsequent call with a DIFFERENT key does NOT rotate it
    /// (the reconnect case that used to lose queued messages), and (3) the peer
    /// NodeId still updates on reconnect even though the key is frozen.
    #[test]
    fn remember_queue_pair_is_set_once_for_the_key() {
        let roost = tempdir().unwrap();
        let secrets = mem_backend();
        let mut store = load(roost.path(), secrets.clone());
        store.upsert(rec(1, "laptop")).unwrap();

        // First pair: empty slot is filled.
        store
            .remember_queue_pair(DeviceId([1; 16]), Some([0xab; 32]), Some([0xcd; 32]))
            .unwrap();
        assert_eq!(
            store.get(DeviceId([1; 16])).unwrap().queue_pair_key,
            Some([0xcd; 32])
        );

        // Reconnect: a freshly (re-)derived, DIFFERENT key must NOT overwrite —
        // the stored first-pair key is preserved so queued ciphertext stays
        // decryptable. The NodeId, however, still refreshes.
        store
            .remember_queue_pair(DeviceId([1; 16]), Some([0x11; 32]), Some([0x99; 32]))
            .unwrap();
        let reloaded = load(roost.path(), secrets);
        let got = reloaded.get(DeviceId([1; 16])).unwrap();
        assert_eq!(
            got.queue_pair_key,
            Some([0xcd; 32]),
            "the queue key must be frozen at its first-pair value, never rotated on reconnect"
        );
        assert_eq!(
            got.peer_node_id,
            Some([0x11; 32]),
            "the peer NodeId must still update on reconnect"
        );
    }

    #[test]
    fn migrates_v2_file_on_load() {
        let roost = tempdir().unwrap();
        let path = peers_file(roost.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let v2 = EnvelopeV2 {
            version: 2,
            peers: vec![PeerRecordV2 {
                device_id: [9; 16],
                fingerprint: [0; 32],
                display_name: "v2-paired".into(),
                paired_at_millis: 100,
                last_seen_millis: 200,
                last_known_addr: Some("corvux1:abc".into()),
                last_known_pin: Some("000000".into()),
            }],
        };
        std::fs::write(&path, postcard::to_allocvec(&v2).unwrap()).unwrap();

        let secrets = mem_backend();
        let store = PeerStore::load_with_secrets(roost.path(), secrets.clone())
            .expect("v2 file should migrate");
        let got = store.get(DeviceId([9; 16])).expect("peer preserved");
        assert_eq!(got.display_name, "v2-paired");
        assert_eq!(got.last_known_addr.as_deref(), Some("corvux1:abc"));
        // New fields default to None for migrated peers.
        assert!(got.peer_node_id.is_none());
        assert!(got.queue_pair_key.is_none());
        // The PIN that was on disk in v2 is now silently dropped — v2
        // had no concept of secret separation. The user re-pairs once;
        // future records land in the keychain via v4.

        // Reload should now see v4 cleanly.
        let reloaded = load(roost.path(), secrets);
        assert_eq!(reloaded.list().len(), 1);
    }

    #[test]
    fn migrates_v3_lifts_secrets_to_keychain_and_strips_them_from_disk() {
        let roost = tempdir().unwrap();
        let path = peers_file(roost.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let v3 = EnvelopeV3 {
            version: 3,
            peers: vec![PeerRecordV3 {
                device_id: [3; 16],
                fingerprint: [0; 32],
                display_name: "v3-paired".into(),
                paired_at_millis: 100,
                last_seen_millis: 200,
                last_known_addr: Some("corvux1:zzz".into()),
                last_known_pin: Some("424242".into()),
                peer_node_id: Some([0xaa; 32]),
                queue_pair_key: Some([0xbb; 32]),
            }],
        };
        std::fs::write(&path, postcard::to_allocvec(&v3).unwrap()).unwrap();

        let secrets = mem_backend();
        let store =
            PeerStore::load_with_secrets(roost.path(), secrets.clone()).expect("v3 should migrate");
        // Reading exposes the migrated secrets via the in-memory record.
        let got = store.get(DeviceId([3; 16])).unwrap();
        assert_eq!(got.last_known_pin.as_deref(), Some("424242"));
        assert_eq!(got.queue_pair_key, Some([0xbb; 32]));

        // The keychain backend got the secrets.
        let id_hex = hex::encode([3u8; 16]);
        assert_eq!(
            secrets.get(&secret_store::keys::sync_pin(&id_hex)),
            Some("424242".to_string())
        );
        assert_eq!(
            secrets.get(&secret_store::keys::sync_queue_key(&id_hex)),
            Some(hex::encode([0xbb; 32]))
        );

        // The on-disk file is now v4 and contains NO secrets.
        let bytes = std::fs::read(&path).unwrap();
        let env: Envelope = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(env.version, 4);
        let raw_str = format!("{bytes:?}");
        assert!(
            !raw_str.contains("424242"),
            "v4 envelope must not contain the migrated PIN"
        );
    }

    #[test]
    fn remove_clears_keychain_entries() {
        let roost = tempdir().unwrap();
        let secrets = mem_backend();
        let mut store = load(roost.path(), secrets.clone());
        store.upsert(rec(1, "laptop")).unwrap();
        store
            .remember_connection(DeviceId([1; 16]), None, Some("121212".into()))
            .unwrap();
        let id_hex = hex::encode([1u8; 16]);
        assert_eq!(
            secrets.get(&secret_store::keys::sync_pin(&id_hex)),
            Some("121212".to_string())
        );

        store.remove(DeviceId([1; 16])).unwrap();
        assert_eq!(secrets.get(&secret_store::keys::sync_pin(&id_hex)), None);
    }
}
