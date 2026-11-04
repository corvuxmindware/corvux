//! Lock a secret file down to the current user — the desktop analogue of the
//! mobile at-rest protections (Android `allowBackup=false`, iOS
//! `NSURLIsExcludedFromBackupKey`).
//!
//! On desktop there is no per-app sandbox encrypting our data dir, so files we
//! write under `app_data_dir()` are readable by whatever the parent directory's
//! ACL/mode allows. For most files that's fine, but the sync crypto identity is
//! crown-jewel material:
//!
//!   * `<app_data>/sync/identity.bin` — the Ed25519 device / iroh-NodeId secret.
//!   * `<app_data>/sync/tls.bin` — the QUIC TLS certificate **and private key**.
//!
//! A copy of either lets another local principal impersonate this device to its
//! paired peers. So we tighten both to "current user only":
//!
//!   * **Unix** — `chmod 0600`. Other unprivileged users can't read it (root
//!     still can; documented, and root already owns the box).
//!   * **Windows** — `icacls /inheritance:r /grant:r <user>:(F)`. Stripping
//!     inheritance drops the inherited *Administrators* and *SYSTEM* ACEs that a
//!     profile folder carries by default, so a second account — even a local
//!     admin — can no longer read the key. (A true admin can re-take ownership;
//!     this raises the bar to "deliberate privileged act," not "open the file.")
//!
//! Idempotent and best-effort: re-running it on an already-locked file is a
//! no-op, and any failure is returned for the caller to log — never fatal,
//! matching the sync layer's "storage failure never blocks the app" contract.
//! Because it's idempotent, callers re-assert it on every load, which
//! retro-tightens files created before this lock shipped.

use std::path::Path;

/// Restrict `path` (which must already exist) to read/write by the current user
/// only. No-op success on platforms without a supported mechanism.
pub fn restrict_to_current_user(path: &Path) -> Result<(), String> {
    imp::restrict(path)
}

#[cfg(unix)]
mod imp {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    pub fn restrict(path: &Path) -> Result<(), String> {
        let mut perms = fs::metadata(path)
            .map_err(|e| format!("stat {}: {e}", path.display()))?
            .permissions();
        perms.set_mode(0o600);
        fs::set_permissions(path, perms).map_err(|e| format!("chmod 600 {}: {e}", path.display()))
    }
}

#[cfg(windows)]
mod imp {
    use std::os::windows::process::CommandExt;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    /// `CreateProcess` flag — don't allocate a console for the child. The app is
    /// built `windows_subsystem = "windows"` (no console of its own), so a bare
    /// `Command::spawn` of a console tool like `icacls` flashes a black window.
    /// Same constant the MCP sidecar launcher uses (`commands/mcp.rs`).
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    /// Absolute path to the real `icacls`, so a planted `icacls.exe` on `PATH`
    /// or in the process CWD can't be run in its place. Falls back to the bare
    /// name only if `SystemRoot` is somehow unset.
    fn icacls_exe() -> PathBuf {
        match std::env::var_os("SystemRoot") {
            Some(root) => PathBuf::from(root).join("System32").join("icacls.exe"),
            None => PathBuf::from("icacls.exe"),
        }
    }

    pub fn restrict(path: &Path) -> Result<(), String> {
        let username = std::env::var("USERNAME").unwrap_or_default();
        if username.is_empty() {
            return Err("USERNAME env var unavailable".into());
        }
        // /inheritance:r strips inherited ACEs (incl. Administrators/SYSTEM);
        // /grant:r replaces (rather than adds to) the current user's ACE with
        // full control. /Q suppresses the success summary so a normal save
        // doesn't spam any attached console.
        let output = Command::new(icacls_exe())
            .arg(path)
            .arg("/inheritance:r")
            .arg("/grant:r")
            .arg(format!("{username}:(F)"))
            .arg("/Q")
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .map_err(|e| format!("spawn icacls: {e}"))?;
        if !output.status.success() {
            return Err(format!(
                "icacls exit {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        Ok(())
    }
}

#[cfg(not(any(unix, windows)))]
mod imp {
    use std::path::Path;
    pub fn restrict(_path: &Path) -> Result<(), String> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Locking a file must not break the owner's ability to read it back — the
    /// whole point is "tight, but still ours." Cross-user *denial* needs a
    /// second account and is deferred to the manual pre-release pass (same note
    /// as identity.rs's AUDIT-014 test).
    #[test]
    fn restrict_keeps_file_readable_by_owner() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret.bin");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(b"crown-jewel-key-material").unwrap();
        drop(f);

        restrict_to_current_user(&path).expect("restrict should succeed for the owner");

        // Idempotent: a second call on the already-locked file is fine.
        restrict_to_current_user(&path).expect("restrict should be idempotent");

        let back = std::fs::read(&path).expect("owner can still read after locking");
        assert_eq!(back, b"crown-jewel-key-material");
    }

    #[cfg(unix)]
    #[test]
    fn restrict_sets_0600_on_unix() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret.bin");
        std::fs::write(&path, b"x").unwrap();
        restrict_to_current_user(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "expected 0600, got {:o}", mode & 0o777);
    }
}
