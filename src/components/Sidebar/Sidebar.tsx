import { For, Show, createEffect, createMemo, createRoot, createSignal, on, onCleanup, onMount } from "solid-js";
import { roostStore, type FileEntry } from "../../stores/roost";
import { tabStore } from "../../stores/tabs";
import { graphStore } from "../../stores/graph";
import { syncStatusStore } from "../../stores/sync";
import { commands, unwrap } from "../../lib/invoke";
import { errMsg } from "../../lib/error";
import { log } from "../../lib/log";
import { isMobile as isMobilePlatform } from "../../lib/platform";
import ContextMenu, { type MenuItem } from "./ContextMenu";
import MoveToSheet from "./MoveToSheet";
import { newNoteDestDir, relFolderLabel } from "../../lib/new-note-dest";
import { toastStore } from "../Toast/Toast";
import { promptStore } from "../Modals/PromptModal";
import {
  loadCollapsedPaths,
  saveCollapsedPaths,
} from "../../lib/folder-collapse-state";
import RoostSwitcher from "./RoostSwitcher";
import ManageRoostsModal from "../Roost/ManageRoostsModal";

interface SidebarProps {
  onAfterOpenFile?: () => void;
  /** Open the Settings panel. Renders a gear icon next to the roost
   *  switcher at the bottom of the sidebar — matches Obsidian's layout. */
  onOpenSettings?: () => void;
  /** Create or open today's daily note. Renders a calendar-day icon in
   *  the sidebar action toolbar so the user can jump straight to today
   *  from the file panel — on mobile this was previously only reachable
   *  through the ⋮ overflow menu, which is hidden while the drawer is
   *  open. */
  onDailyNote?: () => void;
  /** Switch the drawer to the Bookmarks panel. Renders a bookmark icon
   *  next to the daily-note button in the mobile action toolbar — keeps
   *  bookmarks reachable from the file panel now that it's been dropped
   *  from the mobile top tab strip (Files · Search · Tasks · More). */
  onBookmarks?: () => void;
}

// AUDIT-075: Module-level signals need an owner so they have HMR
// cleanup semantics (otherwise solid-refresh leaks them on every
// reload) and so they follow the same convention as the stores in
// `src/stores/*` — every long-lived signal in the app is rooted.
// The signals themselves keep module-level identity, so the exported
// helpers (`openNewNoteInput`, `openNewFolderInput`) and the in-file
// callers keep working without changes.
const sidebarSignals = createRoot(() => {
  // Shared drag state
  const [dragSource, setDragSource] = createSignal<string | null>(null);
  const [showNewNote, setShowNewNote] = createSignal(false);
  const [newNoteName, setNewNoteName] = createSignal("");
  const [showNewFolder, setShowNewFolder] = createSignal(false);
  const [newFolderName, setNewFolderName] = createSignal("");

  // Folders the user has explicitly collapsed. Empty set = all expanded
  // (preserves the previous "every folder mounts open" UX). Hoisted to a
  // module signal so the virtualized flattener can ask about any path
  // without round-tripping through component state, and so collapse-all
  // is a single signal write instead of a fan-out.
  const [collapsedPaths, setCollapsedPaths] = createSignal<Set<string>>(new Set());

  // Path of the entry whose context menu is currently open (or null).
  // Hoisted to module level so individual FileRow / FolderRow components
  // can derive their "I'm the menu target" highlight without prop drilling
  // contextMenu state through FileTree. Kept in sync with the in-component
  // contextMenu position signal — see Sidebar's handleContextMenu setter.
  const [menuTargetPath, setMenuTargetPath] = createSignal<string | null>(null);

  // Restore each roost's collapsed-folder set when it opens (and reset to
  // "all expanded" when no roost is open). Writes happen in the collapse
  // mutators below, not here, so loading never triggers a save-back.
  createEffect(
    on(roostStore.roostPath, (vp) => {
      setCollapsedPaths(vp ? loadCollapsedPaths(vp) : new Set<string>());
    }),
  );

  return {
    dragSource,
    setDragSource,
    showNewNote,
    setShowNewNote,
    newNoteName,
    setNewNoteName,
    showNewFolder,
    setShowNewFolder,
    newFolderName,
    setNewFolderName,
    collapsedPaths,
    setCollapsedPaths,
    menuTargetPath,
    setMenuTargetPath,
  };
});

const {
  dragSource,
  setDragSource,
  showNewNote,
  setShowNewNote,
  newNoteName,
  setNewNoteName,
  showNewFolder,
  setShowNewFolder,
  newFolderName,
  setNewFolderName,
  collapsedPaths,
  setCollapsedPaths,
  menuTargetPath,
  setMenuTargetPath,
} = sidebarSignals;

export function openNewNoteInput() {
  setShowNewNote(true);
  setShowNewFolder(false);
}

export function openNewFolderInput() {
  setShowNewFolder(true);
  setShowNewNote(false);
}

function isCollapsed(path: string): boolean {
  return collapsedPaths().has(path);
}

function toggleCollapsed(path: string) {
  setCollapsedPaths((prev) => {
    const next = new Set(prev);
    if (next.has(path)) next.delete(path);
    else next.add(path);
    return next;
  });
  saveCollapsedPaths(roostStore.roostPath(), collapsedPaths());
}

function collapseAllFolders() {
  // Walk the live tree and add every folder path. The flattener picks
  // this up on its next memo recompute.
  const all = new Set<string>();
  function walk(entries: FileEntry[]) {
    for (const e of entries) {
      if (e.is_dir) {
        all.add(e.path);
        walk(e.children);
      }
    }
  }
  walk(roostStore.files());
  setCollapsedPaths(all);
  saveCollapsedPaths(roostStore.roostPath(), all);
}

function expandAllFolders() {
  const empty = new Set<string>();
  setCollapsedPaths(empty);
  saveCollapsedPaths(roostStore.roostPath(), empty);
}

/// `true` only when the roost has folders AND every *live* folder is in
/// the collapsed set. Walks the tree once (cheap at roost scale, ~hundreds
/// of folders) rather than comparing set sizes: the persisted collapsed
/// set can hold stale paths for since-deleted folders, so a size compare
/// would falsely report "all collapsed." Called inside JSX so SolidJS
/// tracks `roostStore.files()` and recomputes on tree changes.
/// Empty-roost and partial-collapse both return `false` → the button shows
/// "collapse all" (the action that has work to do).
export function allFoldersCollapsed(): boolean {
  const collapsed = collapsedPaths();
  let total = 0;
  let allCollapsed = true;
  function walk(entries: FileEntry[]) {
    for (const e of entries) {
      if (e.is_dir) {
        total++;
        if (!collapsed.has(e.path)) allCollapsed = false;
        walk(e.children);
      }
    }
  }
  walk(roostStore.files());
  return total > 0 && allCollapsed;
}

