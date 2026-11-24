//! Native filesystem operations whose guarantees must hold across processes.

use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

/// Open one read/write handle without following a final-component symlink.
pub(crate) fn open_no_follow(path: &Path) -> io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0);
    }
    let file = options.open(path)?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        use windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
        if file.metadata()?.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "refusing a reparse point",
            ));
        }
    }
    Ok(file)
}

/// Rename a file OR directory and refuse to replace an existing target. This is
/// the only way `NativeFs::create` and `NativeFs::rename` publish a name.
///
/// The kernel primitive (`renameat2(RENAME_NOREPLACE)` on Linux,
/// `renamex_np(RENAME_EXCL)` on Apple, `MoveFileExW` without
/// `MOVEFILE_REPLACE_EXISTING` on Windows) is tried first. Some storage cannot
/// do it and answers EINVAL / ENOSYS / ENOTSUP: FAT and exFAT cards, FUSE
/// mounts, old NFS or SMB shares, kernels before 3.15. Only on those answers
/// does it use a fallback that is STILL no-replace:
///
/// 1. A regular file is hard-linked to the new name (`link` fails with
///    `EEXIST` if the name is taken, in the same atomic step) and the old name
///    is then unlinked.
/// 2. Anything else, or storage with no hard links (FAT, exFAT), reserves the
///    new name with an exclusive create (`mkdir` for a folder, `O_EXCL` for a
///    file), checks the placeholder is still the empty one it made (same
///    identity, still empty; otherwise it stops without touching what is
///    there), then renames over it. Only that empty placeholder can be
///    replaced. A writer that swaps the placeholder in the instant between the
///    check and the rename is still overwritten: see [`reserve_then_replace`].
///
/// If even that is unavailable it fails with a plain error and changes nothing.
/// It never falls back to "check, then overwrite". Any other error (the target
/// exists, no permission, no space, a different device) is returned as it is.
///
/// **Android never makes the kernel call** ([`kernel_primitive_allowed`]): it
/// goes straight to the two fallback tiers. Hard links may be refused on some
/// Android storage; that drops to the placeholder tier.
pub(crate) fn rename_no_replace(from: &Path, to: &Path) -> io::Result<()> {
    let ops: &dyn NoReplaceOps = &OsOps;
    rename_no_replace_with(ops, !kernel_primitive_allowed(), from, to)
}

/// May the raw `renameat2` syscall be made on this platform? Not on Android.
///
/// An errno fallback cannot protect against it there: on API 26-29 the app
/// seccomp filter can answer a syscall it does not allow with SIGSYS, which
/// kills the process, not with ENOSYS, so every new note or rename would crash
/// the app on those devices. There is no safe runtime API-level helper in this
/// codebase to gate on (and reading it would need new FFI that cannot be built
/// or run here), so Android uses the fallback tiers unconditionally, on every
/// API level. `os_rename_no_replace` has no Android arm either, so the call
/// cannot be reached even by mistake. `cfg!`, not `#[cfg]`, so the decision
/// compiles and is tested on every platform.
fn kernel_primitive_allowed() -> bool {
    !cfg!(target_os = "android")
}

/// The steps of [`rename_no_replace`] that depend on what the storage supports.
/// A trait so tests can make each one fail the way a FAT card, a FUSE mount or
/// an old kernel does; Windows cannot reproduce any of those.
trait NoReplaceOps {
    /// The atomic kernel primitive.
    fn primitive(&self, from: &Path, to: &Path) -> io::Result<()>;
    /// Fallback 1: a new hard link, which fails if `to` exists.
    fn hard_link(&self, from: &Path, to: &Path) -> io::Result<()>;
    /// Remove a name (fallback 1: the old name; rollback: the new one).
    fn unlink(&self, path: &Path) -> io::Result<()>;
    /// Fallback 2: claim the new name with an exclusive create. Returns what
    /// identifies the placeholder that was created, so it can be checked again.
    fn reserve(&self, to: &Path, is_dir: bool) -> io::Result<PlaceholderId>;
    /// Fallback 2: rename over the placeholder that `reserve` made.
    fn replace(&self, from: &Path, to: &Path) -> io::Result<()>;
}

struct OsOps;

impl NoReplaceOps for OsOps {
    fn primitive(&self, from: &Path, to: &Path) -> io::Result<()> {
        os_rename_no_replace(from, to)
    }

    fn hard_link(&self, from: &Path, to: &Path) -> io::Result<()> {
        std::fs::hard_link(from, to)
    }

    fn unlink(&self, path: &Path) -> io::Result<()> {
        std::fs::remove_file(path)
    }

    fn reserve(&self, to: &Path, is_dir: bool) -> io::Result<PlaceholderId> {
        if is_dir {
            std::fs::create_dir(to)?;
            // No handle exists for a folder: the stat right after `mkdir` is the
            // best identity available (the kernel itself refuses to rename over
            // a folder that has content).
            Ok(placeholder_id(&std::fs::symlink_metadata(to)?))
        } else {
            // `create_new` is O_EXCL / CREATE_NEW: it fails if anything is
            // there, including a dangling symlink, and never follows one.
            let file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(to)?;
            // The identity comes from the HANDLE we created, not from a second
            // look at the path, so it is exactly the file we made.
            Ok(placeholder_id(&file.metadata()?))
        }
    }

