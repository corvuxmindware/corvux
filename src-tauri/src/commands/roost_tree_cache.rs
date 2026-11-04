//! Disk-backed cache of `Vec<FileEntry>` for fast roost opens.
//!
//! On Android SAF, even with the Round 7 ContentResolver fast path, a
//! cold roost walk costs ~500ms-1.6s for 213 entries (provider noise).
//! That's the floor of the SAF stack, not Corvux code. The cache flips
//! every subsequent open after first launch into "read JSON from disk
//! and return" — a few milliseconds.
//!
//! Strategy:
//! 1. Cache file lives under `app_data_dir/vaults/{hash}/file-tree.json`
//!    (same per-roost directory the graph + search DBs use, via
//!    [`crate::paths::roost_db_dir`]).
//! 2. On roost open: read cache → return immediately if present. Spawn
//!    a background task that walks fresh; if the result differs, emit
//!    a `roost://tree-changed` event so the frontend updates.
//! 3. Mutations (create/delete/rename/move) invalidate by deleting the
//!    cache file. Next open rebuilds it.
//!
//! Failure mode is "rebuild from source of truth" — every cache error
//! falls back to a fresh walk. The cache is purely an optimization.

use std::path::PathBuf;

use tauri::AppHandle;
use tauri_specta::Event;

use crate::events;

use super::files::FileEntry;

/// Resolve the cache file path for a roost key. Mirrors the strategy
/// used by `roost_db_dir` so the cache co-locates with the graph + FTS
/// DBs (one directory per roost, hashed).
fn cache_file(app: &AppHandle, roost_key: &str) -> Result<PathBuf, String> {
    let dir = crate::paths::roost_db_dir(app, roost_key)?;
    Ok(dir.join("file-tree.json"))
}

/// Bump when `FileEntry`'s serialized shape changes so a stale cache written
/// by an older build is discarded (parses to `None` → fresh walk) rather than
/// silently loaded (MR-B6). Before this the cache was a bare `Vec<FileEntry>`
/// with no version tag, so a serde-compatible shape change could load a
/// partial/stale tree.
const CACHE_VERSION: u32 = 1;

/// Owned envelope for reads.
#[derive(serde::Deserialize)]
struct CacheEnvelope {
    version: u32,
    entries: Vec<FileEntry>,
}

/// Borrowing envelope for writes — avoids cloning the whole tree.
#[derive(serde::Serialize)]
struct CacheEnvelopeRef<'a> {
    version: u32,
    entries: &'a [FileEntry],
}

/// Read the cached `Vec<FileEntry>` for `roost_key` if present and current.
/// Returns `None` if the cache doesn't exist, fails to parse, or was written
/// by a build with a different `CACHE_VERSION` — the caller treats all three
/// as "do a fresh walk". A bare pre-versioning cache also parses to `None`
/// here, so it degrades safely on upgrade.
pub fn read(app: &AppHandle, roost_key: &str) -> Option<Vec<FileEntry>> {
    let path = cache_file(app, roost_key).ok()?;
    let bytes = std::fs::read(&path).ok()?;
    let env: CacheEnvelope = serde_json::from_slice(&bytes).ok()?;
    if env.version != CACHE_VERSION {
        tracing::debug!(
            roost = %roost_key,
            found = env.version,
            want = CACHE_VERSION,
            "tree_cache: version mismatch, ignoring"
        );
        return None;
    }
    tracing::debug!(roost = %roost_key, entries = env.entries.len(), "tree_cache: hit");
    Some(env.entries)
}

/// Persist `entries` as the cached tree for `roost_key`, tagged with the
/// current `CACHE_VERSION`. Best-effort — failures are logged at debug and
/// ignored. Atomicity isn't required: a partial write just looks like a
/// corrupt cache on next read, which falls through to a fresh walk.
pub fn write(app: &AppHandle, roost_key: &str, entries: &[FileEntry]) {
    let path = match cache_file(app, roost_key) {
        Ok(p) => p,
        Err(e) => {
            tracing::debug!(error = %e, "tree_cache: cache_file failed");
            return;
        }
    };
    let bytes = match serde_json::to_vec(&CacheEnvelopeRef {
        version: CACHE_VERSION,
        entries,
    }) {
        Ok(b) => b,
        Err(e) => {
            tracing::debug!(error = %e, "tree_cache: serialize failed");
            return;
        }
    };
    if let Err(e) = std::fs::write(&path, &bytes) {
        tracing::debug!(error = %e, path = %path.display(), "tree_cache: write failed");
    } else {
        tracing::debug!(
            roost = %roost_key,
            entries = entries.len(),
            bytes = bytes.len(),
            "tree_cache: wrote"
        );
    }
}

/// Drop the cache for `roost_key`. Called from mutation IPCs so the
/// next open rebuilds from disk.
pub fn invalidate(app: &AppHandle, roost_key: &str) {
    let Ok(path) = cache_file(app, roost_key) else {
        return;
    };
    let _ = std::fs::remove_file(&path);
}

