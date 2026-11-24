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

/// Stable error token returned by `open_roost` when the user declined the
/// native first-open confirmation (R2). The frontend matches this to stay on
/// the current roost instead of surfacing a raw error. Keep in sync with
/// `isRoostOpenDeclined` in the TS layer.
const ROOST_OPEN_DECLINED: &str = "ROOST_OPEN_DECLINED";

/// Cap on the confirmed-roots record. Every entry past the first required a
/// human click in a native dialog (or was created by the backend itself, or
/// seeded once from the pre-upgrade manifest), so the list cannot be flooded
/// silently; the cap just keeps the file bounded. Removing a roost in Manage
/// Roosts revokes its entry ([`revoke_confirmed_roots`]), which frees a slot.
const MAX_CONFIRMED_ROOTS: usize = 256;

/// Serializes every confirmed-roots read-modify-write (R2): record, revoke and
/// the one-time seed. A plain `std` mutex, held only around synchronous file
/// work and NEVER across an `.await` — the dialog is deliberately not held
/// under this lane (a dialog left open must not block other roost work), and
/// the sync commands that create app-owned roots can take it too.
static CONFIRMED_ROOTS_WRITE: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Roots confirmed in THIS run whose record could not be written (disk full,
/// read-only app-data, cap reached for an app-created root). Without this, a
/// failed save made every later `open_roost` — which also runs on every focus
/// and tree refresh — ask again. Memory only, on purpose: across a restart the
/// root is unconfirmed again (fail closed), never trusted from a record that
/// was never written.
static SESSION_CONFIRMED_ROOTS: std::sync::Mutex<std::collections::BTreeSet<String>> =
    std::sync::Mutex::new(std::collections::BTreeSet::new());

/// RF-3: written once, the first time this build starts, BEFORE the seed below
/// copies anything — so the seed can never run a second time, even if writing
/// `confirmed_roost_roots.json` fails or the file is later deleted.
const CONFIRMED_ROOTS_SEED_MARKER: &str = "confirmed_roost_roots.seeded";

/// Poison-tolerant lock: both mutexes guard plain data whose file side is
/// written atomically (tmp + rename), so a panic elsewhere cannot leave it
/// half-updated, and a poisoned lock must not brick every roost open.
fn lock_or_recover<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Backend-owned record of roost roots the user confirmed in a native
/// dialog (`app_data_dir/confirmed_roost_roots.json`) — sharp-edges R2.
///
/// `roosts.json` cannot serve as this list: it is written by the webview,
/// and the F10 gate confirms REMOVALS only, so a hijacked renderer can ADD
/// any path (the home folder) and then call `open_roost` on it. This file
/// is written only by the backend, only after a native confirm answered
/// yes. Fail closed: unreadable or garbage ⇒ empty ⇒ every open re-confirms.
fn confirmed_roots_file(dir: &Path) -> PathBuf {
    dir.join("confirmed_roost_roots.json")
}

fn load_confirmed_roots(dir: &Path) -> std::collections::BTreeSet<String> {
    let Ok(text) = fs::read_to_string(confirmed_roots_file(dir)) else {
        return Default::default();
    };
    match serde_json::from_str::<Vec<String>>(&text) {
        Ok(list) => list.into_iter().collect(),
        Err(_) => Default::default(),
    }
}

fn save_confirmed_roots(
    dir: &Path,
    roots: &std::collections::BTreeSet<String>,
) -> Result<(), String> {
    let json = serde_json::to_string_pretty(roots).map_err(|e| e.to_string())?;
    write_app_data_atomic(dir, "confirmed_roost_roots.json", json.as_bytes())
}

/// tmp + rename write of a small backend-owned record in app-data (never a
/// roost path). Shared by the confirmed-roots record and its seed marker.
fn write_app_data_atomic(dir: &Path, name: &str, bytes: &[u8]) -> Result<(), String> {
    let tmp = dir.join(format!("{name}.tmp"));
    fs::write(&tmp, bytes).map_err(|e| format!("write {name}: {e}"))?;
    fs::rename(&tmp, dir.join(name)).map_err(|e| format!("rename {name}: {e}"))
}

/// Every root confirmed so far: the persisted record plus this run's
/// confirmations that could not be persisted ([`SESSION_CONFIRMED_ROOTS`]).
pub(crate) fn confirmed_roots_now(dir: &Path) -> std::collections::BTreeSet<String> {
    let mut roots = load_confirmed_roots(dir);
    roots.extend(lock_or_recover(&SESSION_CONFIRMED_ROOTS).iter().cloned());
    roots
}

/// Add `root` to the confirmed record. The caller MUST hold
/// [`CONFIRMED_ROOTS_WRITE`]. A failed write is remembered for this run only
/// (see [`SESSION_CONFIRMED_ROOTS`]) and is not an error. `Err` only when the
/// record is full.
fn record_confirmed_root_locked(dir: &Path, root: &str) -> Result<(), String> {
    let key = crate::paths::normalize_key(root);
    let mut roots = load_confirmed_roots(dir);
    if roots.contains(&key) {
        return Ok(());
    }
    if roots.len() >= MAX_CONFIRMED_ROOTS {
        return Err(format!(
            "Corvux already remembers {MAX_CONFIRMED_ROOTS} roost folders, the most it keeps. \
             Remove a roost you no longer use in Manage Roosts, then open this folder again."
        ));
    }
    roots.insert(key.clone());
    if let Err(e) = save_confirmed_roots(dir, &roots) {
        tracing::warn!(error = %e, "roost: saving the confirmed-roots record failed — remembered for this run only; the next app start asks again");
        lock_or_recover(&SESSION_CONFIRMED_ROOTS).insert(key);
    }
    Ok(())
}

/// RF-3: a root the BACKEND itself just created under app-data
/// (`init_default_roost`, `create_named_roost`, both with a sanitized name) is
/// confirmed without a dialog — the user asked for a new, empty Corvux folder,
/// not access to an existing one. Never fails the creation: if the record is
/// full it is remembered for this run only.
fn confirm_app_created_root(dir: &Path, root: &Path) {
    let root = root.to_string_lossy();
    let _lane = lock_or_recover(&CONFIRMED_ROOTS_WRITE);
    if let Err(e) = record_confirmed_root_locked(dir, &root) {
        tracing::warn!(error = %e, "roost: app-created root not recorded — remembered for this run only");
        lock_or_recover(&SESSION_CONFIRMED_ROOTS).insert(crate::paths::normalize_key(&root));
    }
}

/// RF-3: forget roots the user removed from the roost list through the
/// confirmed removal flow (F10). Opening such a folder again asks again, and
/// the slot in the capped record is freed. Best effort: if the record cannot
/// be rewritten, the entry stays in the file (it was already trusted) and a
/// warning is logged.
fn revoke_confirmed_roots(dir: &Path, removed: &[String]) {
    if removed.is_empty() {
        return;
    }
    let keys: std::collections::BTreeSet<String> = removed
        .iter()
        .map(|p| crate::paths::normalize_key(p))
        .collect();
    let _lane = lock_or_recover(&CONFIRMED_ROOTS_WRITE);
    lock_or_recover(&SESSION_CONFIRMED_ROOTS).retain(|k| !keys.contains(k));
    let mut roots = load_confirmed_roots(dir);
    let before = roots.len();
    roots.retain(|k| !keys.contains(k));
    if roots.len() != before {
        if let Err(e) = save_confirmed_roots(dir, &roots) {
            tracing::warn!(error = %e, "roost: revoking a removed roost's first-open confirmation failed");
        }
    }
}

/// What the one-time upgrade seed did.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RootSeed {
    /// The record or the marker already existed: nothing was read or written.
    AlreadyDone,
    /// First run of this build: the marker was written and this many roots
    /// from the pre-upgrade manifest were recorded as confirmed.
    Seeded(usize),
}