    fn replace(&self, from: &Path, to: &Path) -> io::Result<()> {
        std::fs::rename(from, to)
    }
}

/// `prefer_fallback` skips the kernel primitive entirely (it is never called):
/// the Android rule, see [`kernel_primitive_allowed`].
fn rename_no_replace_with(
    ops: &dyn NoReplaceOps,
    prefer_fallback: bool,
    from: &Path,
    to: &Path,
) -> io::Result<()> {
    if prefer_fallback {
        log_fallback_once(&"the kernel call is never made on this platform");
        return fallback(ops, from, to, None);
    }
    match ops.primitive(from, to) {
        Err(error) if is_unsupported(&error) => {
            log_fallback_once(&error);
            fallback(ops, from, to, Some(error))
        }
        other => other,
    }
}

/// True when the error means "this storage or kernel cannot do that", as
/// opposed to "that cannot be done" (exists, denied, no space, other device).
fn is_unsupported(error: &io::Error) -> bool {
    if error.kind() == io::ErrorKind::Unsupported {
        return true;
    }
    #[cfg(unix)]
    {
        if let Some(code) = error.raw_os_error() {
            // ENOTSUP and EOPNOTSUPP are the same number on Linux, different on Apple.
            return code == libc::EINVAL
                || code == libc::ENOSYS
                || code == libc::ENOTSUP
                || code == libc::EOPNOTSUPP;
        }
    }
    false
}

/// Log once per run that this machine uses the fallback, so a bug report shows it.
fn log_fallback_once(why: &dyn std::fmt::Display) {
    static LOGGED: AtomicBool = AtomicBool::new(false);
    if !LOGGED.swap(true, Ordering::Relaxed) {
        tracing::warn!(
            why = %why,
            "roostfs: using the hard-link / reserve-name fallback for no-overwrite renames"
        );
    }
}

fn nothing_works(cause: &io::Error) -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        format!(
            "This storage cannot create or rename notes safely (no no-overwrite rename, hard links or exclusive create), so nothing was changed. ({cause})"
        ),
    )
}

fn fallback(
    ops: &dyn NoReplaceOps,
    from: &Path,
    to: &Path,
    primitive_error: Option<io::Error>,
) -> io::Result<()> {
    // Not following links: decides file vs folder, and keeps a symlink out of
    // the hard-link path (some platforms' `link` follows it).
    let source = std::fs::symlink_metadata(from)?;
    let is_dir = source.is_dir();
    if is_dir && to != from && to.starts_with(from) {
        // EINVAL also means "a folder cannot move into itself". Say that, not
        // "unsupported" (and say it ourselves when the kernel was never asked).
        return Err(primitive_error.unwrap_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "a folder cannot be moved into itself",
            )
        }));
    }
    if source.is_file() {
        match ops.hard_link(from, to) {
            Ok(()) => return finish_link(ops, from, to),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => return Err(error),
            // FAT, exFAT and some FUSE mounts have no hard links (EPERM / ENOTSUP):
            // reserve the name instead.
            Err(error)
                if is_unsupported(&error) || error.kind() == io::ErrorKind::PermissionDenied => {}
            Err(error) => return Err(error),
        }
    }
    reserve_then_replace(ops, from, to, is_dir)
}

/// The new name exists as a hard link; drop the old name.
fn finish_link(ops: &dyn NoReplaceOps, from: &Path, to: &Path) -> io::Result<()> {
    match ops.unlink(from) {
        Ok(()) => Ok(()),
        Err(unlink_error) => {
            // Both names now point at one file. Undo our link if we can prove
            // the new name is still it; otherwise leave both and say so.
            if same_file(from, to) && ops.unlink(to).is_ok() {
                return Err(unlink_error);
            }
            Err(io::Error::new(
                unlink_error.kind(),
                format!(
                    "The note was given its new name but the old name could not be removed, so both names exist now. ({unlink_error})"
                ),
            ))
        }
    }
}

/// What identifies the placeholder `reserve` made.
///
/// Unix: device and inode of the file or folder we created. Windows: its
/// creation time. `std` has no stable file id on Windows and reading one needs
/// new FFI; Windows never reaches this tier in production (`MoveFileExW` is the
/// primitive on every Windows filesystem), and creation time is a weaker proof
/// because NTFS "tunneling" can hand a re-created name the old creation time.
/// There the zero-length check carries the weight. Elsewhere: nothing to
/// compare, so only the kind and length are checked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PlaceholderId(u64, u64);

fn placeholder_id(metadata: &std::fs::Metadata) -> PlaceholderId {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        PlaceholderId(metadata.dev(), metadata.ino())
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        PlaceholderId(metadata.creation_time(), 0)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = metadata;
        PlaceholderId(0, 0)
    }
}