export function toggleAllFolders() {
  if (allFoldersCollapsed()) {
    expandAllFolders();
  } else {
    collapseAllFolders();
  }
}

// Strip `.md` extension for display (files on disk keep the extension)
/// Touch gesture handler for sidebar rows. Distinguishes tap (open file
/// / toggle folder) from long-press (show context menu) at the pointer-
/// event level, bypassing `click` entirely for touch input. Prior approach
/// tried to swallow the synthesized click after a long-press, but iOS click
/// suppression is brittle (Solid delegates `onClick` to `document` and
/// listener registration order beat us); going directly from pointerup
/// avoids the whole class of race conditions.
///
/// Touch only: mouse and pen still fall through to the row's regular
/// `onClick` and `onContextMenu` props. Desktop UX is unchanged.
function attachTouchGestures(
  el: HTMLElement,
  cb: { onTap: () => void; onLongPress: (x: number, y: number) => void },
): { dispose: () => void; shouldIgnoreClick: (e: MouseEvent) => boolean } {
  let timer: number | undefined;
  let startX = 0;
  let startY = 0;
  let pointerId: number | undefined;
  let isTouch = false;
  let longPressFired = false;
  let cancelled = false;
  let suppressClickUntil = 0;
  const HOLD_MS = 500;
  const MOVE_THRESHOLD = 10;
  const CLICK_SUPPRESSION_MS = 900;

  const cancelTimer = () => {
    if (timer !== undefined) {
      window.clearTimeout(timer);
      timer = undefined;
    }
  };

  const suppressUpcomingClick = () => {
    suppressClickUntil = performance.now() + CLICK_SUPPRESSION_MS;
  };

  const shouldIgnoreClick = (e: MouseEvent): boolean => {
    return performance.now() <= suppressClickUntil && el.contains(e.target as Node);
  };

  const swallowClick = (e: MouseEvent) => {
    if (!shouldIgnoreClick(e)) return;
    e.preventDefault();
    e.stopImmediatePropagation();
    e.stopPropagation();
  };

  const onDown = (e: PointerEvent) => {
    isTouch = e.pointerType === "touch";
    longPressFired = false;
    cancelled = false;
    if (!isTouch) return;
    pointerId = e.pointerId;
    startX = e.clientX;
    startY = e.clientY;
    el.setPointerCapture?.(e.pointerId);
    timer = window.setTimeout(() => {
      timer = undefined;
      longPressFired = true;
      suppressUpcomingClick();
      cb.onLongPress(startX, startY);
    }, HOLD_MS);
  };

  const onMove = (e: PointerEvent) => {
    if (!isTouch || timer === undefined) return;
    if (Math.hypot(e.clientX - startX, e.clientY - startY) > MOVE_THRESHOLD) {
      cancelTimer();
      cancelled = true;
    }
  };

  const onUp = (e: PointerEvent) => {
    if (!isTouch) return;
    cancelTimer();
    suppressUpcomingClick();
    if (pointerId !== undefined && el.hasPointerCapture?.(pointerId)) {
      el.releasePointerCapture(pointerId);
    }
    pointerId = undefined;
    // Long-press already fired its callback inside the timer; the user
    // is just lifting now. Don't also count this as a tap.
    if (longPressFired || cancelled) return;
    if (Math.hypot(e.clientX - startX, e.clientY - startY) > MOVE_THRESHOLD) return;
    cb.onTap();
  };

  const onCancel = () => {
    cancelTimer();
    cancelled = true;
    suppressUpcomingClick();
    if (pointerId !== undefined && el.hasPointerCapture?.(pointerId)) {
      el.releasePointerCapture(pointerId);
    }
    pointerId = undefined;
  };

  // Need to swallow the synthesized click that iOS fires after a touch
  // sequence — otherwise the row's existing onClick prop would fire on
  // top of our onTap (double-open) or after a long-press (open + menu).
  // Do not key this off MouseEvent.detail: Android and iOS WebViews do
  // not agree on whether touch-synthesized clicks look like detail 0
  // or a normal primary click.
  const onDocClickCapture = (e: MouseEvent) => swallowClick(e);

  el.addEventListener("pointerdown", onDown);
  el.addEventListener("pointerup", onUp);
  el.addEventListener("pointercancel", onCancel);
  el.addEventListener("pointermove", onMove);
  el.addEventListener("click", swallowClick, true);
  document.addEventListener("click", onDocClickCapture, true);

  return {
    shouldIgnoreClick,
    dispose: () => {
      cancelTimer();
      if (pointerId !== undefined && el.hasPointerCapture?.(pointerId)) {
        el.releasePointerCapture(pointerId);
      }
      el.removeEventListener("pointerdown", onDown);
      el.removeEventListener("pointerup", onUp);
      el.removeEventListener("pointercancel", onCancel);
      el.removeEventListener("pointermove", onMove);
      el.removeEventListener("click", swallowClick, true);
      document.removeEventListener("click", onDocClickCapture, true);
    },
  };
}

function displayName(entry: FileEntry): string {
  return entry.is_dir ? entry.name : entry.name.replace(/\.md$/i, "");
}

