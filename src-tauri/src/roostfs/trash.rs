//! Shared trash helpers for every [`RoostFs`] backend.
//!
//! Deleting a note or folder never destroys it: it moves to
//! `<roost>/.corvux-trash/<unix-secs>.<name>`, where the user can restore it
//! from a file manager (the delete prompt promises exactly that).
//!
//! Two backends share this module:
//! - `NativeFs` (desktop, iOS, Android app-private roosts) renames into the
//!   trash with `std::fs::rename`.
//! - `SafFs` (Android user-picked folders) asks the storage provider to move
//!   the document, and falls back to [`copy_to`] + delete when the provider
//!   can't move. Before 2026-11-05 SAF deletes were permanent.
//!
//! Names are made unique ([`unique_trash_rel`]) so two deletes of
//! `Daily/a.md` and `Work/a.md` in the same second can't overwrite each
//! other in the trash, and [`TRASH_LOCK`] runs one trash at a time so two
//! concurrent deletes can't pick the same free name (`std::fs::rename`
//! replaces an existing file on Unix AND Windows).

use std::sync::LazyLock;
use std::time::{SystemTime, UNIX_EPOCH};

use super::{Entry, RoostError, RoostFs};

/// Folder at the roost root that holds deleted notes and folders. Hidden
/// (dot-prefixed), so listings, search, the graph and sync all skip it.
pub const TRASH_DIR: &str = ".corvux-trash";

/// Stop looking for a free trash name after this many candidates.
const MAX_NAME_TRIES: u32 = 1000;

/// Held by every backend's `trash()` from picking the free name until the
/// move/copy into it is done. Without it two concurrent deletes of
/// same-named notes could both pick `<ts>.a.md`: the later rename silently
/// replaced the earlier trashed note, and on the SAF copy path the cleanup
/// of a failed copy could delete it. Deletes are rare, so one at a time
/// (process-wide, across roosts) costs nothing noticeable.
pub(crate) static TRASH_LOCK: LazyLock<tokio::sync::Mutex<()>> =
    LazyLock::new(|| tokio::sync::Mutex::new(()));

/// Seconds since the Unix epoch; the timestamp prefix of a trash entry.
pub(crate) fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Refuse to trash the trash folder itself or anything already inside it.
/// Moving `.corvux-trash` into itself fails on desktop, but on SAF the copy
/// fallback would copy the trash into itself and then delete the original,
/// losing every trashed note.
pub(crate) fn ensure_trashable(rel: &str) -> Result<(), RoostError> {
    let rel = rel.trim_matches('/');
    if rel == TRASH_DIR || rel.starts_with(&format!("{TRASH_DIR}/")) {
        return Err(RoostError::Io(format!(
            "{rel} is already in the trash; remove it from a file manager"
        )));
    }
    Ok(())
}

/// Last segment of a roost-relative path: `"a/b.md"` -> `"b.md"`.
// Only the Android SAF backend calls this outside tests.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub(crate) fn base_name(rel: &str) -> &str {
    rel.trim_end_matches('/')
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or("unnamed")
}

/// The `n`-th candidate trash file name. `n == 0` is the historical layout
/// `<ts>.<name>`; later candidates are `<ts>-<n>.<name>`.
pub(crate) fn trash_name(ts: u64, n: u32, name: &str) -> String {
    if n == 0 {
        format!("{ts}.{name}")
    } else {
        format!("{ts}-{n}.{name}")
    }
}

/// Pick a roost-relative path inside [`TRASH_DIR`] that nothing occupies yet.
/// Call it while holding [`TRASH_LOCK`].
pub(crate) async fn unique_trash_rel<F: RoostFs + ?Sized>(
    fs: &F,
    ts: u64,
    name: &str,
) -> Result<String, RoostError> {
    for n in 0..MAX_NAME_TRIES {
        let rel = format!("{TRASH_DIR}/{}", trash_name(ts, n, name));
        if !fs.exists(&rel).await? {
            return Ok(rel);
        }
    }
    Err(RoostError::AlreadyExists(format!(
        "{TRASH_DIR}/{} (and {MAX_NAME_TRIES} numbered variants)",
        trash_name(ts, 0, name)
    )))
}

