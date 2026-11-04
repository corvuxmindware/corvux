//! Roost file watcher.
//!
//! Wraps `notify-debouncer-full` to emit [`LocalChange`] events for markdown
//! files inside the roost. The watcher runs on a blocking thread (notify's
//! native handler); events are bridged to async code via an unbounded Tokio
//! channel.
//!
//! # Filtering
//!
//! Not every filesystem event is interesting:
//!
//! - `.corvux/**` is our own CRDT + peer storage — ignoring it prevents an
//!   infinite self-trigger loop (we write the CRDT, watcher fires, we try
//!   to re-sync…).
//! - Dotfiles at the roost root (`.git/`, `.obsidian/`, `.trash/`, etc.)
//!   are never user content. Skip them wholesale.
//! - Non-markdown files (images, PDFs, attachments) aren't synced in the
//!   MVP. Hand-wave them for now; phase 2 lifts this.
//! - Temp / editor-swap files (`*.tmp`, `~*`, `.#*`) are transient and
//!   firing on them wastes cycles.
//!
//! # Debounce
//!
//! Editors often write a file as "create temp → rename" which fires two
//! events in milliseconds. The debouncer collapses consecutive events for
//! the same path inside a 250 ms window. For a saving user that's fast
//! enough to feel live; for a scripted bulk-edit it prevents a flood.
//!
//! # Self-writes
//!
//! The engine also writes `.md` files (when applying a remote change). The
//! watcher *will* fire on those writes. Deduplication happens one layer up
//! in `NoteDoc::apply_markdown`, which compares the new text to the CRDT's
//! rendered text and no-ops if identical. So the wasted work is bounded
//! and the CRDT stays authoritative.

use std::path::{Path, PathBuf};
use std::time::Duration;

use notify::RecursiveMode;
use notify_debouncer_full::{new_debouncer, DebounceEventResult, Debouncer, RecommendedCache};
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::sync::error::StoreError;
use crate::sync::protocol::NotePath;
use crate::sync::store::to_note_path;

/// One actionable change the watcher observed. `content` is read lazily by
/// the engine, not the watcher — keeps the watcher thread cheap and avoids
/// races where the file gets rewritten between notify-event and engine-pull.
#[derive(Debug, Clone)]
pub enum LocalChange {
    /// A markdown file was created or modified. Engine should re-read it
    /// and apply to the CRDT. Used on desktop where the `notify` watcher
    /// only has the path — the engine pulls content via `std::fs` after
    /// the fact.
    Modified { path: NotePath, abs: PathBuf },
    /// Same as `Modified` but carries content inline — used on Android
    /// where the sync engine is driven by IPC writes (no inotify watcher
    /// on SAF), and read-by-path via `std::fs` would fail anyway.
    ModifiedWithContent { path: NotePath, content: String },
    /// A markdown file was deleted. Engine should tombstone the CRDT.
    Removed { path: NotePath },
    /// A directory (or other non-markdown path) under the roost was removed.
    /// For a recursive folder delete the OS sometimes reports only this single
    /// event and never the per-file `Removed` events the note-tombstone path
    /// relies on (notably macOS FSEvents; also the case when `is_watchable`
    /// would have dropped the children). The engine resolves it against its
    /// docstore: every tracked note at, or nested under, `prefix` is
    /// tombstoned. `prefix` is roost-relative, `/`-separated, no trailing slash.
    RemovedPrefix { prefix: NotePath },
}

/// Debounce window for filesystem events. 250 ms is long enough to coalesce
/// editor save-patterns (write → rename → chmod) into one event per file,
/// short enough that a quick typing-save-typing-save cycle still feels
/// responsive to collaborators.
const DEBOUNCE: Duration = Duration::from_millis(250);

/// Handle on a running watcher. Drop to stop.
pub struct RoostWatcher {
    _debouncer: Debouncer<notify::RecommendedWatcher, RecommendedCache>,
}

impl RoostWatcher {
    /// Start watching `roost` recursively. Change events are pushed onto
    /// `out`. Returns a handle that must be kept alive for the watcher to
    /// run — dropping it stops the watcher.
    pub fn start(
        roost: PathBuf,
        out: mpsc::UnboundedSender<LocalChange>,
    ) -> Result<Self, StoreError> {
        let roost_for_handler = roost.clone();
        let mut debouncer =
            new_debouncer(DEBOUNCE, None, move |res: DebounceEventResult| match res {
                Ok(events) => {
                    for ev in events {
                        for path in &ev.paths {
                            if let Some(change) = classify(&roost_for_handler, path, &ev.kind) {
                                if out.send(change).is_err() {
                                    // Receiver gone; engine has shut down.
                                    return;
                                }
                            }
                        }
                    }
                }
                Err(errors) => {
                    for e in errors {
                        warn!(error = %e, "notify error");
                    }
                }
            })
            .map_err(|e| StoreError::Io(std::io::Error::other(e.to_string())))?;

        debouncer
            .watch(&roost, RecursiveMode::Recursive)
            .map_err(|e| StoreError::Io(std::io::Error::other(e.to_string())))?;

        debug!(roost = %roost.display(), "watcher started");
        Ok(Self {
            _debouncer: debouncer,
        })
    }
}