export default function Sidebar(props: SidebarProps) {
  const [showManageRoosts, setShowManageRoosts] = createSignal(false);
  const [contextMenu, setContextMenu] = createSignal<{
    x: number;
    y: number;
    entry: FileEntry;
  } | null>(null);
  // Entry whose "Move to folder…" sheet is open (null = closed).
  const [moveTarget, setMoveTarget] = createSignal<FileEntry | null>(null);

  // Move an entry into `destDir` via the move_entry IPC, then sync the UI:
  // refresh the tree, repoint an open tab, and re-index the moved note's
  // graph node at its new path (move_entry purges the old id). Shared by
  // the folder-picker sheet (mobile) and could back any future caller.
  async function moveEntryTo(entry: FileEntry, destDir: string) {
    try {
      const newPath = await unwrap(commands.moveEntry(entry.path, destDir));
      tabStore.onFileRenamed(entry.path, newPath);
      void graphStore.indexPath(newPath);
      await roostStore.refreshFiles({ force: true });
    } catch (e) {
      log.error("sidebar", "move-to-folder failed", e);
      alert(`Move failed: ${errMsg(e)}`);
    }
  }

  async function handleCreateNote() {
    const name = newNoteName().trim();
    if (name) {
      const path = await roostStore.createNewNote(name);
      // Brand-new empty note — always land in write mode.
      if (path) tabStore.openTab(path, { readOnly: false });
      setNewNoteName("");
      setShowNewNote(false);
    }
  }

  // Folder a new note will be created in, shown in the New Note input so
  // the user always sees where it lands. Mirrors the resolution in
  // roostStore.createNewNote: the open note's folder, else roost root.
  const newNoteDestLabel = () => {
    const root = roostStore.roostPath() ?? "";
    return relFolderLabel(newNoteDestDir(tabStore.activeTabPath(), root), root);
  };

  async function handleCreateFolder() {
    const name = newFolderName().trim();
    const roost = roostStore.roostPath();
    if (name && roost) {
      try {
        const path = `${roost}/${name}`;
        await unwrap(commands.createFolder(path));
        // AUDIT-065: refresh the file tree only — re-running openRoost
        // re-initialises the whole roost lifecycle (DB, sync, etc.).
        await roostStore.refreshFiles({ force: true });
        setNewFolderName("");
        setShowNewFolder(false);
      } catch (e) {
        log.error("sidebar", "failed to create folder", e);
      }
    }
  }

  function handleContextMenu(e: MouseEvent, entry: FileEntry) {
    e.preventDefault();
    e.stopPropagation();
    setContextMenu({ x: e.clientX, y: e.clientY, entry });
    setMenuTargetPath(entry.path);
  }

  async function copyPathToClipboard(path: string) {
    try {
      await navigator.clipboard.writeText(path);
      toastStore.show("Path copied to clipboard", "success", 2000);
    } catch (e) {
      log.error("sidebar", "failed to copy path", e);
      toastStore.show("Couldn't copy to clipboard", "error", 3000);
    }
  }

  function getContextMenuItems(entry: FileEntry): MenuItem[] {
    if (entry.is_dir) {
      return [
        {
          label: "New Note Here",
          action: async () => {
            const name = await promptStore.ask({
              title: "New note",
              placeholder: "Note name",
              confirmLabel: "Create",
            });
            if (name) {
              const fileName = name.endsWith(".md") ? name : `${name}.md`;
              const path = `${entry.path}/${fileName}`;
              try {
                await unwrap(commands.createFile(path));
                // create_file writes to disk but doesn't index — give the
                // new note a graph node now, not on first save.
                void graphStore.indexPath(path);
                // AUDIT-065: tree refresh only.
                await roostStore.refreshFiles({ force: true });
                tabStore.openTab(path);
              } catch (e) {
                log.error("sidebar", "failed to create note", e);
              }
            }
          },
        },
        {
          label: "New Subfolder",
          action: async () => {
            const name = await promptStore.ask({
              title: "New subfolder",
              placeholder: "Folder name",
              confirmLabel: "Create",
            });
            if (name) {
              try {
                await unwrap(commands.createFolder(`${entry.path}/${name}`));
                // AUDIT-065: tree refresh only.
                await roostStore.refreshFiles({ force: true });
              } catch (e) {
                log.error("sidebar", "failed to create folder", e);
              }
            }
          },
        },
        {
          label: "Move to folder…",
          action: () => setMoveTarget(entry),
        },
        { label: "", action: () => {}, separator: true },
        {
          label: "Delete Folder",
          danger: true,
          action: async () => {
            // Folder deletes are recursive (everything inside goes to
            // trash). Confirm before doing it — the trash recovers, but
            // a misclick on a 500-note folder is still a bad day.
            const ok = window.confirm(
              `Delete "${entry.name}" and everything inside it?\n\nThe folder is moved to .corvux-trash/ (recoverable).`,
            );
            if (!ok) return;
            try {
              await unwrap(commands.deleteFolder(entry.path));
              // AUDIT-065: tree refresh only.
              await roostStore.refreshFiles({ force: true });
            } catch (e) {
              alert(`${e}`);
            }
          },
        },
      ];
    }

    return [
      {
        label: "Open",
        action: () => {
          tabStore.openTab(entry.path);
          props.onAfterOpenFile?.();
        },
      },
      {
        label: "Rename",
        action: () => {
          // Trigger inline rename via a custom event on the entry
          const el = document.querySelector(`[data-path="${CSS.escape(entry.path)}"]`);
          if (el) el.dispatchEvent(new CustomEvent("start-rename"));
        },
      },
      {
        label: "Duplicate",
        action: async () => {
          try {
            const content = await unwrap(commands.readFile(entry.path));
            const baseName = entry.name.replace(/\.md$/, "");
            // Same Windows-aware path split as handleRenameSubmit — the
            // single-separator regex used to leave `dir` as the whole
            // path on Windows.
            const sep = entry.path.includes("\\") ? "\\" : "/";
            const dir = entry.path.replace(/[\\/][^\\/]+$/, "");
            const newPath = `${dir}${sep}${baseName} (copy).md`;
            await unwrap(commands.createFile(newPath));
            await unwrap(commands.writeFile(newPath, content));
            // Index the duplicate at its new path (write_file doesn't).
            void graphStore.indexPath(newPath);
            // AUDIT-065: tree refresh only.
            await roostStore.refreshFiles({ force: true });
            tabStore.openTab(newPath);
          } catch (e) {
            log.error("sidebar", "failed to duplicate", e);
          }
        },
      },
      {
        label: "Move to folder…",
        action: () => setMoveTarget(entry),
      },
      { label: "", action: () => {}, separator: true },
      {
        label: "Copy Roost path",
        action: () => {
          const roost = roostStore.roostPath();
          // Strip the roost-root prefix to get the portable, in-roost path
          // (e.g. "Idea/BRAINSTORM.md"). Both `\` and `/` separators occur
          // because the roost root is a native absolute path. Fall back to
          // the absolute path if the entry somehow sits outside the roost.
          let rel = entry.path;
          if (roost && entry.path.startsWith(roost)) {
            rel = entry.path.slice(roost.length).replace(/^[\\/]+/, "");
          }
          void copyPathToClipboard(rel);
        },
      },
      {
        label: "Copy absolute path",
        action: () => void copyPathToClipboard(entry.path),
      },
      { label: "", action: () => {}, separator: true },
      {
        label: "Delete",
        danger: true,
        action: async () => {
          const roost = roostStore.roostPath();
          if (!roost) return;
          // Confirm before deleting. The file goes to `.corvux-trash/`,
          // so it's recoverable from a file manager — call that out so
          // the user knows the safety net exists. Mirrors the folder-
          // delete prompt for consistency.
          const ok = window.confirm(
            `Delete "${entry.name}"?\n\nThe file is moved to .corvux-trash/ inside your roost and can be restored from your file manager.`,
          );
          if (!ok) return;
          try {
            // Flush before trashing so the recoverable copy contains the
            // latest edit and no delayed auto-save can recreate the note.
            if (!(await tabStore.prepareFileDeletion(entry.path))) {
              toastStore.show(
                "The note could not be saved, so it was not deleted. Your edits remain open.",
                "warning",
                7000,
              );
              return;
            }
            await unwrap(commands.deleteFile(entry.path, roost));
            await tabStore.onFileDeleted(entry.path);
            // AUDIT-065: tree refresh only.
            await roostStore.refreshFiles({ force: true });
          } catch (e) {
            log.error("sidebar", "failed to delete", e);
          }
        },
      },
    ];
  }

  const isMobile = () => isMobilePlatform();

  return (
    <div class="flex flex-col h-full select-none">
      <Show when={!isMobile() && roostStore.roostPath()}>
        {/* Centred action toolbar. Order: New file · New folder · Calendar ·
            Collapse all — collapse-all sits last so the creation actions stay
            grouped on the left of the cluster. */}
        <div class="h-10 px-2 flex items-center justify-center gap-1 border-b border-border shrink-0">
          <button
            onClick={() => { setShowNewNote(!showNewNote()); setShowNewFolder(false); }}
            class="w-9 h-9 flex items-center justify-center rounded-md text-text-muted hover:text-text-primary hover:bg-bg-hover transition-colors"
            title="New note"
            aria-label="New note"
          >
            <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.75" stroke-linecap="round" stroke-linejoin="round">
              <path d="M14 3H6a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V9z" />
              <path d="M14 3v6h6" />
              <path d="M12 12v6M9 15h6" />
            </svg>
          </button>
          <button
            onClick={() => { setShowNewFolder(!showNewFolder()); setShowNewNote(false); }}
            class="w-9 h-9 flex items-center justify-center rounded-md text-text-muted hover:text-text-primary hover:bg-bg-hover transition-colors"
            title="New folder"
            aria-label="New folder"
          >
            <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.75" stroke-linecap="round" stroke-linejoin="round">
              <path d="M22 19a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h5l2 3h9a2 2 0 0 1 2 2z" />
              <path d="M12 11v6M9 14h6" />
            </svg>
          </button>
          <Show when={props.onDailyNote}>
            <button
              onClick={() => props.onDailyNote?.()}
              class="w-9 h-9 flex items-center justify-center rounded-md text-text-muted hover:text-text-primary hover:bg-bg-hover transition-colors"
              title="Open today's daily note"
              aria-label="Open today's daily note"
            >
              <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.75" stroke-linecap="round" stroke-linejoin="round">
                <rect x="3" y="4" width="18" height="18" rx="2" />
                <path d="M16 2v4M8 2v4M3 10h18" />
              </svg>
            </button>
          </Show>
          <button
            onClick={toggleAllFolders}
            class="w-9 h-9 flex items-center justify-center rounded-md text-text-muted hover:text-text-primary hover:bg-bg-hover transition-colors"
            title={allFoldersCollapsed() ? "Expand all folders" : "Collapse all folders"}
            aria-label={allFoldersCollapsed() ? "Expand all folders" : "Collapse all folders"}
          >
            <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.75" stroke-linecap="round" stroke-linejoin="round">
              <Show
                when={allFoldersCollapsed()}
                fallback={<path d="M7 3l5 5 5-5M7 21l5-5 5 5" />}
              >
                <path d="M7 8l5-5 5 5M7 16l5 5 5-5" />
              </Show>
            </svg>
          </button>
        </div>
      </Show>

      {/* Mobile — file/folder actions inline above the tree, no collapse
          chevron (the drawer dismisses via backdrop tap). */}
      <Show when={isMobile() && roostStore.roostPath()}>
        <div class="px-2 py-2 flex gap-1 justify-center">
          <button
            onClick={() => { setShowNewNote(!showNewNote()); setShowNewFolder(false); }}
            class="w-11 h-11 flex items-center justify-center rounded-md text-text-muted hover:text-text-primary hover:bg-bg-hover transition-colors"
            title="New note"
            aria-label="New note"
          >
            <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.75" stroke-linecap="round" stroke-linejoin="round">
              <path d="M14 3H6a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V9z" />
              <path d="M14 3v6h6" />
              <path d="M12 12v6M9 15h6" />
            </svg>
          </button>
          <button
            onClick={() => { setShowNewFolder(!showNewFolder()); setShowNewNote(false); }}
            class="w-11 h-11 flex items-center justify-center rounded-md text-text-muted hover:text-text-primary hover:bg-bg-hover transition-colors"
            title="New folder"
            aria-label="New folder"
          >
            <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.75" stroke-linecap="round" stroke-linejoin="round">
              <path d="M22 19a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h5l2 3h9a2 2 0 0 1 2 2z" />
              <path d="M12 11v6M9 14h6" />
            </svg>
          </button>
          <Show when={props.onBookmarks}>
            <button
              onClick={() => props.onBookmarks?.()}
              class="w-11 h-11 flex items-center justify-center rounded-md text-text-muted hover:text-text-primary hover:bg-bg-hover transition-colors"
              title="Bookmarks"
              aria-label="Bookmarks"
            >
              <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.75" stroke-linecap="round" stroke-linejoin="round">
                <path d="M19 21l-7-5-7 5V5a2 2 0 012-2h10a2 2 0 012 2z" />
              </svg>
            </button>
          </Show>
          <Show when={props.onDailyNote}>
            <button
              onClick={() => props.onDailyNote?.()}
              class="w-11 h-11 flex items-center justify-center rounded-md text-text-muted hover:text-text-primary hover:bg-bg-hover transition-colors"
              title="Open today's daily note"
              aria-label="Open today's daily note"
            >
              <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.75" stroke-linecap="round" stroke-linejoin="round">
                <rect x="3" y="4" width="18" height="18" rx="2" />
                <path d="M16 2v4M8 2v4M3 10h18" />
              </svg>
            </button>
          </Show>
          <button
            onClick={toggleAllFolders}
            class="w-11 h-11 flex items-center justify-center rounded-md text-text-muted hover:text-text-primary hover:bg-bg-hover transition-colors"
            title={allFoldersCollapsed() ? "Expand all folders" : "Collapse all folders"}
            aria-label={allFoldersCollapsed() ? "Expand all folders" : "Collapse all folders"}
          >
            <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.75" stroke-linecap="round" stroke-linejoin="round">
              <Show
                when={allFoldersCollapsed()}
                fallback={<path d="M7 3l5 5 5-5M7 21l5-5 5 5" />}
              >
                <path d="M7 8l5-5 5 5M7 16l5 5 5-5" />
              </Show>
            </svg>
          </button>
        </div>
      </Show>

      {/* New note input */}
      <Show when={showNewNote()}>
        <div class="px-2 pb-2 space-y-1">
          <div class="text-[11px] text-text-muted truncate" title={newNoteDestLabel()}>
            New note in <span class="text-text-secondary">{newNoteDestLabel()}</span>
          </div>
          <input
            type="text"
            placeholder="Note name..."
            value={newNoteName()}
            onInput={(e) => setNewNoteName(e.currentTarget.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter") handleCreateNote();
              if (e.key === "Escape") setShowNewNote(false);
            }}
            class="w-full px-2 py-1 rounded bg-bg-tertiary border border-border text-text-primary text-xs focus:outline-none focus:border-accent"
            autofocus
          />
        </div>
      </Show>

      {/* New folder input */}
      <Show when={showNewFolder()}>
        <div class="px-2 pb-2">
          <input
            type="text"
            placeholder="Folder name..."
            value={newFolderName()}
            onInput={(e) => setNewFolderName(e.currentTarget.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter") handleCreateFolder();
              if (e.key === "Escape") setShowNewFolder(false);
            }}
            class="w-full px-2 py-1 rounded bg-bg-tertiary border border-border text-text-primary text-xs focus:outline-none focus:border-accent"
            autofocus
          />
        </div>
      </Show>

      {/* File tree — virtualized FileTree owns its own scroll container */}
      <div class="flex-1 min-h-0 py-1 flex flex-col">
        <Show
          when={roostStore.roostPath()}
          fallback={
            <div class="px-4 py-10 text-center space-y-3">
              <p class="text-text-muted text-xs">No Roost open</p>
              <button
                onClick={() => setShowManageRoosts(true)}
                class="px-3 py-1.5 rounded-md bg-bg-hover hover:bg-bg-active text-text-secondary hover:text-text-primary transition-colors text-xs"
              >
                Open roost…
              </button>
            </div>
          }
        >
          <Show when={roostStore.isOpening()}>
            <div class="px-4 py-6 flex items-center gap-2 text-text-muted text-xs">
              <span class="inline-block w-3 h-3 border-2 border-text-muted border-t-transparent rounded-full animate-spin" />
              Opening roost…
            </div>
          </Show>
          <Show
            when={!roostStore.isOpening() && roostStore.files().length > 0}
            fallback={
              <Show when={!roostStore.isOpening()}>
              <div class="px-4 py-8 text-center space-y-3">
                <svg width="28" height="28" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" class="mx-auto text-text-muted opacity-30">
                  <path d="M14 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8z" />
                  <polyline points="14 2 14 8 20 8" />
                  <line x1="12" y1="18" x2="12" y2="12" />
                  <line x1="9" y1="15" x2="15" y2="15" />
                </svg>
                <p class="text-text-muted text-xs">No notes yet</p>
                <p class="text-text-muted text-[10px] opacity-50">
                  Press <kbd class="px-1 py-0.5 rounded bg-bg-hover border border-border text-[9px]">Ctrl+N</kbd> to create your first note
                </p>
              </div>
              </Show>
            }
          >
            <FileTree
              entries={roostStore.files()}
              onContextMenu={handleContextMenu}
              onAfterOpenFile={props.onAfterOpenFile}
            />
          </Show>
        </Show>
      </div>

      {/* Backlinks moved out: now lives in the right sidebar on desktop
          (RightSidebar.tsx's Backlinks tab) and in the left-overlay
          tab strip on mobile (App.tsx mobilePanels "backlinks" entry).
          Keeping it inside the Files view was forcing the file tree
          off-screen once a note had 10+ backlinks. */}

      {/* Bottom — roost switcher + settings gear, Obsidian-style. The
          switcher takes flex-1 so the gear stays anchored at the right
          regardless of the roost name length. Gear only renders if
          `onOpenSettings` is wired (desktop IconRail still hosts its own
          Settings button, so this is purely additive). */}
      {/* Footer background is the drawer colour so it fills the bottom
          safe-area strip (the Files-panel drawer extends into the inset —
          see LeftPane mobile height). The notes/links row then sits IN that
          strip, edge-aligned so it flanks the centred home-indicator pill. */}
      <div class="border-t border-border bg-bg-secondary">
        <div class="flex items-stretch min-h-[48px] sm:min-h-0">
          <div class="flex-1 min-w-0">
            <RoostSwitcher onOpenManage={() => setShowManageRoosts(true)} />
          </div>
          <Show when={props.onOpenSettings}>
            <button
              onClick={props.onOpenSettings}
              class="shrink-0 px-3 min-w-[44px] sm:min-w-0 flex items-center justify-center text-text-muted hover:text-text-primary hover:bg-bg-hover transition-colors border-l border-border"
              aria-label="Open settings"
              title="Settings"
            >
              <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round">
                <circle cx="12" cy="12" r="3" />
                <path d="M19.4 15a1.65 1.65 0 00.33 1.82l.06.06a2 2 0 01-2.83 2.83l-.06-.06a1.65 1.65 0 00-1.82-.33 1.65 1.65 0 00-1 1.51V21a2 2 0 01-4 0v-.09a1.65 1.65 0 00-1-1.51 1.65 1.65 0 00-1.82.33l-.06.06a2 2 0 01-2.83-2.83l.06-.06a1.65 1.65 0 00.33-1.82 1.65 1.65 0 00-1.51-1H3a2 2 0 010-4h.09a1.65 1.65 0 001.51-1 1.65 1.65 0 00-.33-1.82l-.06-.06a2 2 0 012.83-2.83l.06.06a1.65 1.65 0 001.82.33H9a1.65 1.65 0 001-1.51V3a2 2 0 014 0v.09a1.65 1.65 0 001 1.51 1.65 1.65 0 001.82-.33l.06-.06a2 2 0 012.83 2.83l-.06.06a1.65 1.65 0 00-.33 1.82V9a1.65 1.65 0 001.51 1H21a2 2 0 010 4h-.09a1.65 1.65 0 00-1.51 1z" />
              </svg>
            </button>
          </Show>
        </div>
        <Show when={roostStore.roostPath()}>
          {/* pb = a little fixed breathing room + ~⅓ of the inset, so the
              text sits in the lower band ABOVE the home-indicator pill
              (~8–10px from the edge) rather than under it. Collapses to the
              fixed value on non-inset devices (Touch-ID iPhone / Android /
              desktop), where the row just sits at the drawer bottom. */}
          <div
            class="px-3 pt-1 text-text-muted text-[10px] flex justify-end border-t border-border"
            style={{
              "padding-bottom":
                "calc(0.375rem + env(safe-area-inset-bottom, 0px) * 0.3)",
            }}
          >
            <Show when={graphStore.isReady()} fallback={<span>{roostStore.noteCount()} notes</span>}>
              <span>{graphStore.stats().notes} notes</span>
            </Show>
          </div>
        </Show>
      </div>

      <Show when={showManageRoosts()}>
        <ManageRoostsModal onClose={() => setShowManageRoosts(false)} />
      </Show>

      {/* Context menu */}
      <Show when={contextMenu()}>
        {(ctx) => (
          <ContextMenu
            x={ctx().x}
            y={ctx().y}
            items={getContextMenuItems(ctx().entry)}
            onClose={() => {
              setContextMenu(null);
              setMenuTargetPath(null);
            }}
          />
        )}
      </Show>

      {/* "Move to folder…" picker sheet */}
      <Show when={moveTarget()}>
        {(entry) => (
          <MoveToSheet
            entry={entry()}
            tree={roostStore.files()}
            roostRoot={roostStore.roostPath() ?? ""}
            onClose={() => setMoveTarget(null)}
            onPick={(destDir) => void moveEntryTo(entry(), destDir)}
          />
        )}
      </Show>
    </div>
  );
}

