//! `std::fs`-backed [`RoostFs`] implementation.
//!
//! Used for desktop roosts and the Android app-private default roost.
//! Atomic writes use the standard `write tmp → fsync → rename` dance;
//! trash moves preserve the file by timestamp-prefixing inside
//! `<root>/.corvux-trash/`.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use tokio::fs;
use tokio::io::AsyncWriteExt;

use super::{validate_rel, Entry, RoostError, RoostFs};

pub struct NativeFs {
    root: PathBuf,
    /// AUDIT-076: symlink-resolved root, cached at construction. Every
    /// `safe_resolve` op canonicalizes the deepest existing ancestor of
    /// the requested path and verifies it `starts_with` this — so an
    /// in-roost symlink that escapes (`<roost>/linkdir -> /etc`) gets
    /// caught before any read/write happens.
    canonical_root: PathBuf,
    display: String,
    // Roost-level OS lock. Desktop only; mobile roosts live in app-private
    // storage with single-process semantics enforced by the OS sandbox.
    // Held for the lifetime of this NativeFs — when the Arc<dyn RoostFs> in
    // RoostFsState is replaced (roost closed/switched), this drops, which
    // closes the lockfile and releases the OS lock. Field name starts with
    // `_` because nothing reads the lock after construction; its existence
    // *is* the guard.
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    _lock: Option<super::lock::RoostLock>,
}

