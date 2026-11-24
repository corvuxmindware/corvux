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

pub(crate) mod atomic;
pub mod native;
pub mod trash;
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

    /// Move a file or folder into `<roost>/.corvux-trash/` (never a
    /// permanent delete): the delete prompt tells the user they can restore
    /// it from a file manager. Every backend names the entry with
    /// [`trash::unique_trash_rel`] (`<unix-secs>.<name>`, numbered on clash).
    async fn trash(&self, rel: &str) -> Result<(), RoostError>;

    /// [`trash`](Self::trash) for a folder whose notes the caller has ALREADY
    /// enumerated: `expected` lists them (roost-relative). A backend that moves
    /// the folder as one atomic unit (desktop rename, a provider-side move)
    /// needs no cross-check and ignores it. A backend that has to COPY then
    /// delete (the SAF fallback) must refuse to remove the original unless
    /// every expected note is in its verified copy — including when the copy is
    /// unexpectedly empty — because its own listing is the very thing that may
    /// be wrong.
    async fn trash_expecting(&self, rel: &str, _expected: &[String]) -> Result<(), RoostError> {
        self.trash(rel).await
    }

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

/// True when `rel_path` — an entry found while listing `rel_dir` — is a
/// hidden (dot-prefixed) file or folder below `rel_dir`, or sits anywhere
/// inside one. Recursive listings must drop these: `.corvux-trash/` holds
/// deleted notes, `.corvux/` app data, `.git/`/`.obsidian/` other tools'
/// files. Hidden segments in `rel_dir` itself don't count, so listing
/// `.corvux/skills` on purpose still works.
///
/// The SAF bulk walk used to check only each entry's own name, so files
/// inside a hidden folder (e.g. `.corvux/chat-sessions`) leaked into the
/// Android file tree, graph, search and sync seeding.
// Only the Android SAF backend calls this outside tests.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub(crate) fn hidden_below(rel_dir: &str, rel_path: &str) -> bool {
    let dir = rel_dir.trim_matches('/');
    let path = rel_path.trim_start_matches('/');
    let below = if dir.is_empty() {
        path
    } else {
        path.strip_prefix(dir)
            .and_then(|rest| rest.strip_prefix('/'))
            .unwrap_or(path)
    };
    below.split('/').any(|seg| seg.starts_with('.'))
}

/// Prefix of every SAF plugin rejection that means "these rows are NOT the
/// whole directory" (null cursor, provider error, provider still loading).
/// Spelled identically in `DirectoryListing.kt` (`INCOMPLETE_LISTING_PREFIX`);
/// `tests/saf_trash_guard.rs` keeps the two in lockstep.
// Only the Android SAF backend calls this outside tests.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub(crate) const INCOMPLETE_LISTING_PREFIX: &str = "Directory listing incomplete";

/// Prefix of `moveDocument`'s own rejection when the in-between name (a stray
/// `.corvux-trash/<name>`) is taken. Spelled identically in
/// `DirectoryListing.kt` (`STAGING_NAME_TAKEN_PREFIX`); `tests/saf_trash_guard.rs`
/// keeps the two in lockstep. A marker rather than a text pattern: a note's own
/// name or a provider's exception text can contain any phrase.
// Only the Android SAF backend calls this outside tests.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub(crate) const STAGING_NAME_TAKEN_PREFIX: &str = "Staging name taken";

/// True when a plugin error message says a directory listing was incomplete.
/// Checked BEFORE any substring guess ("not found", ...): a provider's own
/// wording must never turn an incomplete listing into [`RoostError::NotFound`],
/// which callers read as "it isn't there".
// Only the Android SAF backend calls this outside tests.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub(crate) fn is_incomplete_listing(message: &str) -> bool {
    message.contains(INCOMPLETE_LISTING_PREFIX)
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
mod hidden_below_tests {
    use super::hidden_below;

    #[test]
    fn hides_dot_entries_and_everything_inside_them() {
        assert!(hidden_below("", ".corvux-trash"));
        assert!(hidden_below("", ".corvux-trash/1700000000.a.md"));
        assert!(hidden_below("", ".corvux/chat-sessions"));
        assert!(hidden_below("", "Notes/.obsidian/plugins/x/README.md"));
        assert!(hidden_below("Notes", "Notes/.attachments/pic.png"));
    }

    #[test]
    fn keeps_normal_entries() {
        assert!(!hidden_below("", "a.md"));
        assert!(!hidden_below("", "Daily/2026/a.md"));
        assert!(!hidden_below("", "notes.v2/a.md"));
        assert!(!hidden_below("", "/Daily/a.md"));
    }

