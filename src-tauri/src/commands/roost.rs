//! Roost lifecycle commands — open, init default, create named.
//!
//! Kept separate from per-file CRUD in `files.rs` so the "set up a roost"
//! and "mutate files within a roost" concerns can evolve independently.
//! Everything here returns either a `FileEntry` tree (open) or an absolute
//! path (init/create) — nothing mutates a file in an already-open roost.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tauri::{Manager, State};

use crate::roostfs::native::NativeFs;
use crate::roostfs::RoostFs;

use super::files::{FileEntry, RoostFsState};

/// Open the roost at `path` and return its markdown tree.
///
/// `path` branches on prefix: `content://` tree URIs route to the Android
/// SAF backend; everything else is treated as a filesystem path and uses
/// [`NativeFs`]. `display_name` is only consulted for SAF roosts (where
/// the URI's last segment is percent-encoded and unreadable).
/// Outcome of a successful roost open. The frontend uses `bookmark` to
/// persist a possibly-refreshed scoped-access blob (iOS bookmarks can go
/// stale; the plugin re-mints them on resolve). All other consumers of
/// the historical `Vec<FileEntry>` shape continue to ignore it.
#[derive(serde::Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct OpenRoostResponse {
    pub entries: Vec<FileEntry>,
    /// Backend-owned token identifying the active-roost installation that
    /// produced these entries. Async consumers must reject older tokens.
    pub generation: u32,
    /// Refreshed bookmark blob the caller should persist, when applicable.
    /// `None` for roosts that don't use scoped access (Android SAF,
    /// desktop, iOS app-private roosts).
    pub bookmark: Option<String>,
    /// Refreshed absolute path the caller should persist. iOS bookmarks
    /// can relocate (user moved the folder via Files.app); we hand back
    /// the freshly-resolved path so the registry stays in sync.
    pub resolved_path: Option<String>,
}

/// Stable error token returned by `open_roost` when an iOS security-scoped
/// bookmark could not be resumed (stale after an app update, iCloud change,
/// revoked grant, etc.). The frontend matches this to prompt a re-pick
/// ("re-grant access") instead of surfacing a raw EPERM. Keep in sync with
/// `isScopeExpiredError` in the TS layer.
const SCOPE_EXPIRED: &str = "SCOPE_EXPIRED";

/// Outcome of attempting to (re)acquire security-scoped access before any
/// `std::fs` work.
#[allow(dead_code)] // `Expired` is only constructed under #[cfg(ios)].
enum ScopeResume {
    /// Proceed with this path + (possibly refreshed) bookmark.
    Ready {
        path: String,
        bookmark: Option<String>,
    },
    /// A bookmark was supplied but iOS refused to resume access — the folder
    /// must be re-picked to mint a fresh bookmark. We deliberately do NOT
    /// fall back to stat'ing the raw path (it always EPERMs for a scoped
    /// folder, surfacing a confusing error instead of an actionable one).
    Expired,
}

#[tauri::command]
#[specta::specta]
pub async fn open_roost(
    path: String,
    display_name: Option<String>,
    bookmark: Option<String>,
    #[allow(unused_variables)] app: tauri::AppHandle,
    state: State<'_, RoostFsState>,
    perf_state: State<'_, crate::perf::PerfCountersState>,
) -> Result<OpenRoostResponse, String> {
    let started = std::time::Instant::now();
    tracing::info!(target_path = %path, has_bookmark = bookmark.is_some(), "roost_open: start");

    // iOS scoped-access resume. If the caller passed a bookmark, hand it
    // to the iOS picker plugin to re-acquire scope BEFORE we touch the
    // path with `std::fs`. The plugin returns a (possibly refreshed)
    // bookmark + the resolved path; we use those downstream.
    let (effective_path, refreshed_bookmark) =
        match resume_scope_if_needed(&app, &path, bookmark).await {
            ScopeResume::Ready {
                path: p,
                bookmark: bm,
            } => (p, bm),
            ScopeResume::Expired => {
                let elapsed_ms = started.elapsed().as_millis() as u64;
                tracing::warn!(
                    target_path = %path, elapsed_ms,
                    "roost_open: scoped bookmark expired — re-pick required"
                );
                return Err(SCOPE_EXPIRED.to_string());
            }
        };

    // Serialize the complete transition, including scoped-access resolution
    // and candidate validation. Later user requests therefore commit later.
    let _transition = state.transition.lock().await;
    let result = open_roost_impl(effective_path.clone(), display_name, app, &state).await;
    let elapsed_ms = started.elapsed().as_millis() as u64;
    match &result {
        Ok(entries) => {
            tracing::info!(entries = entries.len(), elapsed_ms, "roost_open: done");
            perf_state.bump_roost_open();
        }
        Err(e) => tracing::warn!(error = %e, elapsed_ms, "roost_open: failed"),
    }
    let path_changed = effective_path != path;
    result.map(|entries| OpenRoostResponse {
        entries,
        generation: state.generation() as u32,
        bookmark: refreshed_bookmark,
        resolved_path: if path_changed {
            Some(effective_path)
        } else {
            None
        },
    })
}