/**
 * Virtualized flat file tree.
 *
 * Walks the recursive `entries` into a flat list of `(entry, depth)`
 * rows, respecting the hoisted `collapsedPaths` set. Renders only the
 * rows that intersect the scroll viewport plus a small overscan window;
 * a full-height spacer keeps the scrollbar geometry honest.
 *
 * Performance shape: a 500-note roost used to mount thousands of DOM
 * nodes immediately on first paint. Virtualized, the initial render is
 * bounded by viewport height — typically 30–50 nodes regardless of
 * roost size.
 */
// Mobile rows are taller (FileRow = h-[32px], FolderRow = h-[28px]) so
// the touch targets clear Apple's 44pt minimum and the larger note-name
// font (text-[15px]) has breathing room. Desktop stays at 20-22 to keep
// the file tree dense. The virtualizer must use the same physical height
// the rows actually render at — `sm:` breakpoint (640px viewport) aligns
// roughly with the JS `isMobilePlatform()` UA check, so use the same
// detection for consistency.
const ROW_HEIGHT = () => (isMobilePlatform() ? 34 : 22);
const OVERSCAN_ROWS = 8;

interface FlatRow {
  entry: FileEntry;
  depth: number;
  /// Synthetic "Empty" placeholder rendered under an expanded folder with
  /// no children. Without it, expanding an empty folder produced zero
  /// visual change — the chevron rotated and nothing else happened, which
  /// read as "the click didn't work" (UX audit 2026-07-02).
  emptyGhost?: boolean;
}

