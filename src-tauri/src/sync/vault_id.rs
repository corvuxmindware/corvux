//! Per-roost identifier (AUDIT-015).
//!
//! Each roost carries a 16-byte random id at `<roost>/.corvux/vault-id.bin`,
//! generated on first open. Combined with the `roost_name` already exchanged
//! in the sync handshake, this gives every roost a globally distinct identity
//! that travels with the roost folder. Two physically separate roosts both
//! named "MyBrain" can no longer pair-and-weld their CRDT histories together.
//!
//! ## Storage
//!
//! On native (desktop / Android app-private) backends the id is written
//! directly into the roost root. On SAF-backed roosts — where the engine
//! can't always reach the roost root with `std::fs` — we fall back to a
//! per-installation derived id keyed by the roost path string. Honesty: in
//! that case the id stops moving with the roost, but the handshake-time
//! mismatch check still works, which is the load-bearing property the
//! audit asked for.

use std::io;
use std::path::Path;

use rand::rngs::OsRng;
use rand::RngCore;
use sha2::{Digest, Sha256};

/// Filename inside `<roost>/.corvux/` that holds the 16 random bytes.
pub const VAULT_ID_FILE: &str = "vault-id.bin";

/// Read the roost id from `<roost>/.corvux/vault-id.bin`, creating it with
/// 16 random bytes if missing. For non-native roost roots (e.g. SAF URIs)
/// where the path is not a real filesystem directory, derive a stable id
/// from the path string instead.
pub fn ensure_vault_id_sync(roost_root: &Path) -> io::Result<[u8; 16]> {
    if roost_root.is_dir() {
        ensure_native_vault_id(roost_root)
    } else {
        Ok(derive_id_from_string(&roost_root.to_string_lossy()))
    }
}

fn ensure_native_vault_id(roost_root: &Path) -> io::Result<[u8; 16]> {
    // SYN-B2: every op goes through NativeFs's symlink-safe resolve, so a
    // hostile roost that plants `.corvux` (or `vault-id.bin`) as a symlink
    // escaping the roost is rejected — no read of a planted id, no write out of
    // the roost. On rejection we return Err and the caller falls back to a
    // path-derived id (same graceful degradation as the SAF branch).
    //
    // `NativeFs::new` does NOT take the roost lock (the open roost already
    // holds it elsewhere); we only need the path-safety guarantees here.
    use crate::roostfs::native::NativeFs;
    let fs = NativeFs::new(roost_root);
    let rel = format!(".corvux/{VAULT_ID_FILE}");
    let to_io = |e: crate::roostfs::RoostError| io::Error::other(e.to_string());

    if fs.exists_blocking(&rel).map_err(to_io)? {
        let bytes = fs.read_blocking(&rel).map_err(to_io)?;
        if bytes.len() == 16 {
            let mut out = [0u8; 16];
            out.copy_from_slice(&bytes);
            return Ok(out);
        }
        // Wrong size — treat as corrupt and rewrite. Replacing the id means
        // existing peer records won't match any more and the user has to
        // re-pair; that's the correct outcome when the integrity signal
        // we're using has been tampered with.
        tracing::warn!(
            "vault-id.bin had unexpected size {}; regenerating",
            bytes.len()
        );
    }
    fs.mkdir_all_blocking(".corvux").map_err(to_io)?;
    let mut id = [0u8; 16];
    OsRng.fill_bytes(&mut id);
    // Atomic (tmp + rename, symlink-safe) write via the hardened backend:
    // roost-id loss would force re-pairing, so a power-cut mid-write must
    // leave either the old value or nothing.
    fs.write_atomic_blocking(&rel, &id).map_err(to_io)?;
    Ok(id)
}