/// How the SAF plugin's `moveDocument` failed, from its reject message.
#[derive(Debug, PartialEq, Eq)]
// Only the Android SAF backend calls this outside tests.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub(crate) enum MoveFailure {
    /// The provider can't move this document (Kotlin: "Move not supported:
    /// ..."): copy + delete instead. Checked first, because the provider's
    /// own exception text (e.g. "Already exists /storage/...") is wrapped in
    /// it and must not be mistaken for our name check.
    Unsupported,
    /// The chosen trash name was taken after we checked (Kotlin pre-check:
    /// "Already exists: <to>"): pick another name and move again. Never fall
    /// back to copying onto a name that belongs to another trashed note.
    NameTaken,
    /// Anything else (source gone, bad path, no permission): give up.
    Other,
}

// Only the Android SAF backend calls this outside tests.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub(crate) fn classify_move_error(message: &str) -> MoveFailure {
    if message.contains("Move not supported") {
        MoveFailure::Unsupported
    } else if message.contains("Already exists") {
        MoveFailure::NameTaken
    } else {
        MoveFailure::Other
    }
}

/// A failed [`copy_to`]. `dest_claimed` says whether this copy created
/// `dest` (so the caller may delete the partial copy). When it is `false`
/// something else already owns `dest`, and deleting it would destroy that.
#[derive(Debug)]
// Only the Android SAF backend calls this outside tests.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub(crate) struct CopyError {
    pub error: RoostError,
    pub dest_claimed: bool,
}