/// RF-3 (migration): on the FIRST start of a build that has the R2 first-open
/// gate, treat the roosts the user already had as confirmed, so upgrading does
/// not ask "Open this folder as a roost?" for every roost they already use.
///
/// MUST run from trusted startup code before the main window (and so the
/// webview) exists — `lib.rs` setup does — so `roosts.json` here can only hold
/// what earlier runs wrote. Runs only when neither `confirmed_roost_roots.json`
/// nor the seed marker exists, and writes the marker FIRST: if the marker
/// cannot be written nothing is seeded (the gate then asks — fail closed), and
/// once it exists no later start ever seeds again, so a path a hijacked
/// webview adds to `roosts.json` afterwards still needs the native dialog.
///
/// Accepted trust-on-first-use limit: older builds had no first-open gate, so
/// whatever `roosts.json` listed before this upgrade is trusted as-is. A
/// renderer compromised under an OLD build could have listed any folder
/// there; nothing at upgrade time can tell that apart from a real roost.
/// There is no backend-side active-roost record to seed from (the active roost
/// lives in the webview's localStorage); every roost the app opened is in
/// `roosts.json`, because the registry mirrors there on every open (MR-B1).
pub(crate) fn seed_confirmed_roots_once(dir: &Path) -> Result<RootSeed, String> {
    let _lane = lock_or_recover(&CONFIRMED_ROOTS_WRITE);
    if confirmed_roots_file(dir).exists() || dir.join(CONFIRMED_ROOTS_SEED_MARKER).exists() {
        return Ok(RootSeed::AlreadyDone);
    }
    write_app_data_atomic(
        dir,
        CONFIRMED_ROOTS_SEED_MARKER,
        b"confirmed_roost_roots seeded from the pre-upgrade roosts.json\n",
    )?;
    let listed = match read_manifest_file(dir.join("roosts.json")) {
        Ok(Some(text)) => manifest_paths_checked(&text).unwrap_or_else(|e| {
            tracing::warn!(error = %e, "roost: pre-upgrade manifest invalid — nothing seeded");
            Vec::new()
        }),
        Ok(None) => Vec::new(),
        Err(e) => {
            tracing::warn!(error = %e, "roost: pre-upgrade manifest unreadable — nothing seeded");
            Vec::new()
        }
    };
    let mut roots = std::collections::BTreeSet::new();
    for p in &listed {
        if roots.len() >= MAX_CONFIRMED_ROOTS {
            break;
        }
        roots.insert(crate::paths::normalize_key(p));
    }
    save_confirmed_roots(dir, &roots)?;
    Ok(RootSeed::Seeded(roots.len()))
}

/// The R2 gate decision, without I/O or dialog: does opening this path need
/// the user's answer? Keys are compared through [`crate::paths::normalize_key`]
/// (backslash/case folding per host), the same normalization the index dirs
/// use.
fn roost_open_needs_confirm(
    effective_path: &str,
    confirmed: &std::collections::BTreeSet<String>,
) -> bool {
    !confirmed.contains(&crate::paths::normalize_key(effective_path))
}

/// Ask the human (R2): opening `effective_path` as a roost hands Corvux
/// read/write access to everything inside it. `true` = allow and the caller
/// persists the root as confirmed.
///
/// Worded for the case the user did not start (a hijacked webview): it names
/// the folder, says plainly what "Open roost" grants, and tells them what to
/// do if the folder is unexpected — without blaming a normal user.
async fn authorize_roost_open(
    effective_path: &str,
    confirm: &(impl super::native_confirm::NativeConfirm + ?Sized),
) -> bool {
    let shown = super::native_confirm::dialog_safe(effective_path, 200);
    let body = format!(
        "Corvux wants to open this folder as a roost:\n\n{shown}\n\n\
         If you choose Open roost, Corvux can read and change every file in \
         this folder, including through search, sync and the AI assistant. \
         Corvux asks only once for each folder.\n\n\
         If you don't recognize this folder, choose Cancel. Nothing is opened \
         or changed."
    );
    confirm
        .confirm("Open this folder in Corvux?", &body, "Open roost")
        .await
}

/// The whole R2 gate for one `open_roost`. `Ok(())` = may open. A root not yet
/// confirmed (record or this run's memory) is first checked to be an existing
/// directory — so a typo or a missing drive is reported plainly instead of
/// asking a question about a folder that is not there — then needs a "yes" in
/// the native dialog, after which it is recorded under the lane.
async fn gate_roost_open(
    dir: &Path,
    effective_path: &str,
    confirm: &(impl super::native_confirm::NativeConfirm + ?Sized),
) -> Result<(), String> {
    if !roost_open_needs_confirm(effective_path, &confirmed_roots_now(dir)) {
        return Ok(());
    }
    // (Own line on purpose: the raw-fs ratchet strips `//` as a comment even
    // inside a string, so a `{` after it on the same line would skew its
    // brace-depth count and make it miscount test code as production.)
    let is_saf_uri = effective_path.starts_with("content://");
    if !is_saf_uri {
        check_native_root_is_dir(effective_path)?;
    }
    if !authorize_roost_open(effective_path, confirm).await {
        tracing::warn!(target_path = %effective_path, "roost_open: first-open declined");
        return Err(ROOST_OPEN_DECLINED.to_string());
    }
    let _lane = lock_or_recover(&CONFIRMED_ROOTS_WRITE);
    record_confirmed_root_locked(dir, effective_path)
}

