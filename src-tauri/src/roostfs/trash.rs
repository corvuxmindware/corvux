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

use std::collections::HashSet;
use std::sync::LazyLock;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;

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
    /// Our own pre-check in `moveDocument`: the provider moves under the OLD
    /// name before renaming, and that in-between name (`.corvux-trash/a.md`,
    /// typically a stray left by an earlier failed rename) is taken. Marked by
    /// [`super::STAGING_NAME_TAKEN_PREFIX`]; it must be told apart from a
    /// provider that really cannot move, because with the copy fallback disabled
    /// "unsupported" is a permanent refusal and one stray file would block every
    /// delete of that name.
    StagingNameTaken,
    /// Anything else (source gone, bad path, no permission): give up.
    Other,
}

// Only the Android SAF backend calls this outside tests.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub(crate) fn classify_move_error(message: &str) -> MoveFailure {
    // The MARKER, never a phrase: a note called `Things already in place.md`
    // or a provider's own exception text must not be taken for our pre-check.
    if message.contains(super::STAGING_NAME_TAKEN_PREFIX) {
        MoveFailure::StagingNameTaken
    } else if message.contains("Move not supported") {
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
///
/// On success returns a [`CopyManifest`] — length + blake3 of every file copied
/// and every directory created — which [`verify_copied_content`] checks against
/// BOTH the destination and the (still present) source before anything may
/// delete the original.
// Only the Android SAF backend calls this outside tests.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub(crate) async fn copy_to<F: RoostFs + ?Sized>(
    fs: &F,
    src_rel: &str,
    is_dir: bool,
    descendants: &[Entry],
    dest_rel: &str,
) -> Result<CopyManifest, CopyError> {
    let unclaimed = |error| CopyError {
        error,
        dest_claimed: false,
    };
    let claimed = |error| CopyError {
        error,
        dest_claimed: true,
    };
    let mut manifest = CopyManifest::default();

    if !is_dir {
        let bytes = fs.read(src_rel).await.map_err(unclaimed)?;
        fs.create(dest_rel, &bytes).await.map_err(|e| match e {
            // Another trashed note owns this name: not ours to clean up.
            RoostError::AlreadyExists(_) => unclaimed(e),
            // `create` checks for an existing file first, so whatever is
            // at `dest` after any other failure was made by us.
            e => claimed(e),
        })?;
        manifest.files.push(CopiedFile::of("", &bytes));
        return Ok(manifest);
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
            manifest.dirs.push(suffix.to_string());
        } else {
            let bytes = fs.read(&entry.rel_path).await.map_err(claimed)?;
            fs.create(&to, &bytes).await.map_err(claimed)?;
            manifest.files.push(CopiedFile::of(suffix, &bytes));
        }
    }
    Ok(manifest)
}

/// What [`copy_to`] wrote, as hashes (never the bytes: a 10K-note folder must
/// not be held in memory twice).
#[derive(Debug, Default)]
// Only the Android SAF backend calls this outside tests.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub(crate) struct CopyManifest {
    files: Vec<CopiedFile>,
    /// Directory suffixes created below the destination root (so empty
    /// folders are verified too).
    dirs: Vec<String>,
}

#[derive(Debug)]
// Only the Android SAF backend calls this outside tests.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
struct CopiedFile {
    /// Path below the copied root; `""` for a single-file copy.
    suffix: String,
    len: usize,
    hash: blake3::Hash,
}

#[cfg_attr(not(target_os = "android"), allow(dead_code))]
impl CopiedFile {
    fn of(suffix: &str, bytes: &[u8]) -> Self {
        Self {
            suffix: suffix.to_string(),
            len: bytes.len(),
            hash: blake3::hash(bytes),
        }
    }
}

#[cfg_attr(not(target_os = "android"), allow(dead_code))]
fn join_suffix(base: &str, suffix: &str) -> String {
    if suffix.is_empty() {
        base.to_string()
    } else {
        format!("{}/{suffix}", base.trim_end_matches('/'))
    }
}