function FileTree(props: {
  entries: FileEntry[];
  onContextMenu: (e: MouseEvent, entry: FileEntry) => void;
  onAfterOpenFile?: () => void;
}) {
  let scrollEl!: HTMLDivElement;
  const [scrollTop, setScrollTop] = createSignal(0);
  const [viewportH, setViewportH] = createSignal(600);

  const rows = createMemo<FlatRow[]>(() => {
    const out: FlatRow[] = [];
    function walk(entries: FileEntry[], depth: number) {
      for (const e of entries) {
        out.push({ entry: e, depth });
        if (e.is_dir && !isCollapsed(e.path)) {
          if (e.children.length === 0) {
            out.push({ entry: e, depth: depth + 1, emptyGhost: true });
          } else {
            walk(e.children, depth + 1);
          }
        }
      }
    }
    walk(props.entries, 0);
    return out;
  });

  const visibleRange = createMemo(() => {
    const h = ROW_HEIGHT();
    const start = Math.max(0, Math.floor(scrollTop() / h) - OVERSCAN_ROWS);
    const end = Math.min(
      rows().length,
      Math.ceil((scrollTop() + viewportH()) / h) + OVERSCAN_ROWS,
    );
    return { start, end };
  });

  const visibleRows = createMemo(() => {
    const { start, end } = visibleRange();
    const slice = rows().slice(start, end);
    return slice.map((row, i) => ({ ...row, index: start + i }));
  });

  // Track viewport size — file tree resizes when the user drags the
  // sidebar splitter or rotates the device.
  function attachResizeObserver(el: HTMLDivElement) {
    const ro = new ResizeObserver(() => setViewportH(el.clientHeight));
    ro.observe(el);
    setViewportH(el.clientHeight);
    onCleanup(() => ro.disconnect());
  }

  return (
    <div
      ref={(el) => {
        scrollEl = el;
        attachResizeObserver(el);
      }}
      class="overflow-y-auto flex-1 min-h-0 w-full"
      onScroll={(e) => setScrollTop(e.currentTarget.scrollTop)}
    >
      <div
        class="relative"
        style={{ height: `${rows().length * ROW_HEIGHT()}px` }}
      >
        <For each={visibleRows()}>
          {(row) => (
            <div
              class="absolute left-0 right-0"
              style={{
                top: `${row.index * ROW_HEIGHT()}px`,
                height: `${ROW_HEIGHT()}px`,
              }}
            >
              <Show
                when={!row.emptyGhost}
                fallback={
                  <div
                    class="flex items-center h-full text-xs text-text-muted/60 italic select-none"
                    style={{ "padding-left": `${6 + row.depth * 12 + 14}px` }}
                  >
                    Empty
                  </div>
                }
              >
                <Show
                  when={row.entry.is_dir}
                  fallback={
                    <FileRow
                      entry={row.entry}
                      depth={row.depth}
                      onContextMenu={props.onContextMenu}
                      onAfterOpenFile={props.onAfterOpenFile}
                    />
                  }
                >
                  <FolderRow
                    entry={row.entry}
                    depth={row.depth}
                    onContextMenu={props.onContextMenu}
                  />
                </Show>
              </Show>
            </div>
          )}
        </For>
      </div>
    </div>
  );
}

