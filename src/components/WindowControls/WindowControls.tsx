import { Show } from "solid-js";
import { isMobile, isMacOS } from "../../lib/platform";
import { isTauri } from "../../lib/invoke";

// Width of the min/max/close trio. Used by callers (TabBar, RightSidebar
// header) to reserve space at their right edges so their own right-aligned
// buttons don't sit under this overlay. Each button is `px-3 ≈ 24px wide
// padding + 12px icon ≈ 36px`, three of them = 108. Round up to 120 for
// a small safety margin.
export const WINDOW_CONTROLS_WIDTH = 120;

async function getWindow() {
  const { getCurrentWindow } = await import("@tauri-apps/api/window");
  return getCurrentWindow();
}
async function winMinimize() { (await getWindow()).minimize(); }
async function winToggleMaximize() {
  const w = await getWindow();
  (await w.isMaximized()) ? w.unmaximize() : w.maximize();
}
async function winClose() { (await getWindow()).close(); }

// Windows + Linux only — macOS renders native traffic lights (`decorations`
// stays true on macOS in tauri.conf.json). Mobile has no window chrome.
// Fixed-positioned at the top-right of the window so the trio always sits
// flush to the window edge regardless of whether the right sidebar is open
// or where the main TabBar's flex container ends.
export default function WindowControls() {
  const showWindowChrome = !isMobile() && isTauri && !isMacOS();
  return (
    <Show when={showWindowChrome}>
      <div
        class="fixed top-0 right-0 z-[70] flex items-stretch h-10 bg-bg-secondary/95 backdrop-blur-sm"
        data-tauri-drag-region="false"
      >
        <button
          onClick={winMinimize}
          data-tauri-drag-region="false"
          class="px-3 flex items-center justify-center text-text-secondary hover:text-text-primary hover:bg-bg-hover transition-colors"
          aria-label="Minimize"
          title="Minimize"
        >
          <svg width="12" height="12" viewBox="0 0 12 12" fill="none" stroke="currentColor" stroke-width="1.2">
            <path d="M2 6h8" stroke-linecap="round" />
          </svg>
        </button>
        <button
          onClick={winToggleMaximize}
          data-tauri-drag-region="false"
          class="px-3 flex items-center justify-center text-text-secondary hover:text-text-primary hover:bg-bg-hover transition-colors"
          aria-label="Maximize"
          title="Maximize"
        >
          <svg width="12" height="12" viewBox="0 0 12 12" fill="none" stroke="currentColor" stroke-width="1.2">
            <rect x="2.5" y="2.5" width="7" height="7" />
          </svg>
        </button>
        <button
          onClick={winClose}
          data-tauri-drag-region="false"
          class="px-3 flex items-center justify-center text-text-secondary hover:text-white hover:bg-red-500 transition-colors"
          aria-label="Close"
          title="Close"
        >
          <svg width="12" height="12" viewBox="0 0 12 12" fill="none" stroke="currentColor" stroke-width="1.2">
            <path d="M2 2l8 8M10 2l-8 8" stroke-linecap="round" />
          </svg>
        </button>
      </div>
    </Show>
  );
}
