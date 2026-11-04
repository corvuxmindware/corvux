// Height (px) of the on-screen keyboard currently overlapping the WebView,
// derived from `visualViewport`. 0 when no keyboard is up (and always 0 on
// desktop / where visualViewport is unavailable).
//
// Why this exists: on Android we use `interactive-widget=overlays-content`
// (see index.html) so the keyboard overlays the page WITHOUT resizing the
// layout viewport. That kills the per-keystroke "content bobs up and down"
// reflow loop the old `resizes-content` mode caused — every keyboard-height
// tick re-evaluated `100dvh`, reflowed the whole app, and forced CodeMirror
// to re-measure + re-scroll. The cost of overlaying is that the bottom of the
// page is now hidden behind the keyboard, so consumers read this inset to keep
// the caret / input above the keyboard:
//   - the editor adds it as a CodeMirror scroll margin + bottom scroll room,
//   - the value is also mirrored to the `--corvux-keyboard-inset` CSS var for
//     style-only consumers.
import { createSignal } from "solid-js";
import { isMobile } from "./platform";

const [keyboardInset, setKeyboardInset] = createSignal(0);
const [keyboardOpen, setKeyboardOpen] = createSignal(false);

export { keyboardInset, keyboardOpen };

/// Pure derivation of the keyboard inset from viewport geometry. `innerHeight`
/// is the (un-resized) layout viewport under overlays-content; the visible
/// rectangle is `vvHeight + vvOffsetTop`. The difference is whatever the
/// keyboard covers. Values at or below the 24px jitter floor collapse to 0,
/// and the result is clamped non-negative (some WebViews briefly report a
/// visible rect taller than the layout during animation).
export function computeKeyboardInset(
  innerHeight: number,
  vvHeight: number,
  vvOffsetTop: number,
): number {
  const overlap = innerHeight - (vvHeight + vvOffsetTop);
  return overlap > 24 ? Math.round(overlap) : 0;
}

/// Minimum layout-viewport shrink (px) that counts as "a keyboard opened"
/// rather than browser chrome appearing/disappearing. Android URL bars and
/// gesture hints move ~60-120px; no soft keyboard is that short.
const RESIZE_KEYBOARD_THRESHOLD = 150;

/**
 * Is a soft keyboard open at all? This is NOT the same question as
 * `keyboardInset`, and conflating the two was a real bug.
 *
 * `keyboardInset` answers "how many px of the LAYOUT viewport does the
 * keyboard cover" — the number you need for padding, scroll margins, and
 * placing `position: fixed` chrome. Under `interactive-widget=overlays-content`
 * that is the keyboard height. But not every WebView honours that hint: on a
 * OnePlus 7 / Android 12 the view is RESIZED for the keyboard instead, so the
 * layout viewport already excludes it and the correct inset is 0.
 *
 * Anything that asked `keyboardInset() > 0` to mean "is the user typing" was
 * therefore permanently false on those devices. That is why MobileBottomBar
 * never got out of the way while typing, and why MobileFormatBar (which gated
 * its whole existence on it) never appeared at all.
 *
 * So: detect the resize case by comparing the current layout viewport against
 * the tallest one seen in this orientation. Feed THIS to visibility decisions,
 * and keep feeding `keyboardInset` to geometry — adding a resize-mode height
 * to a scroll margin would over-scroll by the height of the keyboard.
 */
export function computeKeyboardOpen(
  innerHeight: number,
  baselineInnerHeight: number,
  overlayInset: number,
): boolean {
  if (overlayInset > 0) return true;
  return baselineInnerHeight - innerHeight > RESIZE_KEYBOARD_THRESHOLD;
}

let installed = false;