/// Count every `.md` descendant of a folder entry, recursing into
/// sub-folders. Used by FolderRow to paint a small "how many notes are
/// in here" badge — matches the design handoff and the Obsidian/Notion
/// convention. Folders themselves are not counted; only notes.
function countNoteDescendants(entry: FileEntry): number {
  if (!entry.is_dir) return entry.name.endsWith(".md") ? 1 : 0;
  let total = 0;
  for (const child of entry.children) {
    if (child.is_dir) total += countNoteDescendants(child);
    else if (child.name.endsWith(".md")) total += 1;
  }
  return total;
}

function FolderRow(props: {
  entry: FileEntry;
  depth: number;
  onContextMenu: (e: MouseEvent, entry: FileEntry) => void;
}) {
  const [dragOver, setDragOver] = createSignal(false);
  const expanded = () => !isCollapsed(props.entry.path);
  let rowRef!: HTMLDivElement;
  // See FileRow's `pressed` signal for why we track this explicitly
  // instead of relying on `:active` (iOS WebKit unreliability).
  const [pressed, setPressed] = createSignal(false);
  let gestures: ReturnType<typeof attachTouchGestures> | undefined;
  onMount(() => {
    gestures = attachTouchGestures(rowRef, {
      onTap: () => toggleCollapsed(props.entry.path),
      onLongPress: (x, y) => {
        const ev = new MouseEvent("contextmenu", {
          clientX: x,
          clientY: y,
          bubbles: false,
        });
        props.onContextMenu(ev, props.entry);
      },
    });
    onCleanup(() => gestures?.dispose());
  });
  // Cheap at roost scale (hundreds of folders, thousands of notes).
  // Re-runs when roostStore.files() changes because props.entry is a
  // reference into that signal — SolidJS re-renders FolderRow on tree
  // updates. Hidden when zero to avoid a "0" badge on empty folders.
  const noteCount = () => countNoteDescendants(props.entry);

  function handleDrop(e: DragEvent) {
    e.preventDefault();
    setDragOver(false);
    const source = dragSource();
    if (!source || source === props.entry.path) return;

    // Move the dragged entry into this folder
    unwrap(commands.moveEntry(source, props.entry.path))
      .then((newPath) => {
        // AUDIT-065: tree refresh only.
        void roostStore.refreshFiles({ force: true });
        // Update tab path if the moved file is open
        tabStore.onFileRenamed(source, newPath);
        // move_entry purges the old note id; re-create the node at the
        // new path now instead of waiting for the next save.
        void graphStore.indexPath(newPath);
      })
      .catch((e: unknown) => log.error("sidebar", "move failed", e));
    setDragSource(null);
  }

  return (
    <div
      ref={rowRef}
      class={`group mx-1 flex items-center gap-1.5 px-1.5 py-0.5 rounded-md cursor-pointer transition-colors h-[34px] sm:h-[22px] [-webkit-touch-callout:none] [-webkit-user-select:none] [touch-action:manipulation] select-none ${
        dragOver()
          ? "bg-accent/15 ring-1 ring-accent/30"
          : pressed() || menuTargetPath() === props.entry.path
            ? "bg-bg-hover text-text-primary"
            : "text-text-secondary hover:bg-bg-hover hover:text-text-primary"
      }`}
      style={{ "padding-left": `${6 + props.depth * 12}px` }}
      onPointerDown={(e) => {
        if (e.pointerType === "touch") setPressed(true);
      }}
      onPointerUp={() => setPressed(false)}
      onPointerCancel={() => setPressed(false)}
      onPointerLeave={() => setPressed(false)}
      onClick={(e) => {
        if (gestures?.shouldIgnoreClick(e)) {
          e.preventDefault();
          e.stopPropagation();
          return;
        }
        toggleCollapsed(props.entry.path);
      }}
      onContextMenu={(e) => props.onContextMenu(e, props.entry)}
      onDragOver={(e) => {
        e.preventDefault();
        setDragOver(true);
      }}
      onDragLeave={() => setDragOver(false)}
      onDrop={handleDrop}
    >
      <svg
        width="14"
        height="14"
        viewBox="0 0 24 24"
        fill="none"
        stroke="currentColor"
        stroke-width="1.75"
        stroke-linecap="round"
        stroke-linejoin="round"
        class="shrink-0 transition-transform text-text-muted"
        style={{ transform: expanded() ? "rotate(90deg)" : "rotate(0deg)" }}
      >
        <path d="M9 18l6-6-6-6" />
      </svg>
      <span class="text-[15px] sm:text-[13px] font-medium truncate flex-1 min-w-0">{displayName(props.entry)}</span>
      <Show when={noteCount() > 0}>
        <span
          class="shrink-0 text-[10px] text-text-muted tabular-nums opacity-0 group-hover:opacity-100 transition-opacity"
          classList={{ "!opacity-60": !expanded() }}
          aria-label={`${noteCount()} notes`}
          title={`${noteCount()} note${noteCount() === 1 ? "" : "s"}`}
        >
          {noteCount()}
        </span>
      </Show>
    </div>
  );
}

