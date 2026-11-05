mod ai;
mod background_runtime;
mod commands;
// `pub` so benches/integration tests under `benches/` and `tests/` can call
// the same query functions the IPC commands do, without each harness
// reimplementing the Datalog. These only re-expose what each module's
// `mod.rs` already opted into (`pub use ...`); inner submodules stay
// private where their `pub` decls already restrict reach.
pub mod db;
mod diagnostics;
// `pub` so the (future) paid code paths — sync relay enqueue, hosted-AI
// provider — and QA tooling can ask `entitlement::allows(..)`. Public-API
// status also keeps the seam from tripping dead_code while no caller exists
// yet (Phase A stub; real token validation lands in Phase B).
pub mod entitlement;
// `pub` so `events::*` can be referenced from `specta_bridge::build`
// and from inside the emit sites scattered across the crate. Containing
// the typed `tauri_specta::Event` structs in one place keeps emit + the
// generated `bindings.ts` `events` object in lockstep.
pub mod events;
mod ios_backup;
#[cfg(not(any(target_os = "android", target_os = "ios")))]
#[allow(dead_code)] // disabled at runtime (decorations:false owns chrome); kept for re-enablement
mod menu;
#[cfg(target_os = "macos")]
mod notification_macos;
pub mod parser;
mod paths;
mod perf;
pub mod roostfs;
mod secret_store;
mod secure_file;
// `pub` so `tools/gen_bindings.rs` (declared as `[[example]]`) can call
// `specta_bridge::build()` + `exporter()` to regenerate `bindings.ts`
// without booting the Tauri runtime.
pub mod specta_bridge;
// `sync` is crate-internal (see sync/mod.rs's design note: nothing outside
// `sync/` should reach into submodules).
mod sync;

use commands::ai::{ChatCancel, HttpClient, LlamaCppState, Settings, SettingsState};
use commands::graph::GraphDb;
use commands::search::SearchState;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use tauri::{Manager, WebviewUrl, WebviewWindowBuilder};

/// Set the process AppUserModelID on Windows so toast notifications are
/// attributed to "Corvux" instead of the parent shell. Production NSIS
/// installs register this at install time; this call covers `tauri dev`
/// and any case where the binary runs without going through the
/// installer. Idempotent — calling it multiple times is harmless.
/// Errors are swallowed: the toasts still fire, they just keep the
/// generic name. No-op on non-Windows targets.
#[cfg(target_os = "windows")]
fn set_windows_aumid() {
    use windows::core::HSTRING;
    use windows::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID;
    let aumid = HSTRING::from("com.corvuxmindware.corvux");
    let _ = unsafe { SetCurrentProcessExplicitAppUserModelID(&aumid) };
}

#[cfg(not(target_os = "windows"))]
fn set_windows_aumid() {}

/// Reconcile the OS "launch at login" registration with the user's
/// `launch_at_startup` setting. Called once on startup (to apply the
/// persisted choice, or the ON-by-default for a fresh/upgraded install)
/// and again from `save_settings` whenever the toggle flips.
///
/// Idempotent: queries the current registration first and only writes
/// when it differs, so a launch where the setting already matches the OS
/// state touches nothing. A query failure is treated as "unknown" and we
/// attempt the desired state anyway — registering twice or unregistering
/// an absent entry is harmless on every supported platform.
///
/// Desktop-only. On Android/iOS the autostart plugin isn't registered, so
/// this compiles to a no-op and callers don't need to cfg-gate the call.
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub(crate) fn reconcile_autostart(app: &tauri::AppHandle, enabled: bool) {
    use tauri_plugin_autostart::ManagerExt;
    let manager = app.autolaunch();
    // Already in the desired state? Nothing to do.
    if matches!(manager.is_enabled(), Ok(current) if current == enabled) {
        return;
    }
    let result = if enabled {
        manager.enable()
    } else {
        manager.disable()
    };
    if let Err(e) = result {
        tracing::warn!(
            error = %e,
            enabled,
            "autostart: failed to reconcile launch-at-startup registration"
        );
    }
}

#[cfg(any(target_os = "android", target_os = "ios"))]
pub(crate) fn reconcile_autostart(_app: &tauri::AppHandle, _enabled: bool) {}

/// Shutdown guard shared by the window-close handler and the tray "Quit"
/// item. Set once teardown begins so the re-entrant `CloseRequested` that
/// `app.exit(0)` fires (see the close handler) is recognised and let
/// through instead of being re-interpreted as a fresh user close.
static SHUTTING_DOWN: AtomicBool = AtomicBool::new(false);

/// Graceful shutdown then exit: tear down the sync engine (listener
/// socket, peer connections, mDNS) on a 2 s deadline, clear the
/// crash-detection marker, then quit. Shared by a real window close
/// (close-to-tray OFF) and the tray "Quit" menu item.
///
/// The teardown runs off the GUI thread on purpose — the old in-line
/// `block_on` here froze the window ("Not Responding") for up to 2 s.
/// `SHUTTING_DOWN` makes this idempotent: a second call (Quit pressed
/// twice, or the re-entrant exit) no-ops. The crash marker is cleared
/// only AFTER sync teardown, so a hang/panic mid-shutdown still leaves it
/// in place for the next launch's "did we exit cleanly?" check.
fn shutdown_and_exit(app: tauri::AppHandle) {
    use std::sync::atomic::Ordering::SeqCst;
    if SHUTTING_DOWN.swap(true, SeqCst) {
        return;
    }
    tauri::async_runtime::spawn(async move {
        let state = app.state::<commands::sync::SyncEngineState>();
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            commands::sync::sync_shutdown_without_disabling(&app, &state),
        )
        .await;
        diagnostics::mark_session_ended(&app);
        app.exit(0);
    });
}

