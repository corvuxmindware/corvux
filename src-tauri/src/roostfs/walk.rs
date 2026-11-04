//! Backend-agnostic roost walker.
//!
//! Walks every `.md` file under a [`RoostFs`] and yields
//! `(full_path, content)` pairs. `full_path` is computed the same way
//! `scan_roost_backend` computes `FileEntry.path` — so the graph /
//! search indexers can use it directly as the stable note identifier
//! that matches what the frontend passes back when the user navigates.
//!
//! # Performance
//!
//! Listing is sequential (directory entries are tiny). Reading file
//! contents runs **in parallel** via `buffer_unordered`. This matters
//! on Android SAF: every read is a ContentResolver JNI round-trip
//! (~300–600 ms) and a 160-note roost was taking 80+ s sequentially.
//! With 16-way parallelism the same roost hydrates in ~5 s — small
//! enough that "failed to load graph — waiting for roost to
//! initialize" is a brief banner instead of a standing complaint.

use futures_util::stream::{self, StreamExt};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::roostfs::RoostFs;

/// Markdown extension test shared by every walk filter. Obsidian (and our
/// editor) treat both `.md` and `.markdown` as notes; indexing on save
/// already accepts both, so the walkers must too — otherwise a `.markdown`
/// note is indexed when saved but then purged as a "ghost" on the next warm
/// reconcile because the disk listing never reports it (IP-11).
///
/// EDT-3: this is the single source of truth for "is this a note?" — every
/// walker, tree scan, and grep fallback routes through it (or its `Path`
/// sibling below) so no surface can drift back to `.md`-only and start
/// dropping `.markdown` notes. Case-insensitive so Windows users who
/// capitalise (`.MD`, `.Markdown`) aren't silently excluded.
pub(crate) fn is_markdown_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.ends_with(".md") || lower.ends_with(".markdown")
}

/// [`is_markdown_name`] for a filesystem [`Path`](std::path::Path) — checks
/// the final component. Use this at the `std::fs` walk sites that used to
/// test `path.extension() == "md"` (which silently missed `.markdown`).
pub(crate) fn is_markdown_path(path: &std::path::Path) -> bool {
    path.file_name()
        .map(|n| is_markdown_name(&n.to_string_lossy()))
        .unwrap_or(false)
}

/// Short-lived shared cache of a freshly-walked roost. The two cold-path
/// callers (`init_graph` and `init_search`) fire within seconds of each
/// other on first launch — without coalescing they each pay for a full
/// SAF tree walk + 16-way parallel read.
///
/// Single-entry by design: only one roost is open at a time. TTL is
/// short — long enough to fold both inits, short enough that nothing
/// else in the app is tempted to depend on cache freshness.
struct CacheEntry {
    roost_path: String,
    walked_at: Instant,
    files: Arc<Vec<(String, String)>>,
}

static WALK_CACHE: OnceLock<Mutex<Option<CacheEntry>>> = OnceLock::new();

const CACHE_TTL: Duration = Duration::from_secs(30);

fn cache_slot() -> &'static Mutex<Option<CacheEntry>> {
    WALK_CACHE.get_or_init(|| Mutex::new(None))
}

/// Drop any cached walk. Called by `open_roost` when the user opens or
/// switches roosts so a stale cache from a previous session doesn't leak
/// into a fresh init.
pub fn invalidate_walk_cache() {
    if let Ok(mut guard) = cache_slot().lock() {
        *guard = None;
    }
}

/// Number of concurrent file reads. Tuned for Android SAF where
/// individual reads are slow but the syscall cost is fully amortized
/// by parallelism. Going much higher stops helping because the
/// ContentResolver starts serializing internally; 16 is a safe sweet
/// spot that also stays conservative on desktop filesystems.
const READ_CONCURRENCY: usize = 16;