/// Decide whether a notify event should produce a `LocalChange`.
///
/// Called on every raw event; fast-path must be cheap. Path filtering runs
/// first (pure string work); only surviving events touch the `NotePath`
/// normalizer which does a `canonicalize` syscall.
fn classify(roost: &Path, path: &Path, kind: &notify::EventKind) -> Option<LocalChange> {
    use notify::EventKind::*;
    match kind {
        Create(_) | Modify(_) => {
            // Creates/modifies only matter for markdown we'd actually sync.
            if !is_watchable(roost, path) {
                return None;
            }
            if !path.exists() {
                // Rename-temp artifact: event says Modify but the path is
                // gone already. Treat as removal (still a markdown file).
                return make_removed(roost, path);
            }
            let note = to_note_path(roost, path).ok()?;
            Some(LocalChange::Modified {
                path: note,
                abs: path.to_path_buf(),
            })
        }
        Remove(_) => {
            // A removed path can't be stat'd (it's gone) and notify's
            // `RemoveKind` is unreliable across platforms, so we can't ask
            // "was this a directory?". Instead: skip ignored locations; a
            // removed markdown file becomes a note tombstone; anything else
            // (a folder, or a non-md sibling) becomes a prefix removal the
            // engine resolves against its docstore. The non-md-sibling case
            // resolves to zero notes downstream, so it's a harmless no-op.
            if is_ignored_location(roost, path) {
                return None;
            }
            if has_markdown_ext(path) {
                make_removed(roost, path)
            } else {
                make_removed_prefix(roost, path)
            }
        }
        _ => None,
    }
}

/// Derive a roost-relative, `/`-separated path for a removed entry. A removed
/// path can't be canonicalized, so we fall back to manual prefix-stripping
/// (trying the canonical roost first, then the raw roost). Trailing slashes
/// are trimmed so directory and file forms normalize identically. Returns
/// `None` for the roost root or paths outside it.
fn removed_rel(roost: &Path, path: &Path) -> Option<String> {
    let roost_canon = roost.canonicalize().ok()?;
    let rel = path
        .strip_prefix(&roost_canon)
        .or_else(|_| path.strip_prefix(roost))
        .ok()?;
    let s = rel.to_string_lossy().replace('\\', "/");
    let s = s.trim_end_matches('/').to_string();
    if s.is_empty() {
        return None;
    }
    Some(s)
}

fn make_removed(roost: &Path, path: &Path) -> Option<LocalChange> {
    Some(LocalChange::Removed {
        path: NotePath(removed_rel(roost, path)?),
    })
}

fn make_removed_prefix(roost: &Path, path: &Path) -> Option<LocalChange> {
    Some(LocalChange::RemovedPrefix {
        prefix: NotePath(removed_rel(roost, path)?),
    })
}

/// `.corvux/` subpaths that are user-authored AI artifacts (skills,
/// subagent definitions). These ARE meant to sync across devices like
/// notes — the user wrote them, they're per-roost, and they're the
/// kind of content that's painful to keep manually in lock-step across
/// a phone + laptop. Everything else under `.corvux/` (CRDT docstore,
/// sync metadata, traces, indexes) stays device-local and the watcher
/// must continue to reject those paths.
///
/// Order matters here only as a list of prefixes to test — the
/// matcher uses `starts_with` per entry.
pub(crate) const SYNCABLE_CORVUX_PREFIXES: &[&str] = &[".corvux/skills/", ".corvux/agents/"];

/// True when `rel_str` is inside one of the explicit `.corvux/`
/// carve-outs. `rel_str` must use `/` separators (the caller normalizes
/// Windows backslashes before this point).
pub(crate) fn is_corvux_carveout(rel_str: &str) -> bool {
    SYNCABLE_CORVUX_PREFIXES
        .iter()
        .any(|p| rel_str.starts_with(p))
}