function FileRow(props: {
  entry: FileEntry;
  depth: number;
  onContextMenu: (e: MouseEvent, entry: FileEntry) => void;
  onAfterOpenFile?: () => void;
}) {
  const isActive = () => tabStore.activeTabPath() === props.entry.path;
  // H-6: surface a per-note "sync issue" marker when the backend reported it
  // couldn't apply a peer's change to this note. `noteSyncFailures` is keyed by
  // roost-relative, forward-slash note path; the tree entry carries the
  // absolute, platform-separator path — convert before the lookup.
  const syncFailure = createMemo(() => {
    if (props.entry.is_dir) return undefined;
    const roost = roostStore.roostPath() ?? "";
    let rel = props.entry.path;
    if (roost && rel.startsWith(roost)) {
      rel = rel.slice(roost.length).replace(/^[\\/]+/, "");
    }
    rel = rel.replace(/\\/g, "/");
    return syncStatusStore.noteSyncFailures[rel];
  });
  const [renaming, setRenaming] = createSignal(false);
  const [renameName, setRenameName] = createSignal("");
  // Submission latch — both `onKeyDown(Enter)` AND `onBlur` on the
  // rename input call handleRenameSubmit. Without this flag, pressing
  // Enter fires the rename, the input unmounts (renaming flipped to
  // false), which triggers blur → second handleRenameSubmit. The first
  // call succeeds and renames the file; the second hits the now-gone
  // old path and the IPC returns "not found", surfacing a spurious
  // error popup on a rename that actually worked.
  let submitting = false;
  let inputRef!: HTMLInputElement;
  let fileRowRef!: HTMLDivElement;
  // Touch-pressed visual state. iOS Safari's `:active` pseudo-class is
  // unreliable inside WKWebView (it sometimes won't apply during a long
  // hold even with `cursor: pointer` set), so we track press explicitly:
  // on for touch pointerdown, off for up / cancel. Mouse / pen continue
  // to rely on `:hover` from desktop. Gives the user immediate "I see
  // your finger" feedback so the 500ms long-press wait doesn't feel
  // like the app is ignoring them.
  const [pressed, setPressed] = createSignal(false);
  let gestures: ReturnType<typeof attachTouchGestures> | undefined;
  onMount(() => {
    gestures = attachTouchGestures(fileRowRef, {
      onTap: () => {
        tabStore.openTab(props.entry.path);
        props.onAfterOpenFile?.();
      },
      onLongPress: (x, y) => {
        const ev = new MouseEvent("contextmenu", {
          clientX: x,
          clientY: y,
          bubbles: false,
        });
        props.onContextMenu(ev, props.entry);
      },
    });
    onCleanup(() => gestures?.dispose());
  });

  // Listen for inline rename trigger from context menu
  function handleStartRename() {
    const baseName = props.entry.name.replace(/\.md$/, "");
    setRenameName(baseName);
    setRenaming(true);
    submitting = false;
    requestAnimationFrame(() => {
      inputRef?.focus();
      inputRef?.select();
    });
  }

  async function handleRenameSubmit() {
    if (submitting) return;
    submitting = true;
    const newName = renameName().trim();
    if (!newName || newName === props.entry.name.replace(/\.md$/, "")) {
      setRenaming(false);
      return;
    }
    // Path separator: Windows entries come through as `W:\roost\file.md`,
    // POSIX as `/roost/file.md`. The previous regex only matched `/`, so
    // on Windows `dir` ended up as the whole path including filename and
    // the rename targeted a non-existent directory. Match both separators
    // and preserve whichever the source path uses, so the IPC sees a
    // well-formed path in the platform's native form.
    const sep = props.entry.path.includes("\\") ? "\\" : "/";
    const dir = props.entry.path.replace(/[\\/][^\\/]+$/, "");
    const newPath = `${dir}${sep}${newName}.md`;
    try {
      await unwrap(commands.renameFile(props.entry.path, newPath));
      tabStore.onFileRenamed(props.entry.path, newPath);
      // rename_file purges the old note id; re-create the node at the new
      // path now instead of waiting for the next save.
      void graphStore.indexPath(newPath);
      // AUDIT-065: tree refresh only.
      await roostStore.refreshFiles({ force: true });
    } catch (e) {
      const msg = errMsg(e);
      log.error("sidebar", "rename failed", { e, oldPath: props.entry.path, newPath });
      // If the backend says "not found", the sidebar is showing a stale
      // entry — refresh the tree so the user sees current state.
      if (/not\s*found/i.test(msg)) {
        // AUDIT-065: tree refresh only.
        await roostStore.refreshFiles({ force: true });
        alert(
          `Rename failed: file not found on disk.\n\nThis usually means the file was deleted or renamed outside the app. The file tree has been refreshed — try again.`,
        );
      } else {
        alert(`Rename failed: ${msg}\n\nOld: ${props.entry.path}\nNew: ${newPath}`);
      }
    }
    setRenaming(false);
  }

  return (
    <div
      data-path={props.entry.path}
      ref={(el) => {
        el.addEventListener("start-rename", handleStartRename);
        // AUDIT-028: virtualized rows re-enter the viewport during scroll
        // bursts — pair every addEventListener with an onCleanup so the
        // listener count stays bounded over long sessions.
        onCleanup(() => {
          el.removeEventListener("start-rename", handleStartRename);
        });
      }}
    >
      <Show
        when={!renaming()}
        fallback={
          <div
            class="mx-1 flex items-center gap-1 px-1.5 py-0.5 rounded-md h-[34px] sm:h-[22px]"
            style={{ "padding-left": `${6 + props.depth * 12}px` }}
          >
            <input
              ref={inputRef}
              type="text"
              value={renameName()}
              onInput={(e) => setRenameName(e.currentTarget.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter") handleRenameSubmit();
                if (e.key === "Escape") setRenaming(false);
              }}
              onBlur={() => handleRenameSubmit()}
              class="flex-1 min-w-0 px-1.5 py-0.5 rounded bg-bg-tertiary border border-accent text-text-primary text-[15px] sm:text-[13px] focus:outline-none"
            />
          </div>
        }
      >
        <div
          ref={fileRowRef}
          class={`mx-1 flex items-center px-1.5 py-0.5 rounded-md cursor-pointer text-[15px] sm:text-[13px] transition-colors h-[34px] sm:h-[22px] [-webkit-touch-callout:none] [-webkit-user-select:none] [touch-action:manipulation] select-none ${
            isActive()
              ? "bg-accent/15 text-text-primary font-medium"
              : pressed() || menuTargetPath() === props.entry.path
                ? "bg-bg-hover text-text-primary"
                : "text-text-muted hover:bg-bg-hover hover:text-text-primary"
          }`}
          style={{ "padding-left": `${6 + props.depth * 12}px` }}
          onPointerDown={(e) => {
            if (e.pointerType === "touch") setPressed(true);
          }}
          onPointerUp={() => setPressed(false)}
          onPointerCancel={() => setPressed(false)}
          onPointerLeave={() => setPressed(false)}
          onClick={(e) => {
            if (gestures?.shouldIgnoreClick(e)) {
              e.preventDefault();
              e.stopPropagation();
              return;
            }
            tabStore.openTab(props.entry.path);
            props.onAfterOpenFile?.();
          }}
          onContextMenu={(e) => props.onContextMenu(e, props.entry)}
          onDblClick={handleStartRename}
          draggable={true}
          onDragStart={(e) => {
            setDragSource(props.entry.path);
            e.dataTransfer!.effectAllowed = "move";
          }}
          onDragEnd={() => setDragSource(null)}
        >
          <span class="truncate">{displayName(props.entry)}</span>
          <Show when={syncFailure()}>
            <span
              class="ml-auto shrink-0 text-amber-400 text-xs leading-none select-none"
              title={`Sync issue: ${syncFailure()!.reason}`}
              aria-label="This note has a sync issue"
            >
              ⚠
            </span>
          </Show>
        </div>
      </Show>
    </div>
  );
}
