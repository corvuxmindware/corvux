//! Roost filesystem abstraction.
//!
//! Corvux runs against two very different storage backends:
//!
//! - Desktop and Android app-private directories: real file paths, accessed
//!   via `std::fs` / `tokio::fs`. Implemented by [`native::NativeFs`].
//! - Android Storage Access Framework (SAF): user-granted tree URIs, accessed
//!   via Android's `ContentResolver` + `DocumentFile`. Implemented by
//!   `saf::SafFs` (landing in a later slice).
//!
//! The rest of the codebase (graph indexer, search indexer, Tauri file
//! commands) talks to a [`RoostFs`] trait object and doesn't care which
//! backend is live. All paths crossing this boundary are **relative to the
//! roost root**; absolute paths and URIs stay internal to each impl.

use async_trait::async_trait;
use serde::Serialize;
use thiserror::Error;

pub mod native;
pub mod walk;

// Roost-level advisory file lock. Desktop only — mobile roosts live in
// app-private storage where the OS already enforces single-process access.
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub mod lock;

#[cfg(target_os = "android")]
pub mod saf;

/// A single roost entry, as returned by [`RoostFs::list`].
///
/// `rel_path` is relative to the roost root with forward slashes, suitable
/// for display, persistence, and re-entry into other [`RoostFs`] methods.
#[derive(Debug, Clone, Serialize)]
pub struct Entry {
    pub name: String,
    pub rel_path: String,
    pub is_dir: bool,
}

/// Errors every [`RoostFs`] impl can produce. Kept small and stringly so it
/// maps cleanly onto Tauri's `Result<T, String>` IPC convention.
#[derive(Error, Debug)]
pub enum RoostError {
    #[error("not found: {0}")]
    NotFound(String),
    #[error("already exists: {0}")]
    AlreadyExists(String),
    #[error("not a directory: {0}")]
    NotDir(String),
    #[error("directory not empty: {0}")]
    NotEmpty(String),
    #[error("path traversal blocked: {0}")]
    OutsideRoost(String),
    #[error("permission denied: {0}")]
    PermissionDenied(String),
    #[error("{0}")]
    Io(String),
}

impl From<std::io::Error> for RoostError {
    fn from(e: std::io::Error) -> Self {
        use std::io::ErrorKind::*;
        match e.kind() {
            NotFound => Self::NotFound(e.to_string()),
            AlreadyExists => Self::AlreadyExists(e.to_string()),
            PermissionDenied => Self::PermissionDenied(e.to_string()),
            _ => Self::Io(e.to_string()),
        }
    }
}

/// Backend-agnostic roost file operations.
///
/// **Path contract**: all `rel` / `rel_dir` parameters are roost-relative
/// paths with forward slashes. The empty string means the roost root.
/// `..` segments and absolute paths are rejected at the impl boundary.
#[async_trait]
pub trait RoostFs: Send + Sync {
    async fn list(&self, rel_dir: &str) -> Result<Vec<Entry>, RoostError>;

    /// List every descendant (recursive) of `rel_dir` in one call.
    /// Default impl recurses through `list()` — fine for `NativeFs` where
    /// each `list` is a fast `read_dir`. SAF overrides this with a
    /// single-JNI bulk walk; on a deep roost that's the difference
    /// between D round-trips and 1.
    async fn list_recursive(&self, rel_dir: &str) -> Result<Vec<Entry>, RoostError>
    where
        Self: Sync,
    {
        let mut out: Vec<Entry> = Vec::new();
        list_recursive_via_list(self, rel_dir, &mut out).await?;
        Ok(out)
    }

    async fn read(&self, rel: &str) -> Result<Vec<u8>, RoostError>;

    /// Write is atomic: failure leaves any existing file intact.
    async fn write_atomic(&self, rel: &str, bytes: &[u8]) -> Result<(), RoostError>;

    /// Create a new file with initial contents. Fails if it already exists.
    async fn create(&self, rel: &str, initial: &[u8]) -> Result<(), RoostError>;

    /// Move a file to a trash location (not permanent delete). Backends
    /// pick their own trash path — the contract is only "recoverable".
    async fn trash(&self, rel: &str) -> Result<(), RoostError>;

    async fn rename(&self, from_rel: &str, to_rel: &str) -> Result<(), RoostError>;

    async fn mkdir(&self, rel_dir: &str) -> Result<(), RoostError>;

    /// Delete an empty directory. Returns [`RoostError::NotEmpty`] if not.
    /// `delete_folder` no longer routes through here (it goes via `trash`
    /// for recoverability) but the strict-empty semantic is preserved on
    /// the trait for future callers (e.g. agentic write tools that need
    /// to assert empty-ness explicitly).
    #[allow(dead_code)]
    async fn rmdir(&self, rel_dir: &str) -> Result<(), RoostError>;