    #[test]
    fn a_hidden_listing_root_is_allowed() {
        // Skills and agents are listed from inside `.corvux` on purpose.
        assert!(!hidden_below(
            ".corvux/skills",
            ".corvux/skills/summarize.md"
        ));
        assert!(!hidden_below(".corvux/skills/", ".corvux/skills/sub/a.md"));
        assert!(hidden_below(".corvux/skills", ".corvux/skills/.draft/a.md"));
    }
}

#[cfg(test)]
mod incomplete_listing_tests {
    use super::*;

    #[test]
    fn recognizes_the_plugin_rejection_even_when_wrapped() {
        let raw = format!("{INCOMPLETE_LISTING_PREFIX}: provider returned no cursor ('Daily')");
        assert!(is_incomplete_listing(&raw));
        // The plugin layer may prefix its own text.
        assert!(is_incomplete_listing(&format!(
            "plugin invoke error: {raw}"
        )));
    }

    #[test]
    fn provider_wording_cannot_masquerade_as_absence() {
        // A provider error that says "not found" is still an incomplete
        // listing, which `map_plugin_err_str` must classify before guessing
        // `NotFound` from the substring.
        let msg = format!("{INCOMPLETE_LISTING_PREFIX}: document not found ('Work')");
        assert!(is_incomplete_listing(&msg));
    }

    #[test]
    fn ordinary_errors_are_not_incomplete_listings() {
        assert!(!is_incomplete_listing("Not found: a.md"));
        assert!(!is_incomplete_listing("Folder not found: Work"));
        assert!(!is_incomplete_listing(
            "Already exists: .corvux-trash/1.a.md"
        ));
    }
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

/// Fault-injecting backend for tests: a real [`native::NativeFs`] that can fail
/// or silently truncate directory listings and mutate the tree mid-operation,
/// the way an Android storage provider does (null cursor, provider error,
/// concurrent write). Lets host tests reproduce the failure modes behind the
/// SAF trash fallback and the folder-delete enumeration without a device.
#[cfg(test)]
pub(crate) mod test_support {
    use std::collections::HashSet;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    use async_trait::async_trait;

    use super::native::NativeFs;
    use super::trash::{RawEntry, RawTree};
    use super::{Entry, RoostError, RoostFs, INCOMPLETE_LISTING_PREFIX};

    pub(crate) struct FaultyFs {
        inner: NativeFs,
        root: PathBuf,
        /// `list(dir)` fails with an incomplete-listing error for this dir.
        pub fail_list_dir: Option<String>,
        /// The first `raw_descendants` call fails outright.
        pub fail_first_raw: bool,
        /// The first `raw_descendants` call silently omits everything BELOW
        /// this directory (the directory entry itself stays listed) — what
        /// `query(...)?.use { }` did for a null cursor in one nested folder.
        pub omit_children_of_on_first_raw: Option<String>,
        /// Right before the second `raw_descendants` call lists, create this
        /// file: a concurrent write landing mid-trash.
        pub write_before_second_raw: Option<(String, Vec<u8>)>,
        /// EVERY `raw_descendants` call omits everything below this directory:
        /// a provider that is consistently wrong, so the two walks agree.
        pub omit_children_of_always: Option<String>,
        /// EVERY `raw_descendants` call returns no rows at all (an empty
        /// listing reported as complete).
        pub omit_all_raw: bool,
        /// `exists(path)` fails ONCE for each path in the set (a transient
        /// provider failure that is now reported as an error).
        pub exists_err_once: Mutex<HashSet<String>>,
        /// `exists(path)` answers `Ok(false)` ONCE for each path in the set
        /// even though the file is there (what the swallowed `findFile`
        /// failure used to look like). Shared-state knobs, so a test can flip
        /// them after the engine has taken its `Arc`.
        pub exists_lie_missing_once: Mutex<HashSet<String>>,
        /// The `expected` list the last `trash_expecting` call received.
        pub trash_expected_seen: Mutex<Option<Vec<String>>>,
        raw_calls: AtomicUsize,
    }

    impl FaultyFs {
        pub(crate) fn new(root: &Path) -> Self {
            Self {
                inner: NativeFs::new(root),
                root: root.to_path_buf(),
                fail_list_dir: None,
                fail_first_raw: false,
                omit_children_of_on_first_raw: None,
                write_before_second_raw: None,
                omit_children_of_always: None,
                omit_all_raw: false,
                exists_err_once: Mutex::new(HashSet::new()),
                exists_lie_missing_once: Mutex::new(HashSet::new()),
                trash_expected_seen: Mutex::new(None),
                raw_calls: AtomicUsize::new(0),
            }
        }

        pub(crate) fn raw_calls(&self) -> usize {
            self.raw_calls.load(Ordering::SeqCst)
        }
    }

