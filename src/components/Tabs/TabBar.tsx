import { For, Show, createEffect, createSignal, onCleanup, onMount } from "solid-js";
import { tabStore, type Tab } from "../../stores/tabs";
import { isMobile, isMacOS } from "../../lib/platform";
import { isTauri } from "../../lib/invoke";
import TabBarRightCluster from "./TabBarRightCluster";
import { WINDOW_CONTROLS_WIDTH } from "../WindowControls/WindowControls";

interface TabBarProps {
  // Space to reserve before the tab strip. Used on macOS when the left
  // sidebar is collapsed so tabs do not sit under native traffic lights.
  leftInset?: number;
  // Right sidebar toggle. Always passed — the button lives here in both
  // states (the RightSidebar header no longer carries a close button).
  // The icon is the same; only the tooltip / aria-label distinguishes
  // reopen vs hide.
  onToggleRightSidebar: () => void;
  rightSidebarOpen: boolean;
  // When the chat panel is open, `WindowControls` (fixed top-right of
  // the window) sits OVER the chat panel's header — not over the
  // TabBar's right edge. The TabBar should not reserve the 120 px
  // window-chrome spacer in that case; doing so leaves a visibly empty
  // gap to the left of the chat panel.
  chatOpen?: boolean;
  // Plus button next to the tab strip, matching Obsidian's new-tab affordance.
  onNewNote?: () => void;
  // `onOpenSettings` is needed because the SyncIndicator routes users
  // to the sync setup pane when it detects a misconfiguration.
  onOpenSettings?: () => void;
  onInsertImage?: () => void;
  onRecordVoice?: () => void;
  // Reopen-left-sidebar affordance. Only rendered when the sidebar is
  // collapsed AND the sidebar header is hidden (i.e. macOS desktop with
  // sidebar closed — that state currently leaves the user with no
  // visible way back into the sidebar except Ctrl+B). Passed
  // unconditionally; the button only appears when `leftInset > 0`.
  onToggleLeftSidebar?: () => void;
}

/**
 * Row 1 of the window when `decorations: false` is set. Hosts the tab
 * strip (left, scrollable) and the window-control trio (right). The
 * empty area on either side of the tab strip carries
 * `data-tauri-drag-region` so the user can drag the window by grabbing
 * any non-interactive part of this row — matches the Obsidian / VS Code
 * pattern. Always renders on desktop even with zero tabs so the title
 * bar / drag handle is always present.
 */
