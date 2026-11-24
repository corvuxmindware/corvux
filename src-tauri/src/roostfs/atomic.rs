//! Native filesystem operations whose guarantees must hold across processes.

use std::io;
use std::path::Path;

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

/// Atomically rename a file OR directory while refusing an existing target.
/// Unsupported kernels/filesystems return an error rather than falling back to
/// a check followed by an overwriting rename.
pub(crate) fn rename_no_replace(from: &Path, to: &Path) -> io::Result<()> {
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
    #[cfg(unix)]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        let from = CString::new(from.as_os_str().as_bytes())
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        let to = CString::new(to.as_os_str().as_bytes())
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        #[cfg(any(target_os = "linux", target_os = "android"))]
        // SAFETY: both paths are live NUL-terminated buffers. The syscall's
        // arguments match renameat2; using syscall also supports older Bionic.
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
        #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
        return if result == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        };
    }
    #[allow(unreachable_code)]
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "atomic no-replace rename is unavailable",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
