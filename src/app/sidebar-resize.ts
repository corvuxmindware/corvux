import { createSignal } from "solid-js";
import { lsGet, lsSet } from "../lib/storage";

// Sidebar width is user-resizable via a handle on its right edge. Persisted
// across reloads and clamped so the file tree never collapses to nothing or
// pushes the editor off-screen.
export const SIDEBAR_MIN = 200;
export const SIDEBAR_MAX = 560;
export const SIDEBAR_DEFAULT = 260;
const STORAGE_KEY = "corvux:sidebarWidth";

function loadInitialWidth(): number {
  const v = parseInt(lsGet(STORAGE_KEY) ?? "", 10);
  return Number.isFinite(v) && v >= SIDEBAR_MIN && v <= SIDEBAR_MAX
    ? v
    : SIDEBAR_DEFAULT;
}

export function createSidebarResize() {
  const [width, setWidth] = createSignal(loadInitialWidth());

  function startResize(e: PointerEvent) {
    e.preventDefault();
    const startX = e.clientX;
    const startW = width();
    document.body.style.cursor = "col-resize";
    document.body.style.userSelect = "none";
    const onMove = (ev: PointerEvent) => {
      const w = Math.max(
        SIDEBAR_MIN,
        Math.min(SIDEBAR_MAX, startW + (ev.clientX - startX)),
      );
      setWidth(w);
    };
    const onUp = () => {
      document.body.style.cursor = "";
      document.body.style.userSelect = "";
      lsSet(STORAGE_KEY, String(width()));
      window.removeEventListener("pointermove", onMove);
      window.removeEventListener("pointerup", onUp);
    };
    window.addEventListener("pointermove", onMove);
    window.addEventListener("pointerup", onUp);
  }

  function reset() {
    setWidth(SIDEBAR_DEFAULT);
    lsSet(STORAGE_KEY, String(SIDEBAR_DEFAULT));
  }

  return { width, startResize, reset };
}
