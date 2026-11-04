// Left pane: sidebar header (Obsidian-style on desktop, tab strip on
// mobile) + IconRail + active panel content (Files / Search /
// Bookmarks / Backlinks / Outline / Tasks / History). Owns the
// mobile-only ⋮ overflow menu state.
//
// Split off App.tsx — this is the single biggest block in the shell
// (~315 LoC of JSX) and stands alone behind its activePanel signal.

import { createSignal, For, Show, type Setter } from "solid-js";
import Backlinks from "../components/Sidebar/Backlinks";
import BookmarksPanel from "../components/Bookmarks/BookmarksPanel";
import EditHistoryPanel from "../components/EditHistory/EditHistoryPanel";
import IconRail from "../components/IconRail/IconRail";
import OutlinePanel from "../components/Outline/OutlinePanel";
import SearchPanel from "../components/Search/SearchPanel";
import Sidebar from "../components/Sidebar/Sidebar";
import TasksPanel from "../components/Tasks/TasksPanel";
import { GRAPH_TAB_PATH, tabStore } from "../stores/tabs";
import { mobilePrimaryPanels, mobileOverflowPanels } from "./mobile-panels";
import type { createSidebarResize } from "./sidebar-resize";

interface LeftPaneProps {
  isMobile: () => boolean;
  isMacDesktop: () => boolean;
  sidebarVisible: () => boolean;
  activePanel: () => string;
  setActivePanel: Setter<string>;
  toggleLeftSidebar: () => void;
  closeSidebarOnMobile: () => void;
  showChat: () => boolean;
  setShowChat: Setter<boolean>;
  isGraphActive: () => boolean;
  setShowSettings: Setter<boolean>;
  openDailyNote: () => Promise<void>;
  handleOpenResult: (path: string, line: number) => Promise<void>;
  setGoToLine: Setter<number>;
  sidebar: ReturnType<typeof createSidebarResize>;
  desktopLeftWidth: () => number;
}