fn derive_id_from_string(s: &str) -> [u8; 16] {
    let mut hasher = Sha256::new();
    hasher.update(b"corvux-vault-id-fallback-v1");
    hasher.update(s.as_bytes());
    let digest = hasher.finalize();
    let mut out = [0u8; 16];
    out.copy_from_slice(&digest[..16]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn ensure_vault_id_creates_on_first_call() {
        let dir = TempDir::new().unwrap();
        let id = ensure_vault_id_sync(dir.path()).unwrap();
        assert_ne!(id, [0u8; 16], "id must be random, not zero");
        let path = dir.path().join(".corvux").join(VAULT_ID_FILE);
        assert!(path.exists(), "vault-id.bin must be written");
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(bytes.len(), 16);
        assert_eq!(bytes.as_slice(), id.as_slice());
    }

    #[test]
    fn ensure_vault_id_is_stable_across_calls() {
        let dir = TempDir::new().unwrap();
        let a = ensure_vault_id_sync(dir.path()).unwrap();
        let b = ensure_vault_id_sync(dir.path()).unwrap();
        assert_eq!(a, b, "id must be stable across reads");
    }

    #[test]
    fn ensure_vault_id_regenerates_on_wrong_size() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join(".corvux")).unwrap();
        std::fs::write(dir.path().join(".corvux").join(VAULT_ID_FILE), b"too short").unwrap();
        let id = ensure_vault_id_sync(dir.path()).unwrap();
        assert_eq!(id.len(), 16);
        let bytes = std::fs::read(dir.path().join(".corvux").join(VAULT_ID_FILE)).unwrap();
        assert_eq!(bytes.len(), 16);
    }

    #[test]
    fn two_distinct_roosts_get_distinct_ids() {
        let a = TempDir::new().unwrap();
        let b = TempDir::new().unwrap();
        let id_a = ensure_vault_id_sync(a.path()).unwrap();
        let id_b = ensure_vault_id_sync(b.path()).unwrap();
        assert_ne!(id_a, id_b, "fresh roosts must get distinct random ids");
    }

    /// SYN-B2 regression: a hostile roost that plants `.corvux` as a symlink
    /// escaping the roost must NOT get vault-id.bin written through the link
    /// (write escape) nor read a planted id from it (identity spoof). Before
    /// the fix, `ensure_native_vault_id` used raw `std::fs` and followed the
    /// symlink; the fix routes every op through `NativeFs`'s symlink-safe
    /// resolve, so the escaping path is rejected and the caller falls back to
    /// a derived id.
    #[cfg(unix)]
    #[test]
    fn refuses_symlinked_dotcorvux_escape() {
        use std::os::unix::fs::symlink;
        let roost = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        // Plant .corvux as a symlink pointing outside the roost.
        symlink(outside.path(), roost.path().join(".corvux")).unwrap();

        // Must not panic; must not write the id through the escaping link.
        let _ = ensure_vault_id_sync(roost.path());

        assert!(
            !outside.path().join(VAULT_ID_FILE).exists(),
            "vault-id.bin must NOT be written through an escaping .corvux symlink"
        );
        assert!(
            !outside.path().join("vault-id.bin.tmp").exists(),
            "no tmp file may leak outside the roost either"
        );
    }

    /// Companion: a planted id file reachable only through the escaping
    /// symlink must not be adopted as this roost's identity.
    #[cfg(unix)]
    #[test]
    fn does_not_adopt_id_planted_behind_symlink() {
        use std::os::unix::fs::symlink;
        let roost = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        // Attacker pre-plants a known id behind the symlinked .corvux.
        let planted = [0xABu8; 16];
        std::fs::write(outside.path().join(VAULT_ID_FILE), planted).unwrap();
        symlink(outside.path(), roost.path().join(".corvux")).unwrap();

        // If it returns Ok, it must be a path-derived id, never the planted one.
        if let Ok(id) = ensure_vault_id_sync(roost.path()) {
            assert_ne!(
                id, planted,
                "must not adopt an id read through an escaping symlink"
            );
        }
    }

    #[test]
    fn non_directory_path_derives_stable_id() {
        let s = std::path::Path::new("content://com.example/tree/MyBrain");
        let a = ensure_vault_id_sync(s).unwrap();
        let b = ensure_vault_id_sync(s).unwrap();
        assert_eq!(a, b, "derived id must be stable for the same input");
        let other = std::path::Path::new("content://com.example/tree/Work");
        let c = ensure_vault_id_sync(other).unwrap();
        assert_ne!(a, c, "different paths must derive different ids");
    }
}