/// A native roost root must be an existing directory. Shared by the R2 gate
/// (checked before the dialog) and `open_roost_impl` (checked again before
/// anything is installed).
fn check_native_root_is_dir(path: &str) -> Result<(), String> {
    let dir = Path::new(path);
    if dir.is_dir() {
        return Ok(());
    }
    let meta = std::fs::symlink_metadata(dir);
    let detail = match meta {
        Ok(m) if m.file_type().is_file() => "exists as a file, not a directory",
        Ok(m) if m.file_type().is_symlink() => "is a symlink",
        Ok(_) => "exists but is not a directory",
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => "does not exist",
        Err(e) => return Err(format!("stat failed for {}: {}", path, e)),
    };
    Err(format!("Roost path {}: {}", detail, path))
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

    // R2 (sharp-edges residual gap): `open_roost` takes any absolute path,
    // so a hijacked webview could open the user's home folder as a roost
    // and read `.ssh/…` through normal file IPC. First open of a root the
    // backend has never confirmed needs the user's answer in a native OS
    // dialog the webview cannot click; confirmed roots open silently. The
    // confirmed set lives backend-side (`confirmed_roost_roots.json`),
    // because the webview-writable `roosts.json` cannot be the trust
    // anchor (F10 confirms removals only). Roosts that existed before the
    // upgrade were seeded once at startup, and app-created roots were
    // recorded when the backend made them (RF-3) — see `gate_roost_open`.
    {
        let dir = app
            .path()
            .app_data_dir()
            .map_err(|e| format!("app_data_dir: {e}"))?;
        gate_roost_open(
            &dir,
            &effective_path,
            &super::native_confirm::OsDialog(app.clone()),
        )
        .await?;
    }

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

    check_native_root_is_dir(&path)?;
    let dir = PathBuf::from(&path);
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
    tokio::task::spawn_blocking(move || scan_roost_root(&dir))
        .await
        .map_err(|e| format!("roost scan task failed: {e}"))?
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
        match tokio::task::spawn_blocking(move || scan_roost_root(&dir)).await {
            Ok(Ok(fresh)) => super::roost_tree_cache::apply_if_changed(
                &app, &roost_key, generation, &cached, fresh, started,
            ),
            // An unreadable root keeps the last good cached tree.
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "roost: native tree revalidate skipped, root unreadable")
            }
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

    // RF-3: the backend chose this folder (fixed name under app-data), so
    // opening it must not ask "Open this folder as a roost?".
    confirm_app_created_root(&data_dir, &roost);

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

    // RF-3: the backend just created this empty folder under app-data from a
    // sanitized slug, so opening it must not ask "Open this folder as a roost?".
    confirm_app_created_root(&data_dir, &roost);

    Ok(roost.to_string_lossy().to_string())
}

/// MR-B1: persist the roost registry to a durable on-disk manifest at
/// `app_data_dir/roosts.json`. The frontend registry lives in localStorage
/// (fast, but lost if the webview store is cleared/corrupted); this mirror lets
/// the app recover the roost list after such a wipe instead of leaving the
/// on-disk roosts invisible. The frontend owns the JSON shape (a `RoostEntry[]`
/// blob) — we just store it, written atomically (tmp + rename) so a crash
/// mid-write can't truncate the manifest.
///
/// F10 (sharp-edges 2026-11-11): this blob is also the AUTHORITY for a
/// destructive startup decision — `paths::gc_orphaned_index_dirs` deletes the
/// index dir and scoped sync credentials of every roost the manifest does not
/// list. So the backend no longer trusts it blindly:
///
/// * the shape is validated (array, bounded size/count, every entry has a real
///   `path`) — garbage is rejected, never stored;
/// * a write that DROPS a previously listed roost needs the user's answer in a
///   native OS dialog the webview cannot click, unless they already confirmed
///   that roost's removal there through [`delete_roost_index`].
#[tauri::command]
#[specta::specta]
pub async fn write_roost_manifest(app: tauri::AppHandle, json: String) -> Result<(), String> {
    use tauri::Manager;
    let next = manifest_paths_checked(&json)?;
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("app_data_dir: {e}"))?;
    fs::create_dir_all(&dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;
    let path = dir.join("roosts.json");

    // One lane for the WHOLE read-previous → confirm → write sequence, held
    // across the dialog. Without it, a write that landed while the dialog was
    // open (a roost added meanwhile) would be overwritten by this one's
    // snapshot, and the next start's GC would delete that roost's index and
    // sync credentials (reviewer finding #7).
    let _lane = MANIFEST_WRITE.lock().await;

    let prev = match read_manifest_file(path.clone()) {
        Ok(None) => PrevManifest::Absent,
        Ok(Some(text)) => match crate::paths::manifest_paths(&text) {
            Some(paths) => PrevManifest::Listed(paths),
            None => PrevManifest::Unreadable,
        },
        Err(_) => PrevManifest::Unreadable,
    };
    let confirmed_snapshot = CONFIRMED_REMOVALS
        .lock()
        .map_err(|e| format!("Lock error: {e}"))?
        .clone();
    authorize_manifest_write(
        &prev,
        &next,
        &confirmed_snapshot,
        &super::native_confirm::OsDialog(app.clone()),
    )
    .await?;

    let tmp = dir.join("roosts.json.tmp");
    fs::write(&tmp, json.as_bytes()).map_err(|e| format!("write manifest tmp: {e}"))?;
    fs::rename(&tmp, &path).map_err(|e| format!("rename manifest: {e}"))?;

    // RF-3: a roost the user removed (the removal was authorized above) loses
    // its first-open confirmation too — re-adding that folder later asks again,
    // and its slot in the capped confirmed-roots record is freed.
    if let PrevManifest::Listed(prev_paths) = &prev {
        revoke_confirmed_roots(
            &dir,
            &unconfirmed_removals(prev_paths, &next, &Default::default()),
        );
        // SY-5b: ...and its hosting-PIN secret slots (keyed by the roost PATH,
        // so nothing else can ever find them again) leave the keychain.
        super::sync::purge_hosting_pins(
            &unconfirmed_removals(prev_paths, &next, &Default::default()),
            &next,
        );
    }

    // A confirmed removal is single-use: once the manifest no longer lists the
    // roost, the confirmation is spent, so removing it again after a later
    // re-add asks again.
    if let Ok(mut confirmed) = CONFIRMED_REMOVALS.lock() {
        let still_listed: std::collections::BTreeSet<String> = next
            .iter()
            .map(|p| crate::paths::normalize_key(p))
            .collect();
        confirmed.retain(|k| still_listed.contains(k));
    }
    Ok(())
}

/// Serializes `write_roost_manifest` end to end (see its comment).
static MANIFEST_WRITE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// What the manifest on disk looked like before this write.
enum PrevManifest {
    /// No file: a first write (or an install that predates MR-B1).
    Absent,
    Listed(Vec<String>),
    /// The file exists but cannot be read or parsed. Nothing can be compared, so
    /// "no removals" would be a lie — a hostile replacement list would then
    /// authorize GC of every roost it omits with no prompt.
    Unreadable,
}

/// Decide whether this write needs the user, and ask. Pure policy + the dialog;
/// no filesystem.
async fn authorize_manifest_write(
    prev: &PrevManifest,
    next: &[String],
    confirmed: &std::collections::BTreeSet<String>,
    confirm: &(impl super::native_confirm::NativeConfirm + ?Sized),
) -> Result<(), String> {
    match prev {
        PrevManifest::Absent => Ok(()),
        PrevManifest::Listed(prev) => {
            authorize_removals(&unconfirmed_removals(prev, next, confirmed), confirm).await
        }
        PrevManifest::Unreadable => {
            let body = "Corvux's saved list of roosts could not be read, so it cannot tell \
                        which roosts this change would stop tracking.\n\n\
                        Replace it with the list the app just sent? Roosts missing from the \
                        new list lose their search index and sync pairing the next time \
                        Corvux starts. Your notes are NOT deleted.\n\n\
                        If you did not just change roosts in Corvux, choose Cancel.";
            if confirm
                .confirm("Replace the saved roost list?", body, "Replace")
                .await
            {
                Ok(())
            } else {
                Err("Roost removal was cancelled.".to_string())
            }
        }
    }
}