impl NativeFs {
    /// Test/legacy constructor — does not acquire a roost lock. Production
    /// callers should use [`NativeFs::open_locked`] so two Corvux instances
    /// can't race writes against the same roost.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        let display = root.to_string_lossy().into_owned();
        // AUDIT-076: canonicalize root once. Fall back to the raw root if
        // canonicalize fails (eg root doesn't exist yet); safe_resolve will
        // then refuse anything that can't be canonicalized either, so a
        // dangling root just makes every op fail closed.
        let canonical_root = std::fs::canonicalize(&root).unwrap_or_else(|_| root.clone());
        Self {
            root,
            canonical_root,
            display,
            #[cfg(not(any(target_os = "android", target_os = "ios")))]
            _lock: None,
        }
    }

    /// Open a roost and acquire its exclusive lock. On desktop, this fails
    /// with [`super::lock::LockError::AlreadyLocked`] if another Corvux
    /// instance already has the roost open. On Android/iOS the lock step
    /// is skipped (single-process sandbox) and this is equivalent to
    /// [`NativeFs::new`].
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    pub fn open_locked(root: impl Into<PathBuf>) -> Result<Self, super::lock::LockError> {
        let root = root.into();
        let display = root.to_string_lossy().into_owned();
        let lock = super::lock::RoostLock::acquire(&root)?;
        let canonical_root = std::fs::canonicalize(&root).unwrap_or_else(|_| root.clone());
        Ok(Self {
            root,
            canonical_root,
            display,
            _lock: Some(lock),
        })
    }

    #[cfg(any(target_os = "android", target_os = "ios"))]
    pub fn open_locked(root: impl Into<PathBuf>) -> Result<Self, std::convert::Infallible> {
        Ok(Self::new(root))
    }

    pub fn root_path(&self) -> &Path {
        &self.root
    }

    /// AUDIT-076: resolve `rel` to an absolute path that, after symlink
    /// resolution at *every* ancestor, still lives inside the roost. The
    /// textual `validate_rel` check only catches `..` and absolute paths;
    /// this catches in-roost symlinks (eg `<roost>/linkdir -> /etc`, then
    /// reading `linkdir/passwd`) which slip past the textual check. For
    /// not-yet-created paths we canonicalize the deepest existing
    /// ancestor and reassemble, so write/create/rename targets are also
    /// covered.
    async fn safe_resolve(&self, rel: &str) -> Result<PathBuf, RoostError> {
        // Delegate to the shared sync core so the symlink-escape contract has
        // exactly ONE implementation (the sync `*_blocking` twins below use it
        // too). `resolve_within` is pure `std::fs`; run it off the async worker
        // via `spawn_blocking`, exactly as `tokio::fs` does internally.
        let root = self.root.clone();
        let canonical_root = self.canonical_root.clone();
        let rel = rel.to_string();
        tokio::task::spawn_blocking(move || resolve_within(&root, &canonical_root, &rel))
            .await
            .map_err(|e| RoostError::Io(format!("resolve join failed: {e}")))?
    }

    // ── Sync twins (AUDIT-076 / SYN-B2) ────────────────────────────────────
    // Rare non-async callers (vault_id, resolved during sync-context path
    // setup where `roost_db_dir` is sync) need symlink-safe roost I/O without a
    // tokio runtime. Each mirrors its async sibling and shares the
    // `resolve_within` core, so the escape contract can't drift between the
    // two. `std::fs` here is the hardened backend — the one sanctioned place
    // for raw fs on a roost path.

    /// Sync twin of [`RoostFs::read`].
    pub(crate) fn read_blocking(&self, rel: &str) -> Result<Vec<u8>, RoostError> {
        let p = resolve_within(&self.root, &self.canonical_root, rel)?;
        Ok(std::fs::read(&p)?)
    }

    /// Sync twin of [`RoostFs::exists`] (an escaping path reports `false`, not
    /// an error — matches the async method, avoids leaking existence).
    pub(crate) fn exists_blocking(&self, rel: &str) -> Result<bool, RoostError> {
        match resolve_within(&self.root, &self.canonical_root, rel) {
            Ok(p) => Ok(p.exists()),
            Err(RoostError::OutsideRoost(_)) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Symlink-safe idempotent `create_dir_all` (unlike the async `mkdir`,
    /// which errors on an existing dir; vault_id needs create-if-absent).
    pub(crate) fn mkdir_all_blocking(&self, rel_dir: &str) -> Result<(), RoostError> {
        let p = resolve_within(&self.root, &self.canonical_root, rel_dir)?;
        std::fs::create_dir_all(&p)?;
        Ok(())
    }

    /// Sync twin of [`RoostFs::write_atomic`], including the AUDIT-076b
    /// tmp-symlink defense (refuse a pre-planted `*.corvux.tmp` symlink;
    /// `create_new` so a concurrent plant fails closed).
    pub(crate) fn write_atomic_blocking(&self, rel: &str, bytes: &[u8]) -> Result<(), RoostError> {
        use std::io::Write;
        let target = resolve_within(&self.root, &self.canonical_root, rel)?;
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut tmp_os = target.clone().into_os_string();
        tmp_os.push(".corvux.tmp");
        let tmp = PathBuf::from(tmp_os);
        match std::fs::symlink_metadata(&tmp) {
            Ok(md) if md.file_type().is_symlink() => {
                return Err(RoostError::OutsideRoost(format!(
                    "temp path is a symlink, refusing write: {rel}"
                )));
            }
            Ok(_) => {
                std::fs::remove_file(&tmp)?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, &target)?;
        Ok(())
    }
}

/// Sync core of [`NativeFs::safe_resolve`] (AUDIT-076): resolve `rel` under
/// `root` to an absolute path that, after symlink resolution at EVERY ancestor,
/// still lives inside `canonical_root`. The textual `validate_rel` only catches
/// `..` and absolute paths; this catches in-roost symlinks that escape (e.g.
/// `<roost>/.corvux -> /outside`). For not-yet-created paths it canonicalizes
/// the deepest existing ancestor and reassembles, so write/create targets are
/// covered too. Shared by the async `safe_resolve` (via `spawn_blocking`) and
/// the sync `*_blocking` twins so the escape contract has ONE implementation.
fn resolve_within(root: &Path, canonical_root: &Path, rel: &str) -> Result<PathBuf, RoostError> {
    let cleaned = validate_rel(rel)?;
    let target = if cleaned.is_empty() {
        root.to_path_buf()
    } else {
        root.join(&cleaned)
    };

    let mut probe = target.clone();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let canonical_existing = loop {
        match std::fs::canonicalize(&probe) {
            Ok(c) => break c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // A component that exists as a symlink yet fails to canonicalize
                // is a DANGLING symlink; the deepest-ancestor fallback would
                // wrongly treat it as a plain creatable file, letting a hostile
                // `note.md -> /outside/x` (target absent) slip past. Fail closed.
                if std::fs::symlink_metadata(&probe)
                    .map(|m| m.file_type().is_symlink())
                    .unwrap_or(false)
                {
                    return Err(RoostError::OutsideRoost(format!(
                        "path component is a dangling symlink: {rel}"
                    )));
                }
                let name = probe.file_name().map(|n| n.to_os_string());
                let parent = probe.parent().map(|p| p.to_path_buf());
                match (name, parent) {
                    (Some(n), Some(p)) if !p.as_os_str().is_empty() => {
                        tail.push(n);
                        probe = p;
                    }
                    _ => {
                        return Err(RoostError::OutsideRoost(format!(
                            "path has no resolvable ancestor: {rel}"
                        )));
                    }
                }
            }
            Err(e) => return Err(e.into()),
        }
    };

    let mut final_path = canonical_existing;
    for name in tail.iter().rev() {
        final_path.push(name);
    }

    if !final_path.starts_with(canonical_root) {
        return Err(RoostError::OutsideRoost(format!(
            "path resolves outside roost: {rel}"
        )));
    }
    Ok(final_path)
}

#[async_trait]
impl RoostFs for NativeFs {
    async fn list(&self, rel_dir: &str) -> Result<Vec<Entry>, RoostError> {
        let dir = self.safe_resolve(rel_dir).await?;
        if !dir.is_dir() {
            return Err(RoostError::NotDir(rel_dir.to_string()));
        }
        let mut out = Vec::new();
        let mut rd = fs::read_dir(&dir).await?;
        while let Some(entry) = rd.next_entry().await? {
            let name = entry.file_name().to_string_lossy().into_owned();
            // Skip hidden files and our own trash — caller decides whether
            // to surface them explicitly (it currently doesn't).
            if name.starts_with('.') || name == ".corvux-trash" {
                continue;
            }
            let ft = entry.file_type().await?;
            let rel_path = if rel_dir.is_empty() {
                name.clone()
            } else {
                format!("{}/{}", rel_dir.trim_end_matches('/'), name)
            };
            out.push(Entry {
                name,
                rel_path,
                is_dir: ft.is_dir(),
            });
        }
        Ok(out)
    }

    async fn read(&self, rel: &str) -> Result<Vec<u8>, RoostError> {
        let p = self.safe_resolve(rel).await?;
        Ok(fs::read(&p).await?)
    }

    async fn write_atomic(&self, rel: &str, bytes: &[u8]) -> Result<(), RoostError> {
        let target = self.safe_resolve(rel).await?;
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).await?;
        }
        // Append ".corvux.tmp" so our in-flight writes don't collide with
        // editors that use `.tmp` and so `.md`-scanners don't index them.
        let mut tmp_os = target.clone().into_os_string();
        tmp_os.push(".corvux.tmp");
        let tmp = PathBuf::from(tmp_os);

        // AUDIT-076b: `tmp` is a sibling path derived by string append, so
        // `safe_resolve(target)` does NOT cover it. A malicious roost that
        // pre-plants `note.md.corvux.tmp -> /etc/passwd` would otherwise
        // have `File::create` follow the symlink and write our bytes
        // outside the roost before the rename. Refuse if `tmp` already
        // exists as a symlink/reparse point; remove a stale regular-file
        // tmp from a crashed prior write (`remove_file` removes the link
        // itself, not the target); then use `create_new` so any concurrent
        // attacker planting a symlink between the remove and the open
        // makes the open fail closed.
        match fs::symlink_metadata(&tmp).await {
            Ok(md) if md.file_type().is_symlink() => {
                return Err(RoostError::OutsideRoost(format!(
                    "temp path is a symlink, refusing write: {rel}"
                )));
            }
            Ok(_) => {
                fs::remove_file(&tmp).await?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }

        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .await?;
        f.write_all(bytes).await?;
        f.sync_all().await?;
        drop(f);
        fs::rename(&tmp, &target).await?;
        Ok(())
    }

    async fn create(&self, rel: &str, initial: &[u8]) -> Result<(), RoostError> {
        let target = self.safe_resolve(rel).await?;
        if target.exists() {
            return Err(RoostError::AlreadyExists(rel.to_string()));
        }
        self.write_atomic(rel, initial).await
    }

    async fn trash(&self, rel: &str) -> Result<(), RoostError> {
        let src = self.safe_resolve(rel).await?;
        if !src.exists() {
            return Err(RoostError::NotFound(rel.to_string()));
        }
        // AUDIT-076c: route `.corvux-trash` through safe_resolve too. The
        // pre-fix code built `self.root.join(".corvux-trash")` raw, so a
        // hostile roost that plants `.corvux-trash` as a symlink to
        // outside would have `fs::rename(&src, &dest)` move the user's
        // file out of the roost. The user thinks they deleted a note;
        // the attacker now has a copy at their chosen destination.
        let trash_dir = self.safe_resolve(".corvux-trash").await?;
        fs::create_dir_all(&trash_dir).await?;
        let name = src
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "unnamed".to_string());
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let dest = trash_dir.join(format!("{ts}.{name}"));
        fs::rename(&src, &dest).await?;
        Ok(())
    }

    async fn rename(&self, from_rel: &str, to_rel: &str) -> Result<(), RoostError> {
        let from = self.safe_resolve(from_rel).await?;
        let to = self.safe_resolve(to_rel).await?;
        if !from.exists() {
            return Err(RoostError::NotFound(from_rel.to_string()));
        }
        if to.exists() {
            return Err(RoostError::AlreadyExists(to_rel.to_string()));
        }
        if let Some(parent) = to.parent() {
            fs::create_dir_all(parent).await?;
        }
        fs::rename(&from, &to).await?;
        Ok(())
    }

    async fn mkdir(&self, rel_dir: &str) -> Result<(), RoostError> {
        let p = self.safe_resolve(rel_dir).await?;
        if p.exists() {
            return Err(RoostError::AlreadyExists(rel_dir.to_string()));
        }
        fs::create_dir_all(&p).await?;
        Ok(())
    }

    async fn rmdir(&self, rel_dir: &str) -> Result<(), RoostError> {
        let p = self.safe_resolve(rel_dir).await?;
        if !p.is_dir() {
            return Err(RoostError::NotDir(rel_dir.to_string()));
        }
        let mut rd = fs::read_dir(&p).await?;
        while let Some(entry) = rd.next_entry().await? {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.starts_with('.') {
                return Err(RoostError::NotEmpty(rel_dir.to_string()));
            }
        }
        fs::remove_dir_all(&p).await?;
        Ok(())
    }

    async fn exists(&self, rel: &str) -> Result<bool, RoostError> {
        // AUDIT-076: route through safe_resolve too. An ancestor symlink
        // would otherwise let exists() probe outside the roost and leak
        // file existence info via the IPC return value.
        match self.safe_resolve(rel).await {
            Ok(p) => Ok(p.exists()),
            Err(RoostError::OutsideRoost(_)) => Ok(false),
            Err(e) => Err(e),
        }
    }

    fn display_root(&self) -> &str {
        &self.display
    }

    fn root_id(&self) -> String {
        self.root.to_string_lossy().into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn fresh() -> (TempDir, NativeFs) {
        let dir = TempDir::new().expect("tempdir");
        let fs = NativeFs::new(dir.path());
        (dir, fs)
    }

    #[tokio::test]
    async fn path_traversal_blocked() {
        let (_d, fs) = fresh();
        let err = fs.read("../secret").await.unwrap_err();
        assert!(matches!(err, RoostError::OutsideRoost(_)));
    }

    #[tokio::test]
    async fn absolute_rel_is_normalized() {
        let (_d, fs) = fresh();
        fs.write_atomic("a.md", b"hi").await.unwrap();
        // Leading slash should be stripped, not escape the root.
        let got = fs.read("/a.md").await.unwrap();
        assert_eq!(got, b"hi");
    }

    #[tokio::test]
    async fn write_read_roundtrip() {
        let (_d, fs) = fresh();
        fs.write_atomic("folder/note.md", b"# hello").await.unwrap();
        assert!(fs.exists("folder/note.md").await.unwrap());
        assert_eq!(fs.read("folder/note.md").await.unwrap(), b"# hello");
    }

    #[tokio::test]
    async fn list_skips_hidden_and_trash() {
        let (_d, fs) = fresh();
        fs.write_atomic("a.md", b"").await.unwrap();
        fs.write_atomic(".hidden.md", b"").await.unwrap();
        fs.mkdir(".corvux-trash").await.unwrap();
        let entries = fs.list("").await.unwrap();
        let names: Vec<_> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["a.md"]);
    }

    #[tokio::test]
    async fn trash_moves_not_deletes() {
        let (dir, fs) = fresh();
        fs.write_atomic("a.md", b"x").await.unwrap();
        fs.trash("a.md").await.unwrap();
        assert!(!fs.exists("a.md").await.unwrap());
        let trash = std::fs::read_dir(dir.path().join(".corvux-trash")).unwrap();
        assert_eq!(trash.count(), 1);
    }

    #[tokio::test]
    async fn create_rejects_duplicate() {
        let (_d, fs) = fresh();
        fs.create("a.md", b"").await.unwrap();
        let err = fs.create("a.md", b"").await.unwrap_err();
        assert!(matches!(err, RoostError::AlreadyExists(_)));
    }

    #[tokio::test]
    async fn rmdir_rejects_nonempty() {
        let (_d, fs) = fresh();
        fs.mkdir("f").await.unwrap();
        fs.write_atomic("f/a.md", b"").await.unwrap();
        let err = fs.rmdir("f").await.unwrap_err();
        assert!(matches!(err, RoostError::NotEmpty(_)));
    }

    // ── Error-path coverage ─────────────────────────────────────────────
    //
    // The audit (#31) called out the absence of explicit failure-mode
    // tests for the roost filesystem. These cover the inputs a confused
    // caller can hand us — missing files, wrong types, traversal attempts,
    // collisions — so behavior under those is locked down by the test
    // suite rather than rediscovered the next time someone refactors.

    #[tokio::test]
    async fn read_missing_file_returns_not_found() {
        let (_d, fs) = fresh();
        let err = fs.read("never-written.md").await.unwrap_err();
        assert!(
            matches!(err, RoostError::NotFound(_) | RoostError::Io(_)),
            "expected NotFound or Io for missing file, got {err:?}"
        );
    }

    #[tokio::test]
    async fn list_on_file_path_errors() {
        let (_d, fs) = fresh();
        fs.write_atomic("note.md", b"x").await.unwrap();
        let err = fs.list("note.md").await.unwrap_err();
        assert!(
            matches!(err, RoostError::NotDir(_)),
            "list on a file should error NotDir, got {err:?}"
        );
    }

    #[tokio::test]
    async fn rename_missing_source_returns_not_found() {
        let (_d, fs) = fresh();
        let err = fs.rename("nope.md", "also.md").await.unwrap_err();
        assert!(
            matches!(err, RoostError::NotFound(_)),
            "rename of missing source must be NotFound, got {err:?}"
        );
    }

    #[tokio::test]
    async fn rename_to_existing_target_rejected() {
        let (_d, fs) = fresh();
        fs.write_atomic("a.md", b"a").await.unwrap();
        fs.write_atomic("b.md", b"b").await.unwrap();
        let err = fs.rename("a.md", "b.md").await.unwrap_err();
        assert!(
            matches!(err, RoostError::AlreadyExists(_)),
            "rename onto existing path must be AlreadyExists, got {err:?}"
        );
        // Both files must still be intact — the failed rename mustn't
        // have nuked the destination's content.
        assert_eq!(fs.read("a.md").await.unwrap(), b"a");
        assert_eq!(fs.read("b.md").await.unwrap(), b"b");
    }

    #[tokio::test]
    async fn mkdir_on_existing_path_rejected() {
        let (_d, fs) = fresh();
        fs.mkdir("foo").await.unwrap();
        let err = fs.mkdir("foo").await.unwrap_err();
        assert!(
            matches!(err, RoostError::AlreadyExists(_)),
            "mkdir on existing dir must be AlreadyExists, got {err:?}"
        );
    }

    #[tokio::test]
    async fn rmdir_on_missing_dir_errors() {
        let (_d, fs) = fresh();
        let err = fs.rmdir("never-made").await.unwrap_err();
        assert!(
            matches!(err, RoostError::NotDir(_) | RoostError::NotFound(_)),
            "rmdir of missing dir must be NotDir/NotFound, got {err:?}"
        );
    }

    #[tokio::test]
    async fn trash_on_missing_file_returns_not_found() {
        let (_d, fs) = fresh();
        let err = fs.trash("ghost.md").await.unwrap_err();
        assert!(
            matches!(err, RoostError::NotFound(_)),
            "trash of missing file must be NotFound, got {err:?}"
        );
    }

    #[tokio::test]
    async fn write_to_dotdot_path_blocked() {
        let (_d, fs) = fresh();
        let err = fs.write_atomic("../escape.md", b"bad").await.unwrap_err();
        assert!(
            matches!(err, RoostError::OutsideRoost(_)),
            "path traversal must be blocked, got {err:?}"
        );
    }

    #[tokio::test]
    async fn rename_dotdot_target_blocked() {
        let (_d, fs) = fresh();
        fs.write_atomic("a.md", b"a").await.unwrap();
        let err = fs.rename("a.md", "../escape.md").await.unwrap_err();
        assert!(
            matches!(err, RoostError::OutsideRoost(_)),
            "rename to ../ must be blocked, got {err:?}"
        );
    }

    // ── Symlink rejection (AUDIT-011 + AUDIT-076) ──────────────────────
    //
    // Creating symlinks on Windows requires admin or developer mode, so
    // these tests are unix-gated. The defense (`safe_resolve` →
    // canonicalize + starts_with) runs on all platforms.
    //
    // AUDIT-076 expanded the test surface: the original final-component
    // check (AUDIT-011) missed ancestor symlinks, so we now also cover
    // `linkdir/file.md` escape and write/create/rename through linkdirs.

    #[cfg(unix)]
    #[tokio::test]
    async fn read_through_symlink_rejected() {
        use std::os::unix::fs::symlink;
        let (dir, fs) = fresh();
        // Create a real victim file OUTSIDE the roost and symlink to it. Using
        // a test-created target (rather than a host path like /etc/hostname,
        // which is a Linux-ism absent on macOS) keeps this cross-platform and
        // exercises the resolvable-escaping-symlink path (canonicalize +
        // starts_with), distinct from the dangling case below.
        let outside = dir.path().parent().unwrap().join("read-symlink-victim.txt");
        std::fs::write(&outside, b"victim").expect("create outside file");
        let link = dir.path().join("leak");
        symlink(&outside, &link).expect("create symlink");
        let err = fs.read("leak").await.unwrap_err();
        assert!(
            matches!(&err, RoostError::OutsideRoost(_)),
            "symlink read must be blocked, got {err:?}"
        );
        let _ = std::fs::remove_file(&outside);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn write_through_symlink_rejected() {
        use std::os::unix::fs::symlink;
        let (dir, fs) = fresh();
        let outside = dir.path().parent().unwrap().join("escape-target");
        let link = dir.path().join("attach");
        symlink(&outside, &link).expect("create symlink");
        let err = fs.write_atomic("attach", b"data").await.unwrap_err();
        assert!(
            matches!(&err, RoostError::OutsideRoost(_)),
            "symlink write must be blocked, got {err:?}"
        );
        // The symlink target outside the roost must not have been created.
        assert!(!outside.exists(), "write must not have escaped via symlink");
    }

    /// AUDIT-076: an in-roost directory symlink that escapes the roost
    /// must not be traversable via `linkdir/file.md`. The pre-fix
    /// `reject_symlink` check only looked at the final path component,
    /// so the ancestor symlink slipped past and `fs::read` followed it
    /// out of the roost.
    #[cfg(unix)]
    #[tokio::test]
    async fn read_through_ancestor_symlink_rejected() {
        use std::os::unix::fs::symlink;
        let (dir, fs) = fresh();
        // Set up an "outside" directory with a victim file we want to
        // make sure the roost can't read.
        let outside = dir.path().parent().unwrap().join("audit076-outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), b"victim").unwrap();
        // Plant a directory symlink inside the roost pointing at it.
        symlink(&outside, dir.path().join("linkdir")).expect("create dir symlink");
        let err = fs.read("linkdir/secret.txt").await.unwrap_err();
        assert!(
            matches!(&err, RoostError::OutsideRoost(_)),
            "read through ancestor symlink must be blocked, got {err:?}"
        );
        // Cleanup
        let _ = std::fs::remove_dir_all(&outside);
    }

    /// AUDIT-076: same shape for write — creating a file under an
    /// in-roost symlinked directory must not escape.
    #[cfg(unix)]
    #[tokio::test]
    async fn write_through_ancestor_symlink_rejected() {
        use std::os::unix::fs::symlink;
        let (dir, fs) = fresh();
        let outside = dir.path().parent().unwrap().join("audit076-write-outside");
        std::fs::create_dir_all(&outside).unwrap();
        symlink(&outside, dir.path().join("linkdir")).expect("create dir symlink");
        let err = fs.write_atomic("linkdir/new.md", b"x").await.unwrap_err();
        assert!(
            matches!(&err, RoostError::OutsideRoost(_)),
            "write through ancestor symlink must be blocked, got {err:?}"
        );
        assert!(
            !outside.join("new.md").exists(),
            "write must not have created a file outside the roost"
        );
        let _ = std::fs::remove_dir_all(&outside);
    }

    /// AUDIT-076: trash/rename/mkdir/rmdir/create previously didn't call
    /// the symlink check at all. Verify each is now blocked.
    #[cfg(unix)]
    #[tokio::test]
    async fn create_and_rename_through_ancestor_symlink_blocked() {
        use std::os::unix::fs::symlink;
        let (dir, fs) = fresh();
        let outside = dir.path().parent().unwrap().join("audit076-cr-outside");
        std::fs::create_dir_all(&outside).unwrap();
        symlink(&outside, dir.path().join("linkdir")).expect("create dir symlink");

        // create
        let err = fs.create("linkdir/x.md", b"x").await.unwrap_err();
        assert!(matches!(&err, RoostError::OutsideRoost(_)));

        // rename source via link
        fs.write_atomic("real.md", b"real").await.unwrap();
        let err = fs.rename("real.md", "linkdir/moved.md").await.unwrap_err();
        assert!(matches!(&err, RoostError::OutsideRoost(_)));
        assert!(
            !outside.join("moved.md").exists(),
            "rename must not have escaped via symlink"
        );

        // mkdir under link
        let err = fs.mkdir("linkdir/sub").await.unwrap_err();
        assert!(matches!(&err, RoostError::OutsideRoost(_)));

        let _ = std::fs::remove_dir_all(&outside);
    }

    /// AUDIT-076b: `write_atomic` builds `tmp = target + ".corvux.tmp"`
    /// by string append — `safe_resolve(target)` does NOT cover `tmp`.
    /// A hostile roost planting `note.md.corvux.tmp` as a symlink pointing
    /// outside would otherwise have `File::create` follow it and write
    /// our bytes to the symlink target before the rename. The fix refuses
    /// pre-existing tmp symlinks and uses `create_new`.
    #[cfg(unix)]
    #[tokio::test]
    async fn write_atomic_refuses_pre_planted_tmp_symlink() {
        use std::os::unix::fs::symlink;
        let (dir, fs) = fresh();
        // Plant a victim file outside the roost.
        let victim = dir.path().parent().unwrap().join("audit076b-victim");
        std::fs::write(&victim, b"USER CONTENT").unwrap();
        // Plant the booby-trap: <roost>/note.md.corvux.tmp -> victim.
        let tmp_link = dir.path().join("note.md.corvux.tmp");
        symlink(&victim, &tmp_link).expect("create tmp symlink");

        let err = fs.write_atomic("note.md", b"new data").await.unwrap_err();
        assert!(
            matches!(&err, RoostError::OutsideRoost(_) | RoostError::Io(_)),
            "write must refuse the symlinked tmp path, got {err:?}"
        );

        // Critical: the victim file content must be untouched. The pre-fix
        // code would have followed the symlink, truncated the victim, and
        // written "new data" into it (then renamed the symlink onto the
        // safe `note.md` slot, leaving the victim damaged).
        let after = std::fs::read(&victim).unwrap();
        assert_eq!(
            after, b"USER CONTENT",
            "write_atomic must NOT have followed the tmp symlink and clobbered the target"
        );
        // The safe-resolved target must also not have been created — the
        // attacker's symlink trick must be a no-op.
        assert!(!dir.path().join("note.md").exists());

        let _ = std::fs::remove_file(&victim);
    }

    /// AUDIT-076b: companion check — a *stale* regular-file tmp from a
    /// crashed prior write is legitimate to overwrite. Verifies the fix
    /// only refuses symlinks; it doesn't break recovery from a hard crash
    /// mid-write that left a tmp behind.
    #[tokio::test]
    async fn write_atomic_recovers_from_stale_regular_file_tmp() {
        let (dir, fs) = fresh();
        // Simulate a crashed write that left a stale tmp behind.
        std::fs::write(dir.path().join("note.md.corvux.tmp"), b"stale junk").unwrap();
        fs.write_atomic("note.md", b"fresh").await.unwrap();
        assert_eq!(fs.read("note.md").await.unwrap(), b"fresh");
        // Stale tmp must be cleaned up after the successful write.
        assert!(!dir.path().join("note.md.corvux.tmp").exists());
    }

    /// AUDIT-076c: `trash` previously built `self.root.join(".corvux-trash")`
    /// raw — a hostile roost that plants `.corvux-trash` as a symlink to
    /// outside would have `fs::rename(&src, &dest)` move the user's file
    /// out of the roost. The fix routes the trash dir through
    /// `safe_resolve` so the symlink is detected before any move happens.
    #[cfg(unix)]
    #[tokio::test]
    async fn trash_refuses_symlinked_trash_dir() {
        use std::os::unix::fs::symlink;
        let (dir, fs) = fresh();
        // Set up an "outside" directory the attacker wants files moved to.
        let outside = dir.path().parent().unwrap().join("audit076c-exfil");
        std::fs::create_dir_all(&outside).unwrap();
        // Plant .corvux-trash as a symlink to it.
        symlink(&outside, dir.path().join(".corvux-trash")).expect("create dir symlink");

        // Write a file the user thinks they're deleting.
        fs.write_atomic("secret.md", b"PRIVATE NOTE").await.unwrap();
        let err = fs.trash("secret.md").await.unwrap_err();
        assert!(
            matches!(&err, RoostError::OutsideRoost(_)),
            "trash must refuse a symlinked trash dir, got {err:?}"
        );

        // The file must still be in the roost, not in the attacker's dir.
        assert!(
            dir.path().join("secret.md").exists(),
            "secret.md must remain in the roost — trash must not have moved it"
        );
        let exfil_contents: Vec<_> = std::fs::read_dir(&outside)
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert!(
            exfil_contents.is_empty(),
            "attacker's exfil dir must remain empty — found {} entries",
            exfil_contents.len()
        );

        let _ = std::fs::remove_dir_all(&outside);
    }

    /// AUDIT-076: `exists()` previously used the unchecked `resolve()`
    /// and could leak existence of files outside the roost via an
    /// in-roost symlink. Verify it now reports false (the right
    /// outcome — pretending the path isn't there is the correct cover).
    #[cfg(unix)]
    #[tokio::test]
    async fn exists_through_ancestor_symlink_returns_false() {
        use std::os::unix::fs::symlink;
        let (dir, fs) = fresh();
        let outside = dir.path().parent().unwrap().join("audit076-exists-outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("victim.txt"), b"v").unwrap();
        symlink(&outside, dir.path().join("linkdir")).expect("create dir symlink");
        assert!(
            !fs.exists("linkdir/victim.txt").await.unwrap(),
            "exists() must not confirm files outside the roost via a symlink"
        );
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[tokio::test]
    async fn write_atomic_leaves_no_tmp_on_success() {
        // Atomic writes must clean up their `.corvux.tmp` sidecar — if a
        // partial-write or rename failure ever leaves one behind, scans
        // would surface it as a "ghost" markdown file.
        let (dir, fs) = fresh();
        fs.write_atomic("note.md", b"x").await.unwrap();
        let mut rd = std::fs::read_dir(dir.path()).unwrap();
        let leftover = rd.any(|e| {
            e.ok()
                .map(|e| e.file_name().to_string_lossy().contains(".corvux.tmp"))
                .unwrap_or(false)
        });
        assert!(!leftover, "atomic write left a .corvux.tmp behind");
    }

    // ── Sync twins (SYN-B2) ─────────────────────────────────────────────
    #[test]
    fn blocking_read_write_exists_roundtrip() {
        let (_d, fs) = fresh();
        assert!(!fs.exists_blocking("n/id.bin").unwrap());
        fs.write_atomic_blocking("n/id.bin", b"payload").unwrap();
        assert!(fs.exists_blocking("n/id.bin").unwrap());
        assert_eq!(fs.read_blocking("n/id.bin").unwrap(), b"payload");
        fs.mkdir_all_blocking("n").unwrap(); // idempotent
        fs.mkdir_all_blocking("n").unwrap();
    }

    /// The sync twins must reject symlink escapes exactly like their async
    /// siblings — this is the SYN-B2 defense vault_id relies on.
    #[cfg(unix)]
    #[test]
    fn blocking_ops_reject_symlink_escape() {
        use std::os::unix::fs::symlink;
        let (dir, fs) = fresh();
        let outside = dir.path().parent().unwrap().join("blk-escape-outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.bin"), b"victim").unwrap();
        symlink(&outside, dir.path().join("linkdir")).expect("symlink");

        assert!(matches!(
            fs.read_blocking("linkdir/secret.bin"),
            Err(RoostError::OutsideRoost(_))
        ));
        assert!(!fs.exists_blocking("linkdir/secret.bin").unwrap());
        assert!(matches!(
            fs.write_atomic_blocking("linkdir/planted.bin", b"x"),
            Err(RoostError::OutsideRoost(_))
        ));
        assert!(!outside.join("planted.bin").exists());
        let _ = std::fs::remove_dir_all(&outside);
    }

    /// Sync twin of `write_atomic_refuses_pre_planted_tmp_symlink` (AUDIT-076b).
    #[cfg(unix)]
    #[test]
    fn write_atomic_blocking_refuses_tmp_symlink() {
        use std::os::unix::fs::symlink;
        let (dir, fs) = fresh();
        let victim = dir.path().parent().unwrap().join("blk-tmp-victim");
        std::fs::write(&victim, b"USER CONTENT").unwrap();
        symlink(&victim, dir.path().join("id.bin.corvux.tmp")).expect("symlink");
        let err = fs.write_atomic_blocking("id.bin", b"new").unwrap_err();
        assert!(matches!(
            err,
            RoostError::OutsideRoost(_) | RoostError::Io(_)
        ));
        assert_eq!(std::fs::read(&victim).unwrap(), b"USER CONTENT");
        assert!(!dir.path().join("id.bin").exists());
        let _ = std::fs::remove_file(&victim);
    }
}
