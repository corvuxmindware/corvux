import { events, isTauri } from "./invoke";
import { roostStore } from "../stores/roost";
import { tabStore, GRAPH_TAB_PATH } from "../stores/tabs";
import { settingsStore } from "../stores/settings";
import { devPerfStore } from "../stores/devPerf";
import { recorderStore } from "../stores/recorder";
import { toastStore } from "../components/Toast/Toast";
import { promptStore } from "../components/Modals/PromptModal";
import { log } from "./log";

/**
 * App-level UI hooks the global shortcut handler mutates. Kept narrow so
 * adding a new shortcut only touches this file and App.tsx's context wiring.
 */
export interface ShortcutContext {
  activePanel: () => string;
  setActivePanel: (panel: string) => void;
  setShowCommandPalette: (open: boolean) => void;
  setShowQuickSwitcher: (open: boolean) => void;
  setShowChat: (open: boolean) => void;
  showChat: () => boolean;
  setShowShortcuts: (open: boolean) => void;
  showShortcuts: () => boolean;
  setShowPathFinder: (open: boolean) => void;
  openDailyNote: () => void;
}

/**
 * Install the global keydown listener that wires Ctrl/Cmd + letter bindings
 * to the app's UI state. Returns a cleanup function so callers can remove
 * the listener on unmount.
 */
export function registerKeyboardShortcuts(ctx: ShortcutContext): () => void {
  const handler = (e: KeyboardEvent) => {
    const mod = e.ctrlKey || e.metaKey;

    // Ctrl+S — immediate manual save (skip for non-file tabs).
    //
    // AUDIT-053: the post-save fan-out (graph re-index + FTS5 +
    // backlinks) now happens inside `tabStore.saveTab` itself via the
    // bundled `save_roost_note` IPC, and the post-save UI update fires
    // through the single `onAfterSave` callback installed in
    // `installSaveAndIdleHooks`. The duplicated inline chain that used
    // to live here (write_file → index_note → get_backlinks +
    // index_search) is gone — one save path for every entry point.
    if (mod && e.key === "s") {
      e.preventDefault();
      const path = tabStore.activeTabPath();
      if (path === GRAPH_TAB_PATH) return;
      void tabStore.saveTab();
      return;
    }

    // Ctrl+D — daily note
    if (mod && e.key === "d") {
      e.preventDefault();
      ctx.openDailyNote();
      return;
    }

    // Ctrl+N — new note
    if (mod && e.key === "n") {
      e.preventDefault();
      void promptStore
        .ask({ title: "New note", placeholder: "Note name", confirmLabel: "Create" })
        .then((name) => {
          if (name) {
            roostStore.createNewNote(name).then((path) => {
              // Brand-new empty note — always land in write mode.
              if (path) tabStore.openTab(path, { readOnly: false });
            });
          }
        });
      return;
    }

    // Ctrl+B — toggle sidebar
    if (mod && e.key === "b") {
      e.preventDefault();
      ctx.setActivePanel(ctx.activePanel() === "files" ? "" : "files");
      return;
    }

    // Ctrl+O — quick switcher
    if (mod && e.key === "o") {
      e.preventDefault();
      ctx.setShowQuickSwitcher(true);
      return;
    }

    // Ctrl+P — command palette
    if (mod && e.key === "p") {
      e.preventDefault();
      ctx.setShowCommandPalette(true);
      return;
    }

    // Ctrl+Shift+F — search in roost
    if (mod && e.shiftKey && e.key === "F") {
      e.preventDefault();
      ctx.setActivePanel("search");
      return;
    }

    // Ctrl+Shift+R — voice memo (record + insert into active note)
    if (mod && e.shiftKey && (e.key === "R" || e.key === "r")) {
      e.preventDefault();
      const path = tabStore.activeTabPath();
      if (path === null || path.startsWith("corvux://")) {
        toastStore.show("Open a note before recording a voice memo", "warning");
        return;
      }
      recorderStore.open();
      return;
    }

    // Ctrl+W — close current tab
    if (mod && e.key === "w") {
      e.preventDefault();
      const path = tabStore.activeTabPath();
      if (path) tabStore.closeTab(path);
      return;
    }

    // Ctrl+L — toggle AI chat
    if (mod && e.key === "l") {
      e.preventDefault();
      ctx.setShowChat(!ctx.showChat());
      return;
    }

    // Ctrl+G — toggle graph tab
    if (mod && e.key === "g") {
      e.preventDefault();
      tabStore.toggleGraphTab();
      return;
    }

    // Ctrl+T — toggle Tasks panel. Pairs with Ctrl+Shift+T (theme) as a
    // "T-family": T = Tasks, Shift+T = Theme. The app has no new-tab
    // shortcut, so there's nothing for the browser "new tab" reflex to
    // collide with internally.
    if (mod && !e.shiftKey && e.key === "t") {
      e.preventDefault();
      ctx.setActivePanel(ctx.activePanel() === "tasks" ? "" : "tasks");
      return;
    }

    // Ctrl+Shift+P — open the path-finder modal (two fuzzy pickers + result).
    // The graph-view's click-to-pick mode is still reachable via the
    // graph-view toolbar for power users; this shortcut now goes to the
    // lighter modal because it works on mobile and doesn't require the
    // force-graph to render first.
    if (mod && e.shiftKey && e.key === "P") {
      e.preventDefault();
      ctx.setShowPathFinder(true);
      return;
    }

    // Ctrl+Shift+T — toggle theme
    if (mod && e.shiftKey && e.key === "T") {
      e.preventDefault();
      settingsStore.toggleTheme();
      return;
    }

    // Ctrl+Tab / Ctrl+Shift+Tab — next/prev tab
    if (e.ctrlKey && e.key === "Tab") {
      e.preventDefault();
      if (e.shiftKey) tabStore.prevTab();
      else tabStore.nextTab();
      return;
    }

    // Ctrl+/ — keyboard shortcut cheat sheet
    if (mod && e.key === "/") {
      e.preventDefault();
      ctx.setShowShortcuts(!ctx.showShortcuts());
      return;
    }

    // Ctrl+Alt+P — hidden dev perf panel toggle. NOT advertised in the
    // shortcut cheat sheet (Ctrl+/) — this is a triage tool for the
    // founder, not a user feature. Renders a floating bottom-right card
    // with process RSS, counters, and the most recent chat-send timing
    // breakdown. Polls every 2s while open, idle while closed.
    if (mod && e.altKey && (e.key === "p" || e.key === "P")) {
      e.preventDefault();
      devPerfStore.toggle();
      return;
    }
  };

  document.addEventListener("keydown", handler);

  // Native menu emits `menu:event` with the same intents as keyboard
  // shortcuts. Install the listener inline so callers get a single
  // cleanup that detaches both.
  let menuCleanup: (() => void) | null = null;
  registerMenuHandler(ctx).then((d) => {
    menuCleanup = d;
  });

  return () => {
    document.removeEventListener("keydown", handler);
    menuCleanup?.();
  };
}