/// Idempotently attach the `visualViewport` listeners that drive
/// `keyboardInset`. No-op on desktop or when `visualViewport` is unavailable.
/// Safe to call from multiple call sites; only the first wires listeners.
export function installKeyboardInset(): void {
  if (installed) return;
  if (!isMobile() || typeof window === "undefined" || !window.visualViewport) {
    return;
  }
  installed = true;

  const vv = window.visualViewport;
  let frame = 0;
  // Tallest layout viewport seen in the current orientation — the
  // no-keyboard baseline for `computeKeyboardOpen`. Grows monotonically so a
  // transient shrink can't poison it, and resets on rotation, where the real
  // baseline genuinely changes.
  let baselineInnerHeight = window.innerHeight;

  // iOS WebKit scrolls the document up to reveal a focused caret when the
  // keyboard opens — dragging the pinned top bar off under the notch. The
  // shell is `position: fixed` (see global.css), so nothing *should* scroll,
  // but WebKit still nudges the document/scrollingElement once on first focus
  // (before the fixed layout settles) and it stays scrolled until the user
  // taps elsewhere. The editor keeps the caret visible itself via its scroll
  // margins, so we just slam every document scroll offset back to 0. Cheap,
  // idempotent, and a no-op once nothing is scrolled.
  const resetTopScroll = () => {
    if (window.scrollX !== 0 || window.scrollY !== 0) window.scrollTo(0, 0);
    const se = document.scrollingElement as HTMLElement | null;
    if (se && se.scrollTop !== 0) se.scrollTop = 0;
    if (document.body.scrollTop !== 0) document.body.scrollTop = 0;
  };

  const sync = () => {
    const next = computeKeyboardInset(
      window.innerHeight,
      vv.height,
      vv.offsetTop,
    );
    setKeyboardInset(next);
    // Baseline only grows: while the keyboard is up (resize mode) innerHeight
    // is small, and letting that lower the baseline would make the keyboard
    // look closed a frame later.
    baselineInnerHeight = Math.max(baselineInnerHeight, window.innerHeight);
    setKeyboardOpen(
      computeKeyboardOpen(window.innerHeight, baselineInnerHeight, next),
    );
    document.documentElement.style.setProperty(
      "--corvux-keyboard-inset",
      `${next}px`,
    );
    // iOS lets you drag-PAN the visual viewport while the keyboard is up,
    // because `100dvh`/`position: fixed` stay full-screen height (taller than
    // the area above the keyboard) so there's hidden overflow to pan to — and
    // panning slides the top bar out of view. Fix the ROOT CAUSE: size the
    // shell to the visible height (vv.height), so there's no overflow and
    // nothing to pan. We deliberately do NOT counter-translate the shell with
    // a transform — `transform` on body/#root would become the containing
    // block for every `position: fixed` child (MobileBottomBar, modals,
    // sheets) and reposition them. vv.height is stable per keyboard state, so
    // this only reflows on open/close, not per keystroke.
    document.documentElement.style.setProperty(
      "--corvux-app-height",
      `${Math.round(vv.height)}px`,
    );
    // Kept for any style-only consumer that wants the raw pan offset; with the
    // shell sized to the visible area this stays ~0 (no overflow to pan).
    document.documentElement.style.setProperty(
      "--corvux-vv-offset-top",
      `${Math.round(vv.offsetTop)}px`,
    );
    resetTopScroll();
  };

  const schedule = () => {
    cancelAnimationFrame(frame);
    frame = requestAnimationFrame(sync);
  };

  vv.addEventListener("resize", schedule);
  vv.addEventListener("scroll", schedule);
  window.addEventListener("orientationchange", () => {
    // Portrait and landscape have genuinely different baselines. Drop the old
    // one, or after a rotation the shorter viewport reads as a stuck keyboard.
    baselineInnerHeight = window.innerHeight;
    schedule();
  });
  // WebKit's caret-scroll fires slightly AFTER focusin, so a single reset on
  // focus races it. Fire a few resets across the keyboard animation to catch
  // the nudge whenever it lands, then stop.
  document.addEventListener(
    "focusin",
    () => {
      resetTopScroll();
      requestAnimationFrame(resetTopScroll);
      setTimeout(resetTopScroll, 150);
      setTimeout(resetTopScroll, 350);
    },
    true,
  );
  // Belt-and-suspenders: any document scroll while the shell is pinned is
  // illegitimate, so snap it back.
  window.addEventListener("scroll", resetTopScroll, { passive: true });

  // The last gap: while the keyboard is up, iOS lets you finger-DRAG the
  // visual viewport over the full-screen layout viewport — sliding the whole
  // app (top bar included) up. That's a browser gesture, not an element
  // scroll, so CSS sizing and scrollTo() can't stop it. Swallow the gesture:
  // preventDefault touchmove when the drag isn't inside a genuinely
  // scrollable element. Inner scrollers (the editor's `.cm-scroller`, lists,
  // chat) still scroll because we bail before preventing; dragging the top
  // bar / chrome / gutters does nothing. Scoped to keyboard-up so normal
  // browsing is untouched, and pinch (2+ touches) is always allowed.
  const isScrollable = (start: EventTarget | null): boolean => {
    let el = start instanceof Element ? start : null;
    while (el && el !== document.body) {
      const oy = getComputedStyle(el).overflowY;
      if ((oy === "auto" || oy === "scroll") && el.scrollHeight > el.clientHeight) {
        return true;
      }
      el = el.parentElement;
    }
    return false;
  };
  document.addEventListener(
    "touchmove",
    (e) => {
      if (keyboardInset() <= 0) return; // only fights the keyboard-up pan
      if (e.touches.length > 1) return; // never block pinch-zoom
      if (isScrollable(e.target)) return; // let real scrollers scroll
      e.preventDefault();
    },
    { passive: false },
  );
  sync();
}