/// Is `to` still the empty placeholder this call created: the same kind of
/// thing, the same identity, and (for a file) still zero bytes? A writer that
/// put data into it, or swapped it for its own file or folder, makes this false.
fn still_ours(to: &Path, id: PlaceholderId, is_dir: bool) -> bool {
    match std::fs::symlink_metadata(to) {
        Ok(metadata) => {
            let right_kind = if is_dir {
                metadata.is_dir()
            } else {
                metadata.is_file() && metadata.len() == 0
            };
            right_kind && placeholder_id(&metadata) == id
        }
        Err(_) => false,
    }
}

/// Reserve the new name, check the reservation is still ours, then rename over
/// it.
///
/// The check closes the case where another writer (an editor, a sync tool)
/// replaced or wrote into the placeholder between the exclusive create and the
/// rename, which the rename would then silently destroy. It runs immediately
/// before the rename to keep that window as small as it can be.
///
/// **Irreducible race, kept on purpose and documented:** on storage with no
/// atomic no-replace rename and no hard links there is no way to make "verify"
/// and "rename" one atomic step. A writer that replaces the placeholder in the
/// few microseconds between [`still_ours`] and the rename is still overwritten.
/// It needs a second program writing the exact name being created or renamed,
/// on FAT/exFAT/FUSE storage, within that window. A folder is safer: the kernel
/// refuses to rename over a folder that has any content.
fn reserve_then_replace(
    ops: &dyn NoReplaceOps,
    from: &Path,
    to: &Path,
    is_dir: bool,
) -> io::Result<()> {
    let id = ops.reserve(to, is_dir).map_err(|error| {
        if is_unsupported(&error) {
            nothing_works(&error)
        } else {
            error
        }
    })?;
    if !still_ours(to, id, is_dir) {
        // Not ours any more: leave whatever is there strictly alone (it is
        // somebody else's), and leave the source where it was.
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "Another program took this name while it was being renamed, so nothing was overwritten.",
        ));
    }
    ops.replace(from, to)
        .inspect_err(|_| release_placeholder(to, id, is_dir))
}

/// Remove the placeholder `reserve` made, but only while it is still ours: the
/// same empty file or empty folder. Anything else at that name is left alone.
fn release_placeholder(to: &Path, id: PlaceholderId, is_dir: bool) {
    if !still_ours(to, id, is_dir) {
        return;
    }
    let _ = if is_dir {
        std::fs::remove_dir(to) // refuses a non-empty folder
    } else {
        std::fs::remove_file(to)
    };
}

#[cfg(unix)]
fn same_file(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (std::fs::symlink_metadata(a), std::fs::symlink_metadata(b)) {
        (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
        _ => false,
    }
}

/// Without inodes there is no proof, so nothing is removed on the strength of it.
#[cfg(not(unix))]
fn same_file(_a: &Path, _b: &Path) -> bool {
    false
}

/// The kernel's atomic no-replace rename, or `Unsupported` where there is none.
fn os_rename_no_replace(from: &Path, to: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        use windows::core::HSTRING;
        use windows::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_WRITE_THROUGH};
        let from = HSTRING::from(from.as_os_str());
        let to = HSTRING::from(to.as_os_str());
        // SAFETY: HSTRINGs keep both NUL-terminated paths alive for this call.
        // Omitting MOVEFILE_REPLACE_EXISTING is the no-clobber guarantee.
        return unsafe { MoveFileExW(&from, &to, MOVEFILE_WRITE_THROUGH) }
            .map_err(|error| io::Error::from_raw_os_error(error.code().0 & 0xffff));
    }
    // Linux and Apple only. There is deliberately NO Android arm: the raw
    // `renameat2` syscall can be answered with SIGSYS there (see
    // `kernel_primitive_allowed`), so Android compiles down to `unavailable`.
    #[cfg(any(target_os = "linux", target_vendor = "apple"))]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        let from = CString::new(from.as_os_str().as_bytes())
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        let to = CString::new(to.as_os_str().as_bytes())
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        #[cfg(target_os = "linux")]
        // SAFETY: both paths are live NUL-terminated buffers. The syscall's
        // arguments match renameat2.
        let result = unsafe {
            libc::syscall(
                libc::SYS_renameat2,
                libc::AT_FDCWD,
                from.as_ptr(),
                libc::AT_FDCWD,
                to.as_ptr(),
                1u32,
            )
        };
        #[cfg(target_vendor = "apple")]
        // SAFETY: both C strings are live for the call and RENAME_EXCL forbids
        // replacement at the same atomic filesystem operation as the rename.
        let result = unsafe { libc::renamex_np(from.as_ptr(), to.as_ptr(), libc::RENAME_EXCL) };
        return if result == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        };
    }
    #[allow(unreachable_code)]
    unavailable(from, to)
}

