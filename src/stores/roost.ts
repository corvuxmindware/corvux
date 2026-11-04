import { createSignal, createRoot } from "solid-js";
import { lsGet, lsSet, lsRemove } from "../lib/storage";
import { log } from "../lib/log";
import {
  commands,
  isTauri,
  setMockRoostPath,
  setBrowserRoost,
  isBrowserRoostBound,
  unwrap,
  type FileEntry,
  type OpenRoostResponse,
} from "../lib/invoke";
import { toastStore } from "../components/Toast/Toast";
import {
  roostRegistry,
  deriveName,
  resolveBookmark,
  isScopeExpiredError,
} from "./roosts";
import { startupStore } from "./startup";
import { saveRoostNote } from "../lib/save";
import { newNoteDestDir } from "../lib/new-note-dest";

// Re-exported from the generated tauri-specta bindings so existing
// `import { FileEntry } from "stores/roost"` call sites keep working.
// Source of truth lives in `src-tauri/src/commands/files.rs` (FileEntry)
// and `commands/roost.rs` (OpenRoostResponse).
export type { FileEntry, OpenRoostResponse };

/// Count markdown files in a (possibly nested) file tree. Pure — exported
/// so the welcome-seed decision can be unit-tested without booting the
/// store singleton.
export function countMdFiles(entries: FileEntry[]): number {
  return entries.reduce((acc, e) => {
    if (e.is_dir) return acc + countMdFiles(e.children);
    return acc + (e.name.endsWith(".md") ? 1 : 0);
  }, 0);
}

/// Decide whether `openRoost` should drop a starter `Welcome to Corvux.md`
/// into a freshly-opened roost. Pure predicate — the single source of truth
/// for the rule, kept testable on its own.
///
/// Seed only when ALL hold:
///   • the roost has zero markdown files (don't litter a populated folder), and
///   • it's a writable native roost (SAF `content://` can't create yet), and
///   • the caller didn't opt out (`skipWelcome`).
///
/// `skipWelcome` exists for the onboarding flow: onboarding has its own
/// consented, opt-out-aware welcome-note step (the checkbox), so the auto-seed
/// must stand down there — otherwise both fire and the user gets a duplicate
/// (`Welcome to Corvux 1.md`) AND unchecking the box fails to yield an empty
/// roost. Outside onboarding the auto-seed is the only path, so it runs.
export function shouldSeedWelcome(
  entries: FileEntry[],
  opts: { isSaf: boolean; skipWelcome: boolean },
): boolean {
  if (opts.isSaf || opts.skipWelcome) return false;
  return countMdFiles(entries) === 0;
}

/// Cheap perf landmarks. Wrapping `performance.mark` lets us no-op on
/// platforms without the User Timing API and centralizes the namespace
/// (`roost:*`) so devtools can filter on it. Zero allocation when
/// `performance` is unavailable.
function perfMark(name: string) {
  if (typeof performance !== "undefined" && performance.mark) {
    try { performance.mark(name); } catch { /* ignore */ }
  }
}

function perfMeasure(name: string, startMark: string) {
  if (typeof performance !== "undefined" && performance.measure) {
    try {
      performance.measure(name, startMark);
    } catch {
      // measure throws if startMark is missing; not fatal.
    }
  }
}

/**
 * Roost lifecycle store — the source of truth for "which roost is open
 * right now and what's inside it". One roost is active at a time; opening
 * a new roost drops the previous backend (and releases its OS lock on
 * desktop). Browser-mode dev replaces the Tauri IPC backend with a File
 * System Access handle but exposes the same surface here.
 */
