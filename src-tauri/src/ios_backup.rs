//! iOS — keep secrets and device sync-identity out of device backups.
//!
//! On iOS, every file the app writes under its container (Library/Application
//! Support, where Tauri's `app_data_dir()` resolves) is **included in the
//! device's iCloud + Finder/iTunes backup by default**. That is the iOS
//! analogue of Android's Auto Backup — which we disabled with
//! `android:allowBackup="false"` (commit a5c42f7) precisely so the encrypted
//! API keys, their HKDF seed, and the sync private keys could not ride a
//! backup off the device.
//!
//! iOS has no manifest flag for this; the per-item control is the
//! `NSURLIsExcludedFromBackupKey` resource value. We set it on the two
//! directories that hold our crown-jewel material:
//!
//!   * `<app_data>/secrets/` — the 32-byte HKDF seed (`.corvux/.enc-seed`)
//!     and the AES-256-GCM-wrapped API keys + per-pair queue keys + pairing
//!     PINs (`.corvux/encrypted-secrets.json`). Without exclusion the seed
//!     rides the same backup as the ciphertext it decrypts, so the at-rest
//!     encryption buys nothing against a restore onto another device.
//!   * `<app_data>/sync/` — the Ed25519 device / iroh-NodeId secret
//!     (`identity.bin`), the QUIC TLS key (`tls.bin`), and the peer registry
//!     (`peers.bin`). A cloned identity could authenticate to paired peers.
//!
//! Excluding a directory excludes its whole subtree, including files created
//! later, so this is set once at startup rather than threaded through every
//! write path. Note content under `<app_data>/roosts/` is deliberately *not*
//! excluded: the markdown is the user's data and backing it up is expected.
//!
//! Best-effort: any failure is logged and never blocks launch, matching the
//! secret-store's "never let storage failure stop the app" contract.

use std::path::Path;

/// Set `NSURLIsExcludedFromBackupKey = true` on `path` (which must already
/// exist). No-op on every platform but iOS.
#[cfg(target_os = "ios")]
pub fn exclude_from_backup(path: &Path) -> Result<(), String> {
    use objc::runtime::{Object, BOOL, NO, YES};
    use objc::{class, msg_send, sel, sel_impl};
    use std::ffi::CString;

    let path_c = CString::new(path.to_string_lossy().as_bytes())
        .map_err(|e| format!("path contains interior NUL: {e}"))?;
    // The resource key is a documented, stable string. Building it from the
    // literal (instead of linking the Foundation `NSURLIsExcludedFromBackupKey`
    // extern static) keeps this fully verifiable with `cargo check
    // --target aarch64-apple-ios`, which type-checks but does not link.
    let key_c = CString::new("NSURLIsExcludedFromBackupKey")
        .expect("static key string has no interior NUL");

    // SAFETY: ordinary Foundation message sends. NSString/NSURL/NSNumber and
    // `-[NSURL setResourceValue:forKey:error:]` have existed since iOS 5; the
    // objects produced are autoreleased temporaries owned by the calling
    // thread's autorelease pool, and we keep no references past this scope.
    unsafe {
        let ns_path: *mut Object =
            msg_send![class!(NSString), stringWithUTF8String: path_c.as_ptr()];
        if ns_path.is_null() {
            return Err("NSString stringWithUTF8String returned nil".into());
        }
        let url: *mut Object = msg_send![class!(NSURL), fileURLWithPath: ns_path];
        if url.is_null() {
            return Err("NSURL fileURLWithPath returned nil".into());
        }
        let key: *mut Object = msg_send![class!(NSString), stringWithUTF8String: key_c.as_ptr()];
        let value: *mut Object = msg_send![class!(NSNumber), numberWithBool: YES];
        let mut err: *mut Object = std::ptr::null_mut();
        let ok: BOOL = msg_send![url, setResourceValue: value forKey: key error: &mut err];
        if ok == NO {
            return Err(format!(
                "setResourceValue NSURLIsExcludedFromBackupKey failed for {}",
                path.display()
            ));
        }
    }
    Ok(())
}

/// No-op on non-iOS targets. Android relies on `allowBackup="false"`; desktop
/// secrets live in the OS keychain, not a backed-up file. Kept as the matching
/// half of the cfg pair so callers (and the contract test) can invoke it
/// unconditionally; genuinely unused in non-test host builds.
#[cfg(not(target_os = "ios"))]
#[cfg_attr(not(test), allow(dead_code))]
pub fn exclude_from_backup(_path: &Path) -> Result<(), String> {
    Ok(())
}

/// Exclude the secret + sync-identity directories from iOS device backup.
/// Call once at startup, after `paths::initialize` and the legacy migration
/// (so the post-migration `secrets/` dir is the one we mark).
pub fn harden_sensitive_dirs() {
    #[cfg(target_os = "ios")]
    {
        let Some(root) = crate::paths::app_data_dir() else {
            tracing::warn!("ios_backup: app_data_dir unset; skipping backup exclusion");
            return;
        };
        // `secrets/` holds the seed + encrypted API/pair keys; `sync/` holds
        // the device crypto identity (NodeId secret, TLS key, peer registry).
        for sub in ["secrets", "sync"] {
            let dir = root.join(sub);
            // The resource key can only be set on an existing item, so create
            // the dir if the owning feature hasn't run yet (covers first run).
            if let Err(e) = std::fs::create_dir_all(&dir) {
                tracing::warn!(dir = %dir.display(), error = %e, "ios_backup: create dir failed");
                continue;
            }
            match exclude_from_backup(&dir) {
                Ok(()) => tracing::info!(dir = %sub, "ios_backup: excluded from device backup"),
                Err(e) => tracing::warn!(dir = %sub, error = %e, "ios_backup: exclude failed"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exclude_is_noop_off_ios() {
        // On the host (non-iOS) the call must succeed as a no-op so callers
        // can invoke it unconditionally. The real iOS path is covered by
        // `cargo check --target aarch64-apple-ios` + on-device QA.
        #[cfg(not(target_os = "ios"))]
        assert!(exclude_from_backup(Path::new("/nonexistent/path")).is_ok());
    }
}
