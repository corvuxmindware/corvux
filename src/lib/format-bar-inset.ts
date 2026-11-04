// Height (px) of the mobile markdown format bar currently overlapping the
// editor, or 0 when the bar isn't on screen (read mode, desktop, graph tab).
//
// Why this exists: `MobileFormatBar` is `position: fixed`, so it paints over
// the editor without taking layout space. Two consumers have to compensate or
// the bar eats the bottom of the note —
//   - `.cm-content`'s mobile `padding-bottom` (global.css), so the last lines
//     can still be scrolled clear of the bar,
//   - CodeMirror's `scrollMargins` (Editor.tsx), so the caret is never parked
//     behind it while typing.
// Same shape and contract as `keyboard-inset.ts`: a signal for TS consumers,
// mirrored to a CSS var for style-only ones.
//
// Measured rather than hardcoded: the bar is one row of `h-11` buttons inside
// `py-1` today (~52px), but nothing pins that, and a stale constant here
// silently re-creates the clipped-last-line bug.
import { createSignal } from "solid-js";

const [formatBarInset, setFormatBarInsetSignal] = createSignal(0);

export { formatBarInset };

/// Publish the bar's current height. Called by `MobileFormatBar` after
/// layout, and with 0 whenever it unmounts or hides.
export function setFormatBarInset(px: number): void {
  const next = Math.max(0, Math.round(px));
  if (next === formatBarInset()) return;
  setFormatBarInsetSignal(next);
  if (typeof document !== "undefined") {
    document.documentElement.style.setProperty(
      "--corvux-format-bar-inset",
      `${next}px`,
    );
  }
}