/// Largest manifest the backend will store. A real registry is a few KB.
const MAX_MANIFEST_BYTES: usize = 1 << 20;
const MAX_MANIFEST_ROOSTS: usize = 512;

/// Roost paths whose removal the user already confirmed natively through
/// [`delete_roost_index`] this session — so the manifest write that follows in
/// the same "Remove roost" flow does not ask a second time.
static CONFIRMED_REMOVALS: std::sync::Mutex<std::collections::BTreeSet<String>> =
    std::sync::Mutex::new(std::collections::BTreeSet::new());

/// Validate a manifest the webview wants stored and return its roost paths.
fn manifest_paths_checked(json: &str) -> Result<Vec<String>, String> {
    if json.len() > MAX_MANIFEST_BYTES {
        return Err("roost manifest is too large".into());
    }
    let v: serde_json::Value =
        serde_json::from_str(json).map_err(|e| format!("roost manifest is not valid JSON: {e}"))?;
    let arr = v
        .as_array()
        .ok_or_else(|| "roost manifest must be a JSON array".to_string())?;
    if arr.len() > MAX_MANIFEST_ROOSTS {
        return Err(format!(
            "roost manifest lists too many roosts (max {MAX_MANIFEST_ROOSTS})"
        ));
    }
    arr.iter()
        .enumerate()
        .map(|(i, e)| {
            e.get("path")
                .and_then(|p| p.as_str())
                .filter(|p| !p.trim().is_empty() && p.len() <= 4096 && !p.contains('\0'))
                .map(str::to_string)
                .ok_or_else(|| format!("roost manifest entry {i} has no usable path"))
        })
        .collect()
}

/// Paths listed in `prev` but absent from `next` whose removal the user has not
/// already confirmed. Compared by `normalize_key` (case/slash-insensitive on
/// Windows), so re-spelling a path is not a removal.
fn unconfirmed_removals(
    prev: &[String],
    next: &[String],
    confirmed: &std::collections::BTreeSet<String>,
) -> Vec<String> {
    let keep: std::collections::BTreeSet<String> = next
        .iter()
        .map(|p| crate::paths::normalize_key(p))
        .collect();
    let mut seen = std::collections::BTreeSet::new();
    prev.iter()
        .filter(|p| {
            let k = crate::paths::normalize_key(p);
            !keep.contains(&k) && !confirmed.contains(&k) && seen.insert(k)
        })
        .cloned()
        .collect()
}

/// Ask for the removal in a native dialog. No-op when nothing is removed.
async fn authorize_removals(
    removed: &[String],
    confirm: &(impl super::native_confirm::NativeConfirm + ?Sized),
) -> Result<(), String> {
    use super::native_confirm::dialog_safe;
    if removed.is_empty() {
        return Ok(());
    }
    let (noun, pronoun) = if removed.len() == 1 {
        ("roost", "it")
    } else {
        ("roosts", "them")
    };
    let mut body = format!(
        "Corvux is about to stop tracking {} {noun}:\n\n",
        removed.len()
    );
    for p in removed.iter().take(6) {
        body.push_str(&format!("\u{2022} {}\n", dialog_safe(p, 90)));
    }
    if removed.len() > 6 {
        body.push_str(&format!(
            "\u{2022} \u{2026}and {} more\n",
            removed.len() - 6
        ));
    }
    body.push_str(&format!(
        "\nYour notes are NOT deleted. Corvux will delete its own search index and sync \
         pairing for {pronoun} the next time it starts.\n\n\
         If you did not just remove this in Manage Roosts, choose Cancel."
    ));
    if confirm
        .confirm("Confirm roost removal", &body, "Remove")
        .await
    {
        Ok(())
    } else {
        Err("Roost removal was cancelled.".to_string())
    }
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
    read_manifest_file(dir.join("roosts.json"))
}