/// True when a path lives somewhere the watcher must never surface,
/// regardless of file type: outside the roost, the roost root itself,
/// device-local `.corvux/` internals (minus the skills/agents carve-out),
/// other tool dot-dirs (`.obsidian`, `.git`, `.trash`, …), or editor temp
/// files. Split out from [`is_watchable`] so removal handling can reuse the
/// location filter *without* the markdown-extension gate — a deleted folder
/// has no `.md` extension but its removal still matters for sync.
fn is_ignored_location(roost: &Path, path: &Path) -> bool {
    let Ok(rel) = path.strip_prefix(roost) else {
        return true; // outside the roost
    };
    let rel_str = rel.to_string_lossy().replace('\\', "/");
    if rel_str.is_empty() {
        return true; // the roost root itself
    }
    // Skills + agents carve-out: `.corvux/skills/<name>/SKILL.md` and
    // `.corvux/agents/<name>/AGENT.md` are user-authored content that
    // belongs across devices. Everything else under `.corvux/` stays
    // device-local.
    let in_carveout = is_corvux_carveout(&rel_str);
    if !in_carveout && (rel_str.starts_with(".corvux/") || rel_str == ".corvux") {
        return true;
    }
    // Other tool roots: .obsidian, .git, .trash, .DS_Store, etc.
    if !in_carveout && rel_str.starts_with('.') {
        return true;
    }
    // Components: reject if any path segment starts with `.` UNLESS
    // it's the `.corvux` root of an explicit carve-out (segment 0).
    // Mid-path hidden dirs like `.corvux/skills/.draft/SKILL.md` stay
    // rejected — that catches editor swap dirs the user didn't intend
    // to ship.
    for (i, comp) in rel.components().enumerate() {
        if let std::path::Component::Normal(os) = comp {
            let s = os.to_string_lossy();
            let is_root_corvux = in_carveout && i == 0;
            if s.starts_with('.') && !is_root_corvux {
                return true;
            }
            // Editor temp files.
            if s.ends_with(".tmp") || s.ends_with('~') || s.starts_with(".#") {
                return true;
            }
        }
    }
    false
}

/// MVP sync scope: only markdown files.
fn has_markdown_ext(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("md" | "markdown")
    )
}