/// One row of a raw listing: the [`Entry`] plus the provider's file size and
/// modified time (`None` when the provider reports neither, e.g. directories).
/// Both walks of [`trash_by_copy`] compare them, so a file edited IN PLACE —
/// same name, same kind — is noticed too, not only an added or missing name.
#[derive(Debug, Clone)]
// Only the Android SAF backend calls this outside tests.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub(crate) struct RawEntry {
    pub entry: Entry,
    pub size: Option<i64>,
    pub modified: Option<i64>,
}

/// The raw (hidden files included) tree access and removal the copy-then-delete
/// trash needs but [`RoostFs`] does not expose. `SafFs` implements it over the
/// plugin; tests implement it over a real folder, which is what lets the whole
/// destructive sequence in [`trash_by_copy`] run on a desktop.
#[async_trait]
// Only the Android SAF backend calls this outside tests.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub(crate) trait RawTree: RoostFs {
    /// Every descendant of `rel` INCLUDING hidden files, roost-relative paths,
    /// with size / modified time where the provider reports them. Must return
    /// `Err` — never a shorter list — when any directory could not be read
    /// completely.
    async fn raw_descendants(&self, rel: &str) -> Result<Vec<RawEntry>, RoostError>;
    /// Remove the ORIGINAL (recursively for a folder). The one destructive call.
    async fn remove_original(&self, rel: &str) -> Result<(), RoostError>;
    /// Best-effort removal of a copy this operation created and then rejected.
    async fn remove_partial_copy(&self, rel: &str);
}

/// Check the copy against the manifest, with the original still in place:
/// every directory exists at the destination, every file there has the
/// recorded length and hash, and every source file STILL has the hash it had
/// when it was copied (an edit that landed mid-copy is in the original but not
/// the copy). Any read error fails: an unreadable destination is not a backup.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub(crate) async fn verify_copied_content<F: RoostFs + ?Sized>(
    fs: &F,
    src_rel: &str,
    dest_rel: &str,
    manifest: &CopyManifest,
) -> Result<(), RoostError> {
    for dir in &manifest.dirs {
        let at = join_suffix(dest_rel, dir);
        if !fs.exists(&at).await? {
            return Err(RoostError::Io(format!(
                "trash copy is missing folder {at}; the original was not deleted"
            )));
        }
    }
    for f in &manifest.files {
        let at = join_suffix(dest_rel, &f.suffix);
        let copied = fs.read(&at).await?;
        if copied.len() != f.len || blake3::hash(&copied) != f.hash {
            return Err(RoostError::Io(format!(
                "trash copy of {at} does not match what was read; the original was not deleted"
            )));
        }
    }
    for f in &manifest.files {
        let at = join_suffix(src_rel, &f.suffix);
        let now = fs.read(&at).await?;
        if now.len() != f.len || blake3::hash(&now) != f.hash {
            return Err(RoostError::Io(format!(
                "{at} changed while it was being moved to the trash; it was not deleted. Try again."
            )));
        }
    }
    Ok(())
}

/// The folder must list exactly the same entries after the copy as before it:
/// an entry that appeared (a concurrent write) is not in the copy, one that
/// vanished or was never listed means the first listing was not the whole
/// folder, and a file whose size or modified time moved was edited after it
/// was copied.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub(crate) fn verify_same_listing(
    before: &[RawEntry],
    after: &[RawEntry],
) -> Result<(), RoostError> {
    let key = |r: &RawEntry| (r.entry.rel_path.clone(), r.entry.is_dir, r.size, r.modified);
    let b: HashSet<_> = before.iter().map(key).collect();
    let a: HashSet<_> = after.iter().map(key).collect();
    if let Some((path, ..)) = a.symmetric_difference(&b).next() {
        return Err(RoostError::Io(format!(
            "the folder changed while it was being moved to the trash (e.g. {path}); it was not deleted. Try again."
        )));
    }
    Ok(())
}