/// Bring the main window back to the foreground — from the system tray,
/// or from a hidden/minimized state. Best-effort: each step is
/// independently ignorable so a transient failure in one doesn't swallow
/// the others.
#[cfg(desktop)]
fn show_main_window(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

/// Install the process-wide Rustls crypto provider before any crate builds a
/// TLS client. Tauri/Wry can construct `reqwest` clients while serving the
/// initial webview protocol response, so doing this only inside sync setup is
/// too late on mobile.
fn install_rustls_crypto_provider() {
    use std::sync::Once;

    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    set_windows_aumid();
    install_rustls_crypto_provider();

    // Install a tracing subscriber so `tracing::info!`/`warn!`/`debug!`
    // from the sync engine (and the rest of the app) actually reach the
    // console / logcat. Without this, every tracing event is silently
    // dropped. Filter defaults to `info` project-wide + `debug` for our
    // own crate, overridable via `RUST_LOG`.
    //
    // Two layers compose under one EnvFilter:
    //   1. `fmt::layer` — stderr/logcat output (devs + crash.log readers).
    //   2. `LogCaptureLayer` — in-memory ring buffer feeding the in-app
    //      Diagnostics panel and the "Copy diagnostic info" button.
    let log_buffer = diagnostics::LogBuffer::new();
    let capture_layer = diagnostics::LogCaptureLayer::new(log_buffer.clone());
    {
        use tracing_subscriber::layer::SubscriberExt;
        use tracing_subscriber::util::SubscriberInitExt;
        // Default to `info` for our crate. The previous default of
        // `corvux=debug` caused tool args/results to land in the in-memory
        // log buffer (and thence into diagnostic bundles) for every user.
        // Devs who want verbose logs set `RUST_LOG=corvux=debug` explicitly.
        let filter = tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
        let _ = tracing_subscriber::registry()
            .with(filter)
            .with(tracing_subscriber::fmt::layer().with_target(true))
            .with(capture_layer.clone())
            .try_init();
    }

    // Panic handler — log crashes to a file so they're not silent in release.
    //
    // We rotate `crash.log` lazily before each append so the file can never
    // grow unbounded across years of installs. Threshold = 1 MiB; we keep
    // up to 3 rotated files (`crash.log.1`/`.2`/`.3`). A single panic
    // message is a few KB, so the 1 MiB limit is generous — a user would
    // have to crash hundreds of times to fill one rotation.
    const CRASH_LOG_MAX_BYTES: u64 = 1 << 20; // 1 MiB
    const CRASH_LOG_KEEP: u32 = 3;
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // CRASH-PII: redact absolute paths (drive layout + OS username) and
        // secret-shaped tokens from the panic text BEFORE it touches disk.
        // The panic payload can interpolate `path.display()` / an io::Error
        // carrying `W:\Users\<name>\Roost\...`; the old hook wrote that raw,
        // so crash.log held unredacted PII even though the diagnostics
        // *export* redacts by default. For a privacy-first product the
        // on-disk artifact must be clean too — a user sending it to support
        // (or a backup/screen-share picking it up) shouldn't leak their disk
        // layout. `redact_text` keeps the useful bits (relative file:line,
        // error type) and only tokenizes path/secret shapes.
        let raw = format!(
            "[PANIC] {}\nLocation: {:?}\nTime: {:?}\n\n",
            info,
            info.location(),
            std::time::SystemTime::now()
        );
        let msg = crate::diagnostics::redact_text(&raw);
        // AUDIT-043b: prefer the canonical app_data_dir (set during
        // `paths::initialize` in the tauri setup callback). Fall back
        // to the legacy `BaseDirs` path only when a panic fires
        // BEFORE setup runs — that early-init window is when the
        // OnceLock is still empty, and writing to the legacy path
        // there is no worse than the prior behaviour. After this
        // session, `paths::run_legacy_migrations` moves any legacy
        // crash.log into the canonical dir on first launch, so the
        // diagnostics reader and the next panic land in the same
        // place.
        let log_dir = crate::paths::crash_log_dir().unwrap_or_else(|| {
            directories::BaseDirs::new()
                .map(|d| d.data_dir().join("corvux"))
                .unwrap_or_else(std::env::temp_dir)
        });
        {
            let _ = std::fs::create_dir_all(&log_dir);
            let log_path = log_dir.join("crash.log");

            // Rotate if the existing log is over the size cap. Failure
            // here is silently ignored — we'd rather risk an oversized
            // log than skip recording a panic.
            if let Ok(meta) = std::fs::metadata(&log_path) {
                if meta.len() >= CRASH_LOG_MAX_BYTES {
                    // crash.log.{N-1} → crash.log.{N}, walking from the
                    // oldest down, then crash.log → crash.log.1.
                    for i in (1..CRASH_LOG_KEEP).rev() {
                        let from = log_dir.join(format!("crash.log.{i}"));
                        let to = log_dir.join(format!("crash.log.{}", i + 1));
                        let _ = std::fs::rename(&from, &to);
                    }
                    let _ = std::fs::rename(&log_path, log_dir.join("crash.log.1"));
                }
            }

            let _ = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&log_path)
                .and_then(|mut f| std::io::Write::write_all(&mut f, msg.as_bytes()));
        }
        // tao's Win32 event loop occasionally panics during shutdown with
        // "cannot move state from Destroyed" (a stray message after the
        // loop is destroyed). The app is already exiting, so it's benign —
        // but the abort skips `RunEvent::Exit`, leaving the session marker
        // behind and false-flagging "closed unexpectedly" on next launch.
        // Recognise that one panic and clear the marker so the next launch
        // treats it as the clean exit it was. Real crashes (any other
        // message) still leave the marker and are surfaced.
        if crate::diagnostics::is_benign_shutdown_panic(&msg) {
            crate::diagnostics::clear_session_marker_standalone();
        }
        // Also run the default handler. Skipped on Windows because the
        // NSIS-installed Corvux runs as a Windows GUI subsystem app — no
        // console attached, so stderr is closed. The default hook's
        // `eprintln!` then panics with "pipe is being closed" inside the
        // panic-handling context, recursively poisoning crash.log with
        // dozens of secondary panics and masking the original cause. The
        // information is already captured above (crash.log + the in-app
        // diagnostics ring buffer), so dropping the stderr write loses
        // nothing in that build mode. Kept on macOS / Linux / `tauri dev`
        // where the binary still has a usable stderr for live debugging.
        #[cfg(not(target_os = "windows"))]
        default_hook(info);
        #[cfg(target_os = "windows")]
        let _ = &default_hook;
    }));

    // tauri-specta: build the type-export surface. See
    // `specta_bridge::build` for the strategy comment.
    //
    // The builder owns BOTH the invoke handler and the typed-event
    // mount API. We pull `invoke_handler()` out eagerly (returns an
    // owned closure that doesn't borrow the builder) so the builder
    // itself can move into the `setup` closure where `mount_events`
    // needs an `AppHandle`. Without this split, the closure would
    // capture `specta_builder` AND we'd still need it for
    // `.invoke_handler(...)` — fighting the borrow checker for no
    // reason.
    let specta_builder = specta_bridge::build();
    // Mobile debug builds can't write to ../src/lib at runtime (read-only FS),
    // and the host dev loop regenerates bindings.ts anyway.
    //
    // A failed export is dev-only and must NOT abort the app: it only
    // happens when the build's CWD isn't `src-tauri/` (so `../src/lib`
    // resolves wrong) or that path is read-only. The committed
    // `bindings.ts` is the source of truth; regeneration is a convenience.
    // The previous `.expect()` here turned an Access-Denied into a
    // startup crash (the "failed to export tauri-specta bindings" panics
    // in crash.log). Downgraded to a warning.
    #[cfg(all(debug_assertions, not(any(target_os = "android", target_os = "ios"))))]
    if let Err(e) = specta_builder.export(specta_bridge::exporter(), specta_bridge::BINDINGS_PATH) {
        tracing::warn!(error = ?e, "specta: bindings.ts export skipped (dev-only; using committed bindings)");
    }
    let specta_invoke_handler = specta_builder.invoke_handler();

    let builder = tauri::Builder::default();

    // Single-instance MUST be the first plugin registered (per the plugin's
    // own docs — it intercepts the second launch before anything else spins
    // up). On a second desktop launch this focuses the window that already
    // owns the roost instead of starting a doomed second process that would
    // hit the roost OS-lock and surface ROOST_OPEN_FAIL. Desktop-only: on
    // mobile the OS sandbox already guarantees a single process and the
    // plugin isn't built for those targets.
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    let builder = builder.plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
        if let Some(window) = app.get_webview_window("main") {
            let _ = window.unminimize();
            let _ = window.set_focus();
        }
    }));

    let builder = builder
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_saf::init())
        .plugin(tauri_plugin_multicast_lock::init())
        .plugin(tauri_plugin_ios_picker::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_process::init());

    // Auto-updater is desktop-only upstream (Win/macOS/Linux). Mobile
    // distribution goes through APK side-loads / TestFlight / store
    // submissions, not the Tauri updater channel. The frontend store
    // (`stores/updater.ts`) cfg-checks platform before invoking the
    // updater commands, so the plugin's absence on mobile is benign.
    //
    // Empty `pubkey` / `endpoints` in `tauri.conf.json` are intentional
    // — that's the "not yet configured" state. Founder fills them in as
    // the first step of `docs/release-process.md` before cutting v0.1.0.
    // The pubkey-empty warning fires from inside `.setup()` below where
    // `app.config()` is cheap.
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    let builder = {
        // "Launch at login" registration. Desktop-only — mobile has no
        // equivalent. The actual enable/disable is driven from Rust via
        // `ManagerExt::autolaunch()` in `reconcile_autostart`, so the
        // webview never invokes the plugin's IPC commands directly and
        // no autostart capability/permission is required.
        let builder = builder.plugin(tauri_plugin_autostart::Builder::new().build());
        // Store-packaged (MSIX) installs must NOT self-update: the Store
        // owns updates there (policy), and a website NSIS update landing
        // beside the MSIX recreates the dual-install "two windows" state
        // (2026-07-09 incident — two app identities). Register the
        // updater plugin only for classic installs; the frontend updater
        // store mirrors this via the `is_store_packaged` command.
        if crate::diagnostics::running_from_store_package() {
            builder
        } else {
            builder.plugin(tauri_plugin_updater::Builder::new().build())
        }
    };

    builder
        .manage(diagnostics::LogBufferState(log_buffer))
        .manage(perf::PerfCountersState::new())
        // State that the setup closure needs to reach must be `manage`d
        // before `.setup(...)` runs. The notification scheduler in
        // GraphDb is the first concrete dependency — install() needs
        // the AppHandle from inside setup. Other state declarations
        // also moved up here for ordering symmetry.
        .manage(GraphDb::new())
        .manage(commands::files::RoostFsState::default())
        .manage(SearchState::default())
        .manage(LlamaCppState(Mutex::new(None)))
        .manage(commands::sync::SyncEngineState::default())
        .manage(commands::index_watcher::IndexWatcherState::default())
        .setup(move |app| {
            // Mount the tauri-specta typed-event channel. Has to happen
            // inside `setup` (needs an `AppHandle`) and BEFORE any
            // backend code emits a typed event — otherwise the listener
            // map is empty and the event silently dropped. Wire compat
            // with legacy `app.emit("name", ...)` is preserved: typed
            // and untyped listeners see the same JSON over the same
            // event-name channel.
            specta_builder.mount_events(app);

            // Bind the app handle into the log capture layer so future
            // tracing events also reach the frontend via `log:event`.
            // Earlier events (before this point) are still in the ring
            // buffer and surface on the next `get_recent_logs` pull.
            capture_layer.set_app(app.handle().clone());

            // AUDIT-043 / 043b / 043c: capture the canonical app-data
            // root into the process-wide OnceLock so code that runs
            // without an AppHandle (the panic hook, the mobile secret
            // store) reaches the same path. Then move any files from
            // the three legacy "rogue" locations into the canonical
            // layout — eager-at-boot so a user who installs, downloads
            // the model, and uninstalls before opening Settings still
            // gets cleaned up by the OS on uninstall.
            if let Err(e) = crate::paths::initialize(app.handle()) {
                tracing::error!(error = %e, "paths::initialize failed — legacy locations remain in use this session");
            } else {
                let report = crate::paths::run_legacy_migrations();
                if report.models_moved + report.crash_logs_moved + report.secrets_moved > 0
                    || !report.errors.is_empty()
                {
                    tracing::info!(
                        models = report.models_moved,
                        crash_logs = report.crash_logs_moved,
                        secrets = report.secrets_moved,
                        errors = report.errors.len(),
                        "paths::run_legacy_migrations: moved files into canonical app_data_dir"
                    );
                    for e in &report.errors {
                        tracing::warn!(error = %e, "paths::migrate: per-file error");
                    }
                }

                // MR-B3: reclaim orphaned per-roost index dirs left behind when a
                // roost was removed from the registry without "delete index".
                // Conservative: skips entirely unless the manifest defines a
                // non-empty registered set (see `gc_orphaned_index_dirs`).
                let gc = crate::paths::gc_orphaned_index_dirs(app.handle());
                if let Some(reason) = &gc.skipped {
                    tracing::debug!(reason = %reason, "paths::gc: skipped orphaned-index sweep");
                } else if !gc.removed.is_empty() || !gc.errors.is_empty() {
                    tracing::info!(
                        removed = gc.removed.len(),
                        errors = gc.errors.len(),
                        "paths::gc: reclaimed orphaned index dirs"
                    );
                    for e in &gc.errors {
                        tracing::warn!(error = %e, "paths::gc: per-dir error");
                    }
                }
            }

            // iOS: exclude the secret + sync-identity dirs from the device's
            // iCloud/Finder backup (the iOS analogue of Android
            // allowBackup=false). Best-effort; never blocks launch. Runs after
            // the legacy migration so the post-migration `secrets/` dir is the
            // one we mark.
            crate::ios_backup::harden_sensitive_dirs();

            // Window creation lives here (not in tauri.conf.json) because
            // hidden_title is a creation-time property that can't be set
            // afterwards. Per-platform chrome:
            //   macOS  → transparent title-bar overlay. The frontend
            //            reserves the traffic-light area in its left
            //            chrome so Files/Search/Bookmarks can sit on the
            //            same row as the native controls, matching
            //            Obsidian's window chrome.
            //   other  → borderless (decorations:false). TabBar's custom
            //            in-app min/max/close (see TabBar.tsx) owns the
            //            chrome.
            //
            // Linux follows the Windows path — same custom-chrome design
            // works there and avoids depending on whatever native window
            // manager the user happens to run.
            // The desktop-only chain (title, sizes, center, resizable,
            // decorations, hidden_title) is gated so iOS / Android still
            // compile — those methods don't exist on the mobile
            // WebviewWindowBuilder (mobile windows are full-screen and
            // OS-managed, no title bar to style).
            let builder = WebviewWindowBuilder::new(app, "main", WebviewUrl::default());

            #[cfg(not(any(target_os = "ios", target_os = "android")))]
            let builder = builder
                .title("Corvux")
                .inner_size(1200.0, 800.0)
                .min_inner_size(800.0, 600.0)
                .center()
                .resizable(true);

            #[cfg(target_os = "macos")]
            let builder = builder
                .hidden_title(true)
                .title_bar_style(tauri::TitleBarStyle::Overlay)
                .traffic_light_position(tauri::LogicalPosition::new(16.0, 20.0));

            #[cfg(all(
                not(any(target_os = "ios", target_os = "android")),
                not(target_os = "macos")
            ))]
            let builder = builder.decorations(false);

            let _main_window = builder.build()?;

            // macOS WKWebView does NOT expose `navigator.mediaDevices` by
            // default — `macOSPrivateApi: true` in tauri.conf.json controls
            // transparency + fullscreen only. Without flipping the private
            // WKPreferences flags below, `getUserMedia` fails with
            // "navigator.mediaDevices is undefined" and our voice-memo
            // recorder can't request the mic. The keys exist on every
            // macOS WebKit shipped to date and are toggled by Safari Tech
            // Preview the same way. Wrapped in `with_webview` so we drive
            // the *actual* WKWebView pointer wry handed us, not a pre-build
            // configuration object that would be too late.
            //
            // A-06 hardening (2026-05-26):
            //   - autoreleasepool wraps the block. `stringWithUTF8String:`
            //     and `numberWithBool:` return autoreleased instances;
            //     `setValue:forKey:` retains them, so even without a pool
            //     this is one-shot at startup, but the explicit pool keeps
            //     us honest if this code ever moves out of `setup`.
            //   - respondsToSelector:@selector(configuration) probe so a
            //     future wry change (wrapping the WKWebView in a view) doesn't
            //     crash at launch; we fall back to a no-op instead.
            #[cfg(target_os = "macos")]
            {
                use objc::rc::autoreleasepool;
                use objc::runtime::{Object, BOOL, YES};
                use objc::{class, msg_send, sel, sel_impl};
                let _ = _main_window.with_webview(|wv| unsafe {
                    autoreleasepool(|| {
                        let wkwebview = wv.inner() as *mut Object;
                        if wkwebview.is_null() {
                            return;
                        }
                        let responds: BOOL =
                            msg_send![wkwebview, respondsToSelector: sel!(configuration)];
                        if responds != YES {
                            tracing::warn!(
                                "macOS mic: WKWebView pointer doesn't respond to \
                                 -configuration; skipping mediaDevices flip. \
                                 (wry hands out an unexpected pointer type?)"
                            );
                            return;
                        }
                        let config: *mut Object = msg_send![wkwebview, configuration];
                        let prefs: *mut Object = msg_send![config, preferences];
                        // Expose navigator.mediaDevices.* (getUserMedia, enumerate).
                        let key_media: *mut Object = msg_send![
                            class!(NSString),
                            stringWithUTF8String: c"mediaDevicesEnabled".as_ptr()
                        ];
                        let yes: *mut Object =
                            msg_send![class!(NSNumber), numberWithBool: true];
                        let _: () = msg_send![prefs, setValue: yes forKey: key_media];
                        // tauri://localhost isn't HTTPS; WKWebView's default
                        // "secure connection required for capture" gate refuses
                        // getUserMedia on non-secure origins. Drop the gate.
                        let key_secure: *mut Object = msg_send![
                            class!(NSString),
                            stringWithUTF8String: c"mediaCaptureRequiresSecureConnection".as_ptr()
                        ];
                        let no: *mut Object =
                            msg_send![class!(NSNumber), numberWithBool: false];
                        let _: () = msg_send![prefs, setValue: no forKey: key_secure];
                    });
                });
            }

            // Crash detection — order matters:
            //   1) Read the previous session's marker into a cached
            //      snapshot. While the marker still reflects the *prior*
            //      run, capture (crashed?, crash.log excerpt) into
            //      `PreviousSessionState`.
            //   2) Write the marker for the *current* session.
            //
            // The frontend's `check_previous_crash` IPC reads from the
            // cached state, NOT the file. Reading the file from the IPC
            // would race against this setup write and report `crashed:
            // true` on every launch (false positive — was the actual
            // bug behind the recurring "Corvux closed unexpectedly"
            // dialog after a clean exit).
            let prev_session = diagnostics::capture_previous_session_state(app.handle());
            app.manage(diagnostics::PreviousSessionState(prev_session));
            diagnostics::mark_session_started(app.handle());

            // Loud-warn if the updater is wired but unconfigured. A
            // forgotten release-process step (empty pubkey or endpoints
            // in tauri.conf.json) would otherwise ship a binary that
            // silently never finds updates — exactly the showstopper the
            // Fri 2026-05-15 dry-run is meant to catch. Better to surface
            // it on every launch.
            #[cfg(not(any(target_os = "android", target_os = "ios")))]
            {
                let plugins = &app.config().plugins.0;
                let pubkey_set = plugins
                    .get("updater")
                    .and_then(|v| v.get("pubkey"))
                    .and_then(|v| v.as_str())
                    .map(|s| !s.is_empty())
                    .unwrap_or(false);
                let endpoints_set = plugins
                    .get("updater")
                    .and_then(|v| v.get("endpoints"))
                    .and_then(|v| v.as_array())
                    .map(|a| !a.is_empty())
                    .unwrap_or(false);
                if !(pubkey_set && endpoints_set) {
                    tracing::warn!(
                        "updater: pubkey={pubkey_set} endpoints={endpoints_set} \
                         in tauri.conf.json — auto-update is INACTIVE for this \
                         build. See docs/release-process.md to enable."
                    );
                }
            }

            // Initialize settings from app data dir
            let data_dir = app
                .path()
                .app_data_dir()
                .expect("failed to resolve app data dir");
            std::fs::create_dir_all(&data_dir).ok();
            let settings_path = data_dir.join("settings.json");
            let mut settings: Settings = if settings_path.exists() {
                let content = std::fs::read_to_string(&settings_path).unwrap_or_default();
                serde_json::from_str(&content).unwrap_or_default()
            } else {
                Settings::default()
            };

            // Hydrate the Gemini API key from the OS keychain when available.
            // Migration: if we see a key in settings.json AND the platform has a
            // keychain, copy it to the keychain and strip it from disk. This
            // runs once per install — subsequent launches load straight from
            // the keychain.
            if secret_store::is_supported() {
                let disk_key = settings.gemini_api_key.take();
                let kc_key = secret_store::get(secret_store::keys::GEMINI_API_KEY)
                    .ok()
                    .flatten();
                let resolved = match (kc_key, disk_key) {
                    (Some(k), _) => Some(k),
                    (None, Some(k)) if !k.is_empty() => {
                        // Migrate: write to keychain, then persist the cleared
                        // settings.json so we don't keep the plaintext copy.
                        if let Err(e) = secret_store::set(secret_store::keys::GEMINI_API_KEY, &k) {
                            tracing::warn!("keychain migration failed, leaving key on disk: {e}");
                            Some(k)
                        } else {
                            if let Ok(json) = serde_json::to_string_pretty(&settings) {
                                let _ = std::fs::write(&settings_path, json);
                            }
                            Some(k)
                        }
                    }
                    _ => None,
                };
                settings.gemini_api_key = resolved;

                // Same migration shape for the OpenRouter key — added in
                // the multi-provider slice. Keychain is the source of
                // truth on platforms that have one; disk is fallback.
                let or_disk_key = settings.openrouter_api_key.take();
                let or_kc_key = secret_store::get(secret_store::keys::OPENROUTER_API_KEY)
                    .ok()
                    .flatten();
                let or_resolved = match (or_kc_key, or_disk_key) {
                    (Some(k), _) => Some(k),
                    (None, Some(k)) if !k.is_empty() => {
                        if let Err(e) =
                            secret_store::set(secret_store::keys::OPENROUTER_API_KEY, &k)
                        {
                            tracing::warn!(
                                "openrouter keychain migration failed, leaving key on disk: {e}"
                            );
                            Some(k)
                        } else {
                            if let Ok(json) = serde_json::to_string_pretty(&settings) {
                                let _ = std::fs::write(&settings_path, json);
                            }
                            Some(k)
                        }
                    }
                    _ => None,
                };
                settings.openrouter_api_key = or_resolved;

                // S-03: Ollama bearer token — same migration shape.
                // Most users never set this (default Ollama is
                // localhost-unauth), but anyone running Ollama behind
                // a proxy now gets keychain storage instead of
                // plaintext settings.json.
                let ol_disk_key = settings.ollama_api_key.take();
                let ol_kc_key = secret_store::get(secret_store::keys::OLLAMA_API_KEY)
                    .ok()
                    .flatten();
                let ol_resolved = match (ol_kc_key, ol_disk_key) {
                    (Some(k), _) => Some(k),
                    (None, Some(k)) if !k.is_empty() => {
                        if let Err(e) =
                            secret_store::set(secret_store::keys::OLLAMA_API_KEY, &k)
                        {
                            tracing::warn!(
                                "ollama keychain migration failed, leaving key on disk: {e}"
                            );
                            Some(k)
                        } else {
                            if let Ok(json) = serde_json::to_string_pretty(&settings) {
                                let _ = std::fs::write(&settings_path, json);
                            }
                            Some(k)
                        }
                    }
                    _ => None,
                };
                settings.ollama_api_key = ol_resolved;

                // OpenAI direct-provider key — same migration shape.
                let oa_disk_key = settings.openai_api_key.take();
                let oa_kc_key = secret_store::get(secret_store::keys::OPENAI_API_KEY)
                    .ok()
                    .flatten();
                let oa_resolved = match (oa_kc_key, oa_disk_key) {
                    (Some(k), _) => Some(k),
                    (None, Some(k)) if !k.is_empty() => {
                        if let Err(e) = secret_store::set(secret_store::keys::OPENAI_API_KEY, &k) {
                            tracing::warn!(
                                "openai keychain migration failed, leaving key on disk: {e}"
                            );
                            Some(k)
                        } else {
                            if let Ok(json) = serde_json::to_string_pretty(&settings) {
                                let _ = std::fs::write(&settings_path, json);
                            }
                            Some(k)
                        }
                    }
                    _ => None,
                };
                settings.openai_api_key = oa_resolved;

                // Anthropic (Claude) direct-provider key — same shape.
                let an_disk_key = settings.anthropic_api_key.take();
                let an_kc_key = secret_store::get(secret_store::keys::ANTHROPIC_API_KEY)
                    .ok()
                    .flatten();
                let an_resolved = match (an_kc_key, an_disk_key) {
                    (Some(k), _) => Some(k),
                    (None, Some(k)) if !k.is_empty() => {
                        if let Err(e) = secret_store::set(secret_store::keys::ANTHROPIC_API_KEY, &k)
                        {
                            tracing::warn!(
                                "anthropic keychain migration failed, leaving key on disk: {e}"
                            );
                            Some(k)
                        } else {
                            if let Ok(json) = serde_json::to_string_pretty(&settings) {
                                let _ = std::fs::write(&settings_path, json);
                            }
                            Some(k)
                        }
                    }
                    _ => None,
                };
                settings.anthropic_api_key = an_resolved;
            }

            // Apply the persisted launch-at-startup choice to the OS
            // before `settings` moves into state. Covers three cases in
            // one call: fresh install (default ON → register), upgrade
            // from a pre-field settings.json (serde default ON →
            // register), and an existing user who turned it off (stays
            // off). No-op on mobile.
            let launch_at_startup = settings.launch_at_startup;

            app.manage(SettingsState {
                path: settings_path,
                settings: Mutex::new(settings),
            });

            reconcile_autostart(app.handle(), launch_at_startup);
            app.manage(ChatCancel {
                flag: Arc::new(AtomicBool::new(false)),
                task: Mutex::new(None),
            });
            app.manage(crate::ai::tools::approval::PendingApprovals::new());

            // Skill registry + hot-reload watcher (Phase 2.B). Initially
            // bound with no roost → only the global ~/.corvux/skills
            // dir is scanned. open_roost rebinds the watcher to the
            // freshly-opened roost's .corvux/skills/ once it lands.
            let skill_registry =
                std::sync::Arc::new(crate::ai::skills::SkillRegistry::new());
            let initial_watcher = crate::ai::skills::spawn_default_watcher(
                skill_registry.clone(),
                None,
            )
            .ok();
            app.manage(crate::commands::ai::skills::SkillRegistryState(
                skill_registry,
            ));
            app.manage(crate::commands::ai::skills::SkillWatcherState(
                std::sync::Mutex::new(initial_watcher),
            ));

            // Subagent registry + hot-reload watcher (Phase 2.C). Same
            // initial-binding story as skills: global-only on startup,
            // roost rebind on open_roost. Built-ins (the bundled
            // `general` subagent) are always available even with no
            // disk presence — see `ai::subagents::builtin`.
            let subagent_registry =
                std::sync::Arc::new(crate::ai::subagents::SubagentRegistry::new());
            let initial_subagent_watcher = crate::ai::subagents::spawn_default_watcher(
                subagent_registry.clone(),
                None,
            )
            .ok();
            app.manage(crate::commands::ai::subagents::SubagentRegistryState(
                subagent_registry,
            ));
            app.manage(crate::commands::ai::subagents::SubagentWatcherState(
                std::sync::Mutex::new(initial_subagent_watcher),
            ));

            // Shared HTTP client — one connection pool for all cloud providers.
            // `User-Agent` is useful for HuggingFace rate-limit friendliness when
            // we start downloading ONNX models.
            let http = reqwest::Client::builder()
                .user_agent(concat!("Corvux/", env!("CARGO_PKG_VERSION")))
                .build()
                .map_err(|e| format!("Failed to build HTTP client: {e}"))?;
            app.manage(HttpClient(Arc::new(http)));

            // Embeddings subsystem removed — retrieval is FTS-driven (the
            // agent's `roost_search` tool + the Search panel both use the
            // SQLite FTS5 index). No background embedding runs, so there is
            // no roost-wide ONNX pass to OOM on low-RAM devices. The
            // drainer module is removed in stage 2 of the teardown.

            // Wire the app handle into the task notification scheduler
            // so it can fire desktop OS notifications when `📅 + ⏰`
            // tasks come due. Without this the scheduler silently no-ops.
            // The actual timer rebuild happens after `init_graph` runs;
            // the handle just has to be in place by then.
            if let Some(graph_db) = app.try_state::<GraphDb>() {
                graph_db
                    .notification_scheduler
                    .install(app.handle().clone());
            }

            // macOS: request notification authorization up front via the modern
            // UserNotifications API, so the system "Allow notifications?" prompt
            // appears once on first launch. The plugin's legacy desktop path
            // never prompts. No-op in dev (no app bundle). See
            // src/notification_macos.rs.
            #[cfg(target_os = "macos")]
            notification_macos::request_authorization();

            // Native app menu disabled — custom in-app TopBar owns the
            // window chrome (drag region + min/max/close + title). Keeping
            // the native File/Edit/View/Help bar alongside `decorations:
            // false` produced a stacked triple-bar at the top of the
            // window. Keyboard shortcuts continue to work via
            // `lib/keyboard-shortcuts.ts` — the menu items were
            // duplicative discoverability, not the source of truth.
            // `menu::build` retained in the module for re-enablement.

            // System tray (desktop only — Android/iOS have no tray
            // surface, and the `tray-icon` feature isn't compiled there).
            // Left-click restores the window; right-click opens a menu with
            // explicit Open / Quit. The icon reuses the app's window icon,
            // so there's no extra asset to ship. Windows tucks the icon
            // into the overflow flyout by default — the user drags it onto
            // the always-shown row themselves; an app can't force that.
            //
            // The tray is always created, independent of `close_to_tray`:
            // that setting only governs what the window's X does (hide vs
            // quit). Quit here is the always-available real exit, routed
            // through the same graceful `shutdown_and_exit` as a close.
            #[cfg(desktop)]
            {
                use tauri::menu::{Menu, MenuItem};
                use tauri::tray::{
                    MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent,
                };

                let open_item =
                    MenuItem::with_id(app, "tray_open", "Open Corvux", true, None::<&str>)?;
                let quit_item =
                    MenuItem::with_id(app, "tray_quit", "Quit Corvux", true, None::<&str>)?;
                let tray_menu = Menu::with_items(app, &[&open_item, &quit_item])?;

                let mut tray = TrayIconBuilder::with_id("corvux-tray")
                    .tooltip("Corvux")
                    .menu(&tray_menu)
                    // Left-click restores the window; the menu is on
                    // right-click so a single tap doesn't pop a menu the
                    // user has to dismiss before seeing their notes.
                    .show_menu_on_left_click(false)
                    .on_menu_event(|app, event| match event.id.as_ref() {
                        "tray_open" => show_main_window(app),
                        "tray_quit" => shutdown_and_exit(app.clone()),
                        _ => {}
                    })
                    .on_tray_icon_event(|tray, event| {
                        if let TrayIconEvent::Click {
                            button: MouseButton::Left,
                            button_state: MouseButtonState::Up,
                            ..
                        } = event
                        {
                            show_main_window(tray.app_handle());
                        }
                    });
                if let Some(icon) = app.default_window_icon() {
                    tray = tray.icon(icon.clone());
                }
                tray.build(app)?;
            }

            Ok(())
        })
        // C11: dispatch every IPC call through the tauri-specta builder.
        // `collect_commands![...]` in `specta_bridge::build` is now the
        // single source of truth for the IPC surface — adding a new
        // command means annotating it with `#[specta::specta]` and
        // appending to that macro. The legacy `tauri::generate_handler!`
        // block lived here during the per-slice migration (C2–C10) so
        // each commit could land independently; it's gone now that the
        // specta surface covers every command.
        .invoke_handler(specta_invoke_handler)
        .on_window_event(|window, event| {
            // Pressing the window's X does one of two things, per the
            // user's `close_to_tray` preference:
            //
            //   * ON  → hide the window to the system tray and keep the
            //           process running. Sync stays live, so the device
            //           keeps receiving deltas while "closed" and
            //           reopening from the tray is instant. NO shutdown
            //           and NO crash-marker clear — we're still up.
            //   * OFF → graceful full shutdown (sync teardown: listener
            //           socket, peer connections, mDNS) on a 2 s deadline,
            //           then exit. The pre-tray behaviour.
            //
            // `shutdown_and_exit` runs teardown off the GUI thread (the
            // old in-line `block_on` froze the window for up to 2 s) and
            // flips `SHUTTING_DOWN`. The re-entrant `CloseRequested` that
            // its `app.exit(0)` fires is recognised by that flag here and
            // let through. The same guard means a Quit-from-tray that's
            // already running won't be undone by this handler.
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                use std::sync::atomic::Ordering::SeqCst;
                // Re-entrant close from our own `app.exit(0)` — let it close.
                if SHUTTING_DOWN.load(SeqCst) {
                    return;
                }

                let close_to_tray = window
                    .app_handle()
                    .try_state::<SettingsState>()
                    .and_then(|s| s.settings.lock().ok().map(|g| g.close_to_tray))
                    .unwrap_or(false);

                if close_to_tray {
                    // Hide to tray — stay alive, don't tear anything down.
                    api.prevent_close();
                    let _ = window.hide();
                    return;
                }

                // Real close: prevent the synchronous destroy so the GUI
                // loop stays responsive while the async teardown runs.
                api.prevent_close();
                shutdown_and_exit(window.app_handle().clone());
            }
        })
        .build(tauri::generate_context!())
        .expect("error while building Corvux")
        .run(|app_handle, event| {
            // Cmd+Q / app-menu Quit / Dock→Quit fire RunEvent::ExitRequested,
            // NOT the window's CloseRequested — so the on_window_event handler
            // above never clears the crash marker on a normal quit, and the
            // next launch false-flags an "unclean shutdown". Clear it here on
            // the final Exit, which fires for BOTH the window-close →
            // app.exit(0) path and the menu/Cmd+Q path. A genuine crash —
            // panic (writes crash.log then aborts), force-kill, OOM, power
            // loss — never reaches RunEvent::Exit, so it still leaves the
            // marker and is correctly surfaced. (Idempotent with the
            // window-close handler's own mark_session_ended call.)
            match event {
                tauri::RunEvent::Exit => {
                    diagnostics::mark_session_ended(app_handle);
                }
                // The app returned to the foreground (mobile background→
                // resume; desktop unminimize/refocus). While suspended the
                // QUIC sync link almost certainly idled out AND the engine's
                // reconnect supervisor timer was frozen, so nothing redials
                // until its next tick — and per-peer backoff may have climbed
                // to minutes. Force an immediate, backoff-resetting redial so
                // returning to the app reconnects at once. `Resumed` is
                // propagated on mobile as of Tauri v2.11; on desktop it fires
                // on focus regain. Spawned on the background runtime because
                // this callback carries no tokio context on Windows release
                // builds (see background_runtime docs).
                tauri::RunEvent::Resumed => {
                    let app = app_handle.clone();
                    background_runtime::handle().spawn(async move {
                        let state = app.state::<commands::sync::SyncEngineState>();
                        let guard = state.engine.lock().await;
                        if let Some(engine) = guard.as_ref() {
                            engine.force_reconnect_all().await;
                        }
                    });
                }
                _ => {}
            }
        });
}