function createRoostStore() {
  const [roostPath, setRoostPath] = createSignal<string | null>(null);
  // Human-readable roost name — set when a SAF/dialog picker returns a
  // display name, otherwise derived from the path's last segment. The
  // sidebar switcher and window title read this; the store persists it
  // alongside the path so it survives restarts.
  const [roostDisplayName, setRoostDisplayName] = createSignal<string | null>(
    null,
  );
  const [files, setFiles] = createSignal<FileEntry[]>([]);
  const [activeFile, setActiveFile] = createSignal<string | null>(null);
  const [activeContent, setActiveContent] = createSignal<string>("");
  const [originalContent, setOriginalContent] = createSignal<string>("");
  const [isModified, setIsModified] = createSignal(false);
  // True while open_roost + welcome-note + reload are in flight. Lets the
  // sidebar show a "Opening roost…" indicator instead of a blank panel
  // when a large roost is being scanned for the first time.
  const [isOpening, setIsOpening] = createSignal(false);
  // Set when open_roost fails with SCOPE_EXPIRED — an iOS security-scoped
  // bookmark that can't be resumed (stale after an app update / iCloud
  // change). The app shell shows a "re-grant access" prompt that re-runs
  // the document picker for this roost. Null = no re-auth needed.
  const [reauthRequest, setReauthRequest] = createSignal<{
    path: string;
    displayName: string;
  } | null>(null);
  const [roostGeneration, setRoostGeneration] = createSignal(0);
  let openRequest = 0;

  /**
   * Open the roost at `path` and load its file tree.
   *
   * Branches:
   * - `content://...` paths route to the Android SAF backend on Android,
   *   error on every other platform.
   * - `/browser-roost/...` paths re-pick a `FileSystemDirectoryHandle`
   *   from the user (browser-mode dev only).
   * - Anything else is treated as a native filesystem path.
   *
   * On a fresh native roost we drop a `Welcome to Corvux.md` note so the
   * editor isn't an empty-state black hole. The optional `displayName`
   * comes from the SAF picker (where the URI's last segment is encoded
   * and unreadable); for native roosts we derive it from the path.
   */
  async function openRoost(
    path: string,
    displayName?: string,
    bookmark?: string,
    opts?: { silent?: boolean; skipWelcome?: boolean },
  ) {
    // Same-path re-open: the post-open effect in app/effects.ts is
    // `createEffect(on(roostPath, ...))` and won't refire when the
    // signal value is unchanged. Without this short-circuit, the
    // spinner-clearing chain (`runStartupSequence` → setPhase("ready"))
    // never runs, and the TopBar shows "Opening roost…" forever.
    // Callers that just want a tree refresh should use `refreshFiles`.
    const currentPath = roostPath();
    if (currentPath && currentPath === path) {
      // User explicitly re-opened — bypass the focus-event debounce.
      await refreshFiles({ force: true });
      return;
    }
    const request = ++openRequest;
    const superseded = () => request !== openRequest;
    const restorePreviousBackend = async () => {
      if (!currentPath || currentPath === path || superseded()) return;
      const previousBookmark = resolveBookmark(
        undefined,
        roostRegistry.roosts(),
        currentPath,
      );
      const rollback = await commands.openRoost(
        currentPath,
        roostDisplayName(),
        previousBookmark ?? null,
      );
      if (rollback.status === "ok" && !superseded()) {
        setRoostGeneration(rollback.data.generation);
        setFiles(rollback.data.entries);
        lastTreeHash = hashFileTree(rollback.data.entries);
      } else if (rollback.status === "error" && !superseded()) {
        // We can no longer honestly claim the previous root is active.
        await commands.closeRoost();
        setRoostPath(null);
        setRoostGeneration(0);
        setFiles([]);
        lastTreeHash = "";
      }
    };
    setIsOpening(true);
    // AUDIT-060: cancel any pending auto-save timers from the previous
    // roost. Without this, a timer scheduled against an old roost path
    // can fire after the switch and write to (or fail against) the new
    // roost's filesystem.
    {
      const { tabStore } = await import("./tabs");
      // Awaited: flush the old roost's modified tabs to disk before the
      // switch tears it down (paths are still valid here). AUDIT-060 intact.
      const flushed = await tabStore.clearAutoSaveTimers();
      if (!flushed) {
        setIsOpening(false);
        toastStore.show(
          "Could not switch roosts because one or more notes failed to save. Your edits remain open.",
          "warning",
          7000,
        );
        return;
      }
    }
    // Drive the global startup phase so the TopBar indicator can show
    // "Opening roost…". The downstream phases are sequenced from the
    // post-open effect in app/effects.ts.
    startupStore.setPhase("roost");
    // Perf instrumentation — landmarks for the roost-open journey. Read
    // them via `performance.getEntriesByType("measure")` in the dev
    // console to see actual numbers without sprinkling console.time
    // calls. Cheap (zero-cost when no tools are attached).
    perfMark("roost:open:start");
    try {
      // Browser dev mode: a `/browser-roost/...` path needs a live FSAccess
      // handle to back the file ops. Page reload kills the handle, so when a
      // registry entry is re-opened (Manage Roosts / RoostSwitcher) we re-pick
      // the folder. The picker call counts as a user gesture because openRoost
      // here was kicked off by a click handler.
      if (
        !isTauri &&
        path.startsWith("/browser-roost/") &&
        !isBrowserRoostBound(path)
      ) {
        if (!("showDirectoryPicker" in window)) {
          toastStore.show(
            "This browser doesn't support the folder picker. Use Tauri dev or Chrome/Edge.",
            "error",
          );
          return;
        }
        const handle = await (
          window as Window & {
            showDirectoryPicker: (
              opts?: object,
            ) => Promise<FileSystemDirectoryHandle>;
          }
        ).showDirectoryPicker({ mode: "readwrite" });
        // Accept whatever the user picks. If they pick a folder with a
        // different name, treat it as the new bound path — the old registry
        // entry just becomes stale and can be removed manually.
        const newPath = `/browser-roost/${handle.name}`;
        setBrowserRoost(handle, newPath);
        path = newPath;
        displayName = handle.name;
      }

      // If the caller didn't thread a bookmark (e.g. RoostSwitcher, a tree
      // refresh re-open), fall back to the one stored in the registry for
      // this path. iOS scoped roosts need it to re-acquire access before
      // the backend touches the filesystem — without it, open_roost stats
      // the raw path and iOS denies it (EPERM / ROOST_OPEN_FAIL).
      bookmark = resolveBookmark(bookmark, roostRegistry.roosts(), path);

      const initial = await commands.openRoost(path, displayName ?? null, bookmark ?? null);
      if (superseded()) return;
      if (initial.status === "error") {
        // iOS scoped bookmark went stale — the OS won't re-grant access from
        // a persisted bookmark. DON'T clear the roost or show a raw error;
        // surface a re-grant prompt that re-runs the document picker for this
        // same roost (ReauthPrompt in the app shell watches reauthRequest).
        if (isScopeExpiredError(initial.error)) {
          await restorePreviousBackend();
          if (superseded()) return;
          setReauthRequest({ path, displayName: displayName ?? deriveName(path) });
          startupStore.setPhase("idle");
          return; // `finally` resets isOpening

        }
        throw initial.error;
      }
      let response: OpenRoostResponse = initial.data;
      perfMeasure("roost:open:ipc", "roost:open:start");

      // iOS scoped roosts: the picker plugin may hand us a refreshed
      // bookmark (stale-bookmark recovery) or relocated path (user moved
      // the folder via Files.app). Use those as the canonical values
      // for both this session and persistence.
      const effectivePath = response.resolvedPath ?? path;
      const refreshedBookmark = response.bookmark ?? bookmark;

      const resolvedName =
        (displayName && displayName.trim()) || deriveName(effectivePath);

      // Welcome-note creation is only safe for writable native roosts.
      // SAF roosts (content://) don't support create yet — Slice 5 lands it.
      // Onboarding opts out (skipWelcome) — it seeds via its own consented,
      // opt-out-aware step, so auto-seeding here would duplicate the note.
      const isSaf = effectivePath.startsWith("content://");
      let entries = response.entries;
      if (shouldSeedWelcome(entries, { isSaf, skipWelcome: opts?.skipWelcome ?? false })) {
        await createWelcomeNote(effectivePath);
        if (superseded()) return;
        const after = await commands.openRoost(
          effectivePath,
          displayName ?? null,
          refreshedBookmark ?? null,
        );
        if (after.status === "error") throw after.error;
        if (superseded()) return;
        response = after.data;
        entries = response.entries;
      }

      if (superseded()) return;
      if (currentPath && currentPath !== effectivePath) {
        const { tabStore } = await import("./tabs");
        if (superseded()) return;
        tabStore.clearTabsAfterRoostSwitch();
      }
      setActiveFile(null);
      setActiveContent("");
      setOriginalContent("");
      setIsModified(false);
      setRoostGeneration(response.generation);
      setRoostPath(effectivePath);
      setRoostDisplayName(resolvedName);
      lsSet("corvux:lastVaultName", resolvedName);
      // Keep the multi-roost registry in sync only after the complete open
      // transaction wins the frontend request race.
      const registryType = refreshedBookmark ? "ios-scoped" : undefined;
      roostRegistry.registerOpened(effectivePath, resolvedName, {
        bookmark: refreshedBookmark,
        type: registryType,
      });
      if (path !== effectivePath) roostRegistry.updatePath(path, effectivePath);
      setFiles(entries);
      lastTreeHash = hashFileTree(entries);
      perfMeasure("roost:open:setFiles", "roost:open:start");
      lsSet("corvux:lastVault", effectivePath);
      if (refreshedBookmark) {
        lsSet("corvux:lastVaultBookmark", refreshedBookmark);
      } else {
        lsRemove("corvux:lastVaultBookmark");
      }
    } catch (e) {
      await restorePreviousBackend();
      if (superseded()) return;
      log.error("roost", "failed to open roost", e);
      if (!opts?.silent) {
        toastStore.showError(`Failed to open roost: ${e}`, e, {
          code: "ROOST_OPEN_FAIL",
          module: "stores.roost.openRoost",
        });
      }
      // Open failed → no roost path will be set → the post-open effect
      // never fires → the phase indicator would otherwise stay stuck on
      // "Opening roost…" forever. Reset to idle so the UI is honest.
      startupStore.setPhase("idle");
      // Re-throw so silent callers (restoreLastRoost) can branch on the
      // failure and clean up stale persisted state. Loud callers (UI
      // buttons) already had a toast shown above; their existing
      // try/catch wrappers swallow the rethrow.
      if (opts?.silent) throw e;
    } finally {
      if (!superseded()) setIsOpening(false);
    }
  }

  async function createWelcomeNote(roost: string) {
    const path = `${roost}/Welcome to Corvux.md`;
    const content = `---
tags:
  - welcome
  - getting-started
---

# Welcome to Corvux

Your AI-native second brain. Here's how to get started:

## Quick Start

- **Create a note** — Press \`Ctrl+N\` or click the + button in the sidebar
- **Daily note** — Press \`Ctrl+D\` to create today's journal
- **Link notes** — Type \`[[\` to link to another note
- **Add tags** — Use \`#tag\` syntax to categorize notes

## Navigation

- \`Ctrl+O\` — Quick switcher (jump to any note)
- \`Ctrl+P\` — Command palette
- \`Ctrl+Shift+F\` — Search across your roost
- \`Ctrl+G\` — Graph view (see your knowledge network)
- \`Ctrl+/\` — Keyboard shortcut cheat sheet

## Corvux Agent

Press \`Ctrl+L\` to open the Corvux Agent. You can:
- Ask questions about your open note
- Search your roost semantically ("What did I write about X?")
- Get summaries, action items, and insights

> [!tip] Set your Gemini API key in the chat panel to enable AI features.

## Links in Action

Try creating a new note and linking back here: [[Welcome to Corvux]]

This note itself has the tags #welcome and #getting-started — they show as colored pills in the editor.

---

*Built with love by Corvux Mindware*
`;
    try {
      await unwrap(commands.createFile(path));
      await unwrap(commands.writeFile(path, content));
    } catch (e) {
      log.error("roost", "failed to create welcome note", e);
    }
  }

  // Re-read the file tree for the currently-open roost without running
  // the full openRoost path (no welcome-note creation, no registry bump).
  //
  // Cost guards (post-ANR-on-Android learning):
  // 1. **In-flight guard**: a SAF walk on a Vivo I2126 measured 5–7s
  //    for 213 entries. If a refresh is already running, a second
  //    refresh that races behind it will also take 5–7s — back-to-back
  //    walks pin the WebView's effective input pipeline, and Android's
  //    input dispatcher fires an ANR after 5s of unprocessed taps.
  //    Coalesce concurrent callers onto the in-flight promise.
  // 2. **Debounce**: focus events on Android fire eagerly (every tab/IME
  //    swap, every dialog dismiss). 10s is long enough that a focus
  //    burst doesn't re-walk; mutation paths bypass with `force: true`.
  // 3. **Skip during initial open**: if `isOpening` is true the user
  //    is already waiting on the cold open — a focus-driven refresh
  //    behind it is pure cost.
  // 4. **Structural diff**: only push the new entries into the `files`
  //    signal if the tree actually changed. A no-op `setFiles` would
  //    cascade re-renders through the sidebar virtualizer.
  let lastRefreshAt = 0;
  const REFRESH_DEBOUNCE_MS = 10_000;
  let lastTreeHash = "";
  let inflightRefresh: Promise<void> | null = null;
  async function refreshFiles(opts?: { force?: boolean }): Promise<void> {
    const path = roostPath();
    const generation = roostGeneration();
    if (!path) return;
    if (isOpening() && !opts?.force) {
      // Initial cold open is already in flight — piggyback on its result.
      return;
    }
    if (inflightRefresh) {
      // Coalesce concurrent callers. The in-flight refresh will pick up
      // any tree changes that landed between when it started and now;
      // a second walk would be redundant work that competes for the
      // SAF stack with the open we're already waiting on.
      return inflightRefresh;
    }
    const now = Date.now();
    if (!opts?.force && now - lastRefreshAt < REFRESH_DEBOUNCE_MS) {
      return;
    }
    lastRefreshAt = now;

    inflightRefresh = (async () => {
      try {
        // Scope is already active from the initial open; we don't
        // re-pass the bookmark on warm refresh since resume_access is
        // idempotent and we'd rather skip the round-trip on each focus.
        const r = await commands.openRoost(
          path,
          roostDisplayName(),
          null,
        );
        if (r.status === "error") {
          log.warn("roost", "refreshFiles failed", r.error);
          return;
        }
        if (roostPath() !== path || roostGeneration() !== generation) return;
        if (r.data.generation !== generation) return;
        const hash = hashFileTree(r.data.entries);
        if (hash !== lastTreeHash) {
          lastTreeHash = hash;
          setFiles(r.data.entries);
        }
      } catch (e) {
        log.warn("roost", "refreshFiles failed", e);
      } finally {
        inflightRefresh = null;
      }
    })();
    return inflightRefresh;
  }

  /// Patch the file tree from a backend-pushed revalidation. Called by
  /// the `roost://tree-changed` listener in `effects.ts` after the Phase
  /// B background walk detects drift from the cached tree. Validated as
  /// a `FileEntry[]` shape; anything malformed is dropped silently
  /// (matches the rest of the IPC error-handling convention).
  function applyTreeFromBackend(payload: unknown) {
    if (!payload || typeof payload !== "object") return;
    const event = payload as {
      roostPath?: unknown;
      generation?: unknown;
      entries?: unknown;
    };
    if (
      event.roostPath !== roostPath() ||
      event.generation !== roostGeneration() ||
      !Array.isArray(event.entries)
    ) return;
    // Trust the backend's serializer — it produces the same shape
    // `open_roost` returns. Cast through `as` rather than runtime-check
    // every nested field; a bad payload here is a backend bug and
    // would have surfaced the moment `roostStore.files()` was rendered.
    const entries = event.entries as FileEntry[];
    const hash = hashFileTree(entries);
    if (hash !== lastTreeHash) {
      lastTreeHash = hash;
      setFiles(entries);
    }
  }

  // Cheap structural fingerprint — joins every path + is_dir bit so adds,
  // removes, and renames register as different. Doesn't track file sizes
  // or mtimes; the tree shape is what the sidebar cares about.
  function hashFileTree(entries: FileEntry[]): string {
    const parts: string[] = [];
    function walk(es: FileEntry[]) {
      for (const e of es) {
        parts.push(e.is_dir ? `D:${e.path}` : `F:${e.path}`);
        if (e.is_dir) walk(e.children);
      }
    }
    walk(entries);
    return parts.join("|");
  }

  async function closeRoost() {
    ++openRequest;
    const { tabStore } = await import("./tabs");
    if (!(await tabStore.closeAllTabs())) {
      toastStore.show(
        "Could not close the roost because one or more notes failed to save. Your edits remain open.",
        "warning",
        7000,
      );
      return;
    }
    const closed = await commands.closeRoost();
    if (closed.status === "error") throw closed.error;
    setRoostPath(null);
    setRoostDisplayName(null);
    setRoostGeneration(0);
    setFiles([]);
    setActiveFile(null);
    setActiveContent("");
    setOriginalContent("");
    setIsModified(false);
    lastTreeHash = "";
    lsRemove("corvux:lastVault");
    lsRemove("corvux:lastVaultName");
    lsRemove("corvux:lastVaultBookmark");
  }

  /**
   * Re-open whichever roost the user had open last, using the path saved
   * to localStorage. Called once at app startup. No-op if no previous
   * roost was recorded — onboarding handles the first-time-launch flow
   * separately.
   */
  async function restoreLastRoost() {
    const last = lsGet("corvux:lastVault");
    const lastName = lsGet("corvux:lastVaultName");
    const lastBookmark = lsGet("corvux:lastVaultBookmark");
    if (!isTauri) {
      if (last) {
        // Restore whichever roost the user previously picked in browser mode.
        if (last !== "/mock-roost") setMockRoostPath(last);
        try {
          await openRoost(last, lastName ?? undefined, undefined, { silent: true });
        } catch (e) {
          if (isDeadPathError(e)) clearStaleLastRoost(last);
        }
      }
      // No stored roost → onboarding gate already shows the picker; nothing to do.
      return;
    }
    if (last) {
      try {
        await openRoost(
          last,
          lastName ?? undefined,
          lastBookmark ?? undefined,
          { silent: true },
        );
      } catch (e) {
        // Clear ONLY when the persisted path is genuinely dead — deleted
        // folder, rotated iOS Data container UUID, revoked iCloud scope.
        // Recoverable failures (the most common one being "roost is
        // locked because another Corvux instance has it open") leave the
        // breadcrumb intact so the user can retry once the conflict is
        // resolved.
        if (isDeadPathError(e)) clearStaleLastRoost(last);
      }
    }
  }

  /// Is this open-roost failure a "the path is gone" error vs a
  /// transient lock conflict? We pattern-match on the Rust error
  /// strings — there's no stable error code yet, so adding new dead-
  /// path messages here is the maintenance cost. Anything not on this
  /// list is treated as recoverable.
  function isDeadPathError(err: unknown): boolean {
    const msg =
      err instanceof Error ? err.message : typeof err === "string" ? err : String(err);
    return (
      /does not exist/i.test(msg) ||
      /not a directory/i.test(msg) ||
      /is a symlink/i.test(msg) ||
      /no such file/i.test(msg) ||
      /bookmark/i.test(msg)
    );
  }

  /// Drop every stored reference to a roost path that no longer opens.
  function clearStaleLastRoost(path: string) {
    lsRemove("corvux:lastVault");
    lsRemove("corvux:lastVaultName");
    lsRemove("corvux:lastVaultBookmark");
    const entry = roostRegistry
      .roosts()
      .find((v) => v.path === path);
    if (entry) roostRegistry.remove(entry.id);
  }

  /**
   * Open a note in the editor. Saves the previously-active note first if
   * it was modified — losing edits to a tab switch is the kind of
   * silent-data-loss bug we don't want.
   */
  async function openFile(path: string) {
    const generation = roostGeneration();
    // Save current file if modified
    if (isModified() && activeFile()) {
      await saveCurrentFile();
    }

    try {
      const content = await unwrap(commands.readFile(path));
      if (!roostGeneration() || roostGeneration() !== generation) return;
      setActiveFile(path);
      setActiveContent(content);
      setOriginalContent(content);
      setIsModified(false);
    } catch (e) {
      log.error("roost", "failed to read file", e);
      toastStore.showError(`File not found or unreadable`, e, {
        code: "FILE_READ_FAIL",
        module: "stores.roost.openFile",
      });
    }
  }

  function updateContent(content: string) {
    setActiveContent(content);
    setIsModified(content !== originalContent());
  }

  /**
   * Persist the active editor buffer to disk via the `write_file` IPC.
   * The Rust side does an atomic `tmp + fsync + rename` so a mid-write
   * crash leaves the previous content intact. Updates `originalContent`
   * + `isModified` so the dirty-dot in the tab/topbar clears.
   */
  async function saveCurrentFile() {
    const path = activeFile();
    if (!path) return;
    const content = activeContent();
    const generation = roostGeneration();

    try {
      // AUDIT-053: route markdown saves through the bundled
      // save_roost_note IPC so this legacy save path (only hit when
      // switching files with unsaved changes — the modern tab flow
      // owns the user-visible save UX) is consistent with the rest.
      const lower = path.toLowerCase();
      if (lower.endsWith(".md") || lower.endsWith(".markdown")) {
        await saveRoostNote(path, content);
      } else {
        await unwrap(commands.writeFile(path, content));
      }
      if (!roostGeneration() || roostGeneration() !== generation) return;
      setOriginalContent(content);
      setIsModified(false);
    } catch (e) {
      log.error("roost", "failed to save file", e);
      toastStore.showError(`Failed to save file`, e, {
        code: "FILE_WRITE_FAIL",
        module: "stores.roost.saveCurrentFile",
      });
    }
  }

  /**
   * Create an empty note and refresh the file tree. Returns the absolute
   * path of the new note (so callers can open a tab on it) or `null` if
   * there's no roost open. Adds the `.md` extension automatically.
   *
   * Destination folder: an explicit `destDir` wins (e.g. the sidebar
   * "New Note Here" on a folder). Otherwise the note lands in the folder
   * of the note currently open — "create where I'm working" — falling
   * back to the roost root when nothing (or a non-file tab like Graph) is
   * open. Centralised here so every entry point (sidebar button, ⌘N, the
   * command palette, the mobile prompt, wiki-links) shares the behaviour.
   */
  async function createNewNote(
    name: string,
    destDir?: string,
  ): Promise<string | null> {
    const roost = roostPath();
    if (!roost) return null;

    // Resolve the destination. Dynamic import of the tabs store avoids a
    // static roost → tabs import cycle.
    let dir: string;
    if (destDir != null) {
      dir = destDir;
    } else {
      const { tabStore } = await import("./tabs");
      dir = newNoteDestDir(tabStore.activeTabPath(), roost);
    }
    const fileName = name.endsWith(".md") ? name : `${name}.md`;
    const sep = dir.includes("\\") ? "\\" : "/";
    const path = `${dir.replace(/[\\/]+$/, "")}${sep}${fileName}`;

    try {
      await unwrap(commands.createFile(path));
      // create_file writes the note to disk but does NOT index it — the
      // graph is only touched on save / move / delete. Without this, a
      // freshly created note has no graph node (missing from backlinks,
      // graph view, search, AI retrieval) until the user first edits and
      // saves it. Index it now. Dynamic import avoids a static
      // roost → graph store import cycle.
      void import("./graph").then(({ graphStore }) => graphStore.indexPath(path));
      await openRoost(roost);
      return path;
    } catch (e) {
      log.error("roost", "failed to create note", e);
      return null;
    }
  }

  /**
   * Move a note to `<roost>/.corvux-trash/<ts>.<name>` (recoverable).
   * Also removes the note from the FTS5 search index and clears the
   * editor if the deleted note was the active one. The Rust side handles
   * graph-DB cleanup.
   */
  async function deleteNote(path: string) {
    const roost = roostPath();
    if (!roost) return;

    try {
      await unwrap(commands.deleteFile(path, roost));
      // Remove from FTS5 search index
      commands.removeFromSearch(path).catch((e) =>
        log.error("roost", "FTS5 remove failed", e)
      );
      if (activeFile() === path) {
        setActiveFile(null);
        setActiveContent("");
        setOriginalContent("");
        setIsModified(false);
      }
      await openRoost(roost);
    } catch (e) {
      log.error("roost", "failed to delete note", e);
    }
  }

  // Count all .md files recursively
  function noteCount(): number {
    function count(entries: FileEntry[]): number {
      return entries.reduce((acc, e) => {
        if (e.is_dir) return acc + count(e.children);
        return acc + 1;
      }, 0);
    }
    return count(files());
  }

  // Get just the filename from a path
  function activeFileName(): string {
    const path = activeFile();
    if (!path) return "";
    return path.split("/").pop() || "";
  }

  return {
    roostPath,
    roostDisplayName,
    roostGeneration,
    files,
    activeFile,
    activeContent,
    isModified,
    isOpening,
    reauthRequest,
    clearReauthRequest: () => setReauthRequest(null),
    openRoost,
    closeRoost,
    openFile,
    updateContent,
    saveCurrentFile,
    createNewNote,
    deleteNote,
    noteCount,
    activeFileName,
    restoreLastRoost,
    refreshFiles,
    applyTreeFromBackend,
  };
}

export const roostStore = createRoot(createRoostStore);