/// On iOS, if we have a persisted bookmark for this roost, resume
/// security-scoped access via the picker plugin before doing any
/// `std::fs` work. Returns `Ready` with the (possibly refreshed) path +
/// bookmark to persist, or `Expired` if the bookmark could not be resumed
/// (caller turns that into `SCOPE_EXPIRED` so the UI prompts a re-pick).
///
/// On non-iOS platforms or when no bookmark is supplied this is a straight
/// pass-through (always `Ready`).
#[allow(unused_variables)]
async fn resume_scope_if_needed(
    app: &tauri::AppHandle,
    path: &str,
    bookmark: Option<String>,
) -> ScopeResume {
    #[cfg(target_os = "ios")]
    {
        use tauri_plugin_ios_picker::IosPickerExt;
        if let Some(bm) = bookmark.as_deref() {
            // run_mobile_plugin is sync; offload to blocking to be safe.
            let app_clone = app.clone();
            let bm_owned = bm.to_string();
            let result = tokio::task::spawn_blocking(move || {
                app_clone.ios_picker().resume_access(&bm_owned)
            })
            .await;
            match result {
                Ok(Ok(resumed)) => {
                    tracing::info!(
                        path = %resumed.path,
                        stale = resumed.stale,
                        "roost_open: scoped access resumed"
                    );
                    return ScopeResume::Ready {
                        path: resumed.path,
                        bookmark: Some(resumed.bookmark),
                    };
                }
                Ok(Err(e)) => {
                    // A bookmark was supplied but the OS refused to resume.
                    // Signal Expired so open_roost returns SCOPE_EXPIRED and
                    // the UI prompts a re-pick — rather than stat'ing the raw
                    // path, which always EPERMs for a scoped folder.
                    tracing::warn!(error = %e, "roost_open: resume_access failed — re-pick required");
                    return ScopeResume::Expired;
                }
                Err(e) => {
                    tracing::warn!(error = %e, "roost_open: resume_access task panicked — re-pick required");
                    return ScopeResume::Expired;
                }
            }
        }
    }
    ScopeResume::Ready {
        path: path.to_string(),
        bookmark,
    }
}

async fn open_roost_impl(
    path: String,
    display_name: Option<String>,
    #[allow(unused_variables)] app: tauri::AppHandle,
    state: &State<'_, RoostFsState>,
) -> Result<Vec<FileEntry>, String> {
    if path.starts_with("content://") {
        #[cfg(target_os = "android")]
        {
            // A different-roost switch must tear sync down FIRST (invariant 3),
            // exactly like the native path below does via
            // `stop_sync_for_roost_switch`. The SAF branch previously swapped the
            // backend without it, so the sync engine kept its Arc-clone of the
            // OLD backend and a stale init could complete against the new roost.
            // Guard on a REAL root change: `open_roost` is called constantly for
            // same-roost refreshes (focus, note_written round-trips), and tearing
            // sync down on each of those would kill sync on every refresh.
            let existing = state.current().await;
            let needs_install = existing
                .as_ref()
                .map(|fs| !same_roost_root(fs.root_id().as_str(), std::path::Path::new(&path)))
                .unwrap_or(true);

            let display = display_name.unwrap_or_else(|| path.clone());
            let backend: Arc<dyn RoostFs> = Arc::new(crate::roostfs::saf::SafFs::new(
                &path,
                &display,
                app.clone(),
            ));
            // Phase B: warm-cache fast path. Even after the Phase A
            // ContentResolver speedup, a cold SAF walk on this device
            // costs ~500ms-1.6s for 213 entries. Reading the JSON
            // cache returns in milliseconds. Background revalidation
            // emits `roost://tree-changed` if the on-disk tree drifts
            // from the cache (e.g., user edited via another app).
            let cached = super::roost_tree_cache::read(&app, &path);
            let entries = if let Some(cached) = cached.as_ref() {
                // A disk cache proves what the tree looked like previously,
                // not that the provider grant is still usable. Validate one
                // root listing before committing the backend; the expensive
                // recursive reconciliation remains in the background.
                backend
                    .list("")
                    .await
                    .map_err(|e| format!("SAF roost is no longer accessible: {e}"))?;
                cached.clone()
            } else {
                let entries = scan_roost_backend(backend.as_ref(), "", &path).await?;
                super::roost_tree_cache::write(&app, &path, &entries);
                entries
            };

            // Validate/scan the candidate before changing global state. A SAF
            // provider failure therefore leaves the previous roost installed.
            let generation = if needs_install {
                install_roost_backend(&app, state, backend.clone()).await
            } else {
                state.generation()
            };

            // notify-based hot reload cannot observe Android SAF tree URIs.
            // Populate both registries through the mounted backend on open;
            // Settings' manual refresh uses the same path.
            if let Err(e) =
                crate::commands::ai::skills::refresh_registry_from_backend(&app, backend.as_ref())
                    .await
            {
                tracing::warn!(error = %e, "skill: SAF refresh failed");
            }
            if let Err(e) = crate::commands::ai::subagents::refresh_registry_from_backend(
                &app,
                backend.as_ref(),
            )
            .await
            {
                tracing::warn!(error = %e, "subagent: SAF refresh failed");
            }

            if let Some(cached) = cached {
                let app_for_bg = app.clone();
                let path_for_bg = path.clone();
                let backend_for_bg = backend.clone();
                tauri::async_runtime::spawn(async move {
                    super::roost_tree_cache::revalidate(
                        app_for_bg,
                        path_for_bg.clone(),
                        generation as u32,
                        backend_for_bg,
                        path_for_bg,
                        cached,
                    )
                    .await;
                });
            }
            return Ok(entries);
        }
        #[cfg(not(target_os = "android"))]
        {
            let _ = display_name;
            return Err("SAF roosts require Android".to_string());
        }
    }

    let dir = PathBuf::from(&path);
    if !dir.is_dir() {
        let meta = std::fs::symlink_metadata(&dir);
        let detail = match meta {
            Ok(m) if m.file_type().is_file() => "exists as a file, not a directory",
            Ok(m) if m.file_type().is_symlink() => "is a symlink",
            Ok(_) => "exists but is not a directory",
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => "does not exist",
            Err(e) => return Err(format!("stat failed for {}: {}", path, e)),
        };
        return Err(format!("Roost path {}: {}", detail, path));
    }
    // Best-effort: ensure a Syncthing ignore file exists so device-local
    // DB/index files under `.corvux/` never propagate via folder sync.
    // Silent on failure — a read-only roost is still openable.
    let _ = ensure_stignore(&dir);

    // Same-roost re-open is a no-op for locking: every focus event and every
    // sync `note_written` triggers a `refreshFiles` → `open_roost` round-trip.
    // The original "drop the Arc, then re-acquire" sequence races any
    // in-flight file command holding a clone of the Arc — when the clone
    // outlives the swap, the lock isn't actually released and the second
    // acquire fails with AlreadyLocked *against ourselves*. Just rescan and
    // return; the existing backend stays mounted.
    if let Some(existing) = state.current().await {
        if same_roost_root(existing.root_id().as_str(), &dir) {
            return open_native_with_cache(&app, dir, &path, state.generation() as u32).await;
        }
    }

    // Acquire the roost-level OS lock. Stops a second Corvux instance from
    // racing writes. This happens while the previous backend is still live, so
    // failure is transactional and leaves the old roost usable.
    let native = NativeFs::open_locked(&dir).map_err(|e| e.to_string())?;
    let backend: Arc<dyn RoostFs> = Arc::new(native);
    let future_generation = (state.generation() + 1) as u32;
    // Likewise, validate the tree before committing the switch. Cache hits may
    // revalidate in the background; their event is generation-scoped.
    let prepared = prepare_native_tree(&app, dir.clone(), &path).await?;
    install_roost_backend(&app, state, backend).await;
    if let Some(cached) = prepared.cached_for_revalidation {
        spawn_native_tree_revalidation(
            app.clone(),
            dir.clone(),
            path.clone(),
            future_generation,
            cached,
        );
    }

    // Rebuild the skill + subagent registry watchers over the new
    // roost's .corvux/{skills,agents}/ dirs. Cheap (~5 µs each to
    // spawn the debouncer threads), best-effort: failures are logged
    // but don't block roost open.
    rebind_skill_watcher(&app, &dir);
    rebind_subagent_watcher(&app, &dir);
    // Keep the graph/search index live for external edits (MCP, Obsidian, vim)
    // even when sync is off — this watcher is independent of the sync engine.
    crate::commands::index_watcher::rebind_index_watcher(&app, &dir);

    Ok(prepared.entries)
}

