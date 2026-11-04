import { createSignal, Show, createEffect, on } from "solid-js";
import ToastContainer from "./components/Toast/Toast";
import BugReportModal from "./components/BugReport/BugReportModal";
import ReauthPrompt from "./components/Roost/ReauthPrompt";
import DevPerfPanel from "./components/DevPerf/DevPerfPanel";
import MobileBottomBar from "./components/MobileBottomBar/MobileBottomBar";
import { isMacOS, isMobile as isMobilePlatform } from "./lib/platform";
import { commands, unwrap } from "./lib/invoke";
import { log } from "./lib/log";
import { loadImage, loadAudio, pasteImage } from "./lib/attachments";
import { createAutoHideOnScroll } from "./lib/auto-hide-on-scroll";
import { keyboardOpen } from "./lib/keyboard-inset";
import { createPaletteCommands } from "./lib/palette-commands";
import { navigateToWikiLink } from "./lib/wiki-link";
import type { PaletteCommand } from "./components/CommandPalette/CommandPalette";
import { registerKeyboardShortcuts } from "./lib/keyboard-shortcuts";
import { bookmarksStore } from "./stores/bookmarks";
import { roostStore } from "./stores/roost";
import { roostSettingsStore } from "./stores/roostSettings";
import { tabStore, GRAPH_TAB_PATH } from "./stores/tabs";
import { graphStore } from "./stores/graph";
import { recorderStore } from "./stores/recorder";
import { pickAndInsertImage } from "./lib/image-attach";
import RecorderSheet from "./components/Recorder/RecorderSheet";
import AppModals from "./app/AppModals";
import ChatPane from "./app/ChatPane";
import EditorArea from "./app/EditorArea";
import LeftPane from "./app/LeftPane";
import { PaneErrorBoundary } from "./components/PaneErrorBoundary";
import RightPane from "./app/RightPane";
import { promptStore } from "./components/Modals/PromptModal";
import WindowControls from "./components/WindowControls/WindowControls";
import { installAppEffects } from "./app/effects";
import {
  createAllNotes,
  createBreadcrumb,
  createDocStats,
  createParsedActive,
} from "./app/derived";
import { createSidebarResize } from "./app/sidebar-resize";
import { createChatResize } from "./app/chat-resize";
import { createRightSidebar } from "./app/right-sidebar";
import { createAndroidBackClose } from "./app/android-back-close";
import RightSidebar from "./components/RightSidebar/RightSidebar";

