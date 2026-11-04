// as_actor_id for Automerge actor binding when needed
#![allow(dead_code)]

//! Stable per-device identity.
//!
//! Each Corvux installation generates an Ed25519 keypair on first launch and
//! persists it alongside the roost-independent app data dir. This identity
//! serves three purposes:
//!
//! 1. **DeviceId** — `SHA256(pubkey)[..16]`, a stable 16-byte handle used in
//!    peer registries and as the seed for Automerge `ActorId`s. Stability
//!    across restarts is critical: Automerge history attributes every op to
//!    an actor, and a wandering actor ID would fragment edit attribution.
//!
//! 2. **Signing** — future protocol extensions (e.g. signed device
//!    advertisements for LAN discovery) will use this key. Not used in the
//!    MVP pairing flow, but the key material is already persisted so we don't
//!    pay a migration later.
//!
//! 3. **Actor ID derivation** — every Automerge change written by this device
//!    is tagged with `actor_id = DeviceId`. Two devices editing offline
//!    produce disjoint actor-op streams that merge cleanly on reconnect.
//!
//! **Privacy note:** the keypair is NOT stored in the roost. It lives in the
//! platform app-data dir (`~/.local/share/corvux/` on Linux, `%APPDATA%/corvux/`
//! on Windows, `~/Library/Application Support/corvux/` on macOS, and the
//! app-private dir on Android/iOS). Syncing the roost does not sync identity;
//! each device has its own.

use std::fs;
use std::path::{Path, PathBuf};

use ed25519_dalek::{Signer, SigningKey, VerifyingKey};
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zeroize::Zeroize;

use super::error::{IdentityError, SyncResult};

/// Current on-disk identity format version. Bump when the layout changes.
const IDENTITY_VERSION: u16 = 1;

/// Stable 16-byte device handle. First half of SHA256(pubkey).
///
/// 16 bytes gives 2^64 expected unique devices before a birthday collision,
/// which is comfortably beyond any realistic Corvux user base. Rendered as
/// 32 lowercase hex chars for logs and for use as an Automerge actor ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DeviceId(pub [u8; 16]);

impl DeviceId {
    pub fn from_pubkey(pubkey: &VerifyingKey) -> Self {
        let digest = Sha256::digest(pubkey.as_bytes());
        let mut out = [0u8; 16];
        out.copy_from_slice(&digest[..16]);
        Self(out)
    }

    pub fn as_hex(&self) -> String {
        hex::encode(self.0)
    }

    /// Automerge actor IDs must be even-length hex strings. `DeviceId` is
    /// already 32 hex chars, so this is a straightforward mapping. Using the
    /// device ID directly makes change attribution human-meaningful in debug
    /// tooling: `git log`-style views can show which device wrote each op.
    pub fn as_actor_id(&self) -> automerge::ActorId {
        automerge::ActorId::from(self.0.as_slice())
    }
}

impl std::fmt::Display for DeviceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Abbreviated form for logs: first 8 hex chars. Full form via `as_hex()`.
        write!(f, "{}…", &self.as_hex()[..8])
    }
}

/// Complete per-device identity — keypair plus derived IDs.
///
/// Never clone this gratuitously; the signing key is secret material.
pub struct Identity {
    signing_key: SigningKey,
    verifying_key: VerifyingKey,
    device_id: DeviceId,
}

impl Identity {
    /// Generate a fresh identity. Uses `OsRng` (platform CSPRNG) — never
    /// reproducible, never seeded from user input. Don't call this unless you
    /// know no identity exists on disk; overwriting an existing identity
    /// orphans all prior Automerge history on this device.
    pub fn generate() -> SyncResult<Self> {
        let signing_key = SigningKey::generate(&mut OsRng);
        let verifying_key = signing_key.verifying_key();
        let device_id = DeviceId::from_pubkey(&verifying_key);
        Ok(Self {
            signing_key,
            verifying_key,
            device_id,
        })
    }

    pub fn device_id(&self) -> DeviceId {
        self.device_id
    }

    pub fn verifying_key(&self) -> &VerifyingKey {
        &self.verifying_key
    }

    pub fn sign(&self, msg: &[u8]) -> [u8; 64] {
        self.signing_key.sign(msg).to_bytes()
    }

    /// Raw Ed25519 secret-key bytes. Crate-private because exposing secret
    /// material is a leak risk — this exists only so the iroh transport
    /// can seed its `SecretKey` from the same Ed25519 identity, keeping
    /// `DeviceId` and Iroh `NodeId` in lockstep without persisting two
    /// separate keypairs. Caller is responsible for not retaining the
    /// returned array.
    pub(crate) fn signing_key_bytes(&self) -> [u8; 32] {
        self.signing_key.to_bytes()
    }