    async fn exists(&self, rel: &str) -> Result<bool, RoostError>;

    /// Human-readable identifier — for UI, logs, and settings display only.
    /// Not a path consumers may join onto.
    fn display_root(&self) -> &str;

    /// Stable identifier for the underlying roost — absolute filesystem path
    /// for `NativeFs`, tree URI for `SafFs`. Used by `open_roost` to detect
    /// "this roost is already mounted; skip the lock dance" so that a focus-
    /// or sync-driven `refreshFiles` call doesn't race the existing lock
    /// against itself.
    fn root_id(&self) -> String;
}

/// Default recursive walk used by [`RoostFs::list_recursive`]. Generic
/// over the concrete `RoostFs` so it can be called from the trait's
/// default body (where `Self: ?Sized` would block `dyn`-coercion).
async fn list_recursive_via_list<T: RoostFs + Sync + ?Sized>(
    fs: &T,
    rel_dir: &str,
    out: &mut Vec<Entry>,
) -> Result<(), RoostError> {
    let entries = fs.list(rel_dir).await?;
    for entry in entries {
        let is_dir = entry.is_dir;
        let rel_path = entry.rel_path.clone();
        out.push(entry);
        if is_dir {
            Box::pin(list_recursive_via_list(fs, &rel_path, out)).await?;
        }
    }
    Ok(())
}

/// Validate a roost-relative path. Rejects `..`, absolute paths, Windows
/// drive prefixes, and verbatim/device prefixes. Returns the cleaned
/// (forward-slash) form.
pub(crate) fn validate_rel(rel: &str) -> Result<String, RoostError> {
    // Reject Windows verbatim/device path prefixes before slash normalization
    // collapses them. `PathBuf::push` on Windows would *replace* the root when
    // joined with one of these.
    if rel.starts_with(r"\\?\") || rel.starts_with(r"\\.\") {
        return Err(RoostError::OutsideRoost(rel.to_string()));
    }

    let cleaned: String = rel.replace('\\', "/").trim_start_matches('/').to_string();

    for seg in cleaned.split('/') {
        if seg == ".." || seg == "." {
            return Err(RoostError::OutsideRoost(rel.to_string()));
        }
        // Windows drive-letter prefix as a segment (`C:`, `z:foo`). On
        // Windows `root.join("C:/...")` replaces the root, so even a single
        // segment shaped like a drive letter is hostile.
        let bytes = seg.as_bytes();
        if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
            return Err(RoostError::OutsideRoost(rel.to_string()));
        }
    }

    // Final safety net: Rust's own absolute-path detection (platform-aware).
    if std::path::Path::new(&cleaned).is_absolute() {
        return Err(RoostError::OutsideRoost(rel.to_string()));
    }

    Ok(cleaned)
}

#[cfg(test)]
mod validate_rel_tests {
    use super::*;

    #[test]
    fn windows_drive_prefix_blocked() {
        // Plain drive letter as a segment.
        assert!(matches!(
            validate_rel("C:/Windows/win.ini"),
            Err(RoostError::OutsideRoost(_))
        ));
        assert!(matches!(
            validate_rel("c:/x"),
            Err(RoostError::OutsideRoost(_))
        ));
        // Backslash-form (gets normalized) still contains the drive segment.
        assert!(matches!(
            validate_rel(r"D:\foo"),
            Err(RoostError::OutsideRoost(_))
        ));
        // Drive prefix nested mid-path (the audit's original payload).
        assert!(matches!(
            validate_rel("subdir/C:/win.ini"),
            Err(RoostError::OutsideRoost(_))
        ));
    }

    #[test]
    fn windows_verbatim_and_device_prefixes_blocked() {
        assert!(matches!(
            validate_rel(r"\\?\C:\foo"),
            Err(RoostError::OutsideRoost(_))
        ));
        assert!(matches!(
            validate_rel(r"\\.\COM1"),
            Err(RoostError::OutsideRoost(_))
        ));
    }

    #[test]
    fn dot_segment_blocked() {
        assert!(matches!(
            validate_rel("./escape"),
            Err(RoostError::OutsideRoost(_))
        ));
    }

    #[test]
    fn normal_relative_paths_allowed() {
        assert_eq!(validate_rel("a.md").unwrap(), "a.md");
        assert_eq!(validate_rel("folder/note.md").unwrap(), "folder/note.md");
        assert_eq!(validate_rel("/a.md").unwrap(), "a.md");
        // Dotfile in name (not dot-segment) — fine.
        assert_eq!(validate_rel(".hidden.md").unwrap(), ".hidden.md");
        // Multi-byte UTF-8 names — fine.
        assert_eq!(validate_rel("资料/笔记.md").unwrap(), "资料/笔记.md");
        assert_eq!(validate_rel("🦀.md").unwrap(), "🦀.md");
    }
}
