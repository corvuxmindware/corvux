import { createSignal } from "solid-js";
import { lsGet, lsSet } from "../lib/storage";

// Right sidebar = the secondary pane that hosts "about the current note"
// views (Backlinks, Outline). VSCode / Obsidian pattern. Desktop-only —
// on mobile these views land in the existing left-overlay tab strip
// instead, so this state never drives a mobile render.
//
// Three persisted signals:
//   - visible:  is the pane open at all
//   - width:    user-resizable, clamped, mirrors `sidebar-resize.ts`
//   - panel:    which tab is selected when the pane opens

export type RightPanel = "backlinks" | "outline";

export const RIGHT_SIDEBAR_MIN = 240;
export const RIGHT_SIDEBAR_MAX = 560;
export const RIGHT_SIDEBAR_DEFAULT = 300;

const KEY_WIDTH = "corvux:rightSidebarWidth";
const KEY_VISIBLE = "corvux:rightSidebarVisible";
const KEY_PANEL = "corvux:rightSidebarPanel";

function loadInitialWidth(): number {
  const v = parseInt(lsGet(KEY_WIDTH) ?? "", 10);
  return Number.isFinite(v) && v >= RIGHT_SIDEBAR_MIN && v <= RIGHT_SIDEBAR_MAX
    ? v
    : RIGHT_SIDEBAR_DEFAULT;
}

function loadInitialVisible(): boolean {
  return lsGet(KEY_VISIBLE) === "1";
}

function loadInitialPanel(): RightPanel {
  const v = lsGet(KEY_PANEL);
  if (v === "backlinks" || v === "outline") {
    return v;
  }
  return "backlinks";
}

export function createRightSidebar() {
  const [visible, setVisibleSignal] = createSignal(loadInitialVisible());
  const [panel, setPanelSignal] = createSignal<RightPanel>(loadInitialPanel());
  const [width, setWidth] = createSignal(loadInitialWidth());

  function setVisible(v: boolean) {
    setVisibleSignal(v);
    lsSet(KEY_VISIBLE, v ? "1" : "0");
  }

  function setPanel(p: RightPanel) {
    setPanelSignal(p);
    lsSet(KEY_PANEL, p);
  }

  // IconRail behavior: clicking a panel button toggles the pane if the
  // current tab is the one clicked, otherwise switches tab (and opens
  // the pane if it was closed). Mirrors how the left activity bar works.
  function toggle(p: RightPanel) {
    if (visible() && panel() === p) {
      setVisible(false);
      return;
    }
    setPanel(p);
    setVisible(true);
  }

  function startResize(e: PointerEvent) {
    e.preventDefault();
    const startX = e.clientX;
    const startW = width();
    document.body.style.cursor = "col-resize";
    document.body.style.userSelect = "none";
    const onMove = (ev: PointerEvent) => {
      // Right sidebar grows leftward — dragging the handle LEFT widens
      // the pane (negative delta increases width).
      const w = Math.max(
        RIGHT_SIDEBAR_MIN,
        Math.min(RIGHT_SIDEBAR_MAX, startW - (ev.clientX - startX)),
      );
      setWidth(w);
    };
    const onUp = () => {
      document.body.style.cursor = "";
      document.body.style.userSelect = "";
      lsSet(KEY_WIDTH, String(width()));
      window.removeEventListener("pointermove", onMove);
      window.removeEventListener("pointerup", onUp);
    };
    window.addEventListener("pointermove", onMove);
    window.addEventListener("pointerup", onUp);
  }

  function reset() {
    setWidth(RIGHT_SIDEBAR_DEFAULT);
    lsSet(KEY_WIDTH, String(RIGHT_SIDEBAR_DEFAULT));
  }

  return {
    visible,
    panel,
    width,
    setVisible,
    setPanel,
    toggle,
    startResize,
    reset,
  };
}
