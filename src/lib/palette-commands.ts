import type { PaletteCommand } from "../components/CommandPalette/CommandPalette";
import { roostStore } from "../stores/roost";
import { tabStore } from "../stores/tabs";
import { graphStore } from "../stores/graph";
import { settingsStore } from "../stores/settings";
import { recorderStore } from "../stores/recorder";
import { toastStore } from "../components/Toast/Toast";
import { promptStore } from "../components/Modals/PromptModal";

/**
 * Callbacks the palette needs to toggle App-level UI state.
 * Passed as a bag so we don't force App.tsx to export each setter.
 */
export interface PaletteContext {
  activePanel: () => string;
  setActivePanel: (panel: string) => void;
  setShowCommandPalette: (open: boolean) => void;
  setShowQuickSwitcher: (open: boolean) => void;
  setShowChat: (open: boolean) => void;
  showChat: () => boolean;
  setShowShortcuts: (open: boolean) => void;
  setShowPathFinder: (open: boolean) => void;
  openDailyNote: () => void;
}

/**
 * Build the command palette list. Lives outside App.tsx so adding a new
 * command doesn't require touching the root component. `settingsStore.theme()`
 * is evaluated at render time, so the toggle-theme label stays in sync.
 */
export function createPaletteCommands(ctx: PaletteContext): PaletteCommand[] {
  return [
    {
      id: "cmd:new-note",
      label: "New Note",
      category: "command",
      shortcut: "Ctrl+N",
      action: () => {
        ctx.setShowCommandPalette(false);
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
      },
    },
    {
      id: "cmd:save",
      label: "Save Current File",
      category: "command",
      shortcut: "Ctrl+S",
      action: () => {
        ctx.setShowCommandPalette(false);
        tabStore.saveTab();
      },
    },
    {
      id: "cmd:save-all",
      label: "Save All Files",
      category: "command",
      action: () => {
        ctx.setShowCommandPalette(false);
        tabStore.saveAllTabs();
      },
    },
    {
      id: "cmd:close-tab",
      label: "Close Current Tab",
      category: "command",
      shortcut: "Ctrl+W",
      action: () => {
        ctx.setShowCommandPalette(false);
        const path = tabStore.activeTabPath();
        if (path) tabStore.closeTab(path);
      },
    },
    {
      id: "cmd:close-all",
      label: "Close All Tabs",
      category: "command",
      action: () => {
        ctx.setShowCommandPalette(false);
        void tabStore.closeAllTabs();
      },
    },
    {
      id: "cmd:toggle-sidebar",
      label: "Toggle Sidebar",
      category: "command",
      shortcut: "Ctrl+B",
      action: () => {
        ctx.setShowCommandPalette(false);
        ctx.setActivePanel(ctx.activePanel() === "files" ? "" : "files");
      },
    },
    {
      id: "cmd:quick-switcher",
      label: "Quick Switcher",
      category: "command",
      shortcut: "Ctrl+O",
      action: () => {
        ctx.setShowCommandPalette(false);
        ctx.setShowQuickSwitcher(true);
      },
    },
    {
      id: "cmd:search",
      label: "Search in Roost",
      category: "command",
      shortcut: "Ctrl+Shift+F",
      action: () => {
        ctx.setShowCommandPalette(false);
        ctx.setActivePanel("search");
      },
    },
    {
      id: "cmd:show-explorer",
      label: "Show Explorer",
      category: "command",
      action: () => {
        ctx.setShowCommandPalette(false);
        ctx.setActivePanel("files");
      },
    },
    {
      id: "cmd:graph-stats",
      label: "Show Graph Stats",
      category: "command",
      action: () => {
        ctx.setShowCommandPalette(false);
        const s = graphStore.stats();
        alert(`Graph: ${s.notes} notes, ${s.links} links, ${s.tags} tags`);
      },
    },
    {
      id: "cmd:toggle-chat",
      label: "Toggle Corvux Agent",
      category: "command",
      shortcut: "Ctrl+L",
      action: () => {
        ctx.setShowCommandPalette(false);
        ctx.setShowChat(!ctx.showChat());
      },
    },
    {
      id: "cmd:toggle-graph",
      label: "Toggle Graph View",
      category: "command",
      shortcut: "Ctrl+G",
      action: () => {
        ctx.setShowCommandPalette(false);
        tabStore.toggleGraphTab();
      },
    },
    {
      id: "cmd:find-path",
      label: "Find Path Between Two Notes",
      category: "command",
      shortcut: "Ctrl+Shift+P",
      action: () => {
        ctx.setShowCommandPalette(false);
        ctx.setShowPathFinder(true);
      },
    },
    {
      id: "cmd:daily-note",
      label: "Open Daily Note",
      category: "command",
      shortcut: "Ctrl+D",
      action: () => {
        ctx.setShowCommandPalette(false);
        ctx.openDailyNote();
      },
    },
    {
      id: "cmd:voice-memo",
      label: "Voice Memo",
      category: "command",
      shortcut: "Ctrl+Shift+R",
      action: () => {
        ctx.setShowCommandPalette(false);
        const path = tabStore.activeTabPath();
        if (path === null || path.startsWith("corvux://")) {
          toastStore.show("Open a note before recording a voice memo", "warning");
          return;
        }
        recorderStore.open();
      },
    },
    {
      id: "cmd:toggle-theme",
      label: `Switch to ${settingsStore.nextTheme().name}`,
      category: "command",
      shortcut: "Ctrl+Shift+T",
      action: () => {
        ctx.setShowCommandPalette(false);
        settingsStore.toggleTheme();
      },
    },
    {
      id: "cmd:shortcuts",
      label: "Keyboard Shortcuts",
      category: "command",
      shortcut: "Ctrl+/",
      action: () => {
        ctx.setShowCommandPalette(false);
        ctx.setShowShortcuts(true);
      },
    },
  ];
}