/// Re-attach the skill hot-reload watcher to the newly-opened roost.
/// Pulled into a helper so the impl can stay readable + so roost-open
/// stays unchanged from the dispatcher's perspective.
fn rebind_skill_watcher(app: &tauri::AppHandle, roost_dir: &Path) {
    use crate::commands::ai::skills::{
        rebind_watcher_for_roost, SkillRegistryState, SkillWatcherState,
    };
    let Some(registry_state) = app.try_state::<SkillRegistryState>() else {
        return; // skills feature not initialized (mobile, tests)
    };
    let Some(watcher_state) = app.try_state::<SkillWatcherState>() else {
        return;
    };
    rebind_watcher_for_roost(registry_state.0.clone(), &watcher_state, Some(roost_dir));
}

/// Mirror of `rebind_skill_watcher` for the subagent registry.
/// Identical structure — kept as two separate helpers so each
/// registry can evolve independently (different reload events,
/// different telemetry, etc.).
fn rebind_subagent_watcher(app: &tauri::AppHandle, roost_dir: &Path) {
    use crate::commands::ai::subagents::{
        rebind_watcher_for_roost, SubagentRegistryState, SubagentWatcherState,
    };
    let Some(registry_state) = app.try_state::<SubagentRegistryState>() else {
        return;
    };
    let Some(watcher_state) = app.try_state::<SubagentWatcherState>() else {
        return;
    };
    rebind_watcher_for_roost(registry_state.0.clone(), &watcher_state, Some(roost_dir));
}

/// Atomically install a prepared backend while excluding sync init/commit.
/// Lock order is the shared lifecycle order: `init → lifecycle → backend`.
async fn install_roost_backend(
    app: &tauri::AppHandle,
    state: &State<'_, RoostFsState>,
    backend: Arc<dyn RoostFs>,
) -> u64 {
    if let Some(sync_state) = app.try_state::<crate::commands::sync::SyncEngineState>() {
        let _init = sync_state.init.lock().await;
        let _lifecycle = sync_state.lifecycle.lock().await;
        stop_sync_for_roost_switch(app, &sync_state).await;
        crate::roostfs::walk::invalidate_walk_cache();
        clear_backend_scoped_state(app);
        clear_roost_watchers(app);
        state.install(backend).await
    } else {
        crate::roostfs::walk::invalidate_walk_cache();
        clear_backend_scoped_state(app);
        clear_roost_watchers(app);
        state.install(backend).await
    }
}

fn clear_backend_scoped_state(app: &tauri::AppHandle) {
    if let Some(graph) = app.try_state::<crate::commands::graph::GraphDb>() {
        graph.clear_for_roost_switch();
    }
    if let Some(search) = app.try_state::<crate::commands::search::SearchState>() {
        search.clear();
    }
}