function App() {
  // ── Modal + panel state ───────────────────────────────────────────────
  // Desktop opens with the Files panel docked. Mobile starts with the
  // drawer CLOSED — the panel is a full-screen overlay there, so a
  // "files" default meant every launch opened on a dimmed backdrop the
  // user had to dismiss first (Playwright mobile audit, 2026-07-02).
  const [activePanel, setActivePanel] = createSignal(
    isMobilePlatform() ? "" : "files",
  );
  const [showChat, setShowChat] = createSignal(false);
  const [showQuickSwitcher, setShowQuickSwitcher] = createSignal(false);
  const [showMobileMenu, setShowMobileMenu] = createSignal(false);
  const [showCommandPalette, setShowCommandPalette] = createSignal(false);
  const [showSettings, setShowSettings] = createSignal(false);
  // Deep-link target for Settings — set BEFORE flipping showSettings so
  // the panel reads the right initial page on mount. Reset to undefined
  // on close so the next plain "open Settings" lands on the index.
  const [settingsInitialPage, setSettingsInitialPage] = createSignal<
    import("./components/Settings/SettingsPanel").PageId | undefined
  >(undefined);
  const openSettings = (
    page?: import("./components/Settings/SettingsPanel").PageId,
  ) => {
    setSettingsInitialPage(page);
    setShowSettings(true);
  };
  const closeSettings = () => {
    setShowSettings(false);
    setSettingsInitialPage(undefined);
  };
  const [showShortcuts, setShowShortcuts] = createSignal(false);
  const [showPathFinder, setShowPathFinder] = createSignal(false);
  const [showOnboarding, setShowOnboarding] = createSignal(false);
  const [goToLine, setGoToLine] = createSignal(0);

  // Track the most-recent file tab so the graph tab can focus the note
  // the user came from, not the first file in `tabs()` (which is what
  // the naive `tabs.find(kind === "file")` fallback resolved to — was
  // the cause of "graph always shows the first file's local view" on
  // 2026-05-26). Updated every time activeTabPath shifts to anything
  // that isn't the graph tab itself.
  const [lastFilePath, setLastFilePath] = createSignal<string | null>(null);
  createEffect(() => {
    const path = tabStore.activeTabPath();
    if (path && path !== GRAPH_TAB_PATH) setLastFilePath(path);
  });

  // Per-tab scroll position cache. The actual scroll container is the outer
  // div wrapping the editor (overflow-y-auto), not CM6's internal scrollDOM,
  // so we save/restore on this element when activeTabPath flips.
  let scrollContainerRef: HTMLDivElement | undefined;
  // Auto-hide the mobile bottom bar while the note content is scrolling;
  // it slides back up once scrolling stops (see createAutoHideOnScroll).
  const bottomBarAutoHide = createAutoHideOnScroll();
  const scrollCache = new Map<string, number>();
  createEffect(
    on(
      () => tabStore.activeTabPath(),
      (_newPath, oldPath) => {
        const el = scrollContainerRef;
        if (!el) return;
        if (oldPath) scrollCache.set(oldPath, el.scrollTop);
        // Defer restore so the new content has rendered into the scroll
        // container before we read or set its height.
        requestAnimationFrame(() => {
          if (scrollContainerRef)
            scrollContainerRef.scrollTop = scrollCache.get(_newPath ?? "") ?? 0;
        });
      },
      { defer: true }
    )
  );

  // Graph is now a tab, not an overlay. `isGraphActive` = active tab is graph.
  const isGraphActive = () => tabStore.activeTabPath() === GRAPH_TAB_PATH;

  // ── Roost helpers ─────────────────────────────────────────────────────
  async function openDailyNote() {
    const roost = roostStore.roostPath();
    if (!roost) return;
    try {
      // Honor Locked Decision #15: the daily-note folder is a roost
      // setting, not a hardcode. The store falls back to "Daily Notes"
      // for fresh roosts that haven't customized it.
      const folder = roostSettingsStore.settings().tasks.daily_note_folder;
      const path = await unwrap(commands.createDailyNote(folder));
      // create_daily_note writes the note to disk but doesn't index it, so
      // a brand-new daily note had no graph node (invisible in the global
      // graph; the local graph synthesizes the active note, which is why it
      // showed there). Index it now — same fix as the other create paths.
      void graphStore.indexPath(path);
      await roostStore.openRoost(roost); // refresh sidebar
      // You open a daily note to write in it — never land the user on a
      // locked page they have to unlock first.
      tabStore.openTab(path, { readOnly: false });
    } catch (e) {
      log.error("app", "failed to create daily note", e);
    }
  }

  async function createNewNoteFromChrome() {
    const roost = roostStore.roostPath();
    if (!roost) {
      setShowOnboarding(true);
      return;
    }
    const name = await promptStore.ask({
      title: "New note",
      placeholder: "Note name",
      confirmLabel: "Create",
    });
    if (!name) return;
    const path = await roostStore.createNewNote(name);
    if (path) tabStore.openTab(path, { readOnly: false });
  }

  const paletteCommands = (): PaletteCommand[] =>
    createPaletteCommands({
      activePanel,
      setActivePanel,
      setShowCommandPalette,
      setShowQuickSwitcher,
      setShowChat,
      showChat,
      setShowShortcuts,
      setShowPathFinder,
      openDailyNote,
    });

  // ── Effect wiring ─────────────────────────────────────────────────────
  installAppEffects({
    setShowOnboarding,
    registerKeyboardShortcuts: () =>
      registerKeyboardShortcuts({
        activePanel,
        setActivePanel,
        setShowCommandPalette,
        setShowQuickSwitcher,
        setShowChat,
        showChat,
        setShowShortcuts,
        showShortcuts,
        setShowPathFinder,
        openDailyNote,
      }),
  });

  // ── Derived state ─────────────────────────────────────────────────────
  const parsed = createParsedActive();
  const allNotes = createAllNotes();
  const stats = createDocStats(parsed);
  const breadcrumb = createBreadcrumb();

  // Load bookmarks whenever the active roost changes — bookmarks are
  // per-roost and live in `.corvux/bookmarks.json`. createEffect tracks
  // roostStore.roostPath() automatically.
  createEffect(() => {
    const vp = roostStore.roostPath();
    if (vp) void bookmarksStore.load(vp);
  });

  const sidebarVisible = () =>
    activePanel() === "files" ||
    activePanel() === "search" ||
    activePanel() === "bookmarks" ||
    activePanel() === "outline" ||
    activePanel() === "tasks" ||
    activePanel() === "history" ||
    activePanel() === "backlinks";

  // On mobile the sidebar + main area can't share the screen comfortably,
  // so navigation actions auto-close the sidebar. Matches Obsidian mobile.
  const isMobile = () => isMobilePlatform();
  const isMacDesktop = () => !isMobile() && isMacOS();
  const desktopRailWidth = 48;
  const desktopTrafficLightInset = 88;
  const desktopLeftWidth = () =>
    sidebarVisible() ? sidebar.width() + desktopRailWidth : desktopRailWidth;
  const closeSidebarOnMobile = () => {
    if (isMobile() && sidebarVisible()) setActivePanel("");
  };

  // Android hardware back closes the open mobile overlays (chat first,
  // then the nav drawer) instead of exiting the app. No-op off Android.
  // (Audit 2026-06-08, MED-5.)
  createAndroidBackClose(
    () => isMobile() && showChat(),
    () => setShowChat(false),
  );
  createAndroidBackClose(
    () => isMobile() && sidebarVisible(),
    () => setActivePanel(""),
  );

  // Left-sidebar toggle. Used by the mobile hamburger (TopBar) AND the
  // desktop button in the TabBar — same logic both places: if visible,
  // close; otherwise re-open to whatever the last picked panel was, or
  // Files as a fallback.
  function toggleLeftSidebar() {
    if (sidebarVisible()) setActivePanel("");
    else if (activePanel() === "") setActivePanel("files");
    else setActivePanel(activePanel());
  }

  // (Mobile drawer panel metadata moved to src/app/mobile-panels.ts — it was
  // duplicated here as dead code and disagreed with what LeftPane rendered.)

  // Sidebar resize state + handler.
  const sidebar = createSidebarResize();
  const chat = createChatResize();
  const rightSidebar = createRightSidebar();

  // Desktop-only: clicking an "about this note" button in the IconRail
  // (Backlinks / Outline) drives the RIGHT sidebar so it can sit alongside
  // the file tree. The mobile flow uses the existing left-overlay
  // activePanel — covered separately below.
  const rightSidebarVisible = () => !isMobile() && rightSidebar.visible();

  // ── Editor helpers ────────────────────────────────────────────────────
  const handleLoadImage = (relPath: string) =>
    loadImage(roostStore.roostPath(), relPath);
  const handleLoadAudio = (relPath: string) =>
    loadAudio(roostStore.roostPath(), relPath);
  const handleImagePaste = (file: File) =>
    pasteImage(roostStore.roostPath(), file);

  /// Delete an audio attachment from the roost (moves to .corvux-trash so
  /// the user can recover it later). Returns `true` if the audio-embed
  /// extension should also remove the matching markdown line from the
  /// active document.
  ///
  /// IMPORTANT: `delete_file` runs the path through `to_rel(path,
  /// roost_root)` which does `path.strip_prefix(roost_root)`. A bare
  /// roost-relative path like `.attachments/paste-XXX.webm` never matches
  /// the prefix, so the IPC bails with "outside the active roost" — that
  /// was the "Failed to delete" toast seen on 2026-05-26. We join here.
  async function handleDeleteAudio(relPath: string): Promise<boolean> {
    const vp = roostStore.roostPath();
    if (!vp) return false;
    const rel = relPath.replace(/^\.\/+/, "");
    // A-15: prefer the roost root's own separator style. Rust's
    // `to_rel` normalises backslashes to forward, but matching the
    // OS-native separator here keeps the path human-readable in log
    // messages and avoids relying on the Rust side to clean up.
    const usesBackslash = vp.includes("\\") && !vp.includes("/");
    const sep = vp.endsWith("/") || vp.endsWith("\\")
      ? ""
      : usesBackslash ? "\\" : "/";
    const relNative = usesBackslash ? rel.replace(/\//g, "\\") : rel;
    const abs = `${vp}${sep}${relNative}`;
    try {
      await unwrap(commands.deleteFile(abs, vp));
      return true;
    } catch (e) {
      log.error("app", `delete audio failed: ${relPath} (abs=${abs})`, e);
      return false;
    }
  }

  /// Navigate to a [[wiki-link]] target — find matching note or create it.
  /// Editor's Ctrl+Click navigation calls this. Same resolver also
  /// powers the chat panel's wiki-link clicks (extracted to
  /// `lib/wiki-link.ts` 2026-05-20). Editor passes `createIfMissing:
  /// true` because Obsidian-style "Ctrl+click a non-existent link to
  /// create the note" is the established editor behavior. The chat
  /// surface passes `false` to avoid auto-creating notes off the
  /// agent's prose.
  async function handleWikiNavigate(target: string) {
    await navigateToWikiLink(target, { createIfMissing: true });
  }

  /// Open a search result at a specific line.
  async function handleOpenResult(path: string, line: number) {
    await tabStore.openTab(path);
    closeSidebarOnMobile();
    setGoToLine(0); // reset so the effect fires even for the same line
    requestAnimationFrame(() => setGoToLine(line));
  }

  return (
    <div class="relative flex h-full w-full bg-bg-primary">
      {/* Window min/max/close trio — fixed at top-right of the window so
          they always sit at the right edge regardless of which side panes
          are open. Windows + Linux only (macOS uses native traffic lights). */}
      <WindowControls />

      {/* macOS reopen-sidebar affordance. The desktop sidebar header
          (which normally hosts the toggle) doesn't render when the
          sidebar collapses on macOS — the wrapper is only 48px wide and
          the header's `pl-[88px]` would overflow it. We render the
          toggle as a floating absolute element anchored at x=88px so it
          lands at the same horizontal position the sidebar-open toggle
          does (which uses `pl-[88px]` to clear the traffic lights). */}
      <Show when={isMacDesktop() && !sidebarVisible()}>
        <button
          onClick={toggleLeftSidebar}
          data-tauri-drag-region="false"
          title="Show left sidebar (Ctrl+B)"
          aria-label="Show left sidebar"
          class="absolute top-0 left-[88px] w-10 h-10 z-[45] flex items-center justify-center rounded-lg text-text-muted hover:text-text-primary hover:bg-bg-hover transition-colors"
        >
          <svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
            <rect x="3" y="4" width="18" height="16" rx="2" />
            <line x1="9" y1="4" x2="9" y2="20" />
          </svg>
        </button>
      </Show>
      {/* Backdrop — mobile only. */}
      <Show when={isMobile() && sidebarVisible()}>
        <div
          class="absolute inset-0 bg-black/40 z-30"
          onClick={() => setActivePanel("")}
        />
      </Show>

      {/* Mobile floating drawer toggle — sits at the right edge of the
          drawer and slides with it. Closed state: x=8 (left of screen,
          acts as opener). Open state: at the panel's right edge, just
          inside the drawer (acts as closer). Same icon both ways.
          Hidden while the AI chat panel is open — the chat takes the
          full mobile viewport and its own header occupies this slot. */}
      <Show when={isMobile() && !showChat()}>
        <button
          onClick={toggleLeftSidebar}
          aria-label={sidebarVisible() ? "Close navigation" : "Open navigation"}
          aria-pressed={sidebarVisible()}
          class={`absolute top-0 z-[45] w-[44px] h-[3.25rem] flex items-center justify-center rounded-lg transition-all duration-200 ${
            sidebarVisible()
              ? "text-accent bg-accent/15"
              : "text-text-secondary hover:text-text-primary hover:bg-bg-hover"
          }`}
          style={{
            // Slide with the drawer via transform (GPU-composited, smooth)
            // rather than animating `left` (which reflows + stutters). Base
            // at left:8 (closed = screen edge); open, translate to just
            // outside the drawer's right edge. Must mirror the sidebar width
            // (LeftPane mobile branch): drawerWidth - 4, so the toggle lands
            // 4px past the panel edge.
            left: "8px",
            transform: sidebarVisible()
              ? "translateX(calc(min(300px, calc(100vw - 24px)) - 4px))"
              : "translateX(0)",
          }}
        >
          {/* 18px to match the mobile sibling icons (tab strip + tree
              actions are all 18px; the desktop rail uses 20px). */}
          <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
            <rect x="3" y="4" width="18" height="16" rx="2" />
            <line x1="9" y1="4" x2="9" y2="20" />
          </svg>
        </button>
      </Show>

      <PaneErrorBoundary label="sidebar">
      <LeftPane
        isMobile={isMobile}
        isMacDesktop={isMacDesktop}
        sidebarVisible={sidebarVisible}
        activePanel={activePanel}
        setActivePanel={setActivePanel}
        toggleLeftSidebar={toggleLeftSidebar}
        closeSidebarOnMobile={closeSidebarOnMobile}
        showChat={showChat}
        setShowChat={setShowChat}
        isGraphActive={isGraphActive}
        setShowSettings={setShowSettings}
        openDailyNote={openDailyNote}
        handleOpenResult={handleOpenResult}
        setGoToLine={setGoToLine}
        sidebar={sidebar}
        desktopLeftWidth={desktopLeftWidth}
      />
      </PaneErrorBoundary>

      {/* Resize handle. */}
      <Show when={sidebarVisible() && !isMobile()}>
        <div
          class="h-full w-1 cursor-col-resize shrink-0 bg-bg-secondary hover:bg-accent/40 transition-colors"
          onPointerDown={sidebar.startResize}
          onDblClick={sidebar.reset}
          title="Drag to resize · double-click to reset"
        />
      </Show>

      <PaneErrorBoundary label="editor">
      <EditorArea
        isMobile={isMobile}
        isMacDesktop={isMacDesktop}
        sidebarVisible={sidebarVisible}
        isGraphActive={isGraphActive}
        showChat={showChat}
        breadcrumb={breadcrumb}
        parsed={parsed}
        stats={stats}
        scrollContainerRef={(el) => (scrollContainerRef = el)}
        onContentScroll={bottomBarAutoHide.onScroll}
        chromeHidden={bottomBarAutoHide.hidden}
        desktopTrafficLightInset={desktopTrafficLightInset}
        allNotes={allNotes}
        goToLine={goToLine}
        setGoToLine={setGoToLine}
        handleWikiNavigate={handleWikiNavigate}
        handleImagePaste={handleImagePaste}
        handleLoadImage={handleLoadImage}
        handleLoadAudio={handleLoadAudio}
        handleDeleteAudio={handleDeleteAudio}
        lastFilePath={lastFilePath}
        rightSidebar={rightSidebar}
        onNewNote={createNewNoteFromChrome}
        onOpenSettings={() => setShowSettings(true)}
        onOpenQuickSwitcher={() => setShowQuickSwitcher(true)}
        onOpenCommandPalette={() => setShowCommandPalette(true)}
        onInsertImage={() => void pickAndInsertImage()}
        onRecordVoice={() => recorderStore.open()}
        onToggleMobileSidebar={toggleLeftSidebar}
        onOpenMobileMenu={() => setShowMobileMenu(true)}
      />
      </PaneErrorBoundary>

      <PaneErrorBoundary label="right panel">
      <RightPane
        rightSidebar={rightSidebar}
        visible={rightSidebarVisible}
        setGoToLine={setGoToLine}
      />
      </PaneErrorBoundary>

      <PaneErrorBoundary label="chat">
      <ChatPane
        showChat={showChat}
        isMobile={isMobile}
        chat={chat}
        onClose={() => setShowChat(false)}
        onOpenSettings={() => {
          // Mobile: chat and settings are both full-screen overlays;
          // opening settings without closing chat leaves them stacked
          // and visibly overlapping. Close chat first so settings has
          // a clean canvas. Desktop keeps chat open — both fit
          // side-by-side in the panel layout.
          if (isMobile()) setShowChat(false);
          openSettings("ai");
        }}
      />
      </PaneErrorBoundary>


      {/* Floating action bar — MOBILE ONLY (reverted desktop display
          2026-06-03). Match platform conventions: a thumb-reachable bottom
          bar is the mobile standard, but on desktop it's non-standard,
          duplicates the left IconRail (which already has Graph + Agent) and
          the top TabBar, and floated over content (it overlapped the graph
          legend). Desktop uses the rail + top bar + shortcuts instead.

          On mobile it's still hidden when: the sidebar drawer is open
          (overlay mode), the chat panel is open (shares the bottom
          y-coordinate), or the Graph view is active (the graph is a
          full-screen view with its own footer/controls; the graph's close
          X handles getting back). */}
      <Show when={isMobile() && !sidebarVisible() && !showChat() && !isGraphActive()}>
        <MobileBottomBar
          onOpenQuickSwitcher={() => setShowQuickSwitcher(true)}
          onToggleGraph={() => {
            // Floating-bar graph opens to the LOCAL (current note's
            // neighbourhood) view by default — the desktop IconRail still
            // opens full because that's the discoverability surface.
            // We only set the mode hint when *opening*; closing/re-
            // showing an existing graph tab should respect whatever the
            // user last left it on.
            const open = tabStore.tabs().find((t) => t.path === GRAPH_TAB_PATH);
            if (!open || tabStore.activeTabPath() !== GRAPH_TAB_PATH) {
              graphStore.requestGraphMode("local");
            }
            tabStore.toggleGraphTab();
          }}
          onToggleChat={() => setShowChat(true)}
          graphOpen={isGraphActive()}
          leftOffset={isMobile() ? 0 : desktopLeftWidth()}
          rightOffset={isMobile() || !rightSidebar.visible() ? 0 : rightSidebar.width()}
          // Hide while scrolling the note (auto-hide) OR while the keyboard is
          // up — a soft keyboard means the user is typing, and the floating bar
          // must never sit over the caret/last lines.
          // `keyboardOpen`, not `keyboardInset > 0`: on WebViews that RESIZE
          // for the keyboard the inset is legitimately 0, and this bar used
          // to stay on screen while typing — stacked under the format bar,
          // both covering the line being edited.
          hidden={bottomBarAutoHide.hidden() || keyboardOpen()}
        />
      </Show>

      <Show when={recorderStore.isOpen()}>
        <RecorderSheet onClose={() => recorderStore.close()} />
      </Show>

      <ToastContainer />
      <BugReportModal />
      <ReauthPrompt />
      <DevPerfPanel />

      <AppModals
        showQuickSwitcher={showQuickSwitcher()}
        closeQuickSwitcher={() => setShowQuickSwitcher(false)}
        showCommandPalette={showCommandPalette()}
        closeCommandPalette={() => setShowCommandPalette(false)}
        paletteCommands={paletteCommands()}
        showSettings={showSettings()}
        closeSettings={closeSettings}
        settingsInitialPage={settingsInitialPage()}
        openShortcuts={() => setShowShortcuts(true)}
        showMobileMenu={showMobileMenu()}
        closeMobileMenu={() => setShowMobileMenu(false)}
        mobileMenuHandlers={{
          onOpenQuickSwitcher: () => setShowQuickSwitcher(true),
          onOpenCommandPalette: () => setShowCommandPalette(true),
          onOpenSettings: () => setShowSettings(true),
          onToggleGraph: () => {
            tabStore.toggleGraphTab();
            if (tabStore.activeTabPath() === GRAPH_TAB_PATH)
              closeSidebarOnMobile();
          },
          onToggleChat: () => {
            const next = !showChat();
            setShowChat(next);
            if (next) closeSidebarOnMobile();
          },
          onToggleOutline: () =>
            setActivePanel(activePanel() === "outline" ? "" : "outline"),
          onOpenPathFinder: () => setShowPathFinder(true),
          // Opens the drawer to the Backlinks panel (replaces the floating
          // mobile backlinks chip — backlinks now live in the ⋮ menu).
          onShowBacklinks: () => setActivePanel("backlinks"),
          // Editing actions moved out of the (trimmed) bottom bar into More.
          onInsertImage: () => void pickAndInsertImage(),
          onRecordVoice: () => recorderStore.open(),
          graphOpen: isGraphActive(),
          chatOpen: showChat(),
          outlineOpen: activePanel() === "outline",
        }}
        showOnboarding={showOnboarding()}
        completeOnboarding={(openChat?: boolean) => {
          setShowOnboarding(false);
          if (openChat) setShowChat(true);
        }}
        showShortcuts={showShortcuts()}
        closeShortcuts={() => setShowShortcuts(false)}
        showPathFinder={showPathFinder()}
        closePathFinder={() => setShowPathFinder(false)}
      />
    </div>
  );
}

export default App;
