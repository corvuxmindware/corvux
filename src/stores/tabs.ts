import { createSignal, createRoot } from "solid-js";
import { commands, unwrap } from "../lib/invoke";
import { log } from "../lib/log";
import { saveRoostNote, SAVE_CONFLICT_PREFIX, type SaveOutcome } from "../lib/save";
import { sha256Hex } from "../lib/hash";
import { isMobile } from "../lib/platform";
import { toastStore } from "../components/Toast/Toast";

export type TabKind = "file" | "graph";

export interface Tab {
  path: string;
  name: string;
  content: string;
  originalContent: string;
  isModified: boolean;
  kind: TabKind;
  /** When true, the editor is locked (read mode). See `defaultReadOnly`. */
  readOnly: boolean;
}

// Sentinel path for the singleton Graph tab. Looks like a URI so it can't
// collide with a real filesystem path.
export const GRAPH_TAB_PATH = "corvux://graph";
export function isFileTab(tab: Tab | undefined): boolean {
  return !!tab && tab.kind === "file";
}

/** Last path segment regardless of separator — handles Windows `\` and
 *  POSIX `/` in the same call. */
function basename(path: string): string {
  const m = path.match(/[^/\\]+$/);
  return m ? m[0] : path;
}

/**
 * Which mode a note opens in when the caller doesn't say.
 *
 * Desktop opens LOCKED (read mode): the unlock toggle sits in the tab bar
 * right next to the note, `Ctrl+E` flips it, and read-first matches how
 * Obsidian-style desktop clients behave.
 *
 * Mobile opens UNLOCKED (write mode). The locked default was a dead end on
 * a phone: the only unlock affordance is a small padlock in the top bar
 * (`TopBar.tsx`), so tapping the note body did nothing at all — no caret,
 * no keyboard — and `MobileFormatBar` is itself gated on write mode, so the
 * formatting buttons that would have taught the user never appeared either.
 * Non-technical testers read that as "I can't type notes here". Opening in
 * write mode makes the first tap behave like every other mobile note app;
 * the padlock still works for anyone who wants read mode.
 *
 * Exported for the unit test — keep it a pure function of `isMobile()`.
 */
export function defaultReadOnly(mobile = isMobile()): boolean {
  return !mobile;
}