/// Recursively collect every markdown file under the backend.
///
/// Hidden entries (`.`-prefixed) and our own trash folder are filtered
/// inside each backend's `list()` — callers don't need to re-filter.
/// Returns entries in an unspecified order; the graph indexer does its
/// own two-pass scan so order doesn't matter.
/// Cached variant — coalesces concurrent cold-path inits (graph + search
/// most commonly). Walks the backend on cache miss and stores the result
/// for `CACHE_TTL`. Returns an `Arc` so both callers share the same
/// allocation; the original `Vec<(String, String)>` shape is preserved
/// at the call sites by cloning out, which is cheap relative to a SAF
/// re-walk.
///
/// The cache is keyed by `roost_path`; switching roosts invalidates via
/// `invalidate_walk_cache` (wired into `open_roost`).
pub async fn collect_markdown_cached(
    fs: &dyn RoostFs,
    roost_path: &str,
) -> Result<Arc<Vec<(String, String)>>, String> {
    {
        let guard = cache_slot()
            .lock()
            .map_err(|e| format!("walk cache lock: {e}"))?;
        if let Some(entry) = guard.as_ref() {
            if entry.roost_path == roost_path && entry.walked_at.elapsed() < CACHE_TTL {
                tracing::debug!(
                    age_ms = entry.walked_at.elapsed().as_millis() as u64,
                    files = entry.files.len(),
                    "walk: cache hit"
                );
                return Ok(entry.files.clone());
            }
        }
    }

    let files = collect_markdown(fs).await?;
    let arc = Arc::new(files);

    if let Ok(mut guard) = cache_slot().lock() {
        *guard = Some(CacheEntry {
            roost_path: roost_path.to_string(),
            walked_at: Instant::now(),
            files: arc.clone(),
        });
    }

    Ok(arc)
}

pub async fn collect_markdown(fs: &dyn RoostFs) -> Result<Vec<(String, String)>, String> {
    // Phase 1: ask the backend for every descendant in one bulk call.
    // `NativeFs` falls back to the trait's default `list()` recursion
    // (which is fine — `read_dir` is microsecond-cheap). `SafFs`
    // overrides to a single JNI round-trip, eliminating the per-dir
    // `listFolder` chain that dominated cold-start on deep roosts.
    let entries = fs.list_recursive("").await.map_err(|e| e.to_string())?;
    let paths: Vec<String> = entries
        .into_iter()
        .filter(|e| !e.is_dir && is_markdown_name(&e.name))
        .map(|e| e.rel_path)
        .collect();

    // Phase 2: fan out reads. Each future owns a String so there's no
    // borrow-across-await drama.
    let display_root = fs.display_root().to_string();
    let mut results: Vec<(String, String)> = Vec::with_capacity(paths.len());

    let mut stream = stream::iter(paths.into_iter().map(|rel_path| {
        let root = display_root.clone();
        async move {
            let bytes = fs
                .read(&rel_path)
                .await
                .map_err(|e| format!("read {rel_path}: {e}"))?;
            let content =
                String::from_utf8(bytes).map_err(|e| format!("non-UTF8 {rel_path}: {e}"))?;
            let full = format!(
                "{}/{}",
                root.trim_end_matches(['/', '\\']),
                rel_path.trim_start_matches('/')
            );
            Ok::<_, String>((full, content))
        }
    }))
    .buffer_unordered(READ_CONCURRENCY);

    // IP-6: a single unreadable or non-UTF-8 file must NOT abort the whole
    // cold index. Before this, `results.push(res?)` bubbled the first such
    // error out of `init_graph`, which then left the DB empty — and because
    // the empty DB has notes==0, every subsequent launch retried the cold
    // scan and hit the same bad file, so the graph was permanently empty
    // (no notes, no search) until the offending file was removed by hand.
    // Now we skip the file with a warning and index everything else.
    let mut skipped = 0usize;
    while let Some(res) = stream.next().await {
        match res {
            Ok(pair) => results.push(pair),
            Err(e) => {
                skipped += 1;
                tracing::warn!(error = %e, "walk: skipping unreadable/non-UTF8 markdown file");
            }
        }
    }
    if skipped > 0 {
        tracing::warn!(
            skipped,
            "walk: some markdown files were skipped during index"
        );
    }
    Ok(results)
}