/// MAY the copy-then-delete fallback remove an original? **No — not until it
/// has been validated on a real device with a safe design for the last step.**
///
/// Everything in [`copy_verify_then_remove`] narrows the window in which the
/// original can be lost, but cannot close it: between the final re-listing and
/// `remove_original` a write can still land, and an error message does not
/// protect a write that arrives after the check. Corvux's own per-note lock
/// cannot stop ANOTHER app writing to the same shared folder either. So the
/// destructive step stays disabled: on a provider that cannot move documents,
/// deleting from inside Corvux fails with [`NO_TRASH_MOVE_MESSAGE`] and the
/// original is left untouched, rather than risking the loss of an edit.
///
/// Flipping this to `true` requires, at minimum, a design that actually closes
/// that window (a mutation barrier cannot; a freeze-by-rename of the original
/// before verifying is the candidate) AND the device evidence listed in
/// `docs/validation/android-saf-validation-2026-11-07.md`.
/// `tests/saf_trash_guard.rs` fails if this is changed without that file
/// recording a completed run.
pub(crate) const COPY_THEN_DELETE_ENABLED: bool = false;

/// What the user sees when the fallback is disabled and the provider cannot
/// move the item into the trash. Nothing was changed.
pub(crate) const NO_TRASH_MOVE_MESSAGE: &str = "This storage location cannot move items to the trash, so nothing was deleted. Delete it with your file manager instead.";

/// Trash by copying, for providers that cannot move a document. Gated by
/// [`COPY_THEN_DELETE_ENABLED`]: while that is `false` this refuses BEFORE
/// copying or touching anything. When enabled it is [`copy_verify_then_remove`].
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub(crate) async fn trash_by_copy<F: RawTree + ?Sized>(
    fs: &F,
    rel: &str,
    is_dir: bool,
    dest: &str,
    expected: &[String],
) -> Result<(), RoostError> {
    if !COPY_THEN_DELETE_ENABLED {
        return Err(RoostError::Io(NO_TRASH_MOVE_MESSAGE.to_string()));
    }
    copy_verify_then_remove(fs, rel, is_dir, dest, expected).await
}

/// Every note the caller already found inside `rel` (`expected`, roost-relative)
/// must be in the copy. If the recursive walk is consistently short — or empty —
/// both walks agree with each other, so the before/after comparison alone
/// passes. `expected` comes from a DIFFERENT query path (`commands::files`
/// enumerates with `RoostFs::list`, one non-recursive query per directory, not
/// the bulk `listTree` walk), so a note missing from the copy exposes a bug or
/// inconsistency in the walk or the copy, and the original must not be removed.
/// An empty manifest with notes expected fails here too. It does NOT defend
/// against a provider that lies identically on every query path; that remains
/// the provider-trust boundary.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub(crate) fn verify_manifest_covers(
    expected: &[String],
    rel: &str,
    manifest: &CopyManifest,
) -> Result<(), RoostError> {
    let prefix = format!("{}/", rel.trim_matches('/'));
    let have: HashSet<&str> = manifest.files.iter().map(|f| f.suffix.as_str()).collect();
    for note in expected {
        let note = note.replace('\\', "/");
        let note = note.trim_start_matches('/');
        let Some(suffix) = note.strip_prefix(&prefix) else {
            return Err(RoostError::Io(format!(
                "expected note {note} is not inside the folder being trashed; it was not deleted"
            )));
        };
        if !have.contains(suffix) {
            return Err(RoostError::Io(format!(
                "the copy of the folder is missing {note}; it was not deleted"
            )));
        }
    }
    Ok(())
}