function createTabStore() {
  const [tabs, setTabs] = createSignal<Tab[]>([]);
  const [activeTabPath, setActiveTabPath] = createSignal<string | null>(null);
  const [recentFiles, setRecentFiles] = createSignal<string[]>([]);

  // Auto-save: debounce timer per tab path
  const autoSaveTimers = new Map<string, ReturnType<typeof setTimeout>>();
  // One promise chain per note. Hash-gated backend saves are deliberately
  // sequential: a second save must hash against the first save's committed
  // base, not race it with the stale base both calls originally loaded.
  const saveQueues = new Map<string, Promise<SaveOutcome | null>>();
  // AUDIT-053: the post-save callback now receives the bundled
  // `SaveOutcome` from the new `save_roost_note` IPC (when the file is a
  // markdown roost note). Non-markdown writes (legacy `write_file` path)
  // pass `outcome: null` — caller falls back to its prior behaviour.
  let onAfterSave:
    | ((path: string, content: string, outcome: SaveOutcome | null) => void)
    | null = null;

  function setOnAfterSave(
    cb: (path: string, content: string, outcome: SaveOutcome | null) => void,
  ) {
    onAfterSave = cb;
  }

  function scheduleAutoSave(path: string) {
    // Virtual tabs (graph) aren't backed by files — nothing to save.
    const current = tabs().find((t) => t.path === path);
    if (!isFileTab(current)) return;

    // Clear any existing timer for this path.
    const existing = autoSaveTimers.get(path);
    if (existing) clearTimeout(existing);

    // Save + graph index: 500ms after last keystroke. The old 2000ms
    // made sync feel sluggish — user typed a word, then waited two
    // full seconds before it even started propagating. 500ms still
    // collapses normal typing bursts into one save, keeping disk I/O
    // and graph-reindex cost bounded.
    //
    // Embedding is NOT scheduled here: `index_note` (run by
    // `onAfterSave`) enqueues the note in the backend `embed_queue`,
    // which the embed-drainer batches every 10s. A separate frontend
    // 10s timer used to fire here too — that doubled Gemini API calls
    // and raced the queue's in-flight bookkeeping.
    const timer = setTimeout(async () => {
      autoSaveTimers.delete(path);
      const tab = tabs().find((t) => t.path === path);
      if (tab && tab.isModified) {
        // AUDIT-053: saveTab now fires onAfterSave itself, so every
        // save (auto-save, Cmd+S, palette) flows through one path.
        // No double-firing here.
        await saveTab(path);
      }
    }, 500);
    autoSaveTimers.set(path, timer);
  }

  function activeTab(): Tab | undefined {
    return tabs().find((t) => t.path === activeTabPath());
  }

  function activeContent(): string {
    return activeTab()?.content ?? "";
  }

  function isModified(): boolean {
    return activeTab()?.isModified ?? false;
  }

  function isReadOnly(): boolean {
    return activeTab()?.readOnly ?? false;
  }

  function toggleReadOnly() {
    const path = activeTabPath();
    if (!path) return;
    setTabs((prev) =>
      prev.map((t) => (t.path === path ? { ...t, readOnly: !t.readOnly } : t))
    );
  }

  function hasUnsavedTabs(): boolean {
    return tabs().some((t) => t.isModified);
  }

  async function openTab(path: string, opts?: { readOnly?: boolean }) {
    const existing = tabs().find((t) => t.path === path);
    if (existing) {
      setActiveTabPath(path);
      if (existing.kind === "file") addToRecent(path);
      return;
    }

    try {
      const content = await unwrap(commands.readFile(path));
      const name = basename(path);
      // Platform-dependent default (see `defaultReadOnly`): desktop locked,
      // mobile unlocked. Callers that just CREATED the note must always pass
      // opts.readOnly explicitly — `create-then-open-explicit.test.ts` fails
      // the build if a new create-and-open site forgets.
      const readOnly = opts?.readOnly ?? defaultReadOnly();

      setTabs((prev) => [
        ...prev,
        { path, name, content, originalContent: content, isModified: false, kind: "file", readOnly },
      ]);
      setActiveTabPath(path);
      addToRecent(path);
    } catch (e) {
      log.error("tabs", "failed to open tab", e);
    }
  }

  // Open or focus the singleton Graph tab. Only one Graph tab can exist; if
  // already open we just switch to it rather than creating a duplicate.
  function openGraphTab() {
    const existing = tabs().find((t) => t.path === GRAPH_TAB_PATH);
    if (existing) {
      setActiveTabPath(GRAPH_TAB_PATH);
      return;
    }
    setTabs((prev) => [
      ...prev,
      {
        path: GRAPH_TAB_PATH,
        name: "Graph View",
        content: "",
        originalContent: "",
        isModified: false,
        kind: "graph",
        readOnly: false,
      },
    ]);
    setActiveTabPath(GRAPH_TAB_PATH);
  }

  // Toggle the graph tab: if open & active, close it; if open but not active,
  // focus it; if not open, create it. Used by the Graph icon and Ctrl+G.
  function toggleGraphTab() {
    const existing = tabs().find((t) => t.path === GRAPH_TAB_PATH);
    if (!existing) {
      openGraphTab();
    } else if (activeTabPath() === GRAPH_TAB_PATH) {
      closeTab(GRAPH_TAB_PATH);
    } else {
      setActiveTabPath(GRAPH_TAB_PATH);
    }
  }

  // Commit any focused editor field (e.g. an open table-cell <input>) to the
  // CodeMirror doc by blurring it — the editor's `focusout` handler runs
  // synchronously and its dispatch flows through updateContent, so `tab.content`
  // is up to date before we read it below. No-op when nothing editor-ish is
  // focused. Without this, a keyboard/background close (Ctrl+W, palette,
  // app-background) with a cell still focused would leave the typed value only
  // in the input, never in the doc — and thus never saved.
  function commitFocusedEditor() {
    const el = document.activeElement as HTMLElement | null;
    if (el && typeof el.blur === "function") el.blur();
  }

  function removeTabState(path: string) {
    const idx = tabs().findIndex((t) => t.path === path);
    if (idx === -1) return false;

    const newTabs = tabs().filter((t) => t.path !== path);
    setTabs(newTabs);

    if (activeTabPath() === path) {
      if (newTabs.length === 0) {
        setActiveTabPath(null);
      } else {
        const nextIdx = Math.min(idx, newTabs.length - 1);
        setActiveTabPath(newTabs[nextIdx].path);
      }
    }
    return true;
  }

  async function closeTab(path: string): Promise<boolean> {
    const closing = tabs().find((t) => t.path === path);
    if (!closing) return true;

    // Commit a focused cell, then FLUSH a pending debounced save before we
    // cancel its timer. Otherwise a fast edit-then-close inside the 500ms
    // auto-save window (common in read mode, where an editable table cell is
    // one of the few things you can change without unlocking) is silently
    // dropped — the timer that would have written it is cleared below.
    // saveTab captures tab.content synchronously before its first await, so
    // firing it now keeps the edit safe even as the tab leaves the list.
    commitFocusedEditor();
    const autoSave = autoSaveTimers.get(path);
    if (autoSave !== undefined) { clearTimeout(autoSave); autoSaveTimers.delete(path); }
    const afterCommit = tabs().find((t) => t.path === path);
    if (afterCommit?.isModified && isFileTab(afterCommit)) {
      await saveTab(path);
      // A conflict/I/O failure keeps the tab dirty. Do not make the only copy
      // of the user's edit disappear merely because close was requested.
      if (tabs().find((t) => t.path === path)?.isModified) return false;
    }
    return removeTabState(path);
  }

  async function closeOtherTabs(keepPath: string) {
    for (const tab of [...tabs()]) {
      if (tab.path !== keepPath) await closeTab(tab.path);
    }
    setActiveTabPath(keepPath);
  }

  async function closeAllTabs() {
    const flushed = await clearAutoSaveTimers();
    if (!flushed) return false;
    setTabs([]);
    setActiveTabPath(null);
    return true;
  }

  /// Clear tabs after the backend has successfully committed a roost switch.
  /// The caller already flushed/cancelled timers while the old backend was
  /// active, so saving here would incorrectly route old paths through the new
  /// backend.
  function clearTabsAfterRoostSwitch() {
    for (const timer of autoSaveTimers.values()) clearTimeout(timer);
    autoSaveTimers.clear();
    setTabs([]);
    setActiveTabPath(null);
  }

  /// AUDIT-060: cancel and drop every pending auto-save timer. Called
  /// on roost switch (roostStore.openRoost) — the old roost's tabs are
  /// no longer reachable, so their save timers must not fire and write
  /// to a path that may not exist (or worse, that exists under a
  /// different roost). Also called from closeAllTabs for the same reason.
  ///
  /// Before dropping the timers we FLUSH every modified tab so an edit made
  /// inside the 500ms debounce isn't lost when the user switches roost /
  /// closes everything. This is awaited so the writes land while the paths
  /// are still valid (tabs() still holds the old roost's tabs at call time,
  /// pre-switch) — AUDIT-060 still holds because no timer survives to fire
  /// AFTER the switch.
  async function clearAutoSaveTimers(): Promise<boolean> {
    commitFocusedEditor();
    const pending = tabs().filter((t) => t.isModified && isFileTab(t));
    for (const tab of pending) {
      await saveTab(tab.path);
    }
    for (const timer of autoSaveTimers.values()) {
      clearTimeout(timer);
    }
    autoSaveTimers.clear();
    return !tabs().some((t) => t.isModified && isFileTab(t));
  }

  function updateContent(content: string) {
    const path = activeTabPath();
    if (!path) return;

    const current = tabs().find((t) => t.path === path);
    // Virtual tabs (graph) have no editable content — ignore edits.
    if (!isFileTab(current)) return;

    const isChanged = content !== current?.originalContent;
    setTabs((prev) =>
      prev.map((t) =>
        t.path === path
          ? { ...t, content, isModified: isChanged }
          : t
      )
    );

    // Schedule auto-save if content changed
    if (isChanged) {
      scheduleAutoSave(path);
    }
  }

  /**
   * AUDIT-053: route markdown writes through the bundled
   * `save_roost_note` IPC (returns the post-save graph + tasks +
   * backlinks + FTS5 status in one round-trip). Non-markdown writes
   * keep using the generic `write_file` IPC. Returns the SaveOutcome
   * for callers that want to short-circuit follow-up work; null for
   * non-markdown / no-op saves.
   */
  /**
   * AUDIT-053: every save (auto-save, Cmd+S, palette, tab close) flows
   * through this one function. Routes markdown writes to the bundled
   * `save_roost_note` IPC (one round-trip; returns graph + tasks +
   * backlinks + FTS5 status). Non-markdown writes use the generic
   * `write_file`. The `onAfterSave` callback fires for EVERY save so
   * the post-save UI update (effects.ts) has a single source of truth
   * — Cmd+S no longer needs its own copy of the post-save chain.
   * Returns the `SaveOutcome` for callers that want it; null for
   * non-markdown / no-op saves.
   */
  function saveTab(path?: string): Promise<SaveOutcome | null> {
    const targetPath = path ?? activeTabPath();
    if (!targetPath) return Promise.resolve(null);

    const previous = saveQueues.get(targetPath) ?? Promise.resolve(null);
    const queued = previous
      .catch(() => null)
      .then(() => performSaveTab(targetPath));
    saveQueues.set(targetPath, queued);
    void queued.finally(() => {
      if (saveQueues.get(targetPath) === queued) saveQueues.delete(targetPath);
    });
    return queued;
  }

  async function performSaveTab(targetPath: string): Promise<SaveOutcome | null> {

    const tab = tabs().find((t) => t.path === targetPath);
    if (!tab || !tab.isModified || !isFileTab(tab)) return null;

    const lower = targetPath.toLowerCase();
    const isMarkdown = lower.endsWith(".md") || lower.endsWith(".markdown");
    const content = tab.content;

    try {
      let outcome: SaveOutcome | null = null;
      if (isMarkdown) {
        // One IPC: write + edit-history + sync notify + graph re-index
        // + FTS5 + backlinks. Replaces the prior 4-IPC fan-out.
        // EDT-1: hash the version we loaded so the backend can refuse the
        // save if disk diverged (another tab / external editor / sync peer)
        // instead of silently clobbering. `originalContent` is exactly that
        // base — it's reset to `content` after every successful save below.
        // Hash failure returns null → backend skips the guard (never blocks).
        const baseHash = await sha256Hex(tab.originalContent);
        outcome = await saveRoostNote(targetPath, content, baseHash);
      } else {
        // Non-markdown tab (rare — config / settings files opened via
        // the file tree). Keep the generic write_file path; no graph
        // or search indexing applies.
        await unwrap(commands.writeFile(targetPath, content));
      }
      setTabs((prev) =>
        prev.map((t) =>
          t.path === targetPath
            ? {
                ...t,
                originalContent: content,
                isModified: t.content !== content,
              }
            : t
        )
      );
      // Fire the post-save callback regardless of save source
      // (auto-save, Cmd+S, palette). Outcome is null for non-markdown.
      if (onAfterSave) onAfterSave(targetPath, content, outcome);
      return outcome;
    } catch (e) {
      // EDT-1: a last-write-wins conflict is expected, not a bug — the file
      // changed on disk since we loaded it. Keep the tab modified (the
      // isModified reset above is skipped on throw, so the user's edits are
      // safe) and tell them to reload+merge rather than losing either side.
      const msg = String((e as { message?: string })?.message ?? e);
      if (msg.includes(SAVE_CONFLICT_PREFIX)) {
        log.warn("tabs", "save conflict — disk diverged from loaded base", e);
        const name = tab.name ?? targetPath;
        toastStore.show(
          `"${name}" changed on disk since you opened it. Your edits are kept unsaved — close and reopen the note to see the other change, then merge and save.`,
          "warning",
          8000,
        );
      } else {
        log.error("tabs", "failed to save", e);
      }
      return null;
    }
  }

  async function saveAllTabs() {
    // Commit a focused cell first so an in-progress table edit is included.
    commitFocusedEditor();
    const unsaved = tabs().filter((t) => t.isModified);
    for (const tab of unsaved) {
      await saveTab(tab.path);
    }
  }

  function switchToTab(path: string) {
    if (tabs().find((t) => t.path === path)) {
      setActiveTabPath(path);
    }
  }

  function nextTab() {
    const tabList = tabs();
    if (tabList.length <= 1) return;
    const idx = tabList.findIndex((t) => t.path === activeTabPath());
    const nextIdx = (idx + 1) % tabList.length;
    setActiveTabPath(tabList[nextIdx].path);
  }

  function prevTab() {
    const tabList = tabs();
    if (tabList.length <= 1) return;
    const idx = tabList.findIndex((t) => t.path === activeTabPath());
    const prevIdx = (idx - 1 + tabList.length) % tabList.length;
    setActiveTabPath(tabList[prevIdx].path);
  }

  // Handle file deletion — discard the buffer. Calling the normal close path
  // here would save a dirty tab *after* the backend moved it to trash,
  // recreating the deleted note.
  async function onFileDeleted(path: string) {
    const timer = autoSaveTimers.get(path);
    if (timer !== undefined) clearTimeout(timer);
    autoSaveTimers.delete(path);
    // Let a save that was already inside IPC settle before the delete caller
    // proceeds. Pending queued saves see the tab removed and become no-ops.
    removeTabState(path);
    await saveQueues.get(path)?.catch(() => null);
    setRecentFiles((prev) => prev.filter((p) => p !== path));
  }

  /** Flush the latest editor buffer before a user-requested trash operation.
   *  The delete command must run only after this resolves true, so the trash
   *  copy contains the latest text and no delayed save can recreate it. */
  async function prepareFileDeletion(path: string): Promise<boolean> {
    if (activeTabPath() === path) commitFocusedEditor();
    const timer = autoSaveTimers.get(path);
    if (timer !== undefined) clearTimeout(timer);
    autoSaveTimers.delete(path);
    const tab = tabs().find((t) => t.path === path);
    if (tab?.isModified && isFileTab(tab)) await saveTab(path);
    return !tabs().find((t) => t.path === path)?.isModified;
  }

  // Handle file rename — update tab path
  function onFileRenamed(oldPath: string, newPath: string) {
    const newName = basename(newPath);
    setTabs((prev) =>
      prev.map((t) =>
        t.path === oldPath ? { ...t, path: newPath, name: newName } : t
      )
    );
    if (activeTabPath() === oldPath) {
      setActiveTabPath(newPath);
    }
    setRecentFiles((prev) =>
      prev.map((p) => (p === oldPath ? newPath : p))
    );
  }

  function addToRecent(path: string) {
    setRecentFiles((prev) => {
      const filtered = prev.filter((p) => p !== path);
      return [path, ...filtered].slice(0, 20);
    });
  }

  // Re-read a tab's content from disk, replacing the cached copy.
  // Used when the sync engine writes a file the user has open — without
  // this the editor keeps showing the pre-sync content until the user
  // manually closes and reopens the tab.
  //
  // For a CLEAN tab, silently adopt the new disk content. For a DIRTY tab we
  // don't overwrite the user's unsaved edits — but we also must not silently
  // drop a genuine remote change (a peer synced an edit to this note): the
  // user's next save would diff their stale buffer against the updated CRDT
  // and delete the remote edit. So we surface the conflict and let the user
  // decide (keep theirs by saving, or take the remote version by closing +
  // reopening). We deliberately do NOT auto 3-way-merge into the live buffer —
  // that's the one change risky enough to corrupt an open document, so it
  // stays a separate, in-editor-validated follow-up.
  async function reloadTabFromDisk(path: string) {
    const existing = tabs().find((t) => t.path === path);
    if (!existing || existing.kind !== "file") return;
    try {
      const content = await unwrap(commands.readFile(path));
      if (content === existing.content) return; // buffer already matches disk
      if (existing.isModified) {
        // Only a GENUINE remote change (disk differs from the base we loaded)
        // is a conflict — not merely the user's own unsaved edits.
        if (content !== existing.originalContent) {
          toastStore.show(
            `"${existing.name}" changed on another device while you have unsaved edits. Saving keeps your version; close and reopen the note to take the other device's version.`,
            "warning",
            8000,
          );
        }
        return;
      }
      setTabs((prev) =>
        prev.map((t) =>
          t.path === path
            ? { ...t, content, originalContent: content, isModified: false }
            : t,
        ),
      );
    } catch (e) {
      log.warn("tabs", `reloadTabFromDisk(${path}) failed`, e);
    }
  }

  return {
    tabs,
    activeTabPath,
    activeTab,
    activeContent,
    isModified,
    isReadOnly,
    toggleReadOnly,
    hasUnsavedTabs,
    recentFiles,
    openTab,
    openGraphTab,
    toggleGraphTab,
    closeTab,
    closeOtherTabs,
    closeAllTabs,
    clearTabsAfterRoostSwitch,
    clearAutoSaveTimers,
    updateContent,
    saveTab,
    saveAllTabs,
    switchToTab,
    nextTab,
    prevTab,
    onFileDeleted,
    prepareFileDeletion,
    onFileRenamed,
    setOnAfterSave,
    reloadTabFromDisk,
  };
}

export const tabStore = createRoot(createTabStore);