/// Close the active roost without deleting user content. This is the backend
/// half of removing the active registry entry: sync and watchers are drained,
/// derived singleton handles are cleared, and all outstanding generations are
/// invalidated before the backend lock is released.
#[tauri::command]
#[specta::specta]
pub async fn close_roost(
    app: tauri::AppHandle,
    state: State<'_, RoostFsState>,
) -> Result<(), String> {
    let _transition = state.transition.lock().await;
    if let Some(sync_state) = app.try_state::<crate::commands::sync::SyncEngineState>() {
        let _init = sync_state.init.lock().await;
        let _lifecycle = sync_state.lifecycle.lock().await;
        stop_sync_for_roost_switch(&app, &sync_state).await;
        state.clear().await;
    } else {
        state.clear().await;
    }
    crate::roostfs::walk::invalidate_walk_cache();
    clear_backend_scoped_state(&app);
    clear_roost_watchers(&app);
    Ok(())
}

fn clear_roost_watchers(app: &tauri::AppHandle) {
    if let (Some(registry), Some(watcher)) = (
        app.try_state::<crate::commands::ai::skills::SkillRegistryState>(),
        app.try_state::<crate::commands::ai::skills::SkillWatcherState>(),
    ) {
        crate::commands::ai::skills::rebind_watcher_for_roost(registry.0.clone(), &watcher, None);
    }
    if let (Some(registry), Some(watcher)) = (
        app.try_state::<crate::commands::ai::subagents::SubagentRegistryState>(),
        app.try_state::<crate::commands::ai::subagents::SubagentWatcherState>(),
    ) {
        crate::commands::ai::subagents::rebind_watcher_for_roost(
            registry.0.clone(),
            &watcher,
            None,
        );
    }
    crate::commands::index_watcher::clear_index_watcher(app);
}

/// Sync owns its own clone of the active `RoostFs`. If the user switches
/// roosts while sync is running, that clone keeps the old native roost lock
/// alive even after `RoostFsState` is replaced. Stop the engine before the
/// backend swap so reopening the prior roost does not conflict with our own
/// stale lock.
async fn stop_sync_for_roost_switch(
    app: &tauri::AppHandle,
    sync_state: &crate::commands::sync::SyncEngineState,
) {
    // Signal any in-flight `sync_init_inner` that a switch happened, BEFORE we
    // touch the engine. Bump unconditionally — even with no engine installed
    // yet, an init for the *old* roost can still be mid-flight and about to
    // install; the generation check at install time is what catches it. Doing
    // this only "if an engine exists" would miss exactly that case.
    sync_state
        .switch_generation
        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let active_sync_roost = {
        let guard = sync_state.engine.lock().await;
        guard.as_ref().map(|engine| engine.roost_path())
    };
    if let Some(roost) = active_sync_roost {
        tracing::info!(
            sync_roost = %roost,
            "roost_open: stopping sync engine before roost switch"
        );
    }
    // Clear discovery/reannounce state even when no engine was installed. A
    // partially-completed init may have created those siblings before its
    // generation check aborted the engine commit.
    crate::commands::sync::sync_shutdown_without_disabling_init_locked(app, sync_state).await;
}

/// Run the recursive directory walk on the blocking-task pool so the async
/// runtime thread stays free to dispatch other IPC commands (e.g. saves)
/// while the walk is in flight. On Android with `/storage/emulated/...`
/// paths this matters a lot — every `read_dir` is a FUSE roundtrip and on
/// a large roost the walk can pin the IPC dispatcher for many seconds,
/// stalling auto-save round-trips during startup.
async fn scan_directory_blocking(dir: PathBuf) -> Result<Vec<FileEntry>, String> {
    tokio::task::spawn_blocking(move || scan_directory(&dir))
        .await
        .map_err(|e| format!("roost scan task failed: {e}"))
}

/// MR-B4: native roost open with the warm-cache fast path SAF already uses —
/// serve the cached tree immediately and revalidate against a fresh
/// `scan_directory` walk in the background (emitting `roost://tree-changed` on
/// drift). Before this, a large native roost re-walked on every open/focus and
/// the UI blocked on it. Uses `scan_directory` for BOTH the cached fill and the
/// revalidation so the two never disagree — a backend `list_recursive` walk
/// would surface symlinks that `scan_directory` filters (AUDIT-076). On a cache
/// miss it scans synchronously and populates the cache for next time.
async fn open_native_with_cache(
    app: &tauri::AppHandle,
    dir: PathBuf,
    roost_key: &str,
    generation: u32,
) -> Result<Vec<FileEntry>, String> {
    let prepared = prepare_native_tree(app, dir.clone(), roost_key).await?;
    if let Some(cached) = prepared.cached_for_revalidation {
        spawn_native_tree_revalidation(app.clone(), dir, roost_key.to_string(), generation, cached);
    }
    Ok(prepared.entries)
}

struct PreparedNativeTree {
    entries: Vec<FileEntry>,
    cached_for_revalidation: Option<Vec<FileEntry>>,
}

async fn prepare_native_tree(
    app: &tauri::AppHandle,
    dir: PathBuf,
    roost_key: &str,
) -> Result<PreparedNativeTree, String> {
    if let Some(cached) = super::roost_tree_cache::read(app, roost_key) {
        return Ok(PreparedNativeTree {
            entries: cached.clone(),
            cached_for_revalidation: Some(cached),
        });
    }
    let entries = scan_directory_blocking(dir).await?;
    super::roost_tree_cache::write(app, roost_key, &entries);
    Ok(PreparedNativeTree {
        entries,
        cached_for_revalidation: None,
    })
}

