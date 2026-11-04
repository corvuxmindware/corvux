import { For, Show } from "solid-js";
import { tabStore, isFileTab } from "../../stores/tabs";
import { isMobile } from "../../lib/platform";
import { startupStore } from "../../stores/startup";
import { recorderStore } from "../../stores/recorder";
import SyncIndicator from "./SyncIndicator";

interface Breadcrumb {
  roost: string;
  segments: string[];
  path: string;
}

interface TopBarProps {
  breadcrumb: () => Breadcrumb | null;
  onOpenQuickSwitcher: () => void;
  onOpenCommandPalette: () => void;
  onOpenSettings: () => void;
  // Mobile-only: hamburger toggles the sidebar drawer, overflow opens the menu.
  onToggleMobileSidebar?: () => void;
  onOpenMobileMenu?: () => void;
  sidebarOpen?: boolean;
}

/**
 * Breadcrumb row — sits BELOW the TabBar (which owns row 1 + window
 * chrome + drag region). Desktop: back/forward + breadcrumb segments +
 * Ctrl+O / Ctrl+P icon buttons + read/write toggle + save + sync
 * indicator. Mobile: hamburger (drawer toggle) + short breadcrumb +
 * save + overflow menu.
 */
export default function TopBar(props: TopBarProps) {
  const mobile = isMobile();

  return (
    // Mobile height matches the drawer's panel-tab strip (icon+label, ~52px)
    // so the bottom border lines up continuously across the drawer edge when
    // the drawer is open. Desktop keeps the compact h-10.
    <div class="h-[3.25rem] sm:h-10 bg-bg-primary border-b border-border flex items-center pl-12 pr-2 sm:px-3 gap-2 sm:gap-3 shrink-0">
      {/* pl-12 on mobile reserves room for the floating drawer-toggle
          (absolute, left:8, w-9) so the breadcrumb doesn't sit under it. */}


      {/* Mobile sidebar toggle is no longer in TopBar — it now floats
          at the right edge of the drawer, so it slides with the panel
          and serves as both opener (closed state) and closer (open). */}

      {/* Startup progress — visible while the app warms up post roost-open.
          Sequenced phases (roost → settings → history → sync) so the user
          sees what's happening instead of a frozen UI. Hidden once `ready`. */}
      <Show when={startupStore.isWarming()}>
        <div
          class="flex items-center gap-2 text-text-muted text-xs"
          role="status"
          aria-live="polite"
          aria-label={startupStore.label()}
        >
          <span class="inline-block w-3.5 h-3.5 border-2 border-text-muted border-t-transparent rounded-full animate-spin shrink-0" />
          <span class="truncate">{startupStore.label()}</span>
        </div>
      </Show>

      <Show when={!mobile}>
        {/* Desktop-only back/forward nav — wired as no-ops; reserved for history */}
        <div class="flex items-center gap-1" data-tauri-drag-region="false">
          <button
            data-tauri-drag-region="false"
            class="text-text-secondary hover:text-text-primary p-1 rounded hover:bg-bg-hover transition-colors"
            title="Back"
            aria-label="Back"
          >
            <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
              <path d="M15 18l-6-6 6-6" />
            </svg>
          </button>
          <button
            data-tauri-drag-region="false"
            class="text-text-secondary hover:text-text-primary p-1 rounded hover:bg-bg-hover transition-colors"
            title="Forward"
            aria-label="Forward"
          >
            <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
              <path d="M9 18l6-6-6-6" />
            </svg>
          </button>
        </div>
      </Show>

      {/* Centered breadcrumb: equal flex-1 spacers on both sides push
          the breadcrumb to the middle of the row (Obsidian layout). If
          the path is wider than the middle gap, `truncate` clips the
          start with an ellipsis — segments stay readable instead of
          every folder collapsing to 3 chars. Roost prefix is dropped
          (Obsidian doesn't show it). Hidden on mobile while the
          sidebar drawer is open — the file name shouldn't bleed
          through next to the drawer. */}
      <div class="flex-1" />
      <Show when={!(mobile && props.sidebarOpen)}>
        <Show when={props.breadcrumb()}>
          {(bc) => (
            <div class="text-sm min-w-0 max-w-[70%] overflow-hidden whitespace-nowrap text-ellipsis" dir="rtl">
              <span dir="ltr">
                <For each={bc().segments}>
                  {(seg, i) => {
                    const isLast = () => i() === bc().segments.length - 1;
                    return (
                      <>
                        <Show when={i() > 0}>
                          <span class="text-text-muted mx-1.5">/</span>
                        </Show>
                        <span
                          class={
                            isLast()
                              ? "text-text-primary font-medium"
                              : "text-text-muted"
                          }
                        >
                          {seg}
                        </span>
                      </>
                    );
                  }}
                </For>
              </span>
            </div>
          )}
        </Show>
        <Show when={!props.breadcrumb()}>
          <span class="text-text-muted text-sm truncate">No file open</span>
        </Show>
      </Show>
      <div class="flex-1" />

      <Show when={!mobile}>
        <button
          onClick={props.onOpenQuickSwitcher}
          data-tauri-drag-region="false"
          class="p-1.5 rounded text-text-secondary hover:text-text-primary hover:bg-bg-hover transition-colors"
          title="Quick Switcher (Ctrl+O)"
          aria-label="Open quick switcher"
        >
          <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
            <circle cx="11" cy="11" r="8" />
            <path d="M21 21l-4.35-4.35" />
          </svg>
        </button>

        <button
          onClick={props.onOpenCommandPalette}
          data-tauri-drag-region="false"
          class="p-1.5 rounded text-text-secondary hover:text-text-primary hover:bg-bg-hover transition-colors"
          title="Command Palette (Ctrl+P)"
          aria-label="Open command palette"
        >
          <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
            <path d="M4 17l6-6-6-6M12 19h8" />
          </svg>
        </button>

        {/* Voice memo — desktop entry point. Disabled when the active tab
            isn't a file (graph tab, no roost). Same gating as the mobile
            bottom bar's mic button. */}
        <button
          onClick={() => {
            const path = tabStore.activeTabPath();
            if (path === null || path.startsWith("corvux://")) return;
            recorderStore.open();
          }}
          data-tauri-drag-region="false"
          disabled={(() => {
            const p = tabStore.activeTabPath();
            return p === null || p.startsWith("corvux://");
          })()}
          class="p-1.5 rounded text-text-secondary hover:text-text-primary hover:bg-bg-hover transition-colors disabled:opacity-40 disabled:cursor-not-allowed"
          title="Voice memo (Ctrl+Shift+R)"
          aria-label="Record voice memo"
        >
          <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
            <rect x="9" y="2" width="6" height="12" rx="3" />
            <path d="M5 10v2a7 7 0 0 0 14 0v-2" />
            <line x1="12" y1="19" x2="12" y2="22" />
            <line x1="8" y1="22" x2="16" y2="22" />
          </svg>
        </button>
      </Show>

      {/* Mobile save-state — relocated here from the removed bottom status
          strip, so the bottom belongs to the action bar alone. Subtle,
          right-aligned, where the eye already rests. Hidden while the mobile
          drawer is open (same as the breadcrumb) so it doesn't bleed through
          behind the floating drawer toggle. */}
      <Show when={mobile && !props.sidebarOpen && isFileTab(tabStore.activeTab())}>
        <span class="text-text-muted text-xs shrink-0 tabular-nums">
          {tabStore.isModified() ? "Saving…" : "Saved"}
        </span>
      </Show>

      {/* Lock toggle — also hidden behind the open mobile drawer to avoid the
          same bleed-through. */}
      <Show when={isFileTab(tabStore.activeTab()) && !(mobile && props.sidebarOpen)}>
        <button
          onClick={() => tabStore.toggleReadOnly()}
          data-tauri-drag-region="false"
          class={`flex items-center justify-center gap-1.5 px-2 py-1 min-h-[44px] min-w-[44px] sm:min-h-0 sm:min-w-0 rounded transition-colors text-xs ${
            tabStore.isReadOnly()
              ? "text-accent hover:text-accent-hover hover:bg-bg-hover"
              : "text-text-secondary hover:text-text-primary hover:bg-bg-hover"
          }`}
          title={tabStore.isReadOnly() ? "Read mode — tap to unlock" : "Write mode — tap to lock"}
          aria-pressed={tabStore.isReadOnly()}
        >
          <Show
            when={tabStore.isReadOnly()}
            fallback={
              <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                <path d="M12 20h9" />
                <path d="M16.5 3.5a2.121 2.121 0 013 3L7 19l-4 1 1-4L16.5 3.5z" />
              </svg>
            }
          >
            <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
              <rect x="3" y="11" width="18" height="11" rx="2" ry="2" />
              <path d="M7 11V7a5 5 0 0110 0v4" />
            </svg>
          </Show>
        </button>
      </Show>

      <Show when={tabStore.isModified()}>
        <div class="flex items-center gap-1" data-tauri-drag-region="false">
          <div class="w-2 h-2 rounded-full bg-accent animate-pulse" />
          <button
            onClick={() => tabStore.saveTab()}
            data-tauri-drag-region="false"
            class="text-sm text-accent hover:text-accent-hover px-2 py-0.5 rounded hover:bg-bg-hover transition-colors"
          >
            Save
          </button>
        </div>
      </Show>

      {/* Removed: decorative pencil-on-paper icon. It was a non-interactive
          visual marker that you were on a file tab, redundant with both
          the tab itself and the read/write toggle to its left. */}

      <SyncIndicator onNeedsSetup={props.onOpenSettings} />

      {/* Right-sidebar toggle lives in the TabBar (row 1) — Obsidian
          convention puts it on the top-most row at the far right. */}

      {/* Window controls moved to TabBar (row 1). This breadcrumb row no
          longer hosts min/max/close. */}

      {/* Mobile overflow (⋮) — moved here from the floating bottom bar so the
          bar holds five primary actions and the rarely-used overflow lives in
          the top-right (the conventional overflow corner). Tapping opens the
          MobileMenu bottom sheet. Hidden while the drawer is open to avoid
          competing with the drawer strip's own "More". */}
      <Show when={mobile && !props.sidebarOpen}>
        <button
          onClick={() => props.onOpenMobileMenu?.()}
          aria-label="More actions"
          class="w-11 h-11 -mr-1 flex items-center justify-center rounded-full text-text-secondary active:bg-bg-hover transition-colors shrink-0"
        >
          <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round">
            <circle cx="12" cy="5" r="1.5" />
            <circle cx="12" cy="12" r="1.5" />
            <circle cx="12" cy="19" r="1.5" />
          </svg>
        </button>
      </Show>
    </div>
  );
}
