import { Show } from "solid-js";
import { tabStore, isFileTab } from "../../stores/tabs";
import { startupStore } from "../../stores/startup";
import SyncIndicator from "../TopBar/SyncIndicator";
import { isMobile } from "../../lib/platform";

// The right-edge cluster of "what the current tab is doing" affordances:
// startup spinner, read/write toggle, dirty-save button, sync indicator.
// Extracted from TabBar so it can also render inside RightSidebar's
// header — when the right sidebar is open, the TabBar shrinks down to
// just the tab strip and these icons would otherwise sit stranded far
// from the window-right edge. RightSidebar pulls them into its header so
// they cluster with the actual right-edge controls (close + min/max/close).
export interface TabBarRightClusterProps {
  /** Required for the SyncIndicator's "tap to open Settings → Sync" affordance.
   *  Omit to hide the indicator entirely. */
  onOpenSettings?: () => void;
  onInsertImage?: () => void;
  onRecordVoice?: () => void;
}

export default function TabBarRightCluster(props: TabBarRightClusterProps) {
  return (
    <>
      <Show when={startupStore.isWarming()}>
        <div
          class="flex items-center gap-2 text-text-muted text-xs px-2 shrink-0"
          role="status"
          aria-live="polite"
          aria-label={startupStore.label()}
          data-tauri-drag-region="false"
        >
          <span class="inline-block w-3 h-3 border-2 border-text-muted border-t-transparent rounded-full animate-spin shrink-0" />
          <span class="truncate max-w-[160px]">{startupStore.label()}</span>
        </div>
      </Show>

      {/* File-tab desktop actions. Hidden on mobile — the left-sidebar
          toggle button sits at the same right edge of the tab bar when
          the drawer is open, and extra chrome overlaps on narrow iPhone
          widths. Mobile attachment actions live in the ⋮ overflow menu. */}
      <Show when={isFileTab(tabStore.activeTab()) && !isMobile()}>
        <div class="flex items-stretch shrink-0" data-tauri-drag-region="false">
          <Show when={props.onInsertImage}>
            <button
              onClick={() => props.onInsertImage?.()}
              data-tauri-drag-region="false"
              class="w-8 flex items-center justify-center text-text-secondary hover:text-text-primary hover:bg-bg-hover transition-colors"
              title="Add image"
              aria-label="Add image"
            >
              <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.9" stroke-linecap="round" stroke-linejoin="round">
                <rect x="3" y="3" width="18" height="18" rx="2" />
                <circle cx="8.5" cy="8.5" r="1.5" />
                <path d="M21 15l-5-5L5 21" />
              </svg>
            </button>
          </Show>
          <Show when={props.onRecordVoice}>
            <button
              onClick={() => props.onRecordVoice?.()}
              data-tauri-drag-region="false"
              class="w-8 flex items-center justify-center text-text-secondary hover:text-text-primary hover:bg-bg-hover transition-colors"
              title="Add audio recording"
              aria-label="Add audio recording"
            >
              <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.9" stroke-linecap="round" stroke-linejoin="round">
                <rect x="9" y="2" width="6" height="12" rx="3" />
                <path d="M5 10v2a7 7 0 0 0 14 0v-2" />
                <line x1="12" y1="19" x2="12" y2="22" />
                <line x1="8" y1="22" x2="16" y2="22" />
              </svg>
            </button>
          </Show>
        </div>
      </Show>

      <Show when={isFileTab(tabStore.activeTab()) && !isMobile()}>
        <button
          onClick={() => tabStore.toggleReadOnly()}
          data-tauri-drag-region="false"
          class={`flex items-center justify-center px-2 transition-colors shrink-0 ${
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
        <button
          onClick={() => tabStore.saveTab()}
          data-tauri-drag-region="false"
          class="flex items-center gap-1 px-2 text-xs text-accent hover:text-accent-hover hover:bg-bg-hover transition-colors shrink-0"
          title="Save (Ctrl+S)"
        >
          <span class="w-2 h-2 rounded-full bg-accent animate-pulse" />
          <span>Save</span>
        </button>
      </Show>

      <Show when={props.onOpenSettings}>
        <div class="flex items-center px-1 shrink-0" data-tauri-drag-region="false">
          <SyncIndicator onNeedsSetup={props.onOpenSettings!} />
        </div>
      </Show>
    </>
  );
}
