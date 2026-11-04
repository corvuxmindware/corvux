//! Shared hardened atomic-write primitive.
//!
//! Unique (PID-scoped) temp file, opened with `create_new` (O_EXCL) so it
//! refuses to write through a pre-existing file OR symlink, fsync'd before
//! commit, and — on Unix, when overwriting — chmod'd to match the
//! destination's existing permission bits so an overwrite can't loosen them.
//! Used by both `roost.rs` (note writes) and `claude_desktop.rs` (Claude
//! Desktop config writes) so the two hand-copies this repo had before can't
//! drift from each other again.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use fs2::FileExt;

/// What a checked write compared against actually found immediately before
/// the rename. `Conflict` means either:
/// - `expected_before: None` (must-not-exist) but the destination now
///   exists, or
/// - `expected_before: Some(x)` but the destination's content isn't `x`
///   (changed OR deleted).
#[derive(Debug)]
pub enum WriteError {
    Conflict,
    Io(String),
}

impl From<String> for WriteError {
    fn from(value: String) -> Self {
        Self::Io(value)
    }
}

impl std::fmt::Display for WriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WriteError::Conflict => write!(f, "conflict: destination changed on disk"),
            WriteError::Io(e) => write!(f, "{e}"),
        }
    }
}

/// Write with no conflict check — for idempotent bootstrap writes (e.g.
/// creating today's daily note from a fixed template) where two concurrent
/// creators writing the same bytes is harmless.
pub fn write_atomic(dest: &Path, content: &[u8]) -> Result<(), String> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("Failed to create folder: {e}"))?;
    }
    with_destination_lock(dest, || {
        let tmp = write_hardened_temp(dest, content)?;
        commit_temp(&tmp, dest)
    })
}

/// Write `content` to `dest` atomically (temp file + rename), refusing to
/// commit unless `expected_before` matches what's actually on disk
/// immediately before the rename (`None` = dest must not exist).
///
/// The comparison and rename run while holding a cross-process advisory lock
/// shared by every corvux-mcp writer targeting this directory. That closes the
/// MCP-to-MCP compare/rename race. A non-cooperating external editor can still
/// write in the tiny compare/rename window; the main app has its own content
/// conflict guard, and making arbitrary editors participate is impossible.
pub fn write_atomic_checked(
    dest: &Path,
    expected_before: Option<&str>,
    content: &[u8],
) -> Result<(), WriteError> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| WriteError::Io(format!("Failed to create folder: {e}")))?;
    }
    with_destination_lock(dest, || {
        let tmp = write_hardened_temp(dest, content).map_err(WriteError::Io)?;
        let current = fs::read_to_string(dest).ok();
        let matches = match (expected_before, current.as_deref()) {
            (None, None) => true,
            (Some(expected), Some(actual)) => expected == actual,
            _ => false,
        };
        if !matches {
            let _ = fs::remove_file(&tmp);
            return Err(WriteError::Conflict);
        }
        commit_temp(&tmp, dest).map_err(WriteError::Io)
    })
}

/// Serialize all MCP commits in a destination directory. A single persistent
/// hidden lock file avoids the unsafe "delete after unlock" pattern (which can
/// split waiters across two inodes). The file contains no user data.
fn with_destination_lock<T, E>(dest: &Path, f: impl FnOnce() -> Result<T, E>) -> Result<T, E>
where
    E: From<String>,
{
    let parent = dest.parent().unwrap_or_else(|| Path::new("."));
    let lock_path = parent.join(".corvux-mcp-write.lock");
    let lock = open_private_append(&lock_path)
        .map_err(|e| E::from(format!("Failed to open MCP write lock: {e}")))?;
    lock.lock_exclusive()
        .map_err(|e| E::from(format!("Failed to acquire MCP write lock: {e}")))?;
    f()
}