fn spawn_native_tree_revalidation(
    app: tauri::AppHandle,
    dir: PathBuf,
    roost_key: String,
    generation: u32,
    cached: Vec<FileEntry>,
) {
    tauri::async_runtime::spawn(async move {
        let started = std::time::Instant::now();
        match tokio::task::spawn_blocking(move || scan_directory(&dir)).await {
            Ok(fresh) => super::roost_tree_cache::apply_if_changed(
                &app, &roost_key, generation, &cached, fresh, started,
            ),
            Err(e) => tracing::warn!(error = %e, "roost: native tree revalidate scan failed"),
        }
    });
}

/// Compare two roost roots in a way that's robust to Windows path quirks
/// (mixed separators, trailing slashes). Both inputs are treated as
/// filesystem paths; SAF tree URIs hit a different branch upstream so we
/// never compare a `content://` against a path here.
///
/// AUDIT-045: case folding is gated on the host's filesystem semantics.
/// Windows / macOS default to case-insensitive FS (NTFS, APFS); Linux
/// is case-sensitive. Folding everywhere meant Linux users with
/// distinct roosts at `/x/Roost` and `/x/roost` had them collide into
/// one. macOS gets the Windows treatment because APFS is case-folding
/// by default at consumer-volume creation time (case-sensitive APFS
/// exists but is opt-in and rare).
pub(crate) fn same_roost_root(existing_root_id: &str, requested: &Path) -> bool {
    let normalize = |s: &str| -> String {
        let cleaned: String = s.replace('\\', "/").trim_end_matches('/').to_string();
        if cfg!(any(target_os = "windows", target_os = "macos")) {
            cleaned.to_lowercase()
        } else {
            cleaned
        }
    };
    normalize(existing_root_id) == normalize(&requested.to_string_lossy())
}

/// Mobile-friendly roost init: picks a writable default path inside the
/// app's private data dir. Onboarding uses this so the user can land in a
/// ready-to-edit state without touching the folder picker on Android/iOS.
/// On desktop it's usable as a "create a default roost for me" shortcut.
/// Idempotent — returns the existing path if the folder is already there.
#[tauri::command]
#[specta::specta]
pub fn init_default_roost(app: tauri::AppHandle) -> Result<String, String> {
    use tauri::Manager;
    let data_dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("Failed to resolve app data dir: {e}"))?;
    // Ensure the app_data_dir parent exists. iOS doesn't create
    // `Library/Application Support/<bundle>/` until something writes to it,
    // and a bare `app_data_dir()` returns the path without creating it.
    fs::create_dir_all(&data_dir).map_err(|e| format!("Failed to create app data dir: {e}"))?;

    let roost = data_dir.join("vault");

    // Self-heal: if a previous build / partial install left a non-directory
    // at the target path, move it aside so we can create the directory.
    // Otherwise `create_dir_all` would return "Not a directory" forever
    // until the user reinstalls (iOS preserves the Data container across
    // reinstalls of the same bundle id).
    if let Ok(meta) = fs::symlink_metadata(&roost) {
        if !meta.is_dir() {
            let backup = data_dir.join(format!(
                "vault.broken-{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0)
            ));
            fs::rename(&roost, &backup).map_err(|e| {
                format!("Roost path is not a directory and couldn't be moved aside: {e}")
            })?;
            tracing::warn!(
                from = %roost.display(),
                to = %backup.display(),
                "init_default_roost: relocated stale non-directory at roost path"
            );
        }
    }

    fs::create_dir_all(&roost).map_err(|e| format!("Failed to create default roost: {e}"))?;

    if !roost.is_dir() {
        return Err(format!(
            "Roost path exists but isn't a directory after create: {}",
            roost.display()
        ));
    }

    let welcome = roost.join("Welcome.md");
    if !welcome.exists() {
        let body = "# Welcome to Corvux\n\nThis is your default roost. Start writing!\n\n- Tap the `+` to create a new note\n- Use `[[double brackets]]` to link notes\n- Use `#tags` to organize\n";
        fs::write(&welcome, body).map_err(|e| format!("Failed to create welcome note: {e}"))?;
    }

    Ok(roost.to_string_lossy().to_string())
}

/// Create a named roost under `<app_data_dir>/roosts/<slug>`. The caller
/// supplies a human name; we slugify defensively (strip path separators
/// and the characters Android/Windows reject in filenames) so the
/// returned path is always writable. Fails if a roost with the same slug
/// already exists — callers show a user-visible error so we don't
/// silently clobber.
#[tauri::command]
#[specta::specta]
pub fn create_named_roost(name: String, app: tauri::AppHandle) -> Result<String, String> {
    use tauri::Manager;
    let slug: String = name
        .trim()
        .chars()
        .map(|c| match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | ' ' => c,
            _ => '_',
        })
        .collect::<String>()
        .trim()
        .to_string();
    if slug.is_empty() {
        return Err("Roost name cannot be empty".to_string());
    }

    let data_dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("Failed to resolve app data dir: {e}"))?;
    // User content and derived indexes must never share a namespace. Older
    // named roosts under `vaults/` remain openable through their registry path;
    // new roosts live under the dedicated content root.
    let roosts_root = data_dir.join("roosts");
    let roost = roosts_root.join(&slug);
    let legacy_roost = data_dir.join("vaults").join(&slug);

    if roost.exists()
        || (legacy_roost.exists() && !crate::paths::is_marked_index_dir(&legacy_roost))
    {
        return Err(format!("A roost named '{}' already exists", slug));
    }
    fs::create_dir_all(&roost).map_err(|e| format!("Failed to create roost directory: {e}"))?;

    Ok(roost.to_string_lossy().to_string())
}