/// The one raw read of `roosts.json` (app-data, not note content), shared by
/// [`read_roost_manifest`] and [`write_roost_manifest`]'s removal check so the
/// AUDIT-033 read lint keeps a single pinned exception for it.
fn read_manifest_file(path: PathBuf) -> Result<Option<String>, String> {
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
/// `paths::existing_index_dirs` resolves the id-keyed and legacy index dirs,
/// which are different directories from user content. Best-effort: a missing dir
/// is success. Intended for roosts that aren't currently open (an open roost
/// holds its DB files, which can block deletion on Windows); the caller
/// tolerates an error and still drops the registry entry.
///
/// F10: this also deletes the roost's scoped sync credentials, so it needs the
/// same out-of-band confirmation as dropping the roost from the manifest. On
/// success the path is remembered as confirmed, so the manifest write that
/// completes the "Remove roost" flow does not ask again.
#[tauri::command]
#[specta::specta]
pub async fn delete_roost_index(app: tauri::AppHandle, roost_path: String) -> Result<(), String> {
    authorize_removals(
        std::slice::from_ref(&roost_path),
        &super::native_confirm::OsDialog(app.clone()),
    )
    .await?;
    // RF-6: FIND the index dirs, never `roost_db_dir` — that writes
    // `.corvux/vault-id.bin` into the folder being removed (a folder this call
    // only knows from a webview-supplied string), just to delete the dir it
    // names. `existing_index_dirs` is read-only and returns the id-keyed and
    // the legacy path-hash dir when they exist.
    let data = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("app_data_dir: {e}"))?;
    let mut result = Ok(());
    for dir in crate::paths::existing_index_dirs(&data, &roost_path) {
        // SYN-I11: this roost's sync credentials live in the keychain under a
        // scope only its `sync/peers.bin` knows — delete them before the file goes.
        crate::sync::store::peer_store::purge_registry_secrets(&dir.join("sync"));
        match fs::remove_dir_all(&dir) {
            Ok(()) => {
                tracing::info!(dir = %dir.display(), "roost: deleted local index dir");
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => result = Err(format!("delete index dir {}: {e}", dir.display())),
        }
    }
    if result.is_ok() {
        if let Ok(mut confirmed) = CONFIRMED_REMOVALS.lock() {
            confirmed.insert(crate::paths::normalize_key(&roost_path));
        }
    }
    result
}

/// Recursively scan a native filesystem directory into `FileEntry`. Hidden
/// files and our trash folder are filtered. Folders first, alphabetically.
fn scan_directory(dir: &Path) -> Vec<FileEntry> {
    match fs::read_dir(dir) {
        Ok(read_dir) => scan_read_dir(read_dir),
        // A NESTED folder we cannot read is skipped (display only). The roost
        // ROOT must go through `scan_roost_root`, which reports the failure.
        Err(_) => Vec::new(),
    }
}

/// Scan the roost ROOT. An unreadable root (expired iOS folder grant,
/// unmounted volume, changed permissions) is an ERROR, never an empty tree:
/// an empty result would be written to the tree cache and shown as "the roost
/// is empty", replacing the last good view (data-safety plan, Batch 1).
fn scan_roost_root(dir: &Path) -> Result<Vec<FileEntry>, String> {
    let read_dir = fs::read_dir(dir).map_err(|e| format!("cannot read the roost folder: {e}"))?;
    Ok(scan_read_dir(read_dir))
}

fn scan_read_dir(read_dir: fs::ReadDir) -> Vec<FileEntry> {
    let mut entries: Vec<FileEntry> = Vec::new();

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
    use std::io::{Read, Seek, SeekFrom, Write};
    // Patterns are written in match-priority order: negations first,
    // then exclusions. Matches Syncthing's first-match-wins semantics.
    const REQUIRED: &[&str] = &[
        "!.corvux/skills",
        "!.corvux/agents",
        ".corvux",
        ".corvux-trash",
        "*.md.tmp",
        "*.md.corvux.tmp",
        // RF-8: every staging file we write ends `.corvux.tmp` — `write_atomic`
        // (`note.md.corvux.tmp`) AND `create` (`note.md.<uuid>.corvux.tmp`,
        // which the `*.md.corvux.tmp` line never matched). One generic suffix
        // so a crash-orphaned staging file cannot sync or index. Existing
        // roosts get the line appended on their next open.
        "*.corvux.tmp",
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
    // Open without following a symlink, then read AND rewrite this same handle.
    // A later path swap cannot redirect the write to another file.
    let mut file = crate::roostfs::atomic::open_no_follow(&path)?;
    let mut existing = String::new();
    file.read_to_string(&mut existing)?;
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
    file.seek(SeekFrom::Start(0))?;
    file.write_all(out.as_bytes())?;
    file.set_len(out.len() as u64)?;
    file.sync_all()
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

    /// RF-8: `create` stages as `<name>.<uuid>.corvux.tmp`, which the old
    /// `*.md.corvux.tmp` line never matched, so a crash left a file that
    /// folder sync would replicate. The generic suffix covers it, and an
    /// existing `.stignore` written by an earlier build is upgraded in place.
    #[test]
    fn stignore_covers_every_staging_file_and_upgrades_older_files() {
        let dir = TempDir::new().unwrap();
        ensure_stignore(dir.path()).unwrap();
        let body = std::fs::read_to_string(dir.path().join(".stignore")).unwrap();
        assert!(body.lines().any(|l| l.trim() == "*.corvux.tmp"), "{body}");

        // An .stignore from the previous build: the six old patterns, one user line.
        let old = "// Managed by Corvux.\nmine\n!.corvux/skills\n!.corvux/agents\n.corvux\n.corvux-trash\n*.md.tmp\n*.md.corvux.tmp\n";
        std::fs::write(dir.path().join(".stignore"), old).unwrap();
        ensure_stignore(dir.path()).unwrap();
        let upgraded = std::fs::read_to_string(dir.path().join(".stignore")).unwrap();
        assert!(
            upgraded.starts_with(old),
            "existing lines stay as they were"
        );
        assert!(upgraded.lines().any(|l| l.trim() == "*.corvux.tmp"));
        assert_eq!(upgraded.matches("*.corvux.tmp").count(), 1);
        // (That every staging name `NativeFs` writes ends in `.corvux.tmp` is
        // pinned in `roostfs/native.rs`: `create_staging_name_ends_in_corvux_tmp…`.)
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

/// Data-safety plan, Batch 1 (tree cache). `open_roost` writes the cache only
/// after `scan_roost_backend(..)?` succeeds and `roost_tree_cache::revalidate`
/// returns early when the walk errors, so a failed walk can never replace the
/// last good cached tree. Both rest on the property pinned here: a listing
/// failure anywhere in the tree surfaces as an ERROR, never as a smaller tree.
///
/// These are CHARACTERIZATION tests: `scan_roost_backend` was not changed, so
/// they pass on the pre-fix code too. The fix that makes the property true on
/// Android is the Kotlin `queryChildrenChecked` (guarded by
/// `tests/saf_trash_guard.rs`); these tests stop a Rust-side regression from
/// re-swallowing the error it now delivers.
#[cfg(test)]
mod scan_failure_tests {
    use super::{scan_roost_backend, scan_roost_root};
    use crate::roostfs::test_support::FaultyFs;
    use tempfile::TempDir;

    fn seed(dir: &TempDir) {
        std::fs::create_dir_all(dir.path().join("Work/deep")).unwrap();
        std::fs::write(dir.path().join("top.md"), "t").unwrap();
        std::fs::write(dir.path().join("Work/a.md"), "a").unwrap();
        std::fs::write(dir.path().join("Work/deep/b.md"), "b").unwrap();
    }

    #[tokio::test]
    async fn a_nested_listing_failure_is_an_error_not_a_smaller_tree() {
        let dir = TempDir::new().unwrap();
        seed(&dir);
        let mut fs = FaultyFs::new(dir.path());
        fs.fail_list_dir = Some("Work/deep".into());

        let err = scan_roost_backend(&fs, "", "/roost").await.unwrap_err();

        assert!(
            err.contains("Directory listing incomplete"),
            "the scan must fail with the incomplete-listing error, got {err}"
        );
    }

    #[tokio::test]
    async fn a_root_listing_failure_is_an_error_not_an_empty_tree() {
        let dir = TempDir::new().unwrap();
        seed(&dir);
        let mut fs = FaultyFs::new(dir.path());
        fs.fail_list_dir = Some(String::new());

        assert!(scan_roost_backend(&fs, "", "/roost").await.is_err());
    }

    #[tokio::test]
    async fn a_readable_tree_still_scans_completely() {
        let dir = TempDir::new().unwrap();
        seed(&dir);
        let fs = FaultyFs::new(dir.path());

        let tree = scan_roost_backend(&fs, "", "/roost").await.unwrap();

        let names: Vec<&str> = tree.iter().map(|e| e.name.as_str()).collect();
        assert!(
            names.contains(&"top.md") && names.contains(&"Work"),
            "{names:?}"
        );
        let work = tree.iter().find(|e| e.name == "Work").unwrap();
        assert!(work.children.iter().any(|c| c.name == "deep"));
    }

    /// Native roost: an unreadable ROOT (expired grant, unmounted volume) used
    /// to scan as an empty tree, which `apply_if_changed` then cached and
    /// emitted as "the roost is empty". It must be an error so the last good
    /// view stays. (Regression test: fails on the pre-fix `scan_directory`.)
    #[test]
    fn an_unreadable_native_root_is_an_error_not_an_empty_tree() {
        let dir = TempDir::new().unwrap();
        let gone = dir.path().join("not-there");
        let err = scan_roost_root(&gone).unwrap_err();
        assert!(err.contains("cannot read the roost folder"), "got {err}");
    }

    #[test]
    fn a_readable_native_root_still_scans() {
        let dir = TempDir::new().unwrap();
        seed(&dir);
        let tree = scan_roost_root(dir.path()).unwrap();
        let names: Vec<&str> = tree.iter().map(|e| e.name.as_str()).collect();
        assert!(
            names.contains(&"top.md") && names.contains(&"Work"),
            "{names:?}"
        );
    }
}

/// F10 (sharp-edges 2026-11-11): a renderer-supplied manifest authorizes a
/// startup `remove_dir_all`, so it is validated and a removal needs a native
/// confirmation. Pure policy + scripted dialog — no window, no app data dir.
#[cfg(test)]
mod manifest_authority_tests {
    use super::super::native_confirm::test_support::Scripted;
    use super::*;
    use std::collections::BTreeSet;

    fn paths(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_normal_registry_validates() {
        let json = r#"[{"id":"v_1","path":"C:/Notes","displayName":"Notes"},
                       {"id":"v_2","path":"content://tree/x","type":"saf"}]"#;
        assert_eq!(
            manifest_paths_checked(json).unwrap(),
            paths(&["C:/Notes", "content://tree/x"])
        );
        assert_eq!(manifest_paths_checked("[]").unwrap(), Vec::<String>::new());
    }

    #[test]
    fn garbage_is_rejected_not_stored() {
        for bad in [
            "",
            "not json",
            "{}",
            "null",
            r#"{"path":"C:/x"}"#,       // object, not array
            r#"[{"id":"x"}]"#,          // no path
            r#"[{"path":""}]"#,         // empty path
            r#"[{"path":"   "}]"#,      // blank path
            r#"[{"path":5}]"#,          // wrong type
            r#"["C:/x"]"#,              // entry is not an object
            r#"[{"path":"a\u0000b"}]"#, // NUL
        ] {
            assert!(manifest_paths_checked(bad).is_err(), "{bad:?} should fail");
        }
    }

    #[test]
    fn size_and_count_are_bounded() {
        let huge = format!(r#"[{{"path":"{}"}}]"#, "a".repeat(MAX_MANIFEST_BYTES));
        assert!(manifest_paths_checked(&huge).is_err());
        let long_path = format!(r#"[{{"path":"{}"}}]"#, "a".repeat(5000));
        assert!(manifest_paths_checked(&long_path).is_err());
        let many = format!(
            "[{}]",
            (0..=MAX_MANIFEST_ROOSTS)
                .map(|i| format!(r#"{{"path":"/r/{i}"}}"#))
                .collect::<Vec<_>>()
                .join(",")
        );
        assert!(manifest_paths_checked(&many).is_err());
    }

    #[test]
    fn dropping_a_listed_roost_is_a_removal_but_adding_or_keeping_is_not() {
        let none = BTreeSet::new();
        assert_eq!(
            unconfirmed_removals(&paths(&["/a", "/b"]), &paths(&["/a"]), &none),
            paths(&["/b"])
        );
        assert!(unconfirmed_removals(&paths(&["/a"]), &paths(&["/a", "/b"]), &none).is_empty());
        assert!(unconfirmed_removals(&paths(&["/a"]), &paths(&["/a"]), &none).is_empty());
        // First-ever write: nothing to compare against, nothing removed.
        assert!(unconfirmed_removals(&[], &paths(&["/a"]), &none).is_empty());
    }

    #[test]
    fn replacing_the_whole_list_is_a_removal_of_everything_it_dropped() {
        // The audit's attack: one manifest naming a single roost.
        let none = BTreeSet::new();
        assert_eq!(
            unconfirmed_removals(&paths(&["/a", "/b", "/c"]), &paths(&["/keep"]), &none),
            paths(&["/a", "/b", "/c"])
        );
    }

    #[test]
    fn a_previously_confirmed_removal_is_not_asked_again() {
        let mut confirmed = BTreeSet::new();
        confirmed.insert(crate::paths::normalize_key("/b"));
        assert_eq!(
            unconfirmed_removals(&paths(&["/a", "/b"]), &paths(&[]), &confirmed),
            paths(&["/a"])
        );
    }

    #[test]
    fn duplicate_previous_entries_are_reported_once() {
        let none = BTreeSet::new();
        assert_eq!(
            unconfirmed_removals(&paths(&["/a", "/a"]), &paths(&[]), &none),
            paths(&["/a"])
        );
    }

    #[tokio::test]
    async fn nothing_removed_never_prompts() {
        let dialog = Scripted::deny();
        authorize_removals(&[], &dialog).await.unwrap();
        assert_eq!(dialog.times_asked(), 0);
    }

    #[tokio::test]
    async fn a_declined_removal_is_refused() {
        let dialog = Scripted::deny();
        let err = authorize_removals(&paths(&["/a", "/b"]), &dialog)
            .await
            .unwrap_err();
        assert!(err.contains("cancelled"), "{err}");
        assert_eq!(dialog.times_asked(), 1, "one prompt for the whole batch");
    }

    #[tokio::test]
    async fn an_allowed_removal_proceeds() {
        let dialog = Scripted::allow();
        authorize_removals(&paths(&["/a"]), &dialog).await.unwrap();
        assert_eq!(dialog.times_asked(), 1);
    }

    // ── write policy over the previous file's state (reviewer #7) ───────

    #[tokio::test]
    async fn a_first_write_never_prompts() {
        let dialog = Scripted::deny();
        authorize_manifest_write(
            &PrevManifest::Absent,
            &paths(&["/a"]),
            &BTreeSet::new(),
            &dialog,
        )
        .await
        .unwrap();
        assert_eq!(dialog.times_asked(), 0);
    }

    #[tokio::test]
    async fn adding_or_keeping_roosts_never_prompts() {
        let dialog = Scripted::deny();
        authorize_manifest_write(
            &PrevManifest::Listed(paths(&["/a"])),
            &paths(&["/a", "/b"]),
            &BTreeSet::new(),
            &dialog,
        )
        .await
        .unwrap();
        assert_eq!(dialog.times_asked(), 0);
    }

    #[tokio::test]
    async fn dropping_a_listed_roost_prompts_and_a_decline_refuses() {
        let dialog = Scripted::deny();
        let err = authorize_manifest_write(
            &PrevManifest::Listed(paths(&["/a", "/b"])),
            &paths(&["/a"]),
            &BTreeSet::new(),
            &dialog,
        )
        .await
        .unwrap_err();
        assert!(err.contains("cancelled"), "{err}");
        assert_eq!(dialog.times_asked(), 1);
    }

    #[tokio::test]
    async fn an_unreadable_previous_manifest_fails_closed_not_open() {
        // The old code treated this as "nothing to compare" = no prompt, so a
        // hostile replacement list authorized GC of everything it omitted.
        let denied = Scripted::deny();
        let err = authorize_manifest_write(
            &PrevManifest::Unreadable,
            &paths(&["/only-this"]),
            &BTreeSet::new(),
            &denied,
        )
        .await
        .unwrap_err();
        assert!(err.contains("cancelled"), "{err}");
        assert_eq!(denied.times_asked(), 1);

        let allowed = Scripted::allow();
        authorize_manifest_write(
            &PrevManifest::Unreadable,
            &paths(&["/only-this"]),
            &BTreeSet::new(),
            &allowed,
        )
        .await
        .unwrap();
    }
}

/// R2 (sharp-edges residual): `open_roost` must only grant access to roots
/// the backend itself confirmed — the webview-writable manifest can add any
/// path without a dialog (F10 confirms removals only).
#[cfg(test)]
mod roost_root_gate_tests {
    use super::super::native_confirm::test_support::Scripted;
    use super::*;
    use std::collections::BTreeSet;
    use tempfile::TempDir;

    fn set_of(keys: &[&str]) -> BTreeSet<String> {
        keys.iter()
            .map(|k| crate::paths::normalize_key(k))
            .collect()
    }

    #[test]
    fn confirmed_roots_round_trip_and_fail_closed() {
        let dir = TempDir::new().unwrap();
        assert!(
            load_confirmed_roots(dir.path()).is_empty(),
            "no file yet = empty (first run re-confirms)"
        );
        let mut roots = set_of(&["/a", "content://tree/x"]);
        roots.insert(crate::paths::normalize_key("C:\\Notes"));
        save_confirmed_roots(dir.path(), &roots).unwrap();
        assert_eq!(load_confirmed_roots(dir.path()), roots);

        // Garbage on disk must fail CLOSED (empty ⇒ re-confirm), never be
        // interpreted as a confirmed list.
        std::fs::write(
            confirmed_roots_file(dir.path()),
            "{\"oops\": true}".as_bytes(),
        )
        .unwrap();
        assert!(load_confirmed_roots(dir.path()).is_empty());
    }

    #[test]
    fn a_confirmed_root_needs_no_dialog_anything_else_does() {
        let confirmed = set_of(&["/roosts/mine"]);
        assert!(!roost_open_needs_confirm("/roosts/mine", &confirmed));
        assert!(
            roost_open_needs_confirm("/", &confirmed),
            "the home folder is not a confirmed roost"
        );
        assert!(roost_open_needs_confirm("/roosts/other", &confirmed));
        assert!(roost_open_needs_confirm("/roosts/mine/..", &confirmed));
    }

    #[cfg(windows)]
    #[test]
    fn root_keys_fold_case_and_slashes_on_windows() {
        let confirmed = set_of(&["C:\\Notes"]);
        assert!(!roost_open_needs_confirm("c:/notes", &confirmed));
    }

    #[tokio::test]
    async fn authorize_roost_open_reports_the_dialog_answer() {
        let allowed = Scripted::allow();
        assert!(authorize_roost_open("/home/me/notes", &allowed).await);
        assert_eq!(allowed.times_asked(), 1);

        // Fail closed: cancel / dismissed / failed dialog = no access.
        let denied = Scripted::deny();
        assert!(!authorize_roost_open("/home/me/notes", &denied).await);
        assert_eq!(denied.times_asked(), 1);
    }
}

/// RF-3: the R2 first-open gate must not ask upgraders about roosts they
/// already use, nor about folders the app itself created — while a folder
/// added to the webview-writable manifest AFTER the one-time seed still asks.
/// Each test uses its own temp dirs as roots, so the process-wide session
/// memory cannot leak between tests.
#[cfg(test)]
mod rf3_migration_tests {
    use super::super::native_confirm::test_support::Scripted;
    use super::super::native_confirm::NativeConfirm;
    use super::*;
    use tempfile::TempDir;

    fn s(p: &Path) -> String {
        p.to_string_lossy().to_string()
    }

    fn write_manifest(app_data: &Path, roots: &[String]) {
        let entries: Vec<serde_json::Value> = roots
            .iter()
            .map(|p| serde_json::json!({ "id": "v_x", "path": p, "displayName": "x" }))
            .collect();
        std::fs::write(
            app_data.join("roosts.json"),
            serde_json::to_string(&entries).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn seeding_happens_once_and_only_when_the_record_is_absent() {
        let app_data = TempDir::new().unwrap();
        let (a, b, later) = (
            TempDir::new().unwrap(),
            TempDir::new().unwrap(),
            TempDir::new().unwrap(),
        );
        write_manifest(app_data.path(), &[s(a.path()), s(b.path())]);

        assert_eq!(
            seed_confirmed_roots_once(app_data.path()).unwrap(),
            RootSeed::Seeded(2)
        );
        let seeded = load_confirmed_roots(app_data.path());
        assert!(!roost_open_needs_confirm(&s(a.path()), &seeded));
        assert!(!roost_open_needs_confirm(&s(b.path()), &seeded));
        assert!(app_data.path().join(CONFIRMED_ROOTS_SEED_MARKER).exists());

        // A path a (hijacked) webview lists afterwards is never seeded.
        write_manifest(
            app_data.path(),
            &[s(a.path()), s(b.path()), s(later.path())],
        );
        assert_eq!(
            seed_confirmed_roots_once(app_data.path()).unwrap(),
            RootSeed::AlreadyDone
        );
        // …not even if the record itself is deleted: the marker blocks it.
        std::fs::remove_file(confirmed_roots_file(app_data.path())).unwrap();
        assert_eq!(
            seed_confirmed_roots_once(app_data.path()).unwrap(),
            RootSeed::AlreadyDone
        );
        assert!(roost_open_needs_confirm(
            &s(later.path()),
            &load_confirmed_roots(app_data.path())
        ));
    }

    #[test]
    fn an_existing_record_without_a_marker_is_left_alone() {
        // A user who already answered dialogs on a build with the gate.
        let app_data = TempDir::new().unwrap();
        let (kept, listed) = (TempDir::new().unwrap(), TempDir::new().unwrap());
        let mut roots = std::collections::BTreeSet::new();
        roots.insert(crate::paths::normalize_key(&s(kept.path())));
        save_confirmed_roots(app_data.path(), &roots).unwrap();
        write_manifest(app_data.path(), &[s(listed.path())]);

        assert_eq!(
            seed_confirmed_roots_once(app_data.path()).unwrap(),
            RootSeed::AlreadyDone
        );
        assert_eq!(load_confirmed_roots(app_data.path()), roots);
    }

    #[test]
    fn a_fresh_install_seeds_nothing_but_still_writes_the_marker() {
        let app_data = TempDir::new().unwrap();
        assert_eq!(
            seed_confirmed_roots_once(app_data.path()).unwrap(),
            RootSeed::Seeded(0)
        );
        // The webview writes roosts.json during this first run; the next
        // start must not seed it.
        let r = TempDir::new().unwrap();
        write_manifest(app_data.path(), &[s(r.path())]);
        assert_eq!(
            seed_confirmed_roots_once(app_data.path()).unwrap(),
            RootSeed::AlreadyDone
        );
        assert!(roost_open_needs_confirm(
            &s(r.path()),
            &load_confirmed_roots(app_data.path())
        ));
    }

    #[tokio::test]
    async fn a_seeded_root_opens_without_the_dialog() {
        let app_data = TempDir::new().unwrap();
        let root = TempDir::new().unwrap();
        write_manifest(app_data.path(), &[s(root.path())]);
        seed_confirmed_roots_once(app_data.path()).unwrap();

        let dialog = Scripted::deny(); // would refuse if asked — it must not be
        gate_roost_open(app_data.path(), &s(root.path()), &dialog)
            .await
            .unwrap();
        assert_eq!(dialog.times_asked(), 0);
    }

    #[tokio::test]
    async fn a_root_added_to_the_manifest_after_seeding_still_asks() {
        let app_data = TempDir::new().unwrap();
        let (old, added) = (TempDir::new().unwrap(), TempDir::new().unwrap());
        write_manifest(app_data.path(), &[s(old.path())]);
        seed_confirmed_roots_once(app_data.path()).unwrap();
        write_manifest(app_data.path(), &[s(old.path()), s(added.path())]);

        let denied = Scripted::deny();
        let err = gate_roost_open(app_data.path(), &s(added.path()), &denied)
            .await
            .unwrap_err();
        assert_eq!(err, ROOST_OPEN_DECLINED);
        assert_eq!(denied.times_asked(), 1);

        let allowed = Scripted::allow();
        gate_roost_open(app_data.path(), &s(added.path()), &allowed)
            .await
            .unwrap();
        assert_eq!(allowed.times_asked(), 1);
        assert!(!roost_open_needs_confirm(
            &s(added.path()),
            &load_confirmed_roots(app_data.path())
        ));
    }

    #[tokio::test]
    async fn an_app_created_root_is_not_asked_about() {
        let app_data = TempDir::new().unwrap();
        let created = app_data.path().join("roosts").join("Work");
        std::fs::create_dir_all(&created).unwrap();
        confirm_app_created_root(app_data.path(), &created);

        let dialog = Scripted::deny();
        gate_roost_open(app_data.path(), &s(&created), &dialog)
            .await
            .unwrap();
        assert_eq!(dialog.times_asked(), 0);
        assert!(!roost_open_needs_confirm(
            &s(&created),
            &load_confirmed_roots(app_data.path())
        ));
    }

    #[tokio::test]
    async fn a_failed_save_does_not_reprompt_in_this_run_but_does_after_a_restart() {
        // An app-data dir that does not exist: every save fails.
        let parent = TempDir::new().unwrap();
        let unwritable = parent.path().join("missing");
        let root = TempDir::new().unwrap();

        let first = Scripted::allow();
        gate_roost_open(&unwritable, &s(root.path()), &first)
            .await
            .unwrap();
        assert_eq!(first.times_asked(), 1);

        let again = Scripted::deny();
        gate_roost_open(&unwritable, &s(root.path()), &again)
            .await
            .unwrap();
        assert_eq!(again.times_asked(), 0, "remembered for this run");

        // Nothing was persisted, so a restart (no session memory) asks again.
        assert!(roost_open_needs_confirm(
            &s(root.path()),
            &load_confirmed_roots(&unwritable)
        ));
    }

    #[tokio::test]
    async fn revoking_a_removed_roost_removes_the_entry_and_asks_again() {
        let app_data = TempDir::new().unwrap();
        let (gone, kept) = (TempDir::new().unwrap(), TempDir::new().unwrap());
        {
            let _lane = lock_or_recover(&CONFIRMED_ROOTS_WRITE);
            record_confirmed_root_locked(app_data.path(), &s(gone.path())).unwrap();
            record_confirmed_root_locked(app_data.path(), &s(kept.path())).unwrap();
        }
        revoke_confirmed_roots(app_data.path(), &[s(gone.path())]);

        let roots = load_confirmed_roots(app_data.path());
        assert!(roost_open_needs_confirm(&s(gone.path()), &roots));
        assert!(!roost_open_needs_confirm(&s(kept.path()), &roots));

        let dialog = Scripted::deny();
        assert!(gate_roost_open(app_data.path(), &s(gone.path()), &dialog)
            .await
            .is_err());
        assert_eq!(dialog.times_asked(), 1);
    }

    #[test]
    fn a_full_record_says_how_to_free_a_slot() {
        let app_data = TempDir::new().unwrap();
        let full: std::collections::BTreeSet<String> = (0..MAX_CONFIRMED_ROOTS)
            .map(|i| format!("/r/{i}"))
            .collect();
        save_confirmed_roots(app_data.path(), &full).unwrap();
        let _lane = lock_or_recover(&CONFIRMED_ROOTS_WRITE);
        let err = record_confirmed_root_locked(app_data.path(), "/r/new").unwrap_err();
        assert!(err.contains("Manage Roosts"), "{err}");
        // An already-recorded root is not refused by the cap.
        record_confirmed_root_locked(app_data.path(), "/r/0").unwrap();
    }

    #[tokio::test]
    async fn a_missing_folder_is_reported_before_any_dialog() {
        let parent = TempDir::new().unwrap();
        let missing = parent.path().join("not-here");
        let dialog = Scripted::allow();
        let err = gate_roost_open(parent.path(), &s(&missing), &dialog)
            .await
            .unwrap_err();
        assert!(err.contains("does not exist"), "{err}");
        assert_eq!(dialog.times_asked(), 0);

        let file = parent.path().join("note.md");
        std::fs::write(&file, "x").unwrap();
        let err = gate_roost_open(parent.path(), &s(&file), &dialog)
            .await
            .unwrap_err();
        assert!(err.contains("not a directory"), "{err}");
        assert_eq!(dialog.times_asked(), 0);
    }

    /// Wiring the unit tests above cannot reach (they need an AppHandle): the
    /// seed must run in startup code BEFORE the main window (and so the
    /// webview) exists, and both app-root creators must record their root.
    #[test]
    fn startup_seed_and_app_created_roots_stay_wired() {
        let src = |rel: &str| {
            std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join(rel))
                .unwrap()
                .replace("\r\n", "\n")
        };
        let lib = src("src/lib.rs");
        let seed = lib
            .find("seed_confirmed_roots_once(")
            .expect("lib.rs setup must call seed_confirmed_roots_once");
        let window = lib
            .find("WebviewWindowBuilder::new(app, \"main\"")
            .expect("main window builder");
        assert!(
            seed < window,
            "the RF-3 seed must run before the webview exists"
        );

        let roost = src("src/commands/roost.rs");
        for f in ["fn init_default_roost(", "fn create_named_roost("] {
            let start = roost.find(f).expect(f);
            // `\u{7d}` is the closing brace, spelled so the source-scanning
            // guards' brace counters stay balanced on this line.
            let end = roost[start..].find("\n\u{7d}\n").expect("fn end");
            let body = &roost[start..start + end];
            assert!(
                body.contains("confirm_app_created_root("),
                "{f} must confirm the root it creates (RF-3)"
            );
        }
    }

    /// Records the dialog text so its wording can be checked.
    struct Recording(std::sync::Mutex<Vec<(String, String)>>);

    #[async_trait::async_trait]
    impl NativeConfirm for Recording {
        async fn confirm(&self, title: &str, body: &str, _allow_label: &str) -> bool {
            self.0
                .lock()
                .unwrap()
                .push((title.to_string(), body.to_string()));
            false
        }
    }

    #[tokio::test]
    async fn the_dialog_names_the_folder_and_says_what_allow_means() {
        let dialog = Recording(std::sync::Mutex::new(Vec::new()));
        assert!(!authorize_roost_open("/home/me/Notes", &dialog).await);
        let asked = dialog.0.lock().unwrap();
        let (title, body) = &asked[0];
        assert!(title.contains("Open this folder"), "{title}");
        assert!(body.contains("/home/me/Notes"), "{body}");
        assert!(body.contains("read and change every file"), "{body}");
        assert!(
            !body.contains("did not just choose"),
            "must not tell a normal user to cancel a flow they did not start: {body}"
        );
    }
}