/// What the primitive answers where there is none (Android, other Unixes):
/// "unsupported", which routes the caller to the fallback tiers.
fn unavailable(_from: &Path, _to: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "atomic no-replace rename is unavailable",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::Mutex;

    #[test]
    fn refuses_existing_files_and_directories() {
        let dir = tempfile::tempdir().unwrap();
        let from = dir.path().join("from");
        let to = dir.path().join("to");
        std::fs::write(&from, b"source").unwrap();
        std::fs::write(&to, b"destination").unwrap();
        assert_eq!(
            rename_no_replace(&from, &to).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(std::fs::read(&from).unwrap(), b"source");
        assert_eq!(std::fs::read(&to).unwrap(), b"destination");
        std::fs::remove_file(&from).unwrap();
        std::fs::remove_file(&to).unwrap();
        std::fs::create_dir(&from).unwrap();
        std::fs::create_dir(&to).unwrap();
        assert!(rename_no_replace(&from, &to).is_err());
        assert!(from.is_dir() && to.is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn refuses_symlink_at_open_and_keeps_handle_after_swap() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("victim");
        std::fs::write(&target, b"victim").unwrap();
        let path = dir.path().join("managed");
        let mut handle = open_no_follow(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink(&target, &path).unwrap();
        handle.write_all(b"safe").unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"victim");
        assert!(open_no_follow(&path).is_err());
    }

    // ── RF-4: the no-replace fallback, on storage that lacks the primitive ──
    //
    // Windows (and a tempdir on ext4) cannot produce an EINVAL from
    // `renameat2`, so each storage-dependent step goes through `NoReplaceOps`
    // and `Faulty` makes it fail the way a FAT card or an old kernel does. The
    // steps that DO run are the real `std::fs` calls on a tempdir.

    type Fault = fn() -> io::Error;

    /// Every shape "this storage cannot do that" takes on this platform.
    fn unsupported_variants() -> Vec<Fault> {
        #[allow(unused_mut)] // only the unix block pushes
        let mut all: Vec<Fault> = vec![|| io::Error::from(io::ErrorKind::Unsupported)];
        #[cfg(unix)]
        {
            all.push(|| io::Error::from_raw_os_error(libc::EINVAL));
            all.push(|| io::Error::from_raw_os_error(libc::ENOSYS));
            all.push(|| io::Error::from_raw_os_error(libc::ENOTSUP));
            all.push(|| io::Error::from_raw_os_error(libc::EOPNOTSUPP));
        }
        all
    }

    /// Real operations, with chosen steps replaced by a failure. Records which
    /// steps ran.
    #[derive(Default)]
    struct Faulty {
        primitive: Option<Fault>,
        link: Option<Fault>,
        reserve: Option<Fault>,
        replace: Option<Fault>,
        /// Removing exactly this path fails (the old name after a link).
        unlink_of: Option<PathBuf>,
        /// Runs right after a real `reserve`, standing in for another program
        /// (an editor, a sync tool) that touches the new name in the gap.
        after_reserve: Option<fn(&Path)>,
        calls: Mutex<Vec<&'static str>>,
    }

    impl Faulty {
        /// No kernel primitive here; everything else works.
        fn no_primitive(fault: Fault) -> Self {
            Self {
                primitive: Some(fault),
                ..Self::default()
            }
        }
        /// No kernel primitive and no hard links (FAT, exFAT).
        fn no_primitive_no_links(fault: Fault) -> Self {
            Self {
                primitive: Some(fault),
                link: Some(fault),
                ..Self::default()
            }
        }
        fn note(&self, step: &'static str) {
            self.calls.lock().unwrap().push(step);
        }
        fn calls(&self) -> Vec<&'static str> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl NoReplaceOps for Faulty {
        fn primitive(&self, from: &Path, to: &Path) -> io::Result<()> {
            self.note("primitive");
            match self.primitive {
                Some(fault) => Err(fault()),
                None => OsOps.primitive(from, to),
            }
        }
        fn hard_link(&self, from: &Path, to: &Path) -> io::Result<()> {
            self.note("hard_link");
            match self.link {
                Some(fault) => Err(fault()),
                None => OsOps.hard_link(from, to),
            }
        }
        fn unlink(&self, path: &Path) -> io::Result<()> {
            self.note("unlink");
            if self.unlink_of.as_deref() == Some(path) {
                return Err(io::Error::from(io::ErrorKind::PermissionDenied));
            }
            OsOps.unlink(path)
        }
        fn reserve(&self, to: &Path, is_dir: bool) -> io::Result<PlaceholderId> {
            self.note("reserve");
            match self.reserve {
                Some(fault) => Err(fault()),
                None => {
                    let id = OsOps.reserve(to, is_dir)?;
                    if let Some(foreign) = self.after_reserve {
                        foreign(to);
                    }
                    Ok(id)
                }
            }
        }
        fn replace(&self, from: &Path, to: &Path) -> io::Result<()> {
            self.note("replace");
            match self.replace {
                Some(fault) => Err(fault()),
                None => OsOps.replace(from, to),
            }
        }
    }

    fn names_in(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// A tempdir holding `from` = "source" (a file).
    fn dir_with_source() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let from = dir.path().join("from.md");
        let to = dir.path().join("to.md");
        std::fs::write(&from, b"source").unwrap();
        (dir, from, to)
    }

    #[test]
    fn unsupported_primitive_falls_back_to_a_hard_link_for_files() {
        for fault in unsupported_variants() {
            let (dir, from, to) = dir_with_source();
            let ops = Faulty::no_primitive(fault);
            unchanged(&ops, &from, &to).unwrap();
            assert_eq!(std::fs::read(&to).unwrap(), b"source");
            assert!(!from.exists(), "the old name is gone");
            assert_eq!(names_in(dir.path()), vec!["to.md"]);
            assert_eq!(ops.calls(), vec!["primitive", "hard_link", "unlink"]);
        }
    }

    #[test]
    fn fallback_still_refuses_an_existing_destination() {
        for fault in unsupported_variants() {
            // With hard links available, and without them.
            for ops in [
                Faulty::no_primitive(fault),
                Faulty::no_primitive_no_links(fault),
            ] {
                let (dir, from, to) = dir_with_source();
                std::fs::write(&to, b"precious").unwrap();
                let error = unchanged(&ops, &from, &to).unwrap_err();
                assert_eq!(error.kind(), io::ErrorKind::AlreadyExists, "{error}");
                assert_eq!(std::fs::read(&to).unwrap(), b"precious", "destination kept");
                assert_eq!(std::fs::read(&from).unwrap(), b"source", "source kept");
                assert_eq!(names_in(dir.path()), vec!["from.md", "to.md"]);
            }
        }
    }

    #[test]
    fn no_hard_links_reserves_the_name_then_replaces_only_the_placeholder() {
        for fault in unsupported_variants() {
            let (dir, from, to) = dir_with_source();
            let ops = Faulty::no_primitive_no_links(fault);
            unchanged(&ops, &from, &to).unwrap();
            assert_eq!(std::fs::read(&to).unwrap(), b"source");
            assert!(!from.exists());
            assert_eq!(names_in(dir.path()), vec!["to.md"], "no placeholder left");
            assert_eq!(
                ops.calls(),
                vec!["primitive", "hard_link", "reserve", "replace"]
            );
        }
    }

    #[test]
    fn a_failed_replace_removes_only_our_empty_placeholder() {
        let (dir, from, to) = dir_with_source();
        let ops = Faulty {
            primitive: Some(unsupported_variants()[0]),
            link: Some(unsupported_variants()[0]),
            replace: Some(|| io::Error::from(io::ErrorKind::PermissionDenied)),
            ..Faulty::default()
        };
        let error = unchanged(&ops, &from, &to).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(std::fs::read(&from).unwrap(), b"source", "source untouched");
        assert_eq!(
            names_in(dir.path()),
            vec!["from.md"],
            "placeholder released"
        );
    }

    #[test]
    fn a_placeholder_someone_wrote_into_is_not_deleted() {
        // Another process wrote into the reserved name before our rename failed:
        // that is their data, not our placeholder.
        let (dir, from, to) = dir_with_source();
        let id = OsOps.reserve(&to, false).unwrap();
        std::fs::write(&to, b"someone elses").unwrap(); // same file, now with data
        release_placeholder(&to, id, false);
        assert_eq!(std::fs::read(&to).unwrap(), b"someone elses");
        std::fs::write(&to, b"").unwrap(); // same file, empty again
        release_placeholder(&to, id, false);
        assert!(!to.exists(), "an empty placeholder is released");
        assert!(from.exists());
        drop(dir);
    }

    // ── a placeholder another program touched is never overwritten ─────────
    //
    // After `reserve` and before the rename another writer (an editor, a sync
    // tool) can write into the placeholder or swap it for its own file. The
    // rename would silently destroy that. `still_ours` is checked in between.

    /// Another program writes its data into the reserved name.
    fn foreign_write_into(to: &Path) {
        std::fs::write(to, b"foreign").unwrap();
    }

    /// Another program swaps the reserved name for its own file. Our
    /// placeholder is moved aside, not deleted, so the new file cannot be handed
    /// the same inode number.
    fn foreign_swap(to: &Path) {
        std::fs::rename(to, to.with_extension("ours")).unwrap();
        std::fs::write(to, b"foreign").unwrap();
    }

    /// ...with an EMPTY file of its own (only the identity can tell).
    #[cfg(unix)]
    fn foreign_empty_swap(to: &Path) {
        std::fs::rename(to, to.with_extension("ours")).unwrap();
        std::fs::File::create(to).unwrap();
    }

    fn racing(after_reserve: fn(&Path)) -> Faulty {
        Faulty {
            primitive: Some(unsupported_variants()[0]),
            link: Some(unsupported_variants()[0]),
            after_reserve: Some(after_reserve),
            ..Faulty::default()
        }
    }

    fn assert_foreign_survived(ops: &Faulty, error: io::Error, from: &Path) {
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists, "{error}");
        assert!(
            error.to_string().contains("nothing was overwritten"),
            "{error}"
        );
        assert_eq!(std::fs::read(from).unwrap(), b"source", "source untouched");
        assert!(
            !ops.calls().contains(&"replace"),
            "the rename must not run: {:?}",
            ops.calls()
        );
    }

    #[test]
    fn a_placeholder_written_into_by_another_program_is_not_clobbered() {
        type Run = fn(&Faulty, &Path, &Path) -> io::Result<()>;
        let runs: [Run; 2] = [unchanged, android_style];
        for through in runs {
            let (dir, from, to) = dir_with_source();
            let ops = racing(foreign_write_into);
            let error = through(&ops, &from, &to).unwrap_err();
            assert_foreign_survived(&ops, error, &from);
            assert_eq!(
                std::fs::read(&to).unwrap(),
                b"foreign",
                "their data is intact"
            );
            assert_eq!(names_in(dir.path()), vec!["from.md", "to.md"]);
        }
    }

    #[test]
    fn a_placeholder_swapped_for_a_foreign_file_is_not_clobbered() {
        let (dir, from, to) = dir_with_source();
        let ops = racing(foreign_swap);
        let error = unchanged(&ops, &from, &to).unwrap_err();
        assert_foreign_survived(&ops, error, &from);
        assert_eq!(
            std::fs::read(&to).unwrap(),
            b"foreign",
            "their file is intact"
        );
        // Our displaced placeholder is not at `to`, so it was not touched either.
        assert_eq!(names_in(dir.path()), vec!["from.md", "to.md", "to.ours"]);
    }

    #[cfg(unix)]
    #[test]
    fn an_empty_foreign_file_is_caught_by_identity_not_just_length() {
        let (dir, from, to) = dir_with_source();
        let ops = racing(foreign_empty_swap);
        let error = unchanged(&ops, &from, &to).unwrap_err();
        assert_foreign_survived(&ops, error, &from);
        assert!(to.is_file() && std::fs::metadata(&to).unwrap().len() == 0);
        assert_eq!(names_in(dir.path()), vec!["from.md", "to.md", "to.ours"]);
    }

    #[cfg(unix)]
    #[test]
    fn a_folder_placeholder_swapped_for_a_foreign_folder_is_not_clobbered() {
        fn foreign_folder(to: &Path) {
            std::fs::rename(to, to.with_extension("ours")).unwrap();
            std::fs::create_dir(to).unwrap();
            std::fs::write(to.join("theirs.md"), b"theirs").unwrap();
        }
        let dir = tempfile::tempdir().unwrap();
        let from = dir.path().join("Folder");
        let to = dir.path().join("Renamed");
        std::fs::create_dir(&from).unwrap();
        std::fs::write(from.join("a.md"), b"a").unwrap();
        let ops = Faulty {
            primitive: Some(unsupported_variants()[0]),
            after_reserve: Some(foreign_folder),
            ..Faulty::default()
        };
        let error = unchanged(&ops, &from, &to).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists, "{error}");
        assert!(!ops.calls().contains(&"replace"));
        assert_eq!(std::fs::read(to.join("theirs.md")).unwrap(), b"theirs");
        assert_eq!(std::fs::read(from.join("a.md")).unwrap(), b"a");
    }

    #[cfg(unix)]
    #[test]
    fn release_never_removes_a_different_file_that_took_the_name() {
        let (dir, from, to) = dir_with_source();
        let id = OsOps.reserve(&to, false).unwrap();
        foreign_empty_swap(&to); // an empty file, but not ours
        release_placeholder(&to, id, false);
        assert!(to.is_file(), "someone else's empty file stays");
        drop((dir, from));
    }

    #[test]
    fn an_untouched_placeholder_still_goes_through() {
        let (dir, from, to) = dir_with_source();
        let ops = Faulty::no_primitive_no_links(unsupported_variants()[0]);
        unchanged(&ops, &from, &to).unwrap();
        assert_eq!(
            ops.calls(),
            vec!["primitive", "hard_link", "reserve", "replace"]
        );
        assert_eq!(std::fs::read(&to).unwrap(), b"source");
        assert_eq!(names_in(dir.path()), vec!["to.md"]);
    }

    #[test]
    fn nothing_available_is_a_clear_error_and_loses_nothing() {
        for fault in unsupported_variants() {
            let (dir, from, to) = dir_with_source();
            let ops = Faulty {
                primitive: Some(fault),
                link: Some(fault),
                reserve: Some(fault),
                ..Faulty::default()
            };
            let error = unchanged(&ops, &from, &to).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::Unsupported);
            let text = error.to_string();
            assert!(
                text.contains("nothing was changed") && text.contains("cannot create or rename"),
                "plain wording expected: {text}"
            );
            assert_eq!(std::fs::read(&from).unwrap(), b"source");
            assert_eq!(names_in(dir.path()), vec!["from.md"]);
            // And it reads as a plain message once it reaches the UI layer.
            let shown = crate::roostfs::RoostError::from(error).to_string();
            assert!(shown.contains("nothing was changed"), "{shown}");
        }
    }

    #[test]
    fn errors_that_are_not_about_support_never_trigger_the_fallback() {
        for fault in [
            (|| io::Error::from(io::ErrorKind::AlreadyExists)) as Fault,
            || io::Error::from(io::ErrorKind::PermissionDenied),
            || io::Error::from(io::ErrorKind::NotFound),
            || io::Error::from(io::ErrorKind::Other),
        ] {
            let (dir, from, to) = dir_with_source();
            let ops = Faulty::no_primitive(fault);
            let error = unchanged(&ops, &from, &to).unwrap_err();
            assert_eq!(error.kind(), fault().kind());
            assert_eq!(ops.calls(), vec!["primitive"], "no fallback for {error}");
            assert_eq!(names_in(dir.path()), vec!["from.md"]);
        }
    }

    #[test]
    fn a_missing_source_is_not_found_through_the_fallback_too() {
        let dir = tempfile::tempdir().unwrap();
        let ops = Faulty::no_primitive(unsupported_variants()[0]);
        let error = unchanged(
            &ops,
            &dir.path().join("ghost.md"),
            &dir.path().join("to.md"),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert!(names_in(dir.path()).is_empty(), "nothing was created");
    }

    // ── Android: the kernel call is never made ─────────────────────────────

    /// The ordinary path: the kernel primitive is asked first.
    fn unchanged(ops: &Faulty, from: &Path, to: &Path) -> io::Result<()> {
        rename_no_replace_with(ops, false, from, to)
    }

    /// The Android path: the kernel primitive is skipped entirely.
    fn android_style(ops: &Faulty, from: &Path, to: &Path) -> io::Result<()> {
        rename_no_replace_with(ops, true, from, to)
    }

    #[test]
    fn only_android_skips_the_kernel_primitive() {
        assert_eq!(kernel_primitive_allowed(), !cfg!(target_os = "android"));
        // Linux, macOS and Windows keep the kernel call (this host is one of them).
        if !cfg!(target_os = "android") {
            assert!(kernel_primitive_allowed());
        }
    }

    #[test]
    fn prefer_fallback_never_calls_the_kernel_primitive() {
        // A primitive that would answer success: if it were called the old name
        // would simply vanish with no link, so "primitive" must be absent.
        let (dir, from, to) = dir_with_source();
        let ops = Faulty::default();
        android_style(&ops, &from, &to).unwrap();
        assert_eq!(ops.calls(), vec!["hard_link", "unlink"]);
        assert_eq!(std::fs::read(&to).unwrap(), b"source");
        assert!(!from.exists());
        assert_eq!(names_in(dir.path()), vec!["to.md"]);

        // Hard links refused (some Android storage): the placeholder tier, still no primitive.
        for fault in unsupported_variants() {
            let (dir, from, to) = dir_with_source();
            let ops = Faulty {
                link: Some(fault),
                ..Faulty::default()
            };
            android_style(&ops, &from, &to).unwrap();
            assert_eq!(ops.calls(), vec!["hard_link", "reserve", "replace"]);
            assert_eq!(std::fs::read(&to).unwrap(), b"source");
            assert_eq!(names_in(dir.path()), vec!["to.md"], "no placeholder left");
        }
        // A permission refusal on `link` (SELinux / FUSE) takes the same route.
        let (dir, from, to) = dir_with_source();
        let ops = Faulty {
            link: Some(|| io::Error::from(io::ErrorKind::PermissionDenied)),
            ..Faulty::default()
        };
        android_style(&ops, &from, &to).unwrap();
        assert_eq!(ops.calls(), vec!["hard_link", "reserve", "replace"]);
        assert_eq!(names_in(dir.path()), vec!["to.md"]);
    }

    #[test]
    fn prefer_fallback_keeps_the_no_clobber_guarantee() {
        for link_fault in [None, Some(unsupported_variants()[0])] {
            let (dir, from, to) = dir_with_source();
            std::fs::write(&to, b"precious").unwrap();
            let ops = Faulty {
                link: link_fault,
                ..Faulty::default()
            };
            let error = android_style(&ops, &from, &to).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::AlreadyExists, "{error}");
            assert!(!ops.calls().contains(&"primitive"));
            assert_eq!(std::fs::read(&to).unwrap(), b"precious");
            assert_eq!(std::fs::read(&from).unwrap(), b"source");
            assert_eq!(names_in(dir.path()), vec!["from.md", "to.md"]);
        }
    }

    #[test]
    fn prefer_fallback_with_nothing_available_is_a_clear_error() {
        let (dir, from, to) = dir_with_source();
        let fault = unsupported_variants()[0];
        let ops = Faulty {
            link: Some(fault),
            reserve: Some(fault),
            ..Faulty::default()
        };
        let error = android_style(&ops, &from, &to).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Unsupported);
        assert!(error.to_string().contains("nothing was changed"), "{error}");
        assert!(!ops.calls().contains(&"primitive"));
        assert_eq!(std::fs::read(&from).unwrap(), b"source");
        assert_eq!(names_in(dir.path()), vec!["from.md"]);
    }

    #[test]
    fn prefer_fallback_reports_a_folder_moved_into_itself_in_its_own_words() {
        #[cfg(unix)]
        {
            let dir = tempfile::tempdir().unwrap();
            let from = dir.path().join("Folder");
            std::fs::create_dir(&from).unwrap();
            let ops = Faulty::default();
            let error = android_style(&ops, &from, &from.join("inside")).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
            assert!(error.to_string().contains("into itself"), "{error}");
            assert!(ops.calls().is_empty(), "nothing was even attempted");
            assert!(!from.join("inside").exists());
        }
    }

    #[test]
    fn the_ordinary_path_still_asks_the_kernel_first() {
        let (dir, from, to) = dir_with_source();
        let ops = Faulty::default();
        unchanged(&ops, &from, &to).unwrap();
        assert_eq!(ops.calls(), vec!["primitive"], "no fallback step needed");
        assert_eq!(std::fs::read(&to).unwrap(), b"source");
        assert_eq!(names_in(dir.path()), vec!["to.md"]);
    }

    #[test]
    fn a_failed_unlink_after_the_link_never_loses_the_note() {
        let (dir, from, to) = dir_with_source();
        let ops = Faulty {
            primitive: Some(unsupported_variants()[0]),
            unlink_of: Some(from.clone()),
            ..Faulty::default()
        };
        let error = unchanged(&ops, &from, &to).unwrap_err();
        assert_eq!(std::fs::read(&from).unwrap(), b"source", "old name kept");
        #[cfg(unix)]
        {
            // Our own link is taken back, so the rename simply did not happen.
            assert!(!to.exists());
            assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        }
        #[cfg(not(unix))]
        {
            // No inode check here: both names stay, and the error says so.
            assert_eq!(std::fs::read(&to).unwrap(), b"source");
            assert!(error.to_string().contains("both names exist"), "{error}");
        }
        drop(dir);
    }

    #[test]
    fn a_symlink_source_skips_the_hard_link_path() {
        // `link` follows a symlink on some platforms; a symlink is moved by
        // reserving the name and renaming it, which moves the link itself.
        #[cfg(unix)]
        {
            let dir = tempfile::tempdir().unwrap();
            let target = dir.path().join("target.md");
            let from = dir.path().join("from.md");
            let to = dir.path().join("to.md");
            std::fs::write(&target, b"target").unwrap();
            std::os::unix::fs::symlink(&target, &from).unwrap();
            let ops = Faulty::no_primitive(unsupported_variants()[0]);
            unchanged(&ops, &from, &to).unwrap();
            assert_eq!(ops.calls(), vec!["primitive", "reserve", "replace"]);
            assert!(std::fs::symlink_metadata(&to)
                .unwrap()
                .file_type()
                .is_symlink());
            assert_eq!(std::fs::read(&target).unwrap(), b"target");
        }
    }

    #[test]
    fn folders_move_by_reserving_the_name_and_refuse_an_existing_one() {
        // `rename(dir, existing empty dir)` replaces it on Unix; Windows does
        // not allow that, and Windows never reaches this path (MoveFileExW has
        // the primitive), so the folder fallback is exercised on Unix only.
        #[cfg(unix)]
        {
            let dir = tempfile::tempdir().unwrap();
            let from = dir.path().join("Folder");
            let to = dir.path().join("Renamed");
            std::fs::create_dir(&from).unwrap();
            std::fs::write(from.join("a.md"), b"a").unwrap();
            let ops = Faulty::no_primitive(unsupported_variants()[0]);
            unchanged(&ops, &from, &to).unwrap();
            assert_eq!(std::fs::read(to.join("a.md")).unwrap(), b"a");
            assert!(!from.exists());
            assert_eq!(ops.calls(), vec!["primitive", "reserve", "replace"]);

            // An existing folder, even an empty one, is refused untouched.
            let other = dir.path().join("Other");
            std::fs::create_dir(&other).unwrap();
            let again = Faulty::no_primitive(unsupported_variants()[0]);
            let error = unchanged(&again, &to, &other).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
            assert_eq!(std::fs::read(to.join("a.md")).unwrap(), b"a");
            assert!(other.is_dir());
        }
    }

    #[test]
    fn a_folder_is_never_moved_into_itself() {
        #[cfg(unix)]
        {
            let dir = tempfile::tempdir().unwrap();
            let from = dir.path().join("Folder");
            std::fs::create_dir(&from).unwrap();
            let into = from.join("inside");
            let ops = Faulty::no_primitive(unsupported_variants()[0]);
            assert!(unchanged(&ops, &from, &into).is_err());
            assert!(from.is_dir());
            assert!(!into.exists(), "no placeholder created inside the folder");
            assert_eq!(ops.calls(), vec!["primitive"]);
        }
    }
}