export default function TabBar(props: TabBarProps) {
  const mobile = isMobile();
  // The min/max/close trio lives in <WindowControls /> fixed at the
  // window's top-right. We only need to reserve space here when the
  // TabBar's right edge IS the window's right edge — i.e. when nothing
  // else is occupying the top-right corner. When the right sidebar or
  // chat panel is open, one of them owns that corner instead, and the
  // spacer would just paint dead pixels to the left of that pane.
  const showWindowChromeSpacer = () =>
    !mobile &&
    isTauri &&
    !isMacOS() &&
    !props.rightSidebarOpen &&
    !props.chatOpen;

  // Refs for auto-scrolling the active tab into view. Captured by inline
  // `ref={(el) => { ... }}` callbacks below — Solid runs them once on mount
  // for each element. `activeTabEl` is reassigned every render of the For,
  // which is fine: the effect below reads the current value after the
  // reactive tracker fires.
  let tabStripRef: HTMLDivElement | undefined;
  let activeTabEl: HTMLDivElement | undefined;

  // Edge-fade signals — set when there's overflow on that side. Drives
  // two absolutely-positioned gradient overlays (rendered as siblings of
  // the scroll container below) that paint the bg color over the
  // leftmost/rightmost 28 px of the strip. The earlier `mask-image`
  // approach only thinned opacity, which produced a barely-visible ghost
  // — the overlay paints actual color matching --color-bg-secondary and
  // reads as a clean "tab cut off here" edge, the way Obsidian/VSCode
  // signal overflow. Updated on scroll, tab change, and window resize.
  const [canScrollLeft, setCanScrollLeft] = createSignal(false);
  const [canScrollRight, setCanScrollRight] = createSignal(false);

  function updateScrollState() {
    if (!tabStripRef) return;
    const { scrollLeft, scrollWidth, clientWidth } = tabStripRef;
    setCanScrollLeft(scrollLeft > 0);
    // 1 px tolerance: subpixel scroll positions otherwise leave the
    // right fade stuck on even when the strip is fully scrolled.
    setCanScrollRight(scrollLeft + clientWidth < scrollWidth - 1);
  }

  onMount(() => {
    if (!tabStripRef) return;
    updateScrollState();
    tabStripRef.addEventListener("scroll", updateScrollState, { passive: true });
    window.addEventListener("resize", updateScrollState);
    onCleanup(() => {
      tabStripRef?.removeEventListener("scroll", updateScrollState);
      window.removeEventListener("resize", updateScrollState);
    });
  });

  createEffect(() => {
    // Track active tab so the effect re-runs on switch / open / close.
    tabStore.activeTabPath();
    // Also track tab count so adding/closing a tab refreshes the fade.
    tabStore.tabs().length;
    // Wait one frame so the For has rendered and `activeTabEl` points at
    // the new active row, then scroll it into view if it's clipped.
    queueMicrotask(() => {
      if (!tabStripRef || !activeTabEl) return;
      const strip = tabStripRef.getBoundingClientRect();
      const tab = activeTabEl.getBoundingClientRect();
      if (tab.left < strip.left || tab.right > strip.right) {
        activeTabEl.scrollIntoView({ block: "nearest", inline: "nearest" });
      }
      updateScrollState();
    });
  });

  function handleMouseDown(e: MouseEvent, tab: Tab) {
    // Middle-click to close
    if (e.button === 1) {
      e.preventDefault();
      tabStore.closeTab(tab.path);
    }
  }

  // Mobile: hide entirely — Obsidian shows only the active note and
  // routes multi-file switching through the Files drawer / Quick
  // Switcher / Command Palette. A persistent tab row on a 360 px phone
  // eats vertical space for zero benefit.
  // Desktop: always render. Even with zero tabs the row is the window
  // drag handle and hosts the custom min/max/close (decorations:false on
  // Windows). With zero tabs the strip just shows the "+" button alone.
  const showTabBar = () => !mobile;
  return (
    <Show when={showTabBar()}>
      <div
        class="h-10 bg-bg-secondary border-b border-border flex items-stretch shrink-0"
        data-tauri-drag-region
      >
        <Show when={(props.leftInset ?? 0) > 0}>
          <div
            class="shrink-0"
            style={{ width: `${props.leftInset}px` }}
            data-tauri-drag-region
          />
        </Show>

        {/* Reopen-sidebar button lives in App.tsx as a floating
            absolute-positioned element so it can match the same x=88px
            anchor the sidebar-open toggle uses. Rendering it in TabBar
            put it ~50px too far right (TabBar starts after the 48px
            IconRail column). */}

        {/*
          Tab strip: hide the WebKit/Firefox scrollbars on the overflow
          container. The previous `scrollbar-thin` class wasn't a Tailwind
          v4 utility (no-op), so the browser drew its default 8px
          horizontal scrollbar inside a 40px-tall row that holds 37px-tall
          tabs — overlapping the bottom of every tab. Wheel-scroll, swipe,
          and shift+wheel still work; we just don't paint a track.
          The `tabStripRef` ref + createEffect below auto-scroll the active
          tab into view when it changes, so opening a tab via Quick Switcher
          or wiki-link doesn't leave it stranded off-screen.
        */}
        <div
          class="relative flex-1 min-w-0 flex"
          data-tauri-drag-region
        >
        <div
          ref={(el) => { tabStripRef = el; }}
          class="flex-1 min-w-0 flex items-end overflow-x-auto [&::-webkit-scrollbar]:hidden [scrollbar-width:none]"
          data-tauri-drag-region
        >
          <div class="flex items-end min-w-0" data-tauri-drag-region="false">
            <Show when={tabStore.tabs().length > 0}>
              <For each={tabStore.tabs()}>
                {(tab) => {
                  const isActive = () => tab.path === tabStore.activeTabPath();
                  return (
                    <div
                      ref={(el) => { if (isActive()) activeTabEl = el; }}
                      class={`group flex items-center gap-1.5 cursor-pointer border-r border-border select-none transition-colors shrink-0 px-3 ${
                        "h-[37px] rounded-t-md"
                      } text-sm max-w-[180px] ${
                        isActive()
                          ? "bg-bg-primary text-text-primary border-t-2 border-t-accent"
                          : "bg-bg-secondary text-text-muted hover:text-text-secondary hover:bg-bg-hover border-t-2 border-t-transparent"
                      }`}
                      onClick={() => tabStore.switchToTab(tab.path)}
                      onMouseDown={(e) => handleMouseDown(e, tab)}
                      data-tauri-drag-region="false"
                    >
                      <Show when={tab.isModified}>
                        <div class="w-2 h-2 rounded-full bg-accent shrink-0" />
                      </Show>

                      <Show
                        when={tab.kind === "graph"}
                        fallback={
                          <svg
                            width="13"
                            height="13"
                            viewBox="0 0 24 24"
                            fill="none"
                            stroke="currentColor"
                            stroke-width="2"
                            class="shrink-0 opacity-60"
                          >
                            <path d="M14 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8z" />
                            <polyline points="14 2 14 8 20 8" />
                          </svg>
                        }
                      >
                        <svg
                          width="13"
                          height="13"
                          viewBox="0 0 24 24"
                          fill="none"
                          stroke="currentColor"
                          stroke-width="1.8"
                          stroke-linecap="round"
                          stroke-linejoin="round"
                          class="shrink-0 opacity-80"
                        >
                          <circle cx="12" cy="12" r="2.2" fill="currentColor" opacity="0.8" />
                          <circle cx="5" cy="5" r="1.8" />
                          <circle cx="19" cy="7" r="1.8" />
                          <circle cx="6" cy="19" r="1.8" />
                          <circle cx="19" cy="17" r="1.8" />
                          <line x1="10" y1="10.5" x2="7" y2="6.5" />
                          <line x1="14" y1="11" x2="17" y2="8.5" />
                          <line x1="10.5" y1="13.5" x2="8" y2="17.5" />
                          <line x1="14" y1="13.5" x2="17.5" y2="15.5" />
                        </svg>
                      </Show>

                      <span class="truncate text-xs">
                        {tab.kind === "graph" ? tab.name : tab.name.replace(/\.md$/, "")}
                      </span>

                      <button
                        class={`shrink-0 rounded p-0.5 transition-colors ${
                          isActive()
                            ? "opacity-60 hover:opacity-100 hover:bg-bg-hover"
                            : "opacity-0 group-hover:opacity-60 hover:!opacity-100 hover:bg-bg-hover"
                        }`}
                        onClick={(e) => {
                          e.stopPropagation();
                          tabStore.closeTab(tab.path);
                        }}
                        data-tauri-drag-region="false"
                        title="Close tab"
                        aria-label={`Close ${tab.name}`}
                      >
                        <svg
                          width="12"
                          height="12"
                          viewBox="0 0 24 24"
                          fill="none"
                          stroke="currentColor"
                          stroke-width="2"
                        >
                          <path d="M18 6L6 18M6 6l12 12" />
                        </svg>
                      </button>
                    </div>
                  );
                }}
              </For>
            </Show>
            <Show when={props.onNewNote}>
              <button
                onClick={props.onNewNote}
                data-tauri-drag-region="false"
                title="New note"
                aria-label="New note"
                class="h-[37px] w-7 flex items-center justify-center shrink-0 border-r border-border border-t-2 border-t-transparent text-text-muted hover:text-text-primary hover:bg-bg-hover transition-colors"
              >
                <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.9" stroke-linecap="round" stroke-linejoin="round">
                  <path d="M12 5v14M5 12h14" />
                </svg>
              </button>
            </Show>
          </div>
        </div>
        {/* Edge overlays — sit above the scrolled tabs, painted in the
            tab bar's own bg color so the clipped tab visibly cuts off
            (vs. the previous mask-image which only thinned opacity and
            barely showed). Pointer-events disabled so wheel-scroll over
            the fade still reaches the strip. */}
        <Show when={canScrollLeft()}>
          <div
            class="pointer-events-none absolute top-0 left-0 h-full w-7"
            style={{
              background: "linear-gradient(to right, var(--color-bg-secondary), transparent)",
            }}
            aria-hidden="true"
          />
        </Show>
        <Show when={canScrollRight()}>
          <div
            class="pointer-events-none absolute top-0 right-0 h-full w-7"
            style={{
              background: "linear-gradient(to left, var(--color-bg-secondary), transparent)",
            }}
            aria-hidden="true"
          />
        </Show>
        </div>

        {/* Right-edge cluster: read/write toggle, save (when dirty),
            startup progress, sync indicator. Always renders in the TabBar
            regardless of right-sidebar state — when the sidebar is open,
            the TabBar narrows but the cluster still sits at its right
            edge, which keeps the icons in the same horizontal band the
            user is used to from the sidebar-closed layout. */}
        <TabBarRightCluster
          onOpenSettings={props.onOpenSettings}
          onInsertImage={props.onInsertImage}
          onRecordVoice={props.onRecordVoice}
        />

        {/* Right-sidebar toggle — lives here in both open and closed
            states. The RightSidebar header no longer carries its own
            close button, so this is the single affordance for opening
            and hiding the pane. */}
        <button
          onClick={props.onToggleRightSidebar}
          data-tauri-drag-region="false"
          title={props.rightSidebarOpen ? "Hide right sidebar" : "Show right sidebar"}
          aria-label={props.rightSidebarOpen ? "Hide right sidebar" : "Show right sidebar"}
          aria-pressed={props.rightSidebarOpen}
          class={`relative w-7 flex items-center justify-center shrink-0 transition-colors ${
            props.rightSidebarOpen
              ? "text-accent hover:text-accent-hover hover:bg-bg-hover"
              : "text-text-secondary hover:text-text-primary hover:bg-bg-hover"
          }`}
        >
          <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round">
            <rect x="3" y="4" width="18" height="16" rx="2" />
            <line x1="15" y1="4" x2="15" y2="20" />
          </svg>
        </button>

        {/* Reserve space under the fixed-positioned <WindowControls />
            overlay so this row's right-cluster buttons don't slide under
            it. Carries the drag region so the user can still grab the
            window by this strip on Windows. */}
        <Show when={showWindowChromeSpacer()}>
          <div
            class="shrink-0"
            style={{ width: `${WINDOW_CONTROLS_WIDTH}px` }}
            data-tauri-drag-region
          />
        </Show>
      </div>
    </Show>
  );
}