/// Copy everything into `dest`, PROVE the copy is complete, and only then
/// remove the original. Any failure before the last line leaves the original
/// exactly as it was and removes the copy this call made.
///
/// The proof is: (1) `raw_descendants` is a checked listing (an error aborts —
/// never a shorter list); (2) every note the caller already found
/// (`expected`) is in the copy ([`verify_manifest_covers`]), so a provider
/// that is consistently wrong about a folder cannot pass by agreeing with
/// itself; (3) every file read back from the destination matches the hash
/// taken at copy time, and every source file still matches it; (4) the folder
/// lists identically AFTER the copy and the verification as it did before —
/// same names, same kinds, same file sizes and modified times — so nothing was
/// added or edited behind the copy's back. Step (4) is deliberately last,
/// right before the delete, to leave the smallest window for a write to slip
/// in. That window is not zero, which is why [`trash_by_copy`] is gated off.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub(crate) async fn copy_verify_then_remove<F: RawTree + ?Sized>(
    fs: &F,
    rel: &str,
    is_dir: bool,
    dest: &str,
    expected: &[String],
) -> Result<(), RoostError> {
    let before = if is_dir {
        fs.raw_descendants(rel).await?
    } else {
        Vec::new()
    };
    let before_entries: Vec<Entry> = before.iter().map(|r| r.entry.clone()).collect();
    let manifest = match copy_to(fs, rel, is_dir, &before_entries, dest).await {
        Ok(m) => m,
        Err(e) => {
            // Leave no half-copied folder behind; the original is untouched.
            // Only remove `dest` if this copy created it: otherwise it is
            // another trashed note that happens to own the name.
            if e.dest_claimed {
                fs.remove_partial_copy(dest).await;
            }
            return Err(e.error);
        }
    };
    let verified = async {
        if is_dir {
            verify_manifest_covers(expected, rel, &manifest)?;
        }
        verify_copied_content(fs, rel, dest, &manifest).await?;
        if is_dir {
            let after = fs.raw_descendants(rel).await?;
            verify_same_listing(&before, &after)?;
        }
        Ok::<(), RoostError>(())
    }
    .await;
    if let Err(e) = verified {
        fs.remove_partial_copy(dest).await;
        return Err(e);
    }
    fs.remove_original(rel).await
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
        // Our own staging-name pre-check is NOT "the provider cannot move".
        assert_eq!(
            classify_move_error("Staging name taken: 'a.md' is already in '.corvux-trash'"),
            MoveFailure::StagingNameTaken
        );
        // A provider's own wording, or a note whose NAME contains the old
        // phrase, stays Unsupported: it must not trigger the rename-aside.
        assert_eq!(
            classify_move_error("Move not supported: provider returned no document"),
            MoveFailure::Unsupported
        );
        assert_eq!(
            classify_move_error("Move not supported: Things already in place.md already in x"),
            MoveFailure::Unsupported
        );
    }

    // ---- verify + trash_by_copy: the SAF copy-then-delete fallback ----------

    use crate::roostfs::test_support::FaultyFs;

    async fn seed_project(fs: &FaultyFs) {
        fs.write_atomic("Proj/a.md", b"a").await.unwrap();
        fs.write_atomic("Proj/sub/b.md", b"b").await.unwrap();
        fs.write_atomic("Proj/.attachments/pic.png", &[0, 159, 146, 150])
            .await
            .unwrap();
        fs.write_atomic("Proj/.hidden.md", b"h").await.unwrap();
        fs.mkdir("Proj/empty").await.unwrap();
    }

    fn e(rel_path: &str, is_dir: bool) -> Entry {
        Entry {
            name: rel_path.rsplit('/').next().unwrap().to_string(),
            rel_path: rel_path.to_string(),
            is_dir,
        }
    }

    /// A raw row with no size/modified (a directory, or a provider that
    /// reports neither).
    fn r(rel_path: &str, is_dir: bool) -> RawEntry {
        RawEntry {
            entry: e(rel_path, is_dir),
            size: None,
            modified: None,
        }
    }

    fn file(rel_path: &str, size: i64, modified: i64) -> RawEntry {
        RawEntry {
            entry: e(rel_path, false),
            size: Some(size),
            modified: Some(modified),
        }
    }

    #[tokio::test]
    async fn trash_by_copy_moves_a_verified_folder() {
        let d = TempDir::new().unwrap();
        let fs = FaultyFs::new(d.path());
        seed_project(&fs).await;

        copy_verify_then_remove(&fs, "Proj", true, ".corvux-trash/7.Proj", &[])
            .await
            .unwrap();

        assert!(!d.path().join("Proj").exists(), "original removed last");
        let t = ".corvux-trash/7.Proj";
        assert_eq!(fs.read(&format!("{t}/a.md")).await.unwrap(), b"a");
        assert_eq!(fs.read(&format!("{t}/sub/b.md")).await.unwrap(), b"b");
        assert_eq!(
            fs.read(&format!("{t}/.attachments/pic.png")).await.unwrap(),
            vec![0, 159, 146, 150]
        );
        assert_eq!(fs.read(&format!("{t}/.hidden.md")).await.unwrap(), b"h");
        assert!(d.path().join(t).join("empty").is_dir());
        assert_eq!(fs.raw_calls(), 2, "listed before and after the copy");
    }

    #[tokio::test]
    async fn a_failed_listing_aborts_before_anything_is_copied_or_removed() {
        let d = TempDir::new().unwrap();
        let mut fs = FaultyFs::new(d.path());
        seed_project(&fs).await;
        fs.fail_first_raw = true;

        let err = copy_verify_then_remove(&fs, "Proj", true, ".corvux-trash/7.Proj", &[])
            .await
            .unwrap_err();

        assert!(matches!(err, RoostError::Io(_)), "got {err:?}");
        assert_eq!(fs.read("Proj/a.md").await.unwrap(), b"a");
        assert_eq!(fs.read("Proj/sub/b.md").await.unwrap(), b"b");
        assert!(!d.path().join(".corvux-trash/7.Proj").exists());
    }

    /// The 2026-11 failure mode: a null cursor inside ONE nested folder used
    /// to read as "that folder is empty", so the copy skipped its files and
    /// the original folder — files and all — was deleted. Even with the Kotlin
    /// listing fixed, the copy must prove completeness itself.
    #[tokio::test]
    async fn a_silently_truncated_listing_cannot_authorize_the_delete() {
        let d = TempDir::new().unwrap();
        let mut fs = FaultyFs::new(d.path());
        seed_project(&fs).await;
        fs.omit_children_of_on_first_raw = Some("Proj/sub".into());

        let err = copy_verify_then_remove(&fs, "Proj", true, ".corvux-trash/7.Proj", &[])
            .await
            .unwrap_err();

        assert!(err.to_string().contains("changed while"), "got {err}");
        assert_eq!(
            fs.read("Proj/sub/b.md").await.unwrap(),
            b"b",
            "the file the truncated listing missed must survive"
        );
        assert_eq!(fs.read("Proj/a.md").await.unwrap(), b"a");
        assert!(
            !d.path().join(".corvux-trash/7.Proj").exists(),
            "the rejected copy is removed"
        );
    }

    #[tokio::test]
    async fn a_write_landing_mid_trash_is_not_deleted() {
        let d = TempDir::new().unwrap();
        let mut fs = FaultyFs::new(d.path());
        seed_project(&fs).await;
        fs.write_before_second_raw = Some(("Proj/late.md".into(), b"late".to_vec()));

        let err = copy_verify_then_remove(&fs, "Proj", true, ".corvux-trash/7.Proj", &[])
            .await
            .unwrap_err();

        assert!(err.to_string().contains("changed while"), "got {err}");
        assert_eq!(fs.read("Proj/late.md").await.unwrap(), b"late");
        assert_eq!(fs.read("Proj/a.md").await.unwrap(), b"a");
        assert!(!d.path().join(".corvux-trash/7.Proj").exists());
    }

    #[tokio::test]
    async fn trash_by_copy_handles_a_single_file() {
        let d = TempDir::new().unwrap();
        let fs = FaultyFs::new(d.path());
        fs.write_atomic("notes/a.md", b"# hello").await.unwrap();

        copy_verify_then_remove(&fs, "notes/a.md", false, ".corvux-trash/1.a.md", &[])
            .await
            .unwrap();

        assert!(!d.path().join("notes/a.md").exists());
        assert_eq!(fs.read(".corvux-trash/1.a.md").await.unwrap(), b"# hello");
        assert_eq!(fs.raw_calls(), 0, "a file needs no tree listing");
    }

    #[tokio::test]
    async fn trash_by_copy_never_removes_a_destination_it_did_not_create() {
        let d = TempDir::new().unwrap();
        let fs = FaultyFs::new(d.path());
        fs.write_atomic("a.md", b"new").await.unwrap();
        fs.write_atomic(".corvux-trash/1.a.md", b"old")
            .await
            .unwrap();

        let err = copy_verify_then_remove(&fs, "a.md", false, ".corvux-trash/1.a.md", &[])
            .await
            .unwrap_err();

        assert!(matches!(err, RoostError::AlreadyExists(_)), "got {err:?}");
        assert_eq!(fs.read("a.md").await.unwrap(), b"new");
        assert_eq!(fs.read(".corvux-trash/1.a.md").await.unwrap(), b"old");
    }

    async fn copied_project() -> (TempDir, FaultyFs, CopyManifest) {
        let d = TempDir::new().unwrap();
        let fs = FaultyFs::new(d.path());
        seed_project(&fs).await;
        let before: Vec<Entry> = fs
            .raw_descendants("Proj")
            .await
            .unwrap()
            .into_iter()
            .map(|r| r.entry)
            .collect();
        let m = copy_to(&fs, "Proj", true, &before, ".corvux-trash/7.Proj")
            .await
            .unwrap();
        (d, fs, m)
    }

    #[tokio::test]
    async fn verify_accepts_a_faithful_copy() {
        let (_d, fs, m) = copied_project().await;
        verify_copied_content(&fs, "Proj", ".corvux-trash/7.Proj", &m)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn verify_rejects_a_tampered_destination_file() {
        let (_d, fs, m) = copied_project().await;
        fs.write_atomic(".corvux-trash/7.Proj/a.md", b"truncated")
            .await
            .unwrap();
        let err = verify_copied_content(&fs, "Proj", ".corvux-trash/7.Proj", &m)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("does not match"), "got {err}");
    }

    #[tokio::test]
    async fn verify_rejects_a_missing_destination_file() {
        let (d, fs, m) = copied_project().await;
        std::fs::remove_file(d.path().join(".corvux-trash/7.Proj/sub/b.md")).unwrap();
        assert!(
            verify_copied_content(&fs, "Proj", ".corvux-trash/7.Proj", &m)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn verify_rejects_a_missing_destination_folder() {
        let (d, fs, m) = copied_project().await;
        std::fs::remove_dir(d.path().join(".corvux-trash/7.Proj/empty")).unwrap();
        let err = verify_copied_content(&fs, "Proj", ".corvux-trash/7.Proj", &m)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("missing folder"), "got {err}");
    }

    #[tokio::test]
    async fn verify_rejects_a_source_edited_after_it_was_copied() {
        let (_d, fs, m) = copied_project().await;
        fs.write_atomic("Proj/a.md", b"edited during the copy")
            .await
            .unwrap();
        let err = verify_copied_content(&fs, "Proj", ".corvux-trash/7.Proj", &m)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("changed while"), "got {err}");
    }

    #[test]
    fn same_listing_ignores_order_but_not_content() {
        let a = [r("P/a.md", false), r("P/s", true), r("P/s/b.md", false)];
        let reordered = [r("P/s/b.md", false), r("P/a.md", false), r("P/s", true)];
        verify_same_listing(&a, &reordered).unwrap();

        // Added behind the copy's back.
        let added = [
            r("P/a.md", false),
            r("P/s", true),
            r("P/s/b.md", false),
            r("P/late.md", false),
        ];
        assert!(verify_same_listing(&a, &added).is_err());
        // The first listing was missing something the second saw (truncation)…
        assert!(verify_same_listing(&a[..2], &a).is_err());
        // …or the entry vanished.
        assert!(verify_same_listing(&a, &a[..2]).is_err());
        // A file replaced by a folder of the same name is a change.
        let swapped = [r("P/a.md", true), r("P/s", true), r("P/s/b.md", false)];
        assert!(verify_same_listing(&a, &swapped).is_err());
    }

    /// An edit in place keeps the name and kind: only size / modified time
    /// reveal it.
    #[test]
    fn same_listing_notices_an_in_place_edit() {
        let before = [file("P/a.md", 10, 1_000), r("P/s", true)];
        verify_same_listing(&before, &[file("P/a.md", 10, 1_000), r("P/s", true)]).unwrap();
        // Content grew.
        assert!(
            verify_same_listing(&before, &[file("P/a.md", 99, 1_000), r("P/s", true)]).is_err()
        );
        // Same length, newer modified time (an edit that kept the size).
        assert!(
            verify_same_listing(&before, &[file("P/a.md", 10, 2_000), r("P/s", true)]).is_err()
        );
    }

    /// A provider that reports neither size nor modified time degrades to a
    /// name-and-kind comparison; it must not make every trash fail.
    #[test]
    fn same_listing_tolerates_providers_without_metadata() {
        let a = [r("P/a.md", false)];
        verify_same_listing(&a, &[r("P/a.md", false)]).unwrap();
    }

    /// The window the audit measured: the source re-hash reads each file
    /// once, early, and a whole tree walk follows. An edit to an existing file
    /// that lands before the final listing keeps every name, so the old
    /// name-only comparison deleted the original and with it the edit.
    #[tokio::test]
    async fn an_in_place_edit_before_the_final_listing_is_not_deleted() {
        let d = TempDir::new().unwrap();
        let mut fs = FaultyFs::new(d.path());
        seed_project(&fs).await;
        fs.write_before_second_raw = Some((
            "Proj/a.md".into(),
            b"edited, and longer than before".to_vec(),
        ));

        let err = copy_verify_then_remove(&fs, "Proj", true, ".corvux-trash/7.Proj", &[])
            .await
            .unwrap_err();

        assert!(err.to_string().contains("changed while"), "got {err}");
        assert_eq!(
            fs.read("Proj/a.md").await.unwrap(),
            b"edited, and longer than before",
            "the in-place edit must survive"
        );
        assert!(!d.path().join(".corvux-trash/7.Proj").exists());
    }

    // ---- the destructive fallback is gated off ------------------------------

    /// An error message does not protect a write that lands after the final
    /// check, and Corvux's own lock cannot stop another app writing. Until a
    /// design that closes that window is validated on a device, the copy
    /// fallback must refuse BEFORE touching anything.
    // Pinning the constant IS the point: flipping it must fail this test.
    #[allow(clippy::assertions_on_constants)]
    #[tokio::test]
    async fn the_destructive_fallback_refuses_and_preserves_the_original() {
        assert!(
            !COPY_THEN_DELETE_ENABLED,
            "re-enabling needs the device evidence recorded in docs/validation/android-saf-validation-2026-11-07.md"
        );
        let d = TempDir::new().unwrap();
        let fs = FaultyFs::new(d.path());
        seed_project(&fs).await;

        let err = trash_by_copy(&fs, "Proj", true, ".corvux-trash/7.Proj", &[])
            .await
            .unwrap_err();

        assert!(
            err.to_string().contains("nothing was deleted"),
            "actionable message, got {err}"
        );
        assert_eq!(fs.read("Proj/a.md").await.unwrap(), b"a");
        assert_eq!(fs.read("Proj/sub/b.md").await.unwrap(), b"b");
        assert!(
            !d.path().join(".corvux-trash/7.Proj").exists(),
            "refused before copying anything"
        );
        assert_eq!(fs.raw_calls(), 0, "refused before even listing");
    }

    #[tokio::test]
    async fn the_gate_also_protects_single_files() {
        let d = TempDir::new().unwrap();
        let fs = FaultyFs::new(d.path());
        fs.write_atomic("a.md", b"mine").await.unwrap();
        let err = trash_by_copy(&fs, "a.md", false, ".corvux-trash/1.a.md", &[])
            .await
            .unwrap_err();
        assert!(err.to_string().contains("nothing was deleted"), "got {err}");
        assert_eq!(fs.read("a.md").await.unwrap(), b"mine");
        assert!(!d.path().join(".corvux-trash").exists());
    }

    // ---- expected-notes cross-check (when the machinery is enabled) ---------

    fn expected_all() -> Vec<String> {
        vec!["Proj/a.md".to_string(), "Proj/sub/b.md".to_string()]
    }

    /// A provider that is consistently wrong gives the same truncated listing
    /// on both walks, so `verify_same_listing` agrees with itself and passes.
    /// Only the independently enumerated notes can catch it.
    #[tokio::test]
    async fn a_consistently_truncated_listing_is_refused_by_the_expected_notes_check() {
        let d = TempDir::new().unwrap();
        let mut fs = FaultyFs::new(d.path());
        seed_project(&fs).await;
        fs.omit_children_of_always = Some("Proj/sub".into());

        let err =
            copy_verify_then_remove(&fs, "Proj", true, ".corvux-trash/7.Proj", &expected_all())
                .await
                .unwrap_err();

        assert!(
            err.to_string().contains("missing Proj/sub/b.md"),
            "got {err}"
        );
        assert_eq!(
            fs.read("Proj/sub/b.md").await.unwrap(),
            b"b",
            "the note the lying listing hid must survive"
        );
        assert!(!d.path().join(".corvux-trash/7.Proj").exists());
    }

    /// An empty manifest is only plausible for a genuinely empty folder. When
    /// notes were discovered first, an empty copy means the listing lied.
    #[tokio::test]
    async fn an_empty_manifest_is_refused_when_notes_were_discovered() {
        let d = TempDir::new().unwrap();
        let mut fs = FaultyFs::new(d.path());
        seed_project(&fs).await;
        fs.omit_all_raw = true;

        let err =
            copy_verify_then_remove(&fs, "Proj", true, ".corvux-trash/7.Proj", &expected_all())
                .await
                .unwrap_err();

        assert!(err.to_string().contains("missing"), "got {err}");
        assert_eq!(fs.read("Proj/a.md").await.unwrap(), b"a");
        assert_eq!(fs.read("Proj/sub/b.md").await.unwrap(), b"b");
        assert!(!d.path().join(".corvux-trash/7.Proj").exists());
    }

    #[tokio::test]
    async fn a_genuinely_empty_folder_still_trashes_when_no_notes_were_expected() {
        let d = TempDir::new().unwrap();
        let fs = FaultyFs::new(d.path());
        fs.mkdir("Empty").await.unwrap();

        copy_verify_then_remove(&fs, "Empty", true, ".corvux-trash/1.Empty", &[])
            .await
            .unwrap();

        assert!(!d.path().join("Empty").exists());
        assert!(d.path().join(".corvux-trash/1.Empty").is_dir());
    }

    #[tokio::test]
    async fn the_expected_notes_check_passes_when_every_note_is_in_the_copy() {
        let d = TempDir::new().unwrap();
        let fs = FaultyFs::new(d.path());
        seed_project(&fs).await;

        copy_verify_then_remove(&fs, "Proj", true, ".corvux-trash/7.Proj", &expected_all())
            .await
            .unwrap();

        assert!(!d.path().join("Proj").exists());
        assert_eq!(
            fs.read(".corvux-trash/7.Proj/sub/b.md").await.unwrap(),
            b"b"
        );
    }

    #[test]
    fn expected_notes_are_matched_by_relative_path_and_normalized() {
        let m = CopyManifest {
            files: vec![
                CopiedFile::of("a.md", b"a"),
                CopiedFile::of("sub/b.md", b"b"),
            ],
            dirs: vec![],
        };
        verify_manifest_covers(&["Proj/a.md".into()], "Proj", &m).unwrap();
        // Backslash and leading-slash spellings of the same note.
        verify_manifest_covers(&["/Proj\\sub\\b.md".into()], "Proj/", &m).unwrap();
        // Absent from the copy.
        assert!(verify_manifest_covers(&["Proj/c.md".into()], "Proj", &m).is_err());
        // A note outside the folder is a caller bug: refuse rather than guess.
        assert!(verify_manifest_covers(&["Other/a.md".into()], "Proj", &m).is_err());
        // Nothing expected: nothing to check (an empty folder is fine).
        verify_manifest_covers(&[], "Proj", &CopyManifest::default()).unwrap();
    }
}