/// MR-B1: persist the roost registry to a durable on-disk manifest at
/// `app_data_dir/roosts.json`. The frontend registry lives in localStorage
/// (fast, but lost if the webview store is cleared/corrupted); this mirror lets
/// the app recover the roost list after such a wipe instead of leaving the
/// on-disk roosts invisible. The frontend owns the JSON shape (a `RoostEntry[]`
/// blob) — we just store it, written atomically (tmp + rename) so a crash
/// mid-write can't truncate the manifest.
#[tauri::command]
#[specta::specta]
pub fn write_roost_manifest(app: tauri::AppHandle, json: String) -> Result<(), String> {
    use tauri::Manager;
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("app_data_dir: {e}"))?;
    fs::create_dir_all(&dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;
    let path = dir.join("roosts.json");
    let tmp = dir.join("roosts.json.tmp");
    fs::write(&tmp, json.as_bytes()).map_err(|e| format!("write manifest tmp: {e}"))?;
    fs::rename(&tmp, &path).map_err(|e| format!("rename manifest: {e}"))?;
    Ok(())
}

/// MR-B1: read back the roost manifest written by [`write_roost_manifest`].
/// `None` means no manifest exists yet (first launch, or an install predating
/// this feature). The frontend hydrates its registry from this only when its
/// localStorage cache is empty, so a normal launch still trusts localStorage.
#[tauri::command]
#[specta::specta]
pub fn read_roost_manifest(app: tauri::AppHandle) -> Result<Option<String>, String> {
    use tauri::Manager;
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("app_data_dir: {e}"))?;
    let path = dir.join("roosts.json");
    match fs::read_to_string(&path) {
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("read manifest: {e}")),
    }
}

/// MR-B3: delete a roost's local index directory (graph.db, search.db, the
/// file-tree cache) — the "Remove and delete local index" action in Manage
/// Roosts, so removing a roost you're done with reclaims its multi-MB index
/// instead of orphaning it under `app_data_dir/vaults/`. ONLY the derived
/// index is removed; the user's roost content (markdown) is never touched —
/// `roost_db_dir` resolves the id-keyed index dir, which is a different
/// directory from user content. Best-effort: a missing dir
/// is success. Intended for roosts that aren't currently open (an open roost
/// holds its DB files, which can block deletion on Windows); the caller
/// tolerates an error and still drops the registry entry.
#[tauri::command]
#[specta::specta]
pub fn delete_roost_index(app: tauri::AppHandle, roost_path: String) -> Result<(), String> {
    let dir = crate::paths::roost_db_dir(&app, &roost_path)?;
    match fs::remove_dir_all(&dir) {
        Ok(()) => {
            tracing::info!(dir = %dir.display(), "roost: deleted local index dir");
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("delete index dir {}: {e}", dir.display())),
    }
}

/// Recursively scan a native filesystem directory into `FileEntry`. Hidden
/// files and our trash folder are filtered. Folders first, alphabetically.
fn scan_directory(dir: &Path) -> Vec<FileEntry> {
    let mut entries: Vec<FileEntry> = Vec::new();

    let Ok(read_dir) = fs::read_dir(dir) else {
        return entries;
    };

    for entry in read_dir.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();

        if name.starts_with('.') || name == ".corvux-trash" {
            continue;
        }

        // AUDIT-076: skip symlinks at every level. `path.is_dir()` and
        // `path.extension()` happily follow symlinks; an in-roost link
        // to `/etc` would otherwise get enumerated and surfaced to the
        // file tree. `file_type()` on the DirEntry does NOT follow.
        let ft = match entry.file_type() {
            Ok(t) => t,
            Err(_) => continue,
        };
        if ft.is_symlink() {
            continue;
        }

        if ft.is_dir() {
            let children = scan_directory(&path);
            entries.push(FileEntry {
                name,
                path: path.to_string_lossy().to_string(),
                is_dir: true,
                children,
            });
        } else if ft.is_file() && crate::roostfs::walk::is_markdown_path(&path) {
            // EDT-3: shared predicate (was `.md`-only, dropped `.markdown`).
            entries.push(FileEntry {
                name,
                path: path.to_string_lossy().to_string(),
                is_dir: false,
                children: vec![],
            });
        }
    }

    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then(a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });

    entries
}

