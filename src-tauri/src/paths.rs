//! App-owned path resolution.
//!
//! Corvux writes two kinds of data: **roost content** (markdown, attachments
//! — user-owned, inside the roost folder, safe to sync) and **derived
//! indices** (graph.db, search.db — device-local, must NOT sync and must
//! not live inside a SAF content:// tree). This module is the single
//! source of truth for the latter.
//!
//! ## AUDIT-043 / 043b / 043c — Unified app-data root
//!
//! Three sites used to resolve "app-private data dir" independently:
//! - `ai/models.rs` via `ProjectDirs::from("com","Corvux","Corvux")` →
//!   137 MB ONNX model file leaked on every uninstall.
//! - `lib.rs` panic hook via `BaseDirs::data_dir().join("corvux")` →
//!   `crash.log` leaked on every uninstall (PII risk: panic messages
//!   can include file paths and tracing context).
//! - `secret_store.rs` mobile branch via
//!   `ProjectDirs::from("com","corvuxmindware","corvux")` → mobile
//!   encrypted secrets at a path the `directories` crate doesn't
//!   officially support on iOS/Android.
//!
//! The OS only cleans up `app.path().app_data_dir()` on uninstall —
//! everything else lingers. After this audit, every consumer routes
//! through the helpers here, which all sit under one canonical root.
//!
//! The root is captured via `initialize()` at app startup (called from
//! the tauri `setup` callback, which is when an `AppHandle` becomes
//! available) and stashed in a process-wide `OnceLock`. The panic hook
//! and the mobile secret store need to read the path WITHOUT an
//! AppHandle in scope, so the static cell is how they reach it.
//!
//! `run_legacy_migrations()` moves files from the three legacy paths
//! into the new layout on first launch after upgrade. Idempotent: a
//! second call is a no-op because the legacy paths are empty after
//! the first call succeeds.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use tauri::{AppHandle, Manager};

/// Marker written only inside app-created derived-index directories. Directory
/// name shape is never sufficient proof that a directory is safe to delete:
/// user-created roost names may legitimately look like legacy hashes.
const INDEX_DIR_MARKER: &str = ".corvux-derived-index";

pub(crate) fn is_marked_index_dir(path: &Path) -> bool {
    path.join(INDEX_DIR_MARKER).is_file()
}

// ── Static state ────────────────────────────────────────────────────

/// Tauri's `app.path().app_data_dir()` captured at startup. Populated
/// by [`initialize`] from inside the tauri `setup` callback. Code that
/// has no `AppHandle` (the panic hook, the mobile secret store) reads
/// this directly via [`app_data_dir`].
static APP_DATA_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Capture the canonical app-data root and ensure the directory exists.
/// Call this exactly once at startup, from inside the tauri `setup`
/// callback where an `AppHandle` is available. Returns `Err` only on
/// the rare case where Tauri itself can't resolve the dir (a bad OS
/// permission state); callers should treat that as fatal.
///
/// A second call returns `Ok(())` silently — useful for the test
/// harness where multiple `tauri::Builder::default()` may run.
pub fn initialize(app: &AppHandle) -> Result<(), String> {
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("app_data_dir: {e}"))?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;
    // OnceLock::set returns Err if already set — ignore for test reentry.
    let _ = APP_DATA_DIR.set(dir);
    Ok(())
}

/// The captured app-data root, or `None` if `initialize` hasn't run
/// yet. Code paths that may execute before tauri setup (panic hook,
/// startup-error toasts) must handle the `None` case.
pub fn app_data_dir() -> Option<&'static Path> {
    APP_DATA_DIR.get().map(|p| p.as_path())
}