export default function LeftPane(props: LeftPaneProps) {
  const [mobileOverflowOpen, setMobileOverflowOpen] = createSignal(false);
  const isOverflowActive = () =>
    mobileOverflowPanels.some((p) => p.id === props.activePanel());

  return (
    <div
      class={`h-full bg-bg-secondary border-r border-border flex flex-col overflow-hidden ${
        props.isMobile() ? "absolute top-0 bottom-0 left-0 z-40" : "relative"
      }`}
      style={
        props.isMobile()
          ? {
              // Mobile sidebar: 300px — slightly narrower than the old 320px
              // floor, capped at 100vw-24 for very small screens. The panel
              // keeps this width whether open or closed and SLIDES via
              // transform (GPU-composited) instead of animating width —
              // animating layout (width/left) reflows every frame and
              // stutters; transform doesn't.
              width: "min(300px, calc(100vw - 24px))",
              // Files panel extends through the bottom safe-area inset so
              // the footer's notes/links stats can sit in the home-indicator
              // strip (flanking the centred pill). Scoped to Files only —
              // other panels (Search/Tasks/History) keep their content above
              // the inset so their last row never slides under the pill.
              // `top-0` + this height (no `bottom`) avoids the over-constraint
              // CSS would hit if top/bottom/height were all set at once.
              height:
                props.activePanel() === "files"
                  ? "calc(100% + env(safe-area-inset-bottom, 0px))"
                  : "100%",
              transform: props.sidebarVisible()
                ? "translateX(0)"
                : "translateX(-100%)",
              transition: "transform 200ms ease",
              "will-change": "transform",
            }
          : {
              width: `${props.desktopLeftWidth()}px`,
              "min-width": `${props.desktopLeftWidth()}px`,
            }
      }
    >
      {/* Mobile-only panel tab strip. Three primary panels + ⋮ overflow. */}
      <Show when={props.isMobile() && props.sidebarVisible()}>
        {/* h-[3.25rem] matches the editor TopBar's mobile height so the
            bottom border lines up continuously across the drawer edge. */}
        <div class="h-[3.25rem] flex items-center border-b border-border bg-bg-tertiary shrink-0 relative">
          <For each={mobilePrimaryPanels}>
            {(panel) => (
              <button
                onClick={() => {
                  props.setActivePanel(panel.id);
                  setMobileOverflowOpen(false);
                }}
                class={`flex-1 flex flex-col items-center gap-0.5 py-2 text-[10px] transition-colors relative ${
                  props.activePanel() === panel.id
                    ? "text-accent"
                    : "text-text-muted hover:text-text-secondary"
                }`}
              >
                <svg
                  width="18"
                  height="18"
                  viewBox="0 0 24 24"
                  fill="none"
                  stroke="currentColor"
                  stroke-width="1.8"
                  stroke-linecap="round"
                  stroke-linejoin="round"
                >
                  <path d={panel.icon} />
                </svg>
                <span>{panel.label}</span>
                <Show when={props.activePanel() === panel.id}>
                  <span class="absolute bottom-0 left-1/4 right-1/4 h-0.5 bg-accent rounded-t" />
                </Show>
              </button>
            )}
          </For>
          <button
            onClick={() => setMobileOverflowOpen((v) => !v)}
            aria-label="More panels"
            aria-expanded={mobileOverflowOpen()}
            class={`flex-1 flex flex-col items-center gap-0.5 py-2 text-[10px] transition-colors relative ${
              isOverflowActive() || mobileOverflowOpen()
                ? "text-accent"
                : "text-text-muted hover:text-text-secondary"
            }`}
          >
            <svg
              width="18"
              height="18"
              viewBox="0 0 24 24"
              fill="currentColor"
              stroke="none"
            >
              <circle cx="12" cy="6" r="1.75" />
              <circle cx="12" cy="12" r="1.75" />
              <circle cx="12" cy="18" r="1.75" />
            </svg>
            <span>More</span>
            <Show when={isOverflowActive()}>
              <span class="absolute bottom-0 left-1/4 right-1/4 h-0.5 bg-accent rounded-t" />
            </Show>
          </button>
          <Show when={mobileOverflowOpen()}>
            <>
              <div
                class="fixed inset-0 z-40"
                onClick={() => setMobileOverflowOpen(false)}
              />
              <div class="absolute top-full right-1 z-50 mt-1 bg-bg-secondary border border-border rounded-md shadow-lg min-w-[170px] py-1">
                <For each={mobileOverflowPanels}>
                  {(panel) => (
                    <button
                      onClick={() => {
                        props.setActivePanel(panel.id);
                        setMobileOverflowOpen(false);
                      }}
                      class={`w-full min-h-[44px] px-3 py-2.5 flex items-center gap-2.5 text-sm text-left transition-colors ${
                        props.activePanel() === panel.id
                          ? "text-accent bg-accent/10"
                          : "text-text-secondary hover:bg-bg-hover hover:text-text-primary"
                      }`}
                    >
                      <svg
                        width="16"
                        height="16"
                        viewBox="0 0 24 24"
                        fill="none"
                        stroke="currentColor"
                        stroke-width="1.8"
                        stroke-linecap="round"
                        stroke-linejoin="round"
                      >
                        <path d={panel.icon} />
                      </svg>
                      <span>{panel.label}</span>
                    </button>
                  )}
                </For>
              </div>
            </>
          </Show>
        </div>
      </Show>

      {/* Desktop left-panel header — Obsidian-style. Hidden on mobile;
          hidden on macOS when the sidebar is collapsed (the 48px-wide
          closed state can't host the pl-[88px] header). */}
      <Show
        when={
          !props.isMobile() &&
          !(props.isMacDesktop() && !props.sidebarVisible())
        }
      >
        <div
          class={`h-10 ${
            props.isMacDesktop() ? "pl-[88px] pr-0" : "pl-1 pr-0"
          } flex items-center gap-0.5 border-b border-border bg-bg-secondary shrink-0`}
          data-tauri-drag-region
        >
          <button
            onClick={props.toggleLeftSidebar}
            title={
              props.sidebarVisible()
                ? "Hide left sidebar (Ctrl+B)"
                : "Show left sidebar (Ctrl+B)"
            }
            aria-label={
              props.sidebarVisible() ? "Hide left sidebar" : "Show left sidebar"
            }
            aria-pressed={props.sidebarVisible()}
            data-tauri-drag-region="false"
            class={`w-10 h-10 flex items-center justify-center rounded-lg transition-colors shrink-0 ${
              props.sidebarVisible()
                ? "text-accent bg-accent/15"
                : "text-text-muted hover:text-text-primary hover:bg-bg-hover"
            }`}
          >
            <svg
              width="20"
              height="20"
              viewBox="0 0 24 24"
              fill="none"
              stroke="currentColor"
              stroke-width="2"
              stroke-linecap="round"
              stroke-linejoin="round"
            >
              <rect x="3" y="4" width="18" height="16" rx="2" />
              <line x1="9" y1="4" x2="9" y2="20" />
            </svg>
          </button>

          <Show when={props.sidebarVisible()}>
            <div class="w-1.5" />
          </Show>

          <Show when={props.sidebarVisible()}>
            <button
              onClick={() => props.setActivePanel("files")}
              title="Files"
              aria-label="Files"
              data-tauri-drag-region="false"
              class={`w-9 h-9 flex items-center justify-center rounded-md transition-colors ${
                props.activePanel() === "files"
                  ? "text-accent bg-accent/15"
                  : "text-text-muted hover:text-text-primary hover:bg-bg-hover"
              }`}
            >
              <svg
                width="18"
                height="18"
                viewBox="0 0 24 24"
                fill="none"
                stroke="currentColor"
                stroke-width="2"
                stroke-linecap="round"
                stroke-linejoin="round"
              >
                <path d="M3 7v10a2 2 0 002 2h14a2 2 0 002-2V9a2 2 0 00-2-2h-6l-2-2H5a2 2 0 00-2 2z" />
              </svg>
            </button>
            <button
              onClick={() => props.setActivePanel("search")}
              title="Search"
              aria-label="Search"
              data-tauri-drag-region="false"
              class={`w-9 h-9 flex items-center justify-center rounded-md transition-colors ${
                props.activePanel() === "search"
                  ? "text-accent bg-accent/15"
                  : "text-text-muted hover:text-text-primary hover:bg-bg-hover"
              }`}
            >
              <svg
                width="18"
                height="18"
                viewBox="0 0 24 24"
                fill="none"
                stroke="currentColor"
                stroke-width="2"
                stroke-linecap="round"
                stroke-linejoin="round"
              >
                <circle cx="11" cy="11" r="8" />
                <path d="M21 21l-4.35-4.35" />
              </svg>
            </button>
            <button
              onClick={() => props.setActivePanel("bookmarks")}
              title="Bookmarks"
              aria-label="Bookmarks"
              data-tauri-drag-region="false"
              class={`w-9 h-9 flex items-center justify-center rounded-md transition-colors ${
                props.activePanel() === "bookmarks"
                  ? "text-accent bg-accent/15"
                  : "text-text-muted hover:text-text-primary hover:bg-bg-hover"
              }`}
            >
              <svg
                width="18"
                height="18"
                viewBox="0 0 24 24"
                fill="none"
                stroke="currentColor"
                stroke-width="2"
                stroke-linecap="round"
                stroke-linejoin="round"
              >
                <path d="M19 21l-7-5-7 5V5a2 2 0 012-2h10a2 2 0 012 2z" />
              </svg>
            </button>
          </Show>

          <div class="flex-1" />
        </div>
      </Show>

      <div class={!props.isMobile() ? "flex flex-1 min-h-0" : "contents"}>
        <Show when={!props.isMobile()}>
          <div class="flex flex-col shrink-0">
            <Show when={props.isMacDesktop() && !props.sidebarVisible()}>
              {/* Title-bar spacer that clears the macOS traffic lights.
                  No border-b: the header divider would otherwise run a
                  short segment directly under the traffic lights, which
                  reads as a stray line. The divider now starts at the
                  editor column's TabBar instead. */}
              <div class="h-10 shrink-0 bg-bg-secondary" />
            </Show>
            <div class="flex-1 min-h-0">
              <IconRail
                activePanel={props.activePanel()}
                chatOpen={props.showChat()}
                graphOpen={props.isGraphActive()}
                tasksOpen={props.activePanel() === "tasks"}
                historyOpen={props.activePanel() === "history"}
                showPanelSwitchers={false}
                onPanelChange={props.setActivePanel}
                onChatToggle={() => {
                  const next = !props.showChat();
                  props.setShowChat(next);
                  if (next) props.closeSidebarOnMobile();
                }}
                onGraphToggle={() => {
                  tabStore.toggleGraphTab();
                  if (tabStore.activeTabPath() === GRAPH_TAB_PATH)
                    props.closeSidebarOnMobile();
                }}
                onTasksToggle={() =>
                  props.setActivePanel(
                    props.activePanel() === "tasks" ? "" : "tasks",
                  )
                }
                onHistoryToggle={() =>
                  props.setActivePanel(
                    props.activePanel() === "history" ? "" : "history",
                  )
                }
                onSettingsClick={() => props.setShowSettings(true)}
              />
            </div>
          </div>
        </Show>

        <div
          class={
            !props.isMobile()
              ? "flex-1 min-w-0 flex flex-col overflow-hidden"
              : "contents"
          }
        >
          <Show when={props.activePanel() === "files"}>
            <div class="flex-1 min-h-0 flex flex-col">
              <Sidebar
                onAfterOpenFile={props.closeSidebarOnMobile}
                onOpenSettings={
                  props.isMobile() ? () => props.setShowSettings(true) : undefined
                }
                onDailyNote={() => {
                  void props.openDailyNote();
                  props.closeSidebarOnMobile();
                }}
                onBookmarks={
                  props.isMobile()
                    ? () => props.setActivePanel("bookmarks")
                    : undefined
                }
              />
            </div>
          </Show>
          <Show when={props.activePanel() === "search"}>
            <div class="flex-1 min-h-0 flex flex-col">
              <SearchPanel onOpenResult={props.handleOpenResult} />
            </div>
          </Show>
          <Show when={props.activePanel() === "bookmarks"}>
            <div class="flex-1 min-h-0 flex flex-col">
              <BookmarksPanel
                onOpen={(path: string) => tabStore.openTab(path)}
              />
            </div>
          </Show>
          <Show when={props.activePanel() === "backlinks"}>
            <div class="flex-1 min-h-0 flex flex-col overflow-y-auto">
              <Show
                when={tabStore.activeTabPath()}
                fallback={
                  <div class="px-4 py-6 text-xs text-text-muted italic">
                    Open a note to see its backlinks.
                  </div>
                }
              >
                <Backlinks />
              </Show>
            </div>
          </Show>
          <Show when={props.activePanel() === "outline"}>
            <div class="flex-1 min-h-0 flex flex-col">
              <OutlinePanel
                onGoToLine={(line) => {
                  props.setGoToLine(0);
                  requestAnimationFrame(() => props.setGoToLine(line));
                }}
              />
            </div>
          </Show>
          <Show when={props.activePanel() === "tasks"}>
            <div class="flex-1 min-h-0 flex flex-col">
              <TasksPanel
                onOpenAtLine={props.handleOpenResult}
                onOpenSettings={() => props.setShowSettings(true)}
              />
            </div>
          </Show>
          <Show when={props.activePanel() === "history"}>
            <div class="flex-1 min-h-0 flex flex-col">
              <EditHistoryPanel />
            </div>
          </Show>
        </div>
      </div>
    </div>
  );
}
