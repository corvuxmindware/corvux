import { createSignal, createRoot } from "solid-js";
import { isTauri, commands, unwrap } from "../lib/invoke";
import { errMsg } from "../lib/error";
import { isMobile } from "../lib/platform";
import { lsGet, lsSet } from "../lib/storage";
import { log } from "../lib/log";

/**
 * Auto-updater store — desktop-only.
 *
 * Wraps `@tauri-apps/plugin-updater` so the rest of the app touches a
 * single store with project conventions (signals, `available()`,
 * `progress()`) instead of importing the plugin module everywhere.
 *
 * Mobile + browser builds early-return with `available = null` from
 * every entry point — the plugin is not registered on those targets
 * (see `lib.rs` cfg-gate), and side-load distribution channels handle
 * their own update story.
 *
 * Lifecycle:
 *   1. App.tsx mount → `scheduleStartupCheck()` waits 30s after launch
 *      so the roost open + warm-up phases finish first, then runs one
 *      `check()` in the background. Failures are silent — surfaced
 *      only via the Settings → About "Check now" button.
 *   2. If an update is found, `available()` flips to a non-null value
 *      and the TopBar `<UpdateBanner />` renders.
 *   3. User clicks "Restart & install" → `installAndRelaunch()` runs
 *      `downloadAndInstall()` (with progress), then `relaunch()` from
 *      `@tauri-apps/plugin-process`. NSIS on Windows handles relaunch
 *      itself; the explicit `relaunch()` covers macOS / Linux paths so
 *      the call site stays uniform.
 *   4. User clicks "Skip this version" → version id is persisted to
 *      localStorage so we don't keep nagging on every launch.
 */

export type UpdateStatus =
  | { kind: "idle" }
  | { kind: "checking" }
  | { kind: "available"; version: string; currentVersion: string; notes: string; date: string | null }
  | { kind: "downloading"; downloaded: number; total: number | null }
  | { kind: "installing" }
  | { kind: "error"; message: string }
  | { kind: "up-to-date" };

const SKIP_KEY = "corvux:updater:skipVersion";
const LAST_CHECK_KEY = "corvux:updater:lastCheckMs";
const STARTUP_DELAY_MS = 30_000;
// Don't run a background check more often than once an hour even if
// the user reopens the app rapidly. Manual checks via Settings always
// bypass this.
const MIN_AUTO_CHECK_INTERVAL_MS = 60 * 60 * 1000;

function isUpdaterSupported(): boolean {
  // Browser dev mode and mobile WebViews never have the updater plugin.
  return isTauri && !isMobile();
}

// ── Microsoft Store (MSIX) gate ─────────────────────────────────────
// A Store-packaged install must not self-update: the Store owns updates
// (policy), and a website NSIS update landing beside the MSIX recreates
// the dual-install "two windows" incident of 2026-07-09. The backend
// also skips registering the updater plugin when packaged (lib.rs);
// this flag keeps the UX honest — background checks stay silent and the
// manual button explains where updates come from.
const [storePackaged, setStorePackaged] = createSignal(false);
let storePackagedProbe: Promise<void> | null = null;

function probeStorePackaged(): Promise<void> {
  if (!isUpdaterSupported()) return Promise.resolve();
  storePackagedProbe ??= (async () => {
    try {
      setStorePackaged(await unwrap(commands.isStorePackaged()));
    } catch (e) {
      // Treat probe failure as a classic install — worst case is the
      // old behavior (a check against the website manifest).
      log.warn("updater", "store-packaged probe failed", e);
    }
  })();
  return storePackagedProbe;
}

function readSkipped(): string | null {
  try {
    return lsGet(SKIP_KEY);
  } catch {
    return null;
  }
}

function writeSkipped(version: string) {
  try {
    lsSet(SKIP_KEY, version);
  } catch {
    /* ignore */
  }
}

function readLastCheck(): number {
  try {
    const raw = lsGet(LAST_CHECK_KEY);
    return raw ? parseInt(raw, 10) || 0 : 0;
  } catch {
    return 0;
  }
}

function writeLastCheck(now: number) {
  try {
    lsSet(LAST_CHECK_KEY, String(now));
  } catch {
    /* ignore */
  }
}