/// Background revalidation: walk the roost fresh, compare against
/// `cached`, emit `roost://tree-changed` on the frontend if they
/// differ, and overwrite the cache so the next open is up to date.
///
/// Spawned by the warm-cache branch of `open_roost`. Must NOT block
/// the IPC return — the user has already gotten the cached tree and
/// is interacting with it.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub async fn revalidate(
    app: AppHandle,
    roost_key: String,
    generation: u32,
    backend: std::sync::Arc<dyn crate::roostfs::RoostFs>,
    display_root: String,
    cached: Vec<FileEntry>,
) {
    let started = std::time::Instant::now();
    let fresh = match super::roost::scan_roost_backend(backend.as_ref(), "", &display_root).await {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(error = %e, "tree_cache: revalidate walk failed");
            return;
        }
    };
    apply_if_changed(&app, &roost_key, generation, &cached, fresh, started);
}

/// Reconcile a freshly-walked tree against the served cache: if they differ,
/// overwrite the cache and emit `roost://tree-changed` so the frontend
/// updates; otherwise confirm the cache and do nothing.
///
/// Shared by the SAF (`scan_roost_backend`) and native (`scan_directory`,
/// MR-B4) revalidation paths so both use identical drift detection — but each
/// MUST supply a tree from ITS OWN scanner. Mixing scanners would spuriously
/// report drift and, worse, leak entries one scanner filters: native
/// `scan_directory` skips symlinks (AUDIT-076) that a backend `list_recursive`
/// walk surfaces, so a native cache filled by `scan_directory` must be
/// revalidated by `scan_directory` too.
pub fn apply_if_changed(
    app: &AppHandle,
    roost_key: &str,
    generation: u32,
    cached: &[FileEntry],
    fresh: Vec<FileEntry>,
    started: std::time::Instant,
) {
    if entries_equal(cached, &fresh) {
        tracing::debug!(
            roost = %roost_key,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "tree_cache: revalidate confirmed cache"
        );
        return;
    }
    tracing::info!(
        roost = %roost_key,
        cached_count = count_files(cached),
        fresh_count = count_files(&fresh),
        elapsed_ms = started.elapsed().as_millis() as u64,
        "tree_cache: revalidate detected drift, emitting roost://tree-changed"
    );
    write(app, roost_key, &fresh);
    if let Err(e) = (events::RoostTreeChanged {
        roost_path: roost_key.to_string(),
        generation,
        entries: fresh,
    })
    .emit(app)
    {
        tracing::warn!(error = %e, "tree_cache: emit failed");
    }
}

/// Structural equality on the file tree. Compares names, paths,
/// is_dir flags, and recursively compares children. Order-sensitive —
/// both `scan_roost_backend` and `scan_directory` sort entries
/// deterministically, so the same roost content always produces the same
/// tree shape.
fn entries_equal(a: &[FileEntry], b: &[FileEntry]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b.iter()).all(|(x, y)| {
        x.name == y.name
            && x.path == y.path
            && x.is_dir == y.is_dir
            && entries_equal(&x.children, &y.children)
    })
}

/// Total leaf count — only used for telemetry on drift events.
fn count_files(entries: &[FileEntry]) -> usize {
    entries.iter().fold(0, |acc, e| {
        if e.is_dir {
            acc + count_files(&e.children)
        } else {
            acc + 1
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::files::FileEntry;

    fn leaf(name: &str, path: &str) -> FileEntry {
        FileEntry {
            name: name.into(),
            path: path.into(),
            is_dir: false,
            children: vec![],
        }
    }

    #[test]
    fn versioned_envelope_roundtrips() {
        let entries = vec![leaf("a.md", "/r/a.md")];
        let bytes = serde_json::to_vec(&CacheEnvelopeRef {
            version: CACHE_VERSION,
            entries: &entries,
        })
        .unwrap();
        let back: CacheEnvelope = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(back.version, CACHE_VERSION);
        assert_eq!(back.entries.len(), 1);
        assert_eq!(back.entries[0].name, "a.md");
    }

    #[test]
    fn bare_vec_cache_is_rejected() {
        // MR-B6: a pre-versioning cache was a bare Vec<FileEntry>. It must fail
        // to parse as the versioned envelope so `read` falls through to a
        // fresh walk instead of trusting an unversioned (possibly stale) shape.
        let bare = vec![leaf("a.md", "/r/a.md")];
        let bytes = serde_json::to_vec(&bare).unwrap();
        assert!(serde_json::from_slice::<CacheEnvelope>(&bytes).is_err());
    }

    #[test]
    fn entries_equal_detects_structural_diff() {
        let a = vec![leaf("x.md", "/r/x.md")];
        assert!(entries_equal(&a, &[leaf("x.md", "/r/x.md")]));
        assert!(!entries_equal(&a, &[leaf("y.md", "/r/y.md")]));
    }
}