/// Copy `src_rel` to `dest_rel` using only [`RoostFs`] calls. The SAF trash
/// uses it when the storage provider can't move a document.
///
/// For a folder, `descendants` must list everything under it **including
/// hidden files** (the normal listings drop dot-files, which would then be
/// lost on delete), with roost-relative paths. Pass an empty slice for a
/// file. `dest_rel` must not exist yet: creating it is the claim, and fails
/// with `AlreadyExists` (and `dest_claimed: false`) if it does. The source is
/// never touched here.
// Only the Android SAF backend calls this outside tests.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub(crate) async fn copy_to<F: RoostFs + ?Sized>(
    fs: &F,
    src_rel: &str,
    is_dir: bool,
    descendants: &[Entry],
    dest_rel: &str,
) -> Result<(), CopyError> {
    let unclaimed = |error| CopyError {
        error,
        dest_claimed: false,
    };
    let claimed = |error| CopyError {
        error,
        dest_claimed: true,
    };

    if !is_dir {
        let bytes = fs.read(src_rel).await.map_err(unclaimed)?;
        return fs.create(dest_rel, &bytes).await.map_err(|e| match e {
            // Another trashed note owns this name: not ours to clean up.
            RoostError::AlreadyExists(_) => unclaimed(e),
            // `create` checks for an existing file first, so whatever is
            // at `dest` after any other failure was made by us.
            e => claimed(e),
        });
    }

    let prefix = format!("{}/", src_rel.trim_end_matches('/'));
    let mut items = Vec::with_capacity(descendants.len());
    for entry in descendants {
        let suffix = entry.rel_path.strip_prefix(&prefix).ok_or_else(|| {
            unclaimed(RoostError::Io(format!(
                "trash copy: {} is not inside {src_rel}",
                entry.rel_path
            )))
        })?;
        items.push((entry, suffix));
    }
    // Parents before children, whatever order the listing came in.
    items.sort_by_key(|(_, suffix)| suffix.matches('/').count());

    // Creating the folder is the claim; `mkdir` fails if it exists.
    fs.mkdir(dest_rel).await.map_err(unclaimed)?;
    for (entry, suffix) in items {
        let to = format!("{dest_rel}/{suffix}");
        if entry.is_dir {
            match fs.mkdir(&to).await {
                // A file copied earlier may already have created it.
                Ok(()) | Err(RoostError::AlreadyExists(_)) => {}
                Err(e) => return Err(claimed(e)),
            }
        } else {
            let bytes = fs.read(&entry.rel_path).await.map_err(claimed)?;
            fs.create(&to, &bytes).await.map_err(claimed)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::roostfs::native::NativeFs;
    use std::path::Path;
    use tempfile::TempDir;

    fn fresh() -> (TempDir, NativeFs) {
        let dir = TempDir::new().expect("tempdir");
        let fs = NativeFs::new(dir.path());
        (dir, fs)
    }

    /// Raw recursive listing that keeps hidden entries, like the Android
    /// `list_tree` plugin call (the trait's listings hide dot-files).
    fn raw_descendants(root: &Path, rel: &str) -> Vec<Entry> {
        fn walk(root: &Path, rel: &str, out: &mut Vec<Entry>) {
            for e in std::fs::read_dir(root.join(rel)).unwrap().flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                let child = format!("{rel}/{name}");
                let is_dir = e.file_type().unwrap().is_dir();
                out.push(Entry {
                    name,
                    rel_path: child.clone(),
                    is_dir,
                });
                if is_dir {
                    walk(root, &child, out);
                }
            }
        }
        let mut out = Vec::new();
        walk(root, rel, &mut out);
        out
    }

    #[test]
    fn base_name_takes_last_segment() {
        assert_eq!(base_name("a.md"), "a.md");
        assert_eq!(base_name("Daily/2026/a.md"), "a.md");
        assert_eq!(base_name("Projects/"), "Projects");
        assert_eq!(base_name(""), "unnamed");
    }

    #[test]
    fn first_candidate_keeps_historical_layout() {
        assert_eq!(trash_name(1_700_000_000, 0, "a.md"), "1700000000.a.md");
        assert_eq!(trash_name(1_700_000_000, 2, "a.md"), "1700000000-2.a.md");
    }

    #[tokio::test]
    async fn unique_name_skips_taken_slots() {
        let (_d, fs) = fresh();
        let first = unique_trash_rel(&fs, 42, "a.md").await.unwrap();
        assert_eq!(first, ".corvux-trash/42.a.md");
        fs.write_atomic(&first, b"one").await.unwrap();
        let second = unique_trash_rel(&fs, 42, "a.md").await.unwrap();
        assert_eq!(second, ".corvux-trash/42-1.a.md");
        fs.write_atomic(&second, b"two").await.unwrap();
        let third = unique_trash_rel(&fs, 42, "a.md").await.unwrap();
        assert_eq!(third, ".corvux-trash/42-2.a.md");
    }

    #[tokio::test]
    async fn copy_file_keeps_bytes_and_source() {
        let (_d, fs) = fresh();
        fs.write_atomic("notes/a.md", b"# hello").await.unwrap();
        copy_to(&fs, "notes/a.md", false, &[], ".corvux-trash/1.a.md")
            .await
            .unwrap();
        assert_eq!(fs.read(".corvux-trash/1.a.md").await.unwrap(), b"# hello");
        assert_eq!(fs.read("notes/a.md").await.unwrap(), b"# hello");
    }

    #[tokio::test]
    async fn copy_folder_keeps_nested_and_hidden_files() {
        let (dir, fs) = fresh();
        fs.write_atomic("Proj/a.md", b"a").await.unwrap();
        fs.write_atomic("Proj/sub/deep/b.md", b"b").await.unwrap();
        fs.write_atomic("Proj/.attachments/pic.png", &[0, 159, 146, 150])
            .await
            .unwrap();
        fs.write_atomic("Proj/.hidden.md", b"h").await.unwrap();
        fs.mkdir("Proj/empty").await.unwrap();

        let descendants = raw_descendants(dir.path(), "Proj");
        copy_to(&fs, "Proj", true, &descendants, ".corvux-trash/7.Proj")
            .await
            .unwrap();

        let t = ".corvux-trash/7.Proj";
        assert_eq!(fs.read(&format!("{t}/a.md")).await.unwrap(), b"a");
        assert_eq!(fs.read(&format!("{t}/sub/deep/b.md")).await.unwrap(), b"b");
        assert_eq!(
            fs.read(&format!("{t}/.attachments/pic.png")).await.unwrap(),
            vec![0, 159, 146, 150]
        );
        assert_eq!(fs.read(&format!("{t}/.hidden.md")).await.unwrap(), b"h");
        assert!(dir.path().join(t).join("empty").is_dir());
        // The copy never touches the source; the caller deletes it after.
        assert_eq!(fs.read("Proj/a.md").await.unwrap(), b"a");
    }

    #[tokio::test]
    async fn copy_folder_accepts_children_listed_before_parents() {
        let (dir, fs) = fresh();
        fs.write_atomic("P/x/y/z.md", b"z").await.unwrap();
        let mut descendants = raw_descendants(dir.path(), "P");
        // Deepest first: the copy must still create parents before children.
        descendants.sort_by_key(|e| std::cmp::Reverse(e.rel_path.len()));
        copy_to(&fs, "P", true, &descendants, ".corvux-trash/1.P")
            .await
            .unwrap();
        assert_eq!(fs.read(".corvux-trash/1.P/x/y/z.md").await.unwrap(), b"z");
    }

    #[tokio::test]
    async fn copy_folder_rejects_entries_outside_the_folder() {
        let (_d, fs) = fresh();
        fs.write_atomic("P/a.md", b"a").await.unwrap();
        let stray = Entry {
            name: "other.md".into(),
            rel_path: "Other/other.md".into(),
            is_dir: false,
        };
        let err = copy_to(&fs, "P", true, &[stray], ".corvux-trash/1.P")
            .await
            .unwrap_err();
        assert!(matches!(err.error, RoostError::Io(_)), "got {err:?}");
        assert!(!err.dest_claimed, "rejected before creating anything");
    }

    #[tokio::test]
    async fn copy_refuses_to_overwrite_an_existing_destination() {
        let (_d, fs) = fresh();
        fs.write_atomic("a.md", b"new").await.unwrap();
        fs.write_atomic(".corvux-trash/1.a.md", b"old")
            .await
            .unwrap();
        let err = copy_to(&fs, "a.md", false, &[], ".corvux-trash/1.a.md")
            .await
            .unwrap_err();
        assert!(
            matches!(err.error, RoostError::AlreadyExists(_)),
            "got {err:?}"
        );
        // The caller must NOT delete `dest`: it is another trashed note.
        assert!(!err.dest_claimed);
        assert_eq!(fs.read(".corvux-trash/1.a.md").await.unwrap(), b"old");
    }

    #[tokio::test]
    async fn copy_folder_onto_an_existing_folder_is_not_claimed() {
        let (dir, fs) = fresh();
        fs.write_atomic("P/a.md", b"new").await.unwrap();
        fs.write_atomic(".corvux-trash/1.P/old.md", b"old")
            .await
            .unwrap();
        let descendants = raw_descendants(dir.path(), "P");
        let err = copy_to(&fs, "P", true, &descendants, ".corvux-trash/1.P")
            .await
            .unwrap_err();
        assert!(!err.dest_claimed);
        assert_eq!(fs.read(".corvux-trash/1.P/old.md").await.unwrap(), b"old");
    }

    #[test]
    fn the_trash_itself_is_not_trashable() {
        assert!(ensure_trashable(".corvux-trash").is_err());
        assert!(ensure_trashable(".corvux-trash/").is_err());
        assert!(ensure_trashable(".corvux-trash/1.a.md").is_err());
        assert!(ensure_trashable("a.md").is_ok());
        assert!(ensure_trashable("Notes/.corvux-trash-ideas.md").is_ok());
        assert!(ensure_trashable(".corvux-trash-old/x.md").is_ok());
    }

    #[test]
    fn move_errors_are_sorted_safely() {
        // Our own name pre-check in SafPlugin.kt `moveDocument`.
        assert_eq!(
            classify_move_error("Already exists: .corvux-trash/1.a.md"),
            MoveFailure::NameTaken
        );
        // The provider exception is wrapped: copy instead, never treat it as
        // "name taken" (that would retry forever on an intermediate clash).
        assert_eq!(
            classify_move_error("Move not supported: Already exists /storage/emulated/0/x"),
            MoveFailure::Unsupported
        );
        assert_eq!(
            classify_move_error("Move not supported: provider returned no document"),
            MoveFailure::Unsupported
        );
        assert_eq!(classify_move_error("Not found: a.md"), MoveFailure::Other);
    }
}