function createUpdaterStore() {
  const [status, setStatus] = createSignal<UpdateStatus>({ kind: "idle" });

  /**
   * Run a single update check. `manual = true` skips the
   * skipped-version filter and the throttle so the user can always
   * force a check from Settings. Returns the resulting status; the
   * signal is updated either way.
   */
  async function check(opts?: { manual?: boolean }): Promise<UpdateStatus> {
    const manual = opts?.manual === true;
    if (!isUpdaterSupported()) {
      const next: UpdateStatus = {
        kind: "error",
        message: "Auto-update not available on this platform.",
      };
      setStatus(next);
      return next;
    }
    await probeStorePackaged();
    if (storePackaged()) {
      // Store installs update through the Microsoft Store. Background
      // checks stay silent; the manual button says why nothing happens.
      const next: UpdateStatus = manual
        ? {
            kind: "error",
            message: "This install updates through the Microsoft Store.",
          }
        : { kind: "up-to-date" };
      setStatus(next);
      return next;
    }
    setStatus({ kind: "checking" });
    writeLastCheck(Date.now());

    try {
      const { check: checkUpdate } = await import("@tauri-apps/plugin-updater");
      const update = await checkUpdate();
      if (!update) {
        const next: UpdateStatus = { kind: "up-to-date" };
        setStatus(next);
        return next;
      }
      // Plugin returns Update object with .version, .currentVersion,
      // .body (release notes), .date (RFC3339-ish or null).
      const skipped = manual ? null : readSkipped();
      if (skipped && skipped === update.version) {
        const next: UpdateStatus = { kind: "up-to-date" };
        setStatus(next);
        return next;
      }
      const next: UpdateStatus = {
        kind: "available",
        version: update.version,
        currentVersion: update.currentVersion,
        notes: update.body ?? "",
        date: update.date ?? null,
      };
      setStatus(next);
      return next;
    } catch (e) {
      const message = errMsg(e);
      if (!manual) {
        // Background checks must not show a scary banner on every
        // launch — common during the pre-publish window when the
        // remote manifest is a placeholder or 404s. Log for the
        // founder's dev console; surface to UI only on manual check.
        log.warn("updater", "background check failed", message);
        const next: UpdateStatus = { kind: "up-to-date" };
        setStatus(next);
        return next;
      }
      const next: UpdateStatus = { kind: "error", message };
      setStatus(next);
      return next;
    }
  }

  /**
   * Download + install + relaunch. Caller should already have observed
   * an `available` status from `check()`. Re-runs the check internally
   * so the plugin object (which is not held across calls) is fresh.
   */
  async function installAndRelaunch(): Promise<void> {
    if (!isUpdaterSupported()) return;
    await probeStorePackaged();
    if (storePackaged()) return;
    try {
      const { check: checkUpdate } = await import("@tauri-apps/plugin-updater");
      const update = await checkUpdate();
      if (!update) {
        setStatus({ kind: "up-to-date" });
        return;
      }
      // Clear the crash-detection marker BEFORE installing. The Windows
      // NSIS updater force-closes us and relaunches; that teardown races
      // with tao's window destroy and can panic ("cannot move state from
      // Destroyed"), which under `panic = "abort"` skips the normal
      // clean-exit marker clear and false-flags an "unclean shutdown" on
      // the next launch. An install+relaunch is an intentional shutdown,
      // so we mark the session ended up front. Best-effort — a failure
      // here must not block the update.
      try {
        await unwrap(commands.markUpdateRelaunch());
      } catch (e) {
        log.warn("updater", "markUpdateRelaunch failed (non-fatal)", e);
      }
      let downloaded = 0;
      let total: number | null = null;
      setStatus({ kind: "downloading", downloaded: 0, total: null });
      await update.downloadAndInstall((event) => {
        switch (event.event) {
          case "Started":
            total = event.data.contentLength ?? null;
            setStatus({ kind: "downloading", downloaded: 0, total });
            break;
          case "Progress":
            downloaded += event.data.chunkLength;
            setStatus({ kind: "downloading", downloaded, total });
            break;
          case "Finished":
            setStatus({ kind: "installing" });
            break;
        }
      });
      // Relaunch — Windows NSIS already exits + relaunches inside
      // downloadAndInstall, so this is a no-op there. macOS and Linux
      // need it. Plugin-process is in the deps for exactly this reason.
      const { relaunch } = await import("@tauri-apps/plugin-process");
      await relaunch();
    } catch (e) {
      const message = errMsg(e);
      setStatus({ kind: "error", message });
    }
  }

  /**
   * Mark the currently-available version as skipped so the banner
   * stops nagging. Manual "Check for updates" still surfaces it.
   */
  function skipCurrent() {
    const s = status();
    if (s.kind === "available") {
      writeSkipped(s.version);
      setStatus({ kind: "up-to-date" });
    }
  }

  /**
   * Schedule the post-launch background check. Idempotent — multiple
   * calls collapse to one timer. Honors the throttle: if a check ran
   * within the last hour, skip and return.
   */
  let startupTimer: number | null = null;
  function scheduleStartupCheck() {
    if (!isUpdaterSupported()) return;
    if (startupTimer !== null) return;
    const sinceLast = Date.now() - readLastCheck();
    if (sinceLast < MIN_AUTO_CHECK_INTERVAL_MS) return;
    startupTimer = window.setTimeout(() => {
      startupTimer = null;
      void check();
    }, STARTUP_DELAY_MS) as unknown as number;
  }

  return {
    status,
    /** Non-null only when an unsnoozed update is ready to install. */
    available: () => {
      const s = status();
      return s.kind === "available" ? s : null;
    },
    /** Non-null only when a download is in progress. */
    downloading: () => {
      const s = status();
      return s.kind === "downloading" ? s : null;
    },
    /** Non-null only when the last action surfaced an error. */
    error: () => {
      const s = status();
      return s.kind === "error" ? s : null;
    },
    isChecking: () => status().kind === "checking",
    isInstalling: () => {
      const k = status().kind;
      return k === "downloading" || k === "installing";
    },
    /** Download progress 0..1, or null when total bytes are unknown
     *  / the plugin reported indeterminate length. */
    downloadProgress: (): number | null => {
      const s = status();
      if (s.kind !== "downloading") return null;
      if (s.total === null || s.total <= 0) return null;
      return Math.min(1, s.downloaded / s.total);
    },
    check,
    installAndRelaunch,
    skipCurrent,
    scheduleStartupCheck,
    /** True when this build supports auto-update at all. */
    isSupported: isUpdaterSupported,
  };
}

export const updaterStore = createRoot(createUpdaterStore);
