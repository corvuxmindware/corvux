import { For, Show, type JSX } from "solid-js";
import { tabStore } from "../../stores/tabs";
import { graphStore } from "../../stores/graph";
import Backlinks from "../Sidebar/Backlinks";
import OutlinePanel from "../Outline/OutlinePanel";
import type { RightPanel } from "../../app/right-sidebar";
import { isMacOS, isMobile } from "../../lib/platform";
import { isTauri } from "../../lib/invoke";
import { WINDOW_CONTROLS_WIDTH } from "../WindowControls/WindowControls";

interface RightSidebarProps {
  panel: RightPanel;
  onPanelChange: (p: RightPanel) => void;
  onGoToLine: (line: number) => void;
}

interface TabDef {
  id: RightPanel;
  label: string;
  icon: JSX.Element;
  badge?: () => boolean;
}

const ICON_BACKLINKS = (
  <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round">
    <path d="M10 13a5 5 0 007.54.54l3-3a5 5 0 00-7.07-7.07l-1.72 1.71" />
    <path d="M14 11a5 5 0 00-7.54-.54l-3 3a5 5 0 007.07 7.07l1.71-1.71" />
  </svg>
);
const ICON_OUTLINE = (
  <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round">
    <path d="M4 6h16M4 12h10M4 18h14" />
  </svg>
);

const TABS: TabDef[] = [
  { id: "backlinks", label: "Backlinks", icon: ICON_BACKLINKS },
  { id: "outline", label: "Outline", icon: ICON_OUTLINE },
];

// Obsidian-style right sidebar. Top toolbar (h-9, same height as the
// main TabBar and the left-pane header) hosts the panel-switcher icons
// on the left and the close button on the right. Content starts below
// the toolbar so the pane reads as a self-contained column.
export default function RightSidebar(props: RightSidebarProps) {
  // Reserve right-padding under the fixed <WindowControls /> overlay so the
  // close button doesn't slide under it. Mirrors the spacer in TabBar.
  const reserveControlsRoom = !isMobile() && isTauri && !isMacOS();
  const rightPad = reserveControlsRoom ? `${WINDOW_CONTROLS_WIDTH}px` : undefined;
  return (
    <div class="h-full flex flex-col overflow-hidden">
      <div
        class="h-10 pl-0 flex items-center gap-0.5 border-b border-border bg-bg-secondary shrink-0"
        style={rightPad ? { "padding-right": rightPad } : { "padding-right": "0.5rem" }}
      >
        <For each={TABS}>
          {(tab) => (
            <button
              onClick={() => props.onPanelChange(tab.id)}
              title={tab.label}
              aria-label={tab.label}
              class={`relative w-9 h-9 flex items-center justify-center rounded-md transition-colors ${
                props.panel === tab.id
                  ? "text-accent bg-accent/15"
                  : "text-text-muted hover:text-text-primary hover:bg-bg-hover"
              }`}
            >
              {tab.icon}
              <Show when={tab.badge && tab.badge() && props.panel !== tab.id}>
                <span class="absolute top-1 right-1 w-1.5 h-1.5 rounded-full bg-yellow" />
              </Show>
            </button>
          )}
        </For>

        <div class="flex-1" />
        {/* No close button here — the right-sidebar toggle lives in TabBar
            so it stays in the same horizontal band as the sync indicator,
            save, read/write, and the min/max/close trio. */}
      </div>

      <div class="flex-1 min-h-0 overflow-y-auto">
        <Show when={props.panel === "backlinks"}>
          <Show
            when={tabStore.activeTabPath() && graphStore.isReady()}
            fallback={
              <div class="px-4 py-6 text-xs text-text-muted italic">
                Open a note to see its backlinks.
              </div>
            }
          >
            <Backlinks />
          </Show>
        </Show>

        <Show when={props.panel === "outline"}>
          <OutlinePanel onGoToLine={props.onGoToLine} />
        </Show>
      </div>
    </div>
  );
}