/// Cheap pre-filter for create/modify events: a syncable markdown file in a
/// non-ignored location. Removals use [`is_ignored_location`] +
/// [`has_markdown_ext`] directly so directory deletes still get through.
fn is_watchable(roost: &Path, path: &Path) -> bool {
    !is_ignored_location(roost, path) && has_markdown_ext(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn is_watchable_accepts_md() {
        let roost = Path::new("/v");
        assert!(is_watchable(roost, Path::new("/v/note.md")));
        assert!(is_watchable(roost, Path::new("/v/sub/note.md")));
    }

    #[test]
    fn is_watchable_rejects_corvux_dir() {
        let roost = Path::new("/v");
        assert!(!is_watchable(
            roost,
            Path::new("/v/.corvux/sync/docs/x.automerge")
        ));
    }

    #[test]
    fn is_watchable_accepts_corvux_skills_carveout() {
        // User-authored SKILL.md inside the roost's .corvux/skills/
        // tree MUST sync. The leading `.corvux` segment that normally
        // disqualifies the path is explicitly carved out for this
        // subdir (and for `.corvux/agents/`).
        let roost = Path::new("/v");
        assert!(is_watchable(
            roost,
            Path::new("/v/.corvux/skills/fitness-summary/SKILL.md")
        ));
        assert!(is_watchable(
            roost,
            Path::new("/v/.corvux/agents/research-deep/AGENT.md")
        ));
    }

    #[test]
    fn is_watchable_keeps_rejecting_other_corvux_subtrees() {
        // Carve-out applies ONLY to the explicit prefixes; everything
        // else under `.corvux/` (sync metadata, traces, indexes, the
        // legacy `memory.md`) stays device-local.
        let roost = Path::new("/v");
        assert!(!is_watchable(
            roost,
            Path::new("/v/.corvux/sync/docs/note.automerge")
        ));
        assert!(!is_watchable(
            roost,
            Path::new("/v/.corvux/traces/abc.jsonl")
        ));
        assert!(!is_watchable(roost, Path::new("/v/.corvux/memory.md")));
    }

    #[test]
    fn is_watchable_rejects_hidden_dirs_inside_skills_carveout() {
        // The carve-out only re-allows the leading `.corvux` segment.
        // Editor scratch dirs nested inside (`.draft/`, `.swp/`) must
        // STILL be rejected so user accidents don't leak across
        // devices.
        let roost = Path::new("/v");
        assert!(!is_watchable(
            roost,
            Path::new("/v/.corvux/skills/.draft/SKILL.md")
        ));
        assert!(!is_watchable(
            roost,
            Path::new("/v/.corvux/skills/foo/.swp/SKILL.md")
        ));
    }

    #[test]
    fn is_watchable_rejects_hidden_dirs() {
        let roost = Path::new("/v");
        assert!(!is_watchable(roost, Path::new("/v/.obsidian/config.json")));
        assert!(!is_watchable(roost, Path::new("/v/.git/HEAD")));
        assert!(!is_watchable(roost, Path::new("/v/notes/.archive/old.md")));
    }

    #[test]
    fn is_watchable_rejects_temp_files() {
        let roost = Path::new("/v");
        assert!(!is_watchable(roost, Path::new("/v/note.md.tmp")));
        assert!(!is_watchable(roost, Path::new("/v/note.md~")));
        assert!(!is_watchable(roost, Path::new("/v/.#note.md")));
    }

    #[test]
    fn is_watchable_rejects_non_markdown() {
        let roost = Path::new("/v");
        assert!(!is_watchable(roost, Path::new("/v/image.png")));
        assert!(!is_watchable(roost, Path::new("/v/doc.pdf")));
    }

    #[test]
    fn folder_path_is_a_valid_location_but_not_watchable() {
        // A plain subfolder is a valid (non-ignored) location, so its removal
        // can become a prefix tombstone — even though it isn't *watchable*
        // (no `.md` extension, nothing to sync on create/modify).
        let roost = Path::new("/v");
        assert!(!is_ignored_location(roost, Path::new("/v/projects")));
        assert!(!is_watchable(roost, Path::new("/v/projects")));
        // Ignored locations stay ignored regardless of extension.
        assert!(is_ignored_location(roost, Path::new("/v/.obsidian/x.json")));
        assert!(is_ignored_location(roost, Path::new("/v/.corvux/sync/db")));
        assert!(is_ignored_location(roost, Path::new("/v/note.md.tmp")));
    }

    #[test]
    fn classify_remove_distinguishes_files_folders_and_ignored() {
        // External folder delete regression: a removed directory must become a
        // `RemovedPrefix` (so the engine tombstones nested notes), a removed
        // `.md` a `Removed`, and an ignored location nothing — even though all
        // three arrive as the same notify `Remove` kind.
        let tmp = tempdir().unwrap();
        let roost = tmp.path().canonicalize().unwrap();
        let kind = notify::EventKind::Remove(notify::event::RemoveKind::Any);

        let folder = classify(&roost, &roost.join("projects"), &kind);
        assert!(
            matches!(&folder, Some(LocalChange::RemovedPrefix { prefix }) if prefix.0 == "projects"),
            "removed folder must yield RemovedPrefix, got {folder:?}"
        );

        let file = classify(&roost, &roost.join("projects").join("a.md"), &kind);
        assert!(
            matches!(&file, Some(LocalChange::Removed { path }) if path.0 == "projects/a.md"),
            "removed markdown must yield Removed, got {file:?}"
        );

        let ignored = classify(
            &roost,
            &roost.join(".obsidian").join("workspace.json"),
            &kind,
        );
        assert!(
            ignored.is_none(),
            "ignored location must yield None, got {ignored:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn watcher_emits_on_markdown_create() {
        let tmp = tempdir().unwrap();
        // macOS tempdir is /var/folders/... symlinked to /private/var/folders/...
        // FSEvents fires from the canonical path; if we watch the symlinked one
        // events never match. Canonicalize before arming the watcher.
        let roost = tmp.path().canonicalize().unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let _w = RoostWatcher::start(roost.clone(), tx).unwrap();

        // Give the watcher a moment to arm.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let note = roost.join("hello.md");
        std::fs::write(&note, "hi").unwrap();

        let got = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .expect("watcher should emit within 3s")
            .expect("channel should deliver an event");
        match got {
            LocalChange::Modified { path, .. } => {
                assert_eq!(path.0, "hello.md");
            }
            other => panic!("expected Modified, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn watcher_ignores_corvux_dir() {
        let tmp = tempdir().unwrap();
        let roost = tmp.path().canonicalize().unwrap();
        let corvux = roost.join(".corvux").join("sync").join("docs");
        std::fs::create_dir_all(&corvux).unwrap();

        let (tx, mut rx) = mpsc::unbounded_channel();
        let _w = RoostWatcher::start(roost.clone(), tx).unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        std::fs::write(corvux.join("a.automerge"), b"x").unwrap();
        // Touch a real file too so we know the watcher itself is alive.
        std::fs::write(roost.join("real.md"), "body").unwrap();

        // Drain everything that lands within a ~1s window. Windows can fire
        // multiple Create/Modify events for a single std::fs::write that
        // outrun the 250ms debounce, so we don't pin the count of real.md
        // events — just that at least one fires AND nothing under .corvux/
        // ever does.
        let mut saw_real = false;
        while let Ok(Some(ev)) = tokio::time::timeout(Duration::from_millis(800), rx.recv()).await {
            match ev {
                LocalChange::Modified { path, .. }
                | LocalChange::ModifiedWithContent { path, .. }
                | LocalChange::Removed { path }
                | LocalChange::RemovedPrefix { prefix: path } => {
                    assert!(
                        !path.0.starts_with(".corvux"),
                        ".corvux/ event leaked: {}",
                        path.0
                    );
                    if path.0 == "real.md" {
                        saw_real = true;
                    }
                }
            }
        }
        assert!(saw_real, "watcher never observed the real.md write");
    }
}