/**
 * Listen for native-menu clicks from the Rust side and dispatch them to the
 * same handlers the keyboard shortcuts use. The menu emits one event id per
 * custom item (`new_note`, `open_roost`, `toggle_sidebar`, etc.); predefined
 * items (Undo/Cut/Quit) are handled by the OS directly and never reach here.
 *
 * Returns a cleanup function that detaches the Tauri event listener.
 */
export async function registerMenuHandler(
  ctx: ShortcutContext,
): Promise<() => void> {
  // Lazily import the Tauri event API so browser-mode (no Tauri runtime)
  // builds don't fail at module load. Browser preview has no native menu.
  let unlisten: (() => void) | null = null;
  if (!isTauri) {
    return () => {};
  }
  try {
    unlisten = await events.menuEvent.listen(async (e) => {
      switch (e.payload) {
        case "new_note": {
          const name = await promptStore.ask({
            title: "New note",
            placeholder: "Note name",
            confirmLabel: "Create",
          });
          if (name) {
            const path = await roostStore.createNewNote(name);
            // Brand-new empty note — always land in write mode.
            if (path) tabStore.openTab(path, { readOnly: false });
          }
          break;
        }
        case "open_roost": {
          // Reuse Ctrl+O — opens the quick switcher (closest in-app
          // "open" surface). A real folder picker is wired through the
          // onboarding flow; the menu falls back to switcher for now.
          ctx.setShowQuickSwitcher(true);
          break;
        }
        case "toggle_sidebar":
          ctx.setActivePanel(ctx.activePanel() === "files" ? "" : "files");
          break;
        case "toggle_chat":
          ctx.setShowChat(!ctx.showChat());
          break;
        case "toggle_graph":
          tabStore.toggleGraphTab();
          break;
        case "open_github":
          // Open the project page in the user's default browser. Tauri's
          // WebView intercepts `window.open` and routes external URLs to
          // the OS, which is exactly what we want here.
          window.open("https://github.com/", "_blank");
          break;
        case "about":
          // Minimal "about" dialog — version comes from the menu label
          // itself; this just acknowledges the click for now. Replace
          // with a proper modal when one exists.
          alert("Corvux\nA local-first AI-native second brain.");
          break;
        default:
          // Unknown id — log so a future menu addition that forgets to
          // register a handler shows up in the console instead of
          // silently doing nothing.
          log.warn("menu", "unhandled event id", e.payload);
      }
    });
  } catch {
    // Not running under Tauri (browser preview / vitest). Nothing to do.
    return () => {};
  }
  return () => unlisten?.();
}
