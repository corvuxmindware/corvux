//! Roost-level advisory lock.
//!
//! Acquires an exclusive non-blocking OS lock on `<roost>/.corvux/.lock`
//! when a roost is opened, releases it when the [`RoostLock`] guard is
//! dropped (i.e. when the roost is closed or replaced). Stops two Corvux
//! instances from racing concurrent writes against the same roost on the
//! same machine.
//!
//! This is **advisory**, not mandatory: external editors that don't honor
//! `flock(2)` / `LockFileEx` will still happily mutate the roost. Atomic
//! writes (tmp + fsync + rename) protect each individual save against
//! crash-tearing, so the worst external-editor failure mode is "last
//! write wins" — which is exactly what users expect when they edit the
//! same file in two apps.
//!
//! Desktop only. Mobile (Android/iOS) roosts sit inside the app's private
//! sandbox; the OS enforces single-process access for free, and `fs4`
//! doesn't compile cleanly through the NDK on every Android target.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use std::fs::TryLockError;

/// RAII handle to the roost-level OS file lock. Drop releases the lock
/// (the OS releases on file close, which `Drop` triggers).
pub struct RoostLock {
    /// Holds the lock for the lifetime of this guard. Field is `_file`
    /// because nothing reads it after construction — the value's
    /// existence is the lock.
    _file: File,
    /// Path to the lockfile. Read only by tests + future diagnostics
    /// (e.g. surfacing the file path in user-visible error messages
    /// when the lock can't be released cleanly).
    #[allow(dead_code)]
    path: PathBuf,
}

impl RoostLock {
    /// Try to acquire an exclusive non-blocking lock on
    /// `<roost_root>/.corvux/.lock`. Returns:
    ///
    /// - `Ok(RoostLock)` — lock held; release on drop.
    /// - `Err(LockError::AlreadyLocked)` — another Corvux instance owns
    ///   the lock. The caller should surface a "roost is open elsewhere"
    ///   error to the user, not retry blindly.
    /// - `Err(LockError::Io(_))` — lockfile creation failed (permission,
    ///   disk full, parent dir not writable). Distinct from the
    ///   already-locked case so the UX can differentiate.
    pub fn acquire(roost_root: &Path) -> Result<Self, LockError> {
        let lock_dir = roost_root.join(".corvux");
        std::fs::create_dir_all(&lock_dir).map_err(LockError::Io)?;
        let lock_path = lock_dir.join(".lock");

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(LockError::Io)?;

        // Non-blocking: returns immediately if another process holds the
        // lock. We never want to block the open_roost command behind a
        // stale lock from a crashed process — the user's UX would be a
        // frozen "Opening roost…" spinner.
        //
        // Rust 1.89 stabilized `File::try_lock`, so we can drop the prior
        // fs4 dep and the platform-specific OS-error tomato-fishing — std
        // maps Unix `flock(LOCK_NB)` and Windows `LockFileEx(_NB)`
        // collisions to a single `TryLockError::WouldBlock` variant.
        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Err(LockError::AlreadyLocked),
            Err(TryLockError::Error(e)) => return Err(LockError::Io(e)),
        }

        // Stamp PID + timestamp into the file for diagnostics. Best-effort:
        // if the write fails (e.g. read-only mount, which is a weird state
        // for a roost but we shouldn't refuse to lock over it) we keep the
        // lock and move on.
        let stamp = format!(
            "pid={}\nstarted={}\n",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
        );
        // `&File` implements both `Read` and `Write`, so we can stamp
        // diagnostic metadata without rebinding `file` as mut. Errors are
        // intentionally ignored — failure to write the PID line should not
        // cause us to release the OS lock.
        let _ = file.set_len(0);
        let _ = (&file).write_all(stamp.as_bytes());

        Ok(Self {
            _file: file,
            path: lock_path,
        })
    }

    #[allow(dead_code)] // used by tests + future error-surface UX
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Distinguishes "someone else has the roost open" from real I/O errors so
/// the open_roost command can show a roost-conflict-specific message.
#[derive(Debug)]
pub enum LockError {
    AlreadyLocked,
    Io(std::io::Error),
}

impl std::fmt::Display for LockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyLocked => write!(
                f,
                "Roost is already open in another Corvux instance. \
                 Close it there, or delete .corvux/.lock if you're sure no other \
                 instance is running."
            ),
            Self::Io(e) => write!(f, "Failed to acquire roost lock: {e}"),
        }
    }
}

impl std::error::Error for LockError {}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn acquires_lock_on_fresh_roost() {
        let tmp = TempDir::new().unwrap();
        let lock = RoostLock::acquire(tmp.path()).expect("first acquire must succeed");
        assert!(lock.path().exists());
    }

    #[test]
    fn second_acquire_on_same_roost_fails_with_already_locked() {
        let tmp = TempDir::new().unwrap();
        let _first = RoostLock::acquire(tmp.path()).expect("first acquire");
        match RoostLock::acquire(tmp.path()) {
            Err(LockError::AlreadyLocked) => {}
            Err(LockError::Io(e)) => panic!("expected AlreadyLocked, got Io: {e}"),
            Ok(_) => panic!("expected AlreadyLocked, second acquire succeeded"),
        }
    }

    #[test]
    fn drop_releases_lock_so_reacquire_works() {
        let tmp = TempDir::new().unwrap();
        {
            let _first = RoostLock::acquire(tmp.path()).expect("first acquire");
        } // drop releases
        let _second = RoostLock::acquire(tmp.path()).expect("re-acquire after drop");
    }

    #[test]
    fn two_different_roosts_can_be_locked_independently() {
        let a = TempDir::new().unwrap();
        let b = TempDir::new().unwrap();
        let _la = RoostLock::acquire(a.path()).expect("roost a");
        let _lb = RoostLock::acquire(b.path()).expect("roost b");
    }
}