fn open_private_append(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

fn commit_temp(tmp: &Path, dest: &Path) -> Result<(), String> {
    fs::rename(tmp, dest).map_err(|e| {
        let _ = fs::remove_file(tmp);
        format!("Failed to commit write to {}: {e}", dest.display())
    })?;
    #[cfg(unix)]
    if let Some(parent) = dest.parent() {
        File::open(parent)
            .and_then(|d| d.sync_all())
            .map_err(|e| format!("Failed to sync destination folder: {e}"))?;
    }
    Ok(())
}

/// Build a PID-scoped sibling temp file for `dest`, containing `content`,
/// fsync'd. Opened with `create_new` (O_EXCL): refuses to write through any
/// pre-existing entry at that path, including a planted symlink, instead of
/// a separate check-then-write race. A stale REGULAR temp file left by a
/// crashed process (only possible if the OS later recycles that exact PID
/// for a new corvux-mcp process) is cleared and retried once; a symlink at
/// that path is never touched or followed.
///
/// PID-scoped rather than per-call-unique on purpose: this process handles
/// one JSON-RPC message at a time (see `main.rs`), so PID alone already
/// disambiguates the only real collision source (a second corvux-mcp
/// process). A per-call-unique name was considered and rejected — it would
/// turn a crash mid-write into permanent orphaned-file litter, since no
/// future write would ever reuse that exact name to clean it up.
fn write_hardened_temp(dest: &Path, content: &[u8]) -> Result<PathBuf, String> {
    let tmp_name = format!(
        "{}.{}.corvux-mcp.tmp",
        dest.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id()
    );
    let mut tmp = dest.to_path_buf();
    tmp.set_file_name(tmp_name);

    let mut first = OpenOptions::new();
    first.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        first.mode(0o600);
    }
    let mut file = match first.open(&tmp) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            let is_symlink = fs::symlink_metadata(&tmp)
                .map(|m| m.file_type().is_symlink())
                .unwrap_or(false);
            if is_symlink {
                return Err(format!(
                    "Temp path is a symlink, refusing write: {}",
                    tmp.display()
                ));
            }
            fs::remove_file(&tmp).map_err(|e| format!("Failed to clear stale temp file: {e}"))?;
            let mut retry = OpenOptions::new();
            retry.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                retry.mode(0o600);
            }
            retry
                .open(&tmp)
                .map_err(|e| format!("Failed to create temp file: {e}"))?
        }
        Err(e) => return Err(format!("Failed to create temp file: {e}")),
    };

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(existing) = fs::metadata(dest) {
            let _ = file.set_permissions(fs::Permissions::from_mode(existing.permissions().mode()));
        }
    }

    file.write_all(content).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        format!("Failed to write temp file: {e}")
    })?;
    file.sync_all().map_err(|e| {
        let _ = fs::remove_file(&tmp);
        format!("Failed to sync temp file: {e}")
    })?;
    Ok(tmp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn write_atomic_then_read_roundtrips_no_leftovers() {
        let dir = TempDir::new().unwrap();
        let dest = dir.path().join("note.md");
        write_atomic(&dest, b"hello").unwrap();
        assert_eq!(fs::read_to_string(&dest).unwrap(), "hello");
        let leftovers: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty());
    }

    #[test]
    fn checked_write_conflicts_when_disk_diverged() {
        let dir = TempDir::new().unwrap();
        let dest = dir.path().join("note.md");
        write_atomic(&dest, b"a").unwrap();
        fs::write(&dest, b"x").unwrap(); // external writer sneaks in
        let err = write_atomic_checked(&dest, Some("a"), b"b").unwrap_err();
        assert!(matches!(err, WriteError::Conflict));
        assert_eq!(fs::read_to_string(&dest).unwrap(), "x"); // untouched
    }

    #[test]
    fn checked_write_create_only_conflicts_if_now_exists() {
        let dir = TempDir::new().unwrap();
        let dest = dir.path().join("note.md");
        fs::write(&dest, b"already here").unwrap();
        let err = write_atomic_checked(&dest, None, b"new").unwrap_err();
        assert!(matches!(err, WriteError::Conflict));
    }

    #[test]
    fn checked_write_proceeds_when_base_matches() {
        let dir = TempDir::new().unwrap();
        let dest = dir.path().join("note.md");
        write_atomic(&dest, b"a").unwrap();
        write_atomic_checked(&dest, Some("a"), b"b").unwrap();
        assert_eq!(fs::read_to_string(&dest).unwrap(), "b");
    }

    #[test]
    fn checked_write_create_only_succeeds_when_absent() {
        let dir = TempDir::new().unwrap();
        let dest = dir.path().join("note.md");
        write_atomic_checked(&dest, None, b"fresh").unwrap();
        assert_eq!(fs::read_to_string(&dest).unwrap(), "fresh");
    }

    #[cfg(unix)]
    #[test]
    fn overwrite_preserves_existing_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new().unwrap();
        let dest = dir.path().join("note.md");
        write_atomic(&dest, b"a").unwrap();
        fs::set_permissions(&dest, fs::Permissions::from_mode(0o600)).unwrap();
        write_atomic(&dest, b"b").unwrap();
        let mode = fs::metadata(&dest).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn temp_write_refuses_through_preexisting_symlink() {
        use std::os::unix::fs::symlink;
        let dir = TempDir::new().unwrap();
        let dest = dir.path().join("note.md");
        let outside = dir.path().parent().unwrap().join("atomic-write-victim.txt");
        let _ = fs::remove_file(&outside);
        let tmp_name = format!("note.md.{}.corvux-mcp.tmp", std::process::id());
        symlink(&outside, dir.path().join(&tmp_name)).unwrap();

        let result = write_atomic(&dest, b"data");

        assert!(
            result.is_err(),
            "write through a planted temp symlink must be refused"
        );
        assert!(
            !outside.exists(),
            "must not have written through the symlink"
        );
        let _ = fs::remove_file(dir.path().join(&tmp_name));
    }

    #[test]
    fn stale_regular_temp_file_is_cleared_and_retried() {
        let dir = TempDir::new().unwrap();
        let dest = dir.path().join("note.md");
        let tmp_name = format!("note.md.{}.corvux-mcp.tmp", std::process::id());
        fs::write(dir.path().join(&tmp_name), b"stale").unwrap();
        write_atomic(&dest, b"fresh").unwrap();
        assert_eq!(fs::read_to_string(&dest).unwrap(), "fresh");
    }

    #[test]
    fn concurrent_checked_writes_one_wins_one_conflicts() {
        use std::sync::{Arc, Barrier};
        let dir = TempDir::new().unwrap();
        let dest = Arc::new(dir.path().join("note.md"));
        write_atomic(&dest, b"base").unwrap();
        let barrier = Arc::new(Barrier::new(2));
        let mut handles = Vec::new();
        for content in [&b"writer-a"[..], &b"writer-b"[..]] {
            let dest = Arc::clone(&dest);
            let barrier = Arc::clone(&barrier);
            let content = content.to_vec();
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                write_atomic_checked(&dest, Some("base"), &content)
            }));
        }
        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let oks = results.iter().filter(|r| r.is_ok()).count();
        assert_eq!(
            oks, 1,
            "exactly one concurrent checked write should succeed"
        );
        assert_eq!(
            results
                .iter()
                .filter(|result| matches!(result, Err(WriteError::Conflict)))
                .count(),
            1,
            "the losing writer must observe a content conflict, not a temp-file collision"
        );
        let final_content = fs::read_to_string(&*dest).unwrap();
        assert!(final_content == "writer-a" || final_content == "writer-b");
    }
}