/// List the full path of every markdown file in the roost **without
/// reading content**. This is Phase 1 of [`collect_markdown`] only — the
/// cheap part (one `list_recursive` call; on SAF a single JNI round-trip,
/// on native a `read_dir` walk). No per-file reads, so it stays fast even
/// on a large SAF roost.
///
/// Paths are built identically to `collect_markdown` (`display_root` +
/// `rel_path`) so callers can feed them straight into
/// `note_id_from_abs_path` and get ids that match what indexing wrote.
/// Used by the warm-open reconcile to detect notes whose files were
/// deleted off-app.
/// Returns `(absolute_display_path, roost_relative_path)` pairs for every
/// markdown file in the roost — no content reads. This is Phase 1 of
/// [`collect_markdown`] only: one `list_recursive` call (a single JNI
/// round-trip on SAF, a `read_dir` walk on native), so it stays fast even on
/// a large SAF roost.
///
/// The warm-open reconcile uses the absolute form to compute note ids
/// (matching what indexing wrote — feed it to `note_id_from_abs_path`) and
/// the relative form to `read()` the few files that turn out to be new on
/// disk but absent from the graph (IP-2, added-while-closed).
pub async fn list_markdown_path_pairs(fs: &dyn RoostFs) -> Result<Vec<(String, String)>, String> {
    let entries = fs.list_recursive("").await.map_err(|e| e.to_string())?;
    let root = fs.display_root().trim_end_matches(['/', '\\']).to_string();
    Ok(entries
        .into_iter()
        .filter(|e| !e.is_dir && is_markdown_name(&e.name))
        .map(|e| {
            let rel = e.rel_path.trim_start_matches('/').to_string();
            (format!("{root}/{rel}"), rel)
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::roostfs::native::NativeFs;
    use tempfile::TempDir;

    #[tokio::test]
    async fn collect_markdown_skips_non_utf8_and_keeps_the_rest() {
        // IP-6: one non-UTF-8 .md file must not abort the whole index.
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("good.md"), "# Good\n[[Other]]\n").unwrap();
        std::fs::write(dir.path().join("also.markdown"), "# Also\n").unwrap(); // IP-11
                                                                               // Invalid UTF-8 bytes in a .md file.
        std::fs::write(dir.path().join("bad.md"), [0xff, 0xfe, 0x00, 0x9c]).unwrap();

        let fs = NativeFs::new(dir.path());
        let out = collect_markdown(&fs).await.expect("walk must not error");
        let names: std::collections::HashSet<String> = out
            .iter()
            .map(|(full, _)| full.rsplit('/').next().unwrap().to_string())
            .collect();

        assert!(names.contains("good.md"), "valid note indexed");
        assert!(
            names.contains("also.markdown"),
            ".markdown extension is indexed (IP-11)"
        );
        assert!(
            !names.contains("bad.md"),
            "non-UTF-8 note skipped, not fatal (IP-6)"
        );
        assert_eq!(out.len(), 2);
    }

    #[tokio::test]
    async fn list_markdown_path_pairs_includes_markdown_ext() {
        // IP-11: the reconcile listing must report `.markdown` too, or those
        // notes get purged as ghosts on warm open.
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.md"), "x").unwrap();
        std::fs::write(dir.path().join("b.markdown"), "y").unwrap();
        std::fs::write(dir.path().join("c.txt"), "z").unwrap(); // not a note

        let fs = NativeFs::new(dir.path());
        let pairs = list_markdown_path_pairs(&fs).await.expect("listing");
        let rels: std::collections::HashSet<String> =
            pairs.iter().map(|(_, rel)| rel.clone()).collect();
        assert!(rels.contains("a.md"));
        assert!(rels.contains("b.markdown"));
        assert!(!rels.iter().any(|r| r.ends_with(".txt")));
        assert_eq!(pairs.len(), 2);
    }
}