/// Scan a [`RoostFs`] backend into the same `FileEntry` shape
/// `scan_directory` returns for native roosts. `display_root` is prepended
/// to the `path` field of each entry so the frontend can treat paths as
/// opaque strings — for SAF that means the tree URI itself, for native
/// the absolute roost root.
///
/// Uses [`RoostFs::list_recursive`] for the actual walk: on `NativeFs`
/// it falls through to per-dir `list()` (cheap `read_dir` on a real
/// filesystem); on `SafFs` it overrides to one bulk `listTree` JNI
/// round-trip. The previous per-directory recursion was the dominant
/// cost on Android — each directory was a separate ContentResolver
/// query, even after Round 2 optimizations elsewhere. Measurement on a
/// 16-entry SAF roost: 3.6–8s before, expected sub-second after.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub(super) async fn scan_roost_backend(
    fs: &dyn RoostFs,
    rel_dir: &str,
    display_root: &str,
) -> Result<Vec<FileEntry>, String> {
    let entries = fs
        .list_recursive(rel_dir)
        .await
        .map_err(|e| e.to_string())?;

    // Build a path → FileEntry map keyed by `rel_path` so we can attach
    // children to their parent directories in a second pass. The
    // recursive walk yields a flat list in unspecified order, so we
    // can't assume parents arrive before children.
    use std::collections::HashMap;
    let mut nodes: HashMap<String, FileEntry> = HashMap::with_capacity(entries.len());
    let mut roots: Vec<String> = Vec::new();

    for e in entries {
        // Match `scan_directory`'s filter: keep directories and markdown
        // files, drop everything else. Non-markdown leaves never make
        // it into the tree. EDT-3: shared predicate (was `.md`-only).
        if !e.is_dir && !crate::roostfs::walk::is_markdown_name(&e.name) {
            continue;
        }
        let abs_path = format!(
            "{}/{}",
            display_root.trim_end_matches('/'),
            e.rel_path.trim_start_matches('/')
        );
        let rel = e.rel_path.clone();
        nodes.insert(
            rel.clone(),
            FileEntry {
                name: e.name,
                path: abs_path,
                is_dir: e.is_dir,
                children: Vec::new(),
            },
        );
        if !rel.contains('/') {
            roots.push(rel);
        }
    }

    // Second pass: attach each non-root node to its parent's children.
    // Order matters — we must attach DEEPEST nodes first so a leaf is
    // placed under its parent before that parent is itself moved up
    // into its own grandparent's children. Sort attachments by
    // descending depth (slash count in the child path) to guarantee
    // bottom-up assembly.
    let mut attachments: Vec<(String, String)> = Vec::new();
    for rel in nodes.keys() {
        if let Some(slash) = rel.rfind('/') {
            attachments.push((rel[..slash].to_string(), rel.clone()));
        }
    }
    attachments.sort_by(|a, b| {
        let da = a.1.matches('/').count();
        let db = b.1.matches('/').count();
        db.cmp(&da)
    });
    for (parent_rel, child_rel) in attachments {
        if let Some(child) = nodes.remove(&child_rel) {
            if let Some(parent) = nodes.get_mut(&parent_rel) {
                parent.children.push(child);
            } else {
                // Parent didn't make it into nodes (filtered out, e.g.
                // a non-markdown leaf in a parent path). Promote the
                // orphan to root level so the user can still see it —
                // matches the pre-fix behaviour where a markdown child
                // of a filtered-out non-md ancestor still appeared.
                nodes.insert(child_rel.clone(), child);
                roots.push(child_rel);
            }
        }
    }

    // Materialize the root level and sort each subtree.
    let mut out: Vec<FileEntry> = roots
        .into_iter()
        .filter_map(|rel| nodes.remove(&rel))
        .collect();
    sort_tree(&mut out);
    Ok(out)
}

/// Recursively sort `FileEntry` trees: directories first within each
/// level, then case-insensitive name. Mirrors `scan_directory`'s order
/// so the sidebar layout doesn't shift between native and SAF roosts.
fn sort_tree(entries: &mut [FileEntry]) {
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then(a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    for e in entries.iter_mut() {
        if e.is_dir {
            sort_tree(&mut e.children);
        }
    }
}

/// Write a `.stignore` file at the roost root that excludes device-local
/// paths Corvux writes (SQLite/Cozo DBs, trash, tmp). Prevents Syncthing
/// from replicating live SQLite files between devices (which would corrupt
/// them). Idempotent — only writes if missing or if the required patterns
/// are absent.
///
/// The `!.corvux/skills` and `!.corvux/agents` negations carve those
/// subdirs back IN — Syncthing's `.stignore` honors negation, and
/// these subtrees hold user-authored skill / subagent definitions
/// that benefit from syncing across a user's devices the same way
/// notes do. Order matters: in Syncthing, the FIRST matching pattern
/// wins, so the negations have to appear BEFORE the `.corvux`
/// exclusion for them to take effect.
fn ensure_stignore(roost: &Path) -> std::io::Result<()> {
    // Patterns are written in match-priority order: negations first,
    // then exclusions. Matches Syncthing's first-match-wins semantics.
    const REQUIRED: &[&str] = &[
        "!.corvux/skills",
        "!.corvux/agents",
        ".corvux",
        ".corvux-trash",
        "*.md.tmp",
        "*.md.corvux.tmp",
    ];
    let path = roost.join(".stignore");
    // AUDIT-079: refuse to touch a symlink — `fs::write` follows it and
    // would truncate/append to whatever it points at. A malicious roost
    // could plant `.stignore -> ~/.bashrc`. NotFound is expected on
    // first-open and falls through to creation.
    match fs::symlink_metadata(&path) {
        Ok(md) if md.file_type().is_symlink() => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                ".stignore is a symlink — refusing to write",
            ));
        }
        Ok(_) | Err(_) => {}
    }
    let existing = fs::read_to_string(&path).unwrap_or_default();
    let missing: Vec<&&str> = REQUIRED
        .iter()
        .filter(|pat| !existing.lines().any(|l| l.trim() == **pat))
        .collect();
    if missing.is_empty() && !existing.is_empty() {
        return Ok(());
    }
    let mut out = String::new();
    if existing.is_empty() {
        out.push_str("// Managed by Corvux. Excludes device-local state from folder sync.\n");
    } else {
        out.push_str(&existing);
        if !out.ends_with('\n') {
            out.push('\n');
        }
    }
    for pat in missing {
        out.push_str(pat);
        out.push('\n');
    }
    fs::write(&path, out)
}