/// AUDIT-043: where AI model files (ONNX, GGUF, tokenizer JSON) live.
/// Replaces the prior `ProjectDirs("com","Corvux","Corvux")/data_dir/
/// models/` location, which sat outside the OS-managed app-data folder
/// and leaked on uninstall.
pub fn ai_models_dir() -> Result<PathBuf, String> {
    let root = app_data_dir().ok_or_else(|| {
        "paths::initialize has not been called; ai_models_dir requested too early".to_string()
    })?;
    let dir = root.join("models");
    std::fs::create_dir_all(&dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;
    Ok(dir)
}

/// AUDIT-043b: where crash.log + rotated copies live. Used by the
/// panic hook in `lib.rs` and by `diagnostics.rs` when reading the
/// previous-session crash excerpt. Returns `None` if `initialize`
/// hasn't run yet — the panic hook falls back to the legacy BaseDirs
/// path in that window (panics during init, before tauri setup
/// completes, are exceedingly rare and the legacy path is what we
/// had before, so it's not a regression).
pub fn crash_log_dir() -> Option<PathBuf> {
    app_data_dir().map(|p| p.to_path_buf())
}

/// AUDIT-043c: where the mobile encrypted secret store lives. Routes
/// secrets into the OS-managed app-private sandbox on iOS / Android
/// instead of the `directories` crate's (unofficial) mobile fallback,
/// which isn't guaranteed to be writable on either platform.
#[allow(dead_code)] // referenced only from the mobile `#[cfg]` branch of secret_store.rs
pub fn mobile_secret_store_dir() -> Result<PathBuf, String> {
    let root = app_data_dir().ok_or_else(|| {
        "paths::initialize has not been called; mobile_secret_store_dir requested too early"
            .to_string()
    })?;
    let dir = root.join("secrets");
    std::fs::create_dir_all(&dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;
    Ok(dir)
}

// ── Roost DB ────────────────────────────────────────────────────────

/// Deterministic per-roost directory under the app's data dir for local
/// indices (CozoDB graph, SQLite FTS, file-tree cache).
///
/// MR-B2: keyed off the roost's STABLE id (`<roost>/.corvux/vault-id.bin`,
/// which travels with the folder) rather than the roost PATH. Path-keying
/// meant moving or renaming the roost folder produced a fresh empty DB and
/// orphaned the old one; an id-keyed dir survives a relocation. The legacy
/// path-hash dir is migrated (renamed) into the id-keyed location on first
/// resolve, so existing users keep their index instead of re-indexing on
/// upgrade. If the id can't be resolved (read-only / offline / disconnected
/// roost, or a SAF root we can't stat), we fall back to the legacy path hash —
/// no worse than before.
pub fn roost_db_dir(app: &AppHandle, roost_key: &str) -> Result<PathBuf, String> {
    let data = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("app_data_dir: {e}"))?;
    let vaults = data.join("vaults");
    let legacy_dir = vaults.join(format!("{:016x}", stable_hash(&normalize_key(roost_key))));

    let dir = match stable_roost_dir_name(roost_key) {
        Some(name) => {
            let id_dir = vaults.join(name);
            // One-time migration: if the id-keyed dir doesn't exist yet but the
            // legacy path-hash dir does, move it over so the existing index
            // carries forward. Best-effort — a failed rename just means the
            // id dir starts fresh (a one-off re-index), never data loss.
            if !id_dir.exists() && legacy_dir.exists() {
                match std::fs::rename(&legacy_dir, &id_dir) {
                    Ok(()) => tracing::info!(
                        to = %id_dir.display(),
                        "roost_db_dir: migrated legacy path-hash index to stable-id dir"
                    ),
                    Err(e) => tracing::warn!(
                        from = %legacy_dir.display(),
                        to = %id_dir.display(),
                        error = %e,
                        "roost_db_dir: legacy index migration failed; id dir starts fresh"
                    ),
                }
            }
            id_dir
        }
        None => legacy_dir,
    };
    std::fs::create_dir_all(&dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;
    std::fs::write(dir.join(INDEX_DIR_MARKER), b"corvux-derived-index-v1\n")
        .map_err(|e| format!("write index marker in {}: {e}", dir.display()))?;
    Ok(dir)
}

/// The stable, relocation-proof directory name for a roost's local index, or
/// `None` if the roost's id can't be resolved (fall back to the path hash).
///
/// Prefixed `id-` so it's visually distinct from a legacy bare-hex path-hash
/// dir. Deletion additionally requires the derived-index marker; names alone
/// are never treated as proof.
fn stable_roost_dir_name(roost_key: &str) -> Option<String> {
    let id = crate::sync::vault_id::ensure_vault_id_sync(Path::new(roost_key)).ok()?;
    Some(format!("id-{}", hex32(&id)))
}

/// Lowercase-hex encoding of a 16-byte roost id (32 chars).
fn hex32(bytes: &[u8; 16]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn normalize_key(key: &str) -> String {
    // NEW-M1: case-folding is gated on the host filesystem's semantics —
    // mirrors `commands::roost::same_roost_root` (AUDIT-045). Windows/macOS
    // default to case-insensitive filesystems (NTFS, consumer APFS), so
    // `C:/Foo` and `c:\foo` must resolve to one index dir. Linux is
    // case-sensitive: `/home/me/Foo` and `/home/me/foo` are DISTINCT roosts,
    // and folding them here collides their legacy path-hash — which lets the
    // legacy→id migration below `rename` one roost's `graph.db` into the
    // other's id-dir (a real cross-roost data steal). Only the legacy
    // fallback dir is derived from this; the primary id-dir is keyed by the
    // per-roost UUID and is unaffected.
    let slashed = key.replace('\\', "/");
    if cfg!(any(target_os = "windows", target_os = "macos")) {
        slashed.to_lowercase()
    } else {
        slashed
    }
}

/// FNV-1a 64-bit. Deterministic across runs, unlike `DefaultHasher`.
/// Sufficient entropy for filesystem directory names — collisions only
/// matter if two different roost paths hash to the same 16 hex chars,
/// and FNV at 2^64 keyspace makes that overwhelmingly unlikely for the
/// handful of roosts any one user will open.
fn stable_hash(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

// ── MR-B3: orphaned index-dir GC ─────────────────────────────────────

/// Summary of a startup index-dir GC sweep. Surfaced via `tracing` so a
/// diagnostic export shows what (if anything) was reclaimed.
#[derive(Debug, Default)]
pub struct IndexGcReport {
    /// Directory names removed from `vaults/`.
    pub removed: Vec<String>,
    /// Set when the sweep was skipped wholesale (no manifest / no registered
    /// roosts), carrying the reason. When set, `removed` is empty.
    pub skipped: Option<String>,
    pub errors: Vec<String>,
}

/// Is `name` an `id-`-keyed index dir (`id-` + 32 lowercase-hex, MR-B2)?
fn is_id_dir_name(name: &str) -> bool {
    match name.strip_prefix("id-") {
        Some(hex) => hex.len() == 32 && hex.bytes().all(|b| b.is_ascii_hexdigit()),
        None => false,
    }
}

/// Is `name` a legacy bare path-hash index dir (`{:016x}` of the FNV hash)?
fn is_legacy_hash_name(name: &str) -> bool {
    name.len() == 16 && name.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Decide whether a `vaults/` child dir is an orphaned Corvux index dir safe to
/// delete. Conservative by construction:
///   - Only `id-`-keyed and legacy path-hash dirs carrying the
///     `.corvux-derived-index` marker are candidates. Names alone never prove a
///     directory is derived state; a user roost may legitimately be hex-shaped.
///   - A dir whose name is in `live` (a registered roost's current index) is
///     kept.
///   - `id-` dirs are only deletable when `all_ids_resolved` — i.e. every
///     registered roost's stable id was resolvable this run, so `live` holds the
///     complete set of live id-dir names. If ANY roost's id couldn't be resolved
///     (offline/unmounted/SAF), we can't attribute id-dirs, so none are removed.
///   - Legacy dirs are always safe: `live` always includes every registered
///     roost's path-hash (computed from its path, no id needed), so a legacy dir
///     not in `live` matches no registered roost.
fn is_orphaned_index_dir(
    path: &Path,
    name: &str,
    live: &HashSet<String>,
    all_ids_resolved: bool,
) -> bool {
    // MR-B3: fail closed. Only `roost_db_dir` creates this marker, so a user
    // content directory can never be deleted merely because its name happens
    // to match an index naming convention. Pre-marker indexes are retained and
    // become eligible after the next normal open stamps them.
    if !is_marked_index_dir(path) {
        return false;
    }
    if live.contains(name) {
        return false;
    }
    if is_id_dir_name(name) {
        return all_ids_resolved;
    }
    if is_legacy_hash_name(name) {
        return true;
    }
    false
}

/// Extract the roost `path` strings from the on-disk manifest JSON
/// (`roosts.json`, a `RoostEntry[]` blob written by the frontend). Returns
/// `None` if the JSON isn't the expected array shape (so the caller can skip the
/// sweep rather than treat "unparseable" as "no roosts").
fn manifest_paths(json: &str) -> Option<Vec<String>> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let arr = v.as_array()?;
    Some(
        arr.iter()
            .filter_map(|e| e.get("path").and_then(|p| p.as_str()).map(str::to_string))
            .collect(),
    )
}

/// MR-B3: at startup, remove orphaned per-roost index dirs under
/// `app_data_dir/vaults/` — dirs whose roost is no longer registered in the
/// manifest. Removing a roost from the registry without the explicit "delete
/// index" action used to orphan its multi-MB `id-…`/legacy index dir forever.
///
/// Safety: the registered set comes from `roosts.json` (the durable manifest
/// mirror). If the manifest is missing/unparseable OR lists zero roosts, the
/// sweep is SKIPPED entirely — we never treat "we don't know the registered
/// set" as "everything is orphaned". Only positively-identified index dirs are
/// removed (see `is_orphaned_index_dir`); user content dirs and unknown dirs are
/// left untouched. Best-effort: per-dir errors are logged, never fatal.
pub fn gc_orphaned_index_dirs(app: &AppHandle) -> IndexGcReport {
    let mut report = IndexGcReport::default();
    let data = match app.path().app_data_dir() {
        Ok(d) => d,
        Err(e) => {
            report.skipped = Some(format!("app_data_dir unavailable: {e}"));
            return report;
        }
    };

    // Registered set from the manifest. Missing / unparseable / empty → skip.
    let manifest = data.join("roosts.json");
    let registered: Vec<String> = match std::fs::read_to_string(&manifest) {
        Ok(s) => match manifest_paths(&s) {
            Some(paths) if !paths.is_empty() => paths,
            Some(_) => {
                report.skipped = Some("manifest lists zero roosts".to_string());
                return report;
            }
            None => {
                report.skipped = Some("manifest not in expected array shape".to_string());
                return report;
            }
        },
        Err(_) => {
            report.skipped = Some("no roosts.json manifest".to_string());
            return report;
        }
    };

    // Build the live set: every registered roost's legacy path-hash name
    // (always computable) plus its stable id-dir name (when resolvable).
    let mut live: HashSet<String> = HashSet::new();
    let mut all_ids_resolved = true;
    for path in &registered {
        live.insert(format!("{:016x}", stable_hash(&normalize_key(path))));
        match stable_roost_dir_name(path) {
            Some(name) => {
                live.insert(name);
            }
            None => all_ids_resolved = false,
        }
    }

    let vaults = data.join("vaults");
    let read_dir = match std::fs::read_dir(&vaults) {
        Ok(r) => r,
        // No vaults/ dir yet (fresh install) → nothing to GC.
        Err(_) => return report,
    };
    for entry in read_dir.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !entry.path().is_dir() {
            continue;
        }
        if !is_orphaned_index_dir(&entry.path(), &name, &live, all_ids_resolved) {
            continue;
        }
        match std::fs::remove_dir_all(entry.path()) {
            Ok(()) => {
                tracing::info!(dir = %name, "paths::gc: removed orphaned index dir");
                report.removed.push(name);
            }
            Err(e) => report
                .errors
                .push(format!("remove {}: {e}", entry.path().display())),
        }
    }
    report
}

// ── Legacy migrations ───────────────────────────────────────────────

/// Summary of what `run_legacy_migrations` did. Surfaced through
/// `tracing::info!` so users' diagnostic exports show whether a
/// migration ran on their machine (useful for bug reports).
#[derive(Debug, Default)]
pub struct MigrationReport {
    pub models_moved: usize,
    pub crash_logs_moved: usize,
    pub secrets_moved: usize,
    pub errors: Vec<String>,
}

/// AUDIT-043 / 043b / 043c: move files from the three legacy "rogue"
/// data dirs into the canonical `app_data_dir`. Eager-at-boot —
/// called from the tauri `setup` callback after `initialize`. Safe
/// to call multiple times: legacy files are gone after the first
/// successful move, so subsequent calls find nothing to migrate.
///
/// Per-file `rename` is atomic on the same volume. Across volumes
/// we fall back to copy-then-delete; the delete only runs after the
/// copy verifies size, so a power-loss in the middle leaves the
/// source intact and the next launch retries.
///
/// If the destination already has a file with the same name (rare —
/// would mean the user downloaded a fresh model into the new path
/// then a downgrade/upgrade dance left both copies on disk), we
/// LEAVE the source alone and log a warning rather than risk
/// destroying user data.
pub fn run_legacy_migrations() -> MigrationReport {
    let mut report = MigrationReport::default();
    let Some(new_root) = app_data_dir() else {
        report
            .errors
            .push("paths::initialize must run before migrations — skipping".to_string());
        return report;
    };

    // 1. AI models: ProjectDirs("com","Corvux","Corvux")/data_dir/models/
    //    → app_data_dir/models/
    if let Some(legacy_models) = legacy_ai_models_dir() {
        if legacy_models.exists() {
            let new_models = new_root.join("models");
            if let Err(e) = std::fs::create_dir_all(&new_models) {
                report
                    .errors
                    .push(format!("mkdir {}: {e}", new_models.display()));
            } else {
                report.models_moved += move_dir_contents(&legacy_models, &new_models, &mut report);
                // Best-effort: remove the legacy dir if empty after the
                // move. If it still has files we don't recognize, leave
                // them — better a stale dir than data loss.
                let _ = std::fs::remove_dir(&legacy_models);
            }
        }
    }

    // 2. crash.log + rotated copies: BaseDirs::data_dir().join("corvux")/
    //    → app_data_dir/
    if let Some(legacy_crash_dir) = legacy_crash_log_dir() {
        if legacy_crash_dir.exists() {
            for name in CRASH_LOG_FILES {
                let from = legacy_crash_dir.join(name);
                if !from.exists() {
                    continue;
                }
                let to = new_root.join(name);
                match move_file_safely(&from, &to) {
                    Ok(true) => report.crash_logs_moved += 1,
                    Ok(false) => {} // destination already populated, source preserved
                    Err(e) => report.errors.push(e),
                }
            }
            // Don't remove the legacy crash dir blindly — it may still
            // hold session markers or other state we haven't migrated
            // yet. Leave it; the absence of crash.log there is enough
            // to prevent re-migration.
        }
    }

    // 3. Mobile secret store: ProjectDirs("com","corvuxmindware","corvux")/
    //    data_dir/.corvux/ → app_data_dir/secrets/
    //    (Mobile-only paths; on desktop the legacy path is unrelated to
    //    secret storage and we leave it alone.)
    #[cfg(any(target_os = "android", target_os = "ios"))]
    if let Some(legacy_secrets) = legacy_mobile_secret_dir() {
        let from_dir = legacy_secrets.join(".corvux");
        if from_dir.exists() {
            let to_dir = new_root.join("secrets").join(".corvux");
            if let Err(e) = std::fs::create_dir_all(&to_dir) {
                report
                    .errors
                    .push(format!("mkdir {}: {e}", to_dir.display()));
            } else {
                for name in MOBILE_SECRET_FILES {
                    let from = from_dir.join(name);
                    if !from.exists() {
                        continue;
                    }
                    let to = to_dir.join(name);
                    match move_file_safely(&from, &to) {
                        Ok(true) => report.secrets_moved += 1,
                        Ok(false) => {}
                        Err(e) => report.errors.push(e),
                    }
                }
            }
        }
    }

    report
}

/// Files we know about in the legacy crash-log dir. Anything else gets
/// left alone — we don't want to move unrelated files (e.g. a future
/// `corvux/` install puts state there for some other reason).
const CRASH_LOG_FILES: &[&str] = &["crash.log", "crash.log.1", "crash.log.2", "crash.log.3"];

#[allow(dead_code)] // referenced only inside #[cfg(mobile)] block above
const MOBILE_SECRET_FILES: &[&str] = &[".enc-seed", "encrypted-secrets.json"];

fn legacy_ai_models_dir() -> Option<PathBuf> {
    directories::ProjectDirs::from("com", "Corvux", "Corvux").map(|p| p.data_dir().join("models"))
}

fn legacy_crash_log_dir() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|d| d.data_dir().join("corvux"))
}

#[cfg(any(target_os = "android", target_os = "ios"))]
fn legacy_mobile_secret_dir() -> Option<PathBuf> {
    directories::ProjectDirs::from("com", "corvuxmindware", "corvux")
        .map(|p| p.data_dir().to_path_buf())
}

/// Move every direct child of `from` into `to`. Returns the count
/// moved. Errors per-file get pushed into the report but don't abort
/// the whole migration — a partial migration is still progress.
fn move_dir_contents(from: &Path, to: &Path, report: &mut MigrationReport) -> usize {
    let entries = match std::fs::read_dir(from) {
        Ok(e) => e,
        Err(e) => {
            report
                .errors
                .push(format!("read_dir {}: {e}", from.display()));
            return 0;
        }
    };
    let mut moved = 0_usize;
    for entry in entries.flatten() {
        let src = entry.path();
        let name = entry.file_name();
        let dst = to.join(&name);
        if src.is_dir() {
            // Recurse for per-model subdirectories (e.g. `nomic-...`).
            if let Err(e) = std::fs::create_dir_all(&dst) {
                report.errors.push(format!("mkdir {}: {e}", dst.display()));
                continue;
            }
            moved += move_dir_contents(&src, &dst, report);
            let _ = std::fs::remove_dir(&src); // best-effort cleanup of now-empty source
        } else {
            match move_file_safely(&src, &dst) {
                Ok(true) => moved += 1,
                Ok(false) => {} // skipped — destination already populated
                Err(e) => report.errors.push(e),
            }
        }
    }
    moved
}

/// Move one file from `from` to `to`. Returns `Ok(true)` on a real
/// move, `Ok(false)` if the destination already exists (we don't
/// overwrite), and `Err` for I/O failure.
///
/// Uses `rename` for the same-volume fast path (atomic). Falls back
/// to copy-then-delete when `rename` fails with `EXDEV` (cross-device
/// link) — the delete only runs after the copy succeeds AND the
/// destination size matches the source, so a partial copy never
/// loses the source.
fn move_file_safely(from: &Path, to: &Path) -> Result<bool, String> {
    if to.exists() {
        tracing::warn!(
            from = %from.display(),
            to = %to.display(),
            "paths::migrate: destination already exists — leaving source intact"
        );
        return Ok(false);
    }
    if let Err(e) = std::fs::rename(from, to) {
        // Any rename failure falls through to copy+verify+delete. The
        // most common cause is cross-volume (EXDEV on Unix; a different
        // error code on Windows) — `rename` can't span volumes on most
        // filesystems. Logging at debug level so a typical migration
        // doesn't spam warn-level logs.
        tracing::debug!(
            from = %from.display(),
            to = %to.display(),
            error = %e,
            "paths::migrate: rename failed, falling back to copy+verify+delete"
        );
        let src_size = std::fs::metadata(from)
            .map(|m| m.len())
            .map_err(|e| format!("stat {}: {e}", from.display()))?;
        std::fs::copy(from, to)
            .map_err(|e| format!("copy {} → {}: {e}", from.display(), to.display()))?;
        let dst_size = std::fs::metadata(to)
            .map(|m| m.len())
            .map_err(|e| format!("stat {}: {e}", to.display()))?;
        if src_size != dst_size {
            // Don't delete source — destination is corrupt.
            let _ = std::fs::remove_file(to);
            return Err(format!(
                "size mismatch after copy of {}: src={} dst={}",
                from.display(),
                src_size,
                dst_size
            ));
        }
        std::fs::remove_file(from).map_err(|e| format!("remove source {}: {e}", from.display()))?;
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_is_slash_insensitive_everywhere() {
        // Backslash/forward-slash always fold, on every platform.
        assert_eq!(normalize_key("a\\b\\c"), normalize_key("a/b/c"));
    }

    #[test]
    fn normalize_case_folding_is_platform_gated() {
        // NEW-M1: case-folding only on case-insensitive filesystems.
        let foo = normalize_key("/home/me/Foo");
        let lower = normalize_key("/home/me/foo");
        if cfg!(any(target_os = "windows", target_os = "macos")) {
            assert_eq!(foo, lower, "Windows/macOS must case-fold");
        } else {
            assert_ne!(
                foo, lower,
                "Linux is case-sensitive — distinct-case roosts must NOT collide \
                 (else the legacy→id migration steals one roost's index)"
            );
        }
    }

    #[test]
    fn stable_hash_is_deterministic() {
        assert_eq!(stable_hash("hello"), stable_hash("hello"));
        assert_ne!(stable_hash("a"), stable_hash("b"));
    }

    // ── MR-B2: stable, relocation-proof roost dir keying ──────────

    #[test]
    fn hex32_encodes_16_bytes_lowercase() {
        assert_eq!(hex32(&[0u8; 16]), "0".repeat(32));
        let mut b = [0u8; 16];
        b[0] = 0xab;
        b[15] = 0x0f;
        let s = hex32(&b);
        assert_eq!(s.len(), 32);
        assert!(s.starts_with("ab") && s.ends_with("0f"), "got {s}");
    }

    #[test]
    fn stable_roost_dir_name_is_prefixed_and_stable() {
        // A real roost dir yields a stable, `id-`-prefixed name derived from
        // its vault-id.bin — so the index dir survives a folder move.
        let dir = tempfile::TempDir::new().unwrap();
        let key = dir.path().display().to_string();
        let a = stable_roost_dir_name(&key).expect("real dir yields an id");
        let b = stable_roost_dir_name(&key).expect("id is stable across calls");
        assert_eq!(a, b);
        assert!(a.starts_with("id-"), "prefixed: {a}");
        assert_eq!(a.len(), "id-".len() + 32, "id- + 32 hex chars");
    }

    #[test]
    fn stable_roost_dir_name_differs_per_roost() {
        let a = tempfile::TempDir::new().unwrap();
        let b = tempfile::TempDir::new().unwrap();
        assert_ne!(
            stable_roost_dir_name(&a.path().display().to_string()),
            stable_roost_dir_name(&b.path().display().to_string()),
        );
    }

    // ── MR-B3: orphaned index-dir GC classification ───────────────

    #[test]
    fn gc_recognizes_id_and_legacy_dir_shapes() {
        assert!(is_id_dir_name(&format!("id-{}", "a".repeat(32))));
        assert!(!is_id_dir_name(&format!("id-{}", "a".repeat(31)))); // wrong len
        assert!(!is_id_dir_name(&format!("id-{}", "g".repeat(32)))); // non-hex
        assert!(!is_id_dir_name("id-short"));
        assert!(!is_id_dir_name("My Notes"));

        assert!(is_legacy_hash_name(&"0".repeat(16)));
        assert!(is_legacy_hash_name("0123456789abcdef"));
        assert!(!is_legacy_hash_name("0123456789abcde")); // 15 chars
        assert!(!is_legacy_hash_name("My Notes")); // slug, not hex
    }

    #[test]
    fn gc_keeps_live_and_content_dirs_removes_orphans() {
        let root = tempfile::tempdir().unwrap();
        let marked = |name: &str| {
            let path = root.path().join(name);
            std::fs::create_dir_all(&path).unwrap();
            std::fs::write(path.join(INDEX_DIR_MARKER), b"marker").unwrap();
            path
        };
        let id_live = format!("id-{}", "a".repeat(32));
        let id_orphan = format!("id-{}", "b".repeat(32));
        let legacy_live = "0".repeat(16);
        let legacy_orphan = "1".repeat(16);
        let mut live = HashSet::new();
        live.insert(id_live.clone());
        live.insert(legacy_live.clone());

        // Live index dirs are kept regardless of resolution state.
        assert!(!is_orphaned_index_dir(
            &marked(&id_live),
            &id_live,
            &live,
            true
        ));
        assert!(!is_orphaned_index_dir(
            &marked(&legacy_live),
            &legacy_live,
            &live,
            true
        ));

        // Content is NEVER a candidate, including the exact shape of both
        // index naming schemes. Directory names alone cannot authorize delete.
        let human = root.path().join("My Notes");
        let hex_content = root.path().join(&legacy_orphan);
        let id_content = root.path().join(&id_orphan);
        std::fs::create_dir_all(&human).unwrap();
        std::fs::create_dir_all(&hex_content).unwrap();
        std::fs::create_dir_all(&id_content).unwrap();
        assert!(!is_orphaned_index_dir(&human, "My Notes", &live, true));
        assert!(!is_orphaned_index_dir(
            &hex_content,
            &legacy_orphan,
            &live,
            true
        ));
        assert!(!is_orphaned_index_dir(&id_content, &id_orphan, &live, true));

        // Orphan id-dir: removable only when all ids resolved.
        let marked_id_orphan = marked(&id_orphan);
        assert!(is_orphaned_index_dir(
            &marked_id_orphan,
            &id_orphan,
            &live,
            true
        ));
        assert!(!is_orphaned_index_dir(
            &marked_id_orphan,
            &id_orphan,
            &live,
            false
        ));

        // Orphan legacy dir: always removable (path-hash is in `live` for every
        // registered roost, so a miss means it matches none of them).
        let marked_legacy_orphan = marked(&legacy_orphan);
        assert!(is_orphaned_index_dir(
            &marked_legacy_orphan,
            &legacy_orphan,
            &live,
            true
        ));
        assert!(is_orphaned_index_dir(
            &marked_legacy_orphan,
            &legacy_orphan,
            &live,
            false
        ));
    }

    #[test]
    fn gc_manifest_paths_parses_entries_and_rejects_non_arrays() {
        let json =
            r#"[{"id":"v1","path":"/a/roost","displayName":"A"},{"id":"v2","path":"/b/roost"}]"#;
        assert_eq!(
            manifest_paths(json),
            Some(vec!["/a/roost".to_string(), "/b/roost".to_string()])
        );
        // Empty array parses to an empty vec (caller treats as "skip").
        assert_eq!(manifest_paths("[]"), Some(vec![]));
        // Non-array shapes → None (caller skips rather than assuming zero roosts).
        assert_eq!(manifest_paths("{}"), None);
        assert_eq!(manifest_paths("not json"), None);
    }

    // ── AUDIT-043 migration tests ─────────────────────────────────
    //
    // These exercise `move_file_safely` and `move_dir_contents`
    // directly so we don't need a Tauri AppHandle. The legacy-path
    // lookups (`legacy_ai_models_dir` etc.) are not unit-tested
    // because their return values depend on the host OS's data-dir
    // layout, which is exactly what we're decoupling from.

    use tempfile::TempDir;

    #[test]
    fn move_file_safely_renames_when_destination_is_empty() {
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("src.bin");
        let dst = dir.path().join("dst.bin");
        std::fs::write(&src, b"payload").unwrap();
        let moved = move_file_safely(&src, &dst).unwrap();
        assert!(moved);
        assert!(!src.exists(), "source should be gone");
        assert_eq!(std::fs::read(&dst).unwrap(), b"payload");
    }

    #[test]
    fn move_file_safely_preserves_source_when_destination_exists() {
        // AUDIT-043: explicit invariant — the migration must never
        // overwrite a pre-existing destination file. The user could
        // have downloaded a fresh copy at the new path; we'd destroy
        // it if we blindly renamed over.
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("src.bin");
        let dst = dir.path().join("dst.bin");
        std::fs::write(&src, b"old").unwrap();
        std::fs::write(&dst, b"new").unwrap();
        let moved = move_file_safely(&src, &dst).unwrap();
        assert!(!moved, "should report skipped");
        assert_eq!(std::fs::read(&src).unwrap(), b"old", "source preserved");
        assert_eq!(
            std::fs::read(&dst).unwrap(),
            b"new",
            "destination preserved"
        );
    }

    #[test]
    fn move_dir_contents_recurses_into_subdirs() {
        // The model layout is `models/<model-name>/{model.onnx,
        // tokenizer.json}` — the migration must recurse one level
        // into the per-model subdir, not just the top-level files.
        let root = TempDir::new().unwrap();
        let from = root.path().join("legacy");
        let to = root.path().join("new");
        std::fs::create_dir_all(from.join("nomic-embed-text-v1.5")).unwrap();
        std::fs::write(
            from.join("nomic-embed-text-v1.5").join("model.onnx"),
            b"onnx-bytes",
        )
        .unwrap();
        std::fs::write(
            from.join("nomic-embed-text-v1.5").join("tokenizer.json"),
            b"tokenizer-bytes",
        )
        .unwrap();
        std::fs::create_dir_all(&to).unwrap();

        let mut report = MigrationReport::default();
        let moved = move_dir_contents(&from, &to, &mut report);
        assert_eq!(moved, 2, "model.onnx + tokenizer.json");
        assert!(report.errors.is_empty(), "no errors: {:?}", report.errors);
        assert!(to.join("nomic-embed-text-v1.5").join("model.onnx").exists());
        assert!(to
            .join("nomic-embed-text-v1.5")
            .join("tokenizer.json")
            .exists());
        // Source files are gone; the per-model subdir is gone (best-effort
        // cleanup); the legacy `models/` parent stays — the migration
        // caller (`run_legacy_migrations`) handles top-level cleanup.
        assert!(!from.join("nomic-embed-text-v1.5").exists());
    }

    #[test]
    fn move_dir_contents_idempotent_on_second_call() {
        // First call moves everything. Second call finds the source
        // empty and is a no-op — what makes the migration safe to
        // run on every boot without a marker file.
        let root = TempDir::new().unwrap();
        let from = root.path().join("legacy");
        let to = root.path().join("new");
        std::fs::create_dir_all(&from).unwrap();
        std::fs::write(from.join("file.bin"), b"x").unwrap();
        std::fs::create_dir_all(&to).unwrap();

        let mut r1 = MigrationReport::default();
        assert_eq!(move_dir_contents(&from, &to, &mut r1), 1);
        let mut r2 = MigrationReport::default();
        assert_eq!(move_dir_contents(&from, &to, &mut r2), 0, "no-op");
        assert!(r2.errors.is_empty());
    }

    #[test]
    fn move_dir_contents_partial_with_existing_destination() {
        // Realistic upgrade scenario: user had the model at the old
        // path AND somehow ended up with a stale copy at the new path
        // (downgrade/upgrade dance). Migration must move only what
        // isn't already there.
        let root = TempDir::new().unwrap();
        let from = root.path().join("legacy");
        let to = root.path().join("new");
        std::fs::create_dir_all(&from).unwrap();
        std::fs::create_dir_all(&to).unwrap();
        std::fs::write(from.join("a.bin"), b"a-from").unwrap();
        std::fs::write(from.join("b.bin"), b"b-from").unwrap();
        std::fs::write(to.join("a.bin"), b"a-to-pre-existing").unwrap();

        let mut report = MigrationReport::default();
        let moved = move_dir_contents(&from, &to, &mut report);
        assert_eq!(moved, 1, "only b.bin moves; a.bin preserved at both ends");
        assert_eq!(std::fs::read(from.join("a.bin")).unwrap(), b"a-from");
        assert_eq!(
            std::fs::read(to.join("a.bin")).unwrap(),
            b"a-to-pre-existing"
        );
        assert!(!from.join("b.bin").exists());
        assert_eq!(std::fs::read(to.join("b.bin")).unwrap(), b"b-from");
    }
}