    fn walk(root: &Path, rel: &str, out: &mut Vec<RawEntry>) {
        for e in std::fs::read_dir(root.join(rel)).unwrap().flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let child = format!("{rel}/{name}");
            let is_dir = e.file_type().unwrap().is_dir();
            // Like the SAF plugin: size / modified only for files.
            let (size, modified) = if is_dir {
                (None, None)
            } else {
                let md = e.metadata().unwrap();
                let ms = md
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_millis() as i64);
                (Some(md.len() as i64), ms)
            };
            out.push(RawEntry {
                entry: Entry {
                    name,
                    rel_path: child.clone(),
                    is_dir,
                },
                size,
                modified,
            });
            if is_dir {
                walk(root, &child, out);
            }
        }
    }

    #[async_trait]
    impl RoostFs for FaultyFs {
        async fn list(&self, rel_dir: &str) -> Result<Vec<Entry>, RoostError> {
            if self.fail_list_dir.as_deref() == Some(rel_dir.trim_matches('/')) {
                return Err(RoostError::Io(format!(
                    "{INCOMPLETE_LISTING_PREFIX}: injected ('{rel_dir}')"
                )));
            }
            self.inner.list(rel_dir).await
        }
        async fn read(&self, rel: &str) -> Result<Vec<u8>, RoostError> {
            self.inner.read(rel).await
        }
        async fn write_atomic(&self, rel: &str, bytes: &[u8]) -> Result<(), RoostError> {
            self.inner.write_atomic(rel, bytes).await
        }
        async fn create(&self, rel: &str, initial: &[u8]) -> Result<(), RoostError> {
            self.inner.create(rel, initial).await
        }
        async fn trash(&self, rel: &str) -> Result<(), RoostError> {
            self.inner.trash(rel).await
        }
        async fn trash_expecting(&self, rel: &str, expected: &[String]) -> Result<(), RoostError> {
            *self.trash_expected_seen.lock().unwrap() = Some(expected.to_vec());
            self.inner.trash(rel).await
        }
        async fn rename(&self, from_rel: &str, to_rel: &str) -> Result<(), RoostError> {
            self.inner.rename(from_rel, to_rel).await
        }
        async fn mkdir(&self, rel_dir: &str) -> Result<(), RoostError> {
            self.inner.mkdir(rel_dir).await
        }
        async fn rmdir(&self, rel_dir: &str) -> Result<(), RoostError> {
            self.inner.rmdir(rel_dir).await
        }
        async fn exists(&self, rel: &str) -> Result<bool, RoostError> {
            let key = rel.trim_matches('/').to_string();
            if self.exists_err_once.lock().unwrap().remove(&key) {
                return Err(RoostError::Io(format!(
                    "{INCOMPLETE_LISTING_PREFIX}: injected stat failure ('{rel}')"
                )));
            }
            if self.exists_lie_missing_once.lock().unwrap().remove(&key) {
                return Ok(false);
            }
            self.inner.exists(rel).await
        }
        fn display_root(&self) -> &str {
            self.inner.display_root()
        }
        fn root_id(&self) -> String {
            self.inner.root_id()
        }
    }

    #[async_trait]
    impl RawTree for FaultyFs {
        async fn raw_descendants(&self, rel: &str) -> Result<Vec<RawEntry>, RoostError> {
            let call = self.raw_calls.fetch_add(1, Ordering::SeqCst) + 1;
            if call == 1 && self.fail_first_raw {
                return Err(RoostError::Io(format!(
                    "{INCOMPLETE_LISTING_PREFIX}: injected ('{rel}')"
                )));
            }
            if call == 2 {
                if let Some((path, bytes)) = &self.write_before_second_raw {
                    self.inner.write_atomic(path, bytes).await?;
                }
            }
            let mut out = Vec::new();
            walk(&self.root, rel, &mut out);
            let omit = self.omit_children_of_always.as_deref().or(if call == 1 {
                self.omit_children_of_on_first_raw.as_deref()
            } else {
                None
            });
            if let Some(dir) = omit {
                let below = format!("{}/", dir.trim_end_matches('/'));
                out.retain(|r| !r.entry.rel_path.starts_with(&below));
            }
            if self.omit_all_raw {
                out.clear();
            }
            Ok(out)
        }

        async fn remove_original(&self, rel: &str) -> Result<(), RoostError> {
            let p = self.root.join(rel);
            let r = if p.is_dir() {
                std::fs::remove_dir_all(&p)
            } else {
                std::fs::remove_file(&p)
            };
            r.map_err(RoostError::from)
        }

        async fn remove_partial_copy(&self, rel: &str) {
            let p = self.root.join(rel);
            let _ = if p.is_dir() {
                std::fs::remove_dir_all(&p)
            } else {
                std::fs::remove_file(&p)
            };
        }
    }
}