#[cfg(test)]
mod stignore_tests {
    use super::*;
    use tempfile::TempDir;

    /// AUDIT-013 regression. On mobile, `.corvux/.enc-seed` lives next to
    /// the encrypted secrets file. If a user ever mounts the roost under a
    /// Syncthing-style external sync layer, that seed MUST not replicate —
    /// the secrets file is unrecoverable without it but its plaintext
    /// presence on a second device would defeat the per-device encryption.
    /// The bare `.corvux` pattern matches that component anywhere in the
    /// tree under Syncthing's path-component semantics.
    #[test]
    fn stignore_excludes_corvux_subtree() {
        let dir = TempDir::new().unwrap();
        ensure_stignore(dir.path()).unwrap();
        let body = std::fs::read_to_string(dir.path().join(".stignore")).unwrap();
        let lines: Vec<&str> = body.lines().map(str::trim).collect();
        assert!(
            lines.contains(&".corvux"),
            ".stignore must exclude .corvux to shield .enc-seed and DBs"
        );
        let i_neg = lines
            .iter()
            .position(|l| *l == "!.corvux/skills")
            .expect("skills negation missing");
        let i_exc = lines
            .iter()
            .position(|l| *l == ".corvux")
            .expect(".corvux exclusion missing");
        assert!(
            i_neg < i_exc,
            "negations must precede the .corvux exclusion (Syncthing first-match-wins)"
        );
    }

    #[test]
    fn stignore_preserves_user_lines_and_adds_required() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(".stignore");
        std::fs::write(&path, "user-written-line\n").unwrap();
        ensure_stignore(dir.path()).unwrap();
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(
            body.contains("user-written-line"),
            "must preserve existing user lines"
        );
        assert!(body.contains(".corvux"), "must add .corvux exclusion");
    }

    #[test]
    fn stignore_idempotent_on_second_call() {
        let dir = TempDir::new().unwrap();
        ensure_stignore(dir.path()).unwrap();
        let first = std::fs::read_to_string(dir.path().join(".stignore")).unwrap();
        ensure_stignore(dir.path()).unwrap();
        let second = std::fs::read_to_string(dir.path().join(".stignore")).unwrap();
        assert_eq!(first, second, "second call must not rewrite the file");
    }

    /// AUDIT-079: a malicious roost can plant `.stignore` as a symlink to
    /// any user-writable file (eg `~/.bashrc`); the pre-fix `fs::write`
    /// would follow it and truncate the target. `ensure_stignore` must
    /// refuse and leave the target untouched.
    #[cfg(unix)]
    #[test]
    fn stignore_refuses_to_write_through_symlink() {
        use std::os::unix::fs::symlink;
        let dir = TempDir::new().unwrap();
        let outside = dir.path().parent().unwrap().join("audit079-victim");
        std::fs::write(&outside, b"USER CONTENT").unwrap();
        symlink(&outside, dir.path().join(".stignore")).expect("create symlink");

        let result = ensure_stignore(dir.path());
        assert!(result.is_err(), "must refuse to write through symlink");

        // The victim file must still hold the original content.
        let after = std::fs::read(&outside).unwrap();
        assert_eq!(
            after, b"USER CONTENT",
            "ensure_stignore must not have followed the symlink and clobbered the target"
        );
        let _ = std::fs::remove_file(&outside);
    }
}

#[cfg(test)]
mod same_roost_root_tests {
    use super::same_roost_root;
    use std::path::Path;

    /// AUDIT-045: shape-of-equality regression — trailing slash and
    /// mixed separators must NOT cause a re-open to look like a
    /// different roost.
    #[test]
    fn trailing_slash_and_separator_are_normalized() {
        let a = "/home/user/Roost";
        let b = Path::new("/home/user/Roost/");
        assert!(same_roost_root(a, b));

        let a = "C:\\Users\\me\\Roost";
        let b = Path::new("C:/Users/me/Roost");
        assert!(same_roost_root(a, b));
    }

    /// AUDIT-045: on Linux the filesystem is case-sensitive, so
    /// `/x/Roost` and `/x/roost` are genuinely distinct trees. Folding
    /// them together used to weld two different roosts into one
    /// registration; the new gated normalize keeps them apart.
    #[cfg(target_os = "linux")]
    #[test]
    fn linux_treats_case_as_distinct() {
        assert!(!same_roost_root(
            "/home/me/Roost",
            Path::new("/home/me/roost")
        ));
        assert!(!same_roost_root("/data/A", Path::new("/data/a")));
    }

    /// AUDIT-045: Windows NTFS is case-folded by default — opening
    /// `c:\users\me\roost` and `C:\Users\Me\Roost` MUST resolve to the
    /// same registration, otherwise users get a "roost already open"
    /// error against their own roost when the explorer hands a
    /// differently-cased path.
    #[cfg(target_os = "windows")]
    #[test]
    fn windows_treats_case_as_identical() {
        assert!(same_roost_root(
            "C:\\Users\\Me\\Roost",
            Path::new("c:\\users\\me\\roost")
        ));
    }

    /// Same logic as Windows: APFS at consumer-default settings is
    /// case-insensitive. We pin the cfg here so a future macOS-only
    /// path quirk lands in a labelled test.
    #[cfg(target_os = "macos")]
    #[test]
    fn macos_treats_case_as_identical() {
        assert!(same_roost_root(
            "/Users/Me/Roost",
            Path::new("/users/me/roost")
        ));
    }
}
