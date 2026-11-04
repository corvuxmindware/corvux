import { createSignal } from "solid-js";
import { lsGet, lsSet } from "../lib/storage";

// Chat panel width is user-resizable via a handle on its LEFT edge.
// Persisted across reloads. Constants differ from the sidebar — the chat
// panel hosts long AI replies + tool-call cards, so the upper bound is
// generous, the floor still readable.
export const CHAT_MIN = 280;
export const CHAT_MAX = 700;
export const CHAT_DEFAULT = 340;
const STORAGE_KEY = "corvux:chatWidth";

function loadInitialWidth(): number {
  const v = parseInt(lsGet(STORAGE_KEY) ?? "", 10);
  return Number.isFinite(v) && v >= CHAT_MIN && v <= CHAT_MAX
    ? v
    : CHAT_DEFAULT;
}

export function createChatResize() {
  const [width, setWidth] = createSignal(loadInitialWidth());

  function startResize(e: PointerEvent) {
    e.preventDefault();
    const startX = e.clientX;
    const startW = width();
    document.body.style.cursor = "col-resize";
    document.body.style.userSelect = "none";
    // Drag LEFT → grow chat (delta is reversed vs sidebar).
    const onMove = (ev: PointerEvent) => {
      const w = Math.max(
        CHAT_MIN,
        Math.min(CHAT_MAX, startW + (startX - ev.clientX)),
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
    setWidth(CHAT_DEFAULT);
    lsSet(STORAGE_KEY, String(CHAT_DEFAULT));
  }

  return { width, startResize, reset };
}