    /// Load from disk if present, otherwise generate and persist. This is the
    /// function the engine startup calls — idempotent, thread-safe via the
    /// filesystem's own atomic rename.
    pub fn load_or_create(dir: &Path) -> SyncResult<Self> {
        let path = identity_path(dir);
        let id = if path.exists() {
            Self::load(&path)?
        } else {
            fs::create_dir_all(dir)?;
            let id = Self::generate()?;
            id.persist(&path)?;
            id
        };
        // AUDIT-014: lock the signing key to the current user (Unix 0600 /
        // Windows ACL). Re-asserted on every load — idempotent — so identities
        // created before the lock shipped get tightened on the next launch.
        // Best-effort: a failure leaves the file on its parent dir's inherited
        // ACL, which is the pre-fix behavior, so we log rather than fail the load.
        if let Err(e) = crate::secure_file::restrict_to_current_user(&path) {
            tracing::warn!(error = %e, "identity.bin: could not restrict permissions to current user");
        }
        Ok(id)
    }

    fn load(path: &Path) -> SyncResult<Self> {
        let bytes = fs::read(path)?;
        let disk: DiskIdentity =
            postcard::from_bytes(&bytes).map_err(|_| IdentityError::Corrupt)?;
        if disk.version != IDENTITY_VERSION {
            return Err(IdentityError::UnsupportedVersion {
                expected: IDENTITY_VERSION,
                found: disk.version,
            }
            .into());
        }
        let signing_key = SigningKey::from_bytes(&disk.signing_key);
        let verifying_key = signing_key.verifying_key();
        // Sanity: stored pubkey must match derived pubkey. If not, the file
        // is tampered or truncated — treat as corrupt.
        if verifying_key.to_bytes() != disk.verifying_key {
            return Err(IdentityError::Corrupt.into());
        }
        let device_id = DeviceId::from_pubkey(&verifying_key);
        // Scrub the signing-key bytes we decoded; the `SigningKey` holds its
        // own copy internally.
        let mut zero = disk.signing_key;
        zero.zeroize();
        Ok(Self {
            signing_key,
            verifying_key,
            device_id,
        })
    }

    fn persist(&self, path: &Path) -> SyncResult<()> {
        let disk = DiskIdentity {
            version: IDENTITY_VERSION,
            signing_key: self.signing_key.to_bytes(),
            verifying_key: self.verifying_key.to_bytes(),
        };
        let bytes = postcard::to_allocvec(&disk).map_err(|_| IdentityError::Corrupt)?;
        // Atomic write: temp file + rename. Identity loss is catastrophic
        // (orphans all history), so we accept the extra syscall.
        let tmp = path.with_extension("bin.tmp");
        fs::write(&tmp, &bytes)?;
        fs::rename(&tmp, path)?;
        // Permissions are tightened to the current user in `load_or_create`
        // (the only caller of `persist`), which re-asserts the lock on every
        // load too — see `crate::secure_file::restrict_to_current_user`.
        Ok(())
    }
}

fn identity_path(dir: &Path) -> PathBuf {
    dir.join("identity.bin")
}

/// On-disk representation. Keep this struct minimal and stable; migrations
/// cost us actor-ID continuity, which breaks Automerge history.
#[derive(Serialize, Deserialize)]
struct DiskIdentity {
    version: u16,
    signing_key: [u8; 32],
    verifying_key: [u8; 32],
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn roundtrip_identity() {
        let dir = tempdir().unwrap();
        let a = Identity::load_or_create(dir.path()).unwrap();
        let b = Identity::load_or_create(dir.path()).unwrap();
        assert_eq!(a.device_id(), b.device_id());
        assert_eq!(a.verifying_key().to_bytes(), b.verifying_key().to_bytes());
    }

    #[test]
    fn device_id_is_deterministic_from_pubkey() {
        let id = Identity::generate().unwrap();
        let derived = DeviceId::from_pubkey(id.verifying_key());
        assert_eq!(id.device_id(), derived);
    }

    #[test]
    fn two_generates_produce_distinct_ids() {
        let a = Identity::generate().unwrap();
        let b = Identity::generate().unwrap();
        assert_ne!(a.device_id(), b.device_id());
    }

    /// AUDIT-014: roundtrip after the ACL tightening must still work.
    /// (Cross-user denial requires a second account and is deferred to
    /// the manual pre-release pass.)
    #[test]
    fn windows_acl_tightening_does_not_break_roundtrip() {
        let dir = tempdir().unwrap();
        let a = Identity::load_or_create(dir.path()).unwrap();
        // Reload — exercises the read path against the post-icacls file.
        let b = Identity::load_or_create(dir.path()).unwrap();
        assert_eq!(a.device_id(), b.device_id());
        assert_eq!(a.verifying_key().to_bytes(), b.verifying_key().to_bytes());
        // Sanity: the file is still readable as the writer.
        let bytes = std::fs::read(dir.path().join("identity.bin")).unwrap();
        assert!(!bytes.is_empty());
    }
}
