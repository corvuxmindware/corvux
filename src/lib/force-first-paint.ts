import { isIOS } from "./platform";

/// iOS-only mitigation for the WKWebView "first frame is never composited
/// until the user touches the screen" stall — the app looks frozen on a cold
/// first launch and only renders once you tap. The DOM is built and laid out
/// (the shell falls back to `100dvh`, so it isn't a zero-height collapse), but
/// WebKit doesn't flush its layer tree to the screen until an event forces a
/// repaint, and the user's touch is what unsticks it. This is a documented
/// iOS class of bug (see e.g. tauri-apps/tauri#5143, #5170, #14371 and Apple
/// Developer Forums "WKWebView shows white screen until restart"), not
/// anything in our render logic — so we force that first repaint ourselves.
///
/// How: flip a *compositor-only* property (`opacity`) on `#root` across a
/// frame boundary, a few times over the cold-start window. The rAF boundary
/// guarantees WebKit paints at least one frame with the dirtied value, which
/// is exactly the flush the touch would have triggered.
///
/// Why opacity and NOT transform: a `transform` on the shell would become the
/// containing block for every `position: fixed` child (the mobile bottom bar,
/// sheets, modals) and reposition them — this is called out explicitly in
/// lib/keyboard-inset.ts. `opacity < 1` creates a stacking context but does
/// NOT establish a fixed-positioning containing block, so the shell and its
/// fixed children never move. The flip (0.9999 → 1) is imperceptible.
///
/// No-op off iOS (Android's Chromium WebView and the desktop full-window
/// WebView don't have this stall) and outside a DOM. NOTE: this cannot be
/// reproduced in jsdom or the browser dev server — verify on an iOS device.
export function nudgeFirstPaint(): void {
  if (typeof document === "undefined" || !isIOS()) return;
  const root = document.getElementById("root");
  if (!root) return;

  let runs = 0;
  const MAX_RUNS = 4; // bracket the cold start; stop once the frame is live
  const GAP_MS = 120;

  const flip = () => {
    // Force a synchronous layout read, then dirty a compositor property so
    // WebKit produces a frame this tick even if nothing else changed.
    void root.offsetHeight;
    root.style.opacity = "0.9999";
    requestAnimationFrame(() => {
      // Restore on the next frame — the paint with the dirtied value has
      // already happened at the frame boundary. Clearing the inline style
      // returns #root to its stylesheet default (opaque); we must never
      // leave it dimmed.
      root.style.opacity = "";
      if (++runs < MAX_RUNS) setTimeout(flip, GAP_MS);
    });
  };

  requestAnimationFrame(flip);
}
