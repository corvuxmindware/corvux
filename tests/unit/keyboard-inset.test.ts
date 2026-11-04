/**
 * Tests for `computeKeyboardInset` in `src/lib/keyboard-inset.ts` — the pure
 * viewport math behind the Android "text bobs while typing" fix.
 *
 * Under `interactive-widget=overlays-content` the layout viewport
 * (`window.innerHeight`) stays fixed while `visualViewport` shrinks to the
 * rectangle the keyboard leaves visible. The inset is the difference, with a
 * 24px floor to swallow address-bar / inset jitter and a non-negative clamp
 * for the frames where the WebView briefly reports a visible rect taller than
 * the layout during the open/close animation.
 */

import { describe, it, expect } from "vitest";
import {
  computeKeyboardInset,
  computeKeyboardOpen,
} from "../../src/lib/keyboard-inset";

describe("computeKeyboardInset", () => {
  it("returns the covered height when the keyboard overlays the page", () => {
    // 800px layout, keyboard leaves 480px visible at offsetTop 0.
    expect(computeKeyboardInset(800, 480, 0)).toBe(320);
  });

  it("returns 0 when no keyboard is up (visible rect fills the layout)", () => {
    expect(computeKeyboardInset(800, 800, 0)).toBe(0);
  });

  it("collapses sub-floor jitter (<= 24px) to 0", () => {
    expect(computeKeyboardInset(800, 780, 0)).toBe(0); // 20px overlap
    expect(computeKeyboardInset(800, 776, 0)).toBe(0); // exactly 24px
    expect(computeKeyboardInset(800, 775, 0)).toBe(25); // just over the floor
  });

  it("accounts for a non-zero visualViewport offsetTop", () => {
    // Pinch-zoom / scroll pushes the visible rect down; the keyboard still
    // covers (800 - (440 + 40)) = 320px.
    expect(computeKeyboardInset(800, 440, 40)).toBe(320);
  });

  it("never returns a negative inset", () => {
    // Visible rect briefly taller than the layout during animation.
    expect(computeKeyboardInset(800, 820, 0)).toBe(0);
  });

  it("rounds fractional device-pixel geometry", () => {
    expect(computeKeyboardInset(800, 479.6, 0)).toBe(320);
  });
});

/**
 * `computeKeyboardOpen` answers a DIFFERENT question from `computeKeyboardInset`:
 * "is the user typing", not "how much of the layout viewport is covered".
 *
 * Conflating them was a real, device-confirmed bug. On a OnePlus 7 / Android 12
 * the WebView is RESIZED for the keyboard rather than overlaying it, so the
 * inset is legitimately 0 the whole time. Everything that tested
 * `keyboardInset() > 0` to mean "typing" was permanently false there:
 * MobileBottomBar never got out of the way, and MobileFormatBar — which gated
 * its entire existence on it — never appeared at all.
 */
describe("computeKeyboardOpen", () => {
  const BASELINE = 800;

  it("is true whenever the keyboard overlays the viewport", () => {
    // Overlay mode: inset carries the signal, innerHeight never moves.
    expect(computeKeyboardOpen(BASELINE, BASELINE, 320)).toBe(true);
  });

  it("is true when the WebView RESIZES instead of overlaying", () => {
    // The device case: inset is 0, but the layout viewport lost the keyboard.
    expect(computeKeyboardOpen(480, BASELINE, 0)).toBe(true);
  });

  it("is false with no keyboard and a steady viewport", () => {
    expect(computeKeyboardOpen(BASELINE, BASELINE, 0)).toBe(false);
  });

  it("ignores browser-chrome sized shrink", () => {
    // Android URL bars / gesture hints move ~60-120px. No keyboard is that
    // short, and treating this as typing would hide the nav bar on a scroll.
    expect(computeKeyboardOpen(BASELINE - 120, BASELINE, 0)).toBe(false);
  });

  it("triggers just past the threshold, not just below it", () => {
    expect(computeKeyboardOpen(BASELINE - 150, BASELINE, 0)).toBe(false);
    expect(computeKeyboardOpen(BASELINE - 151, BASELINE, 0)).toBe(true);
  });

  it("does not report a stuck keyboard when the viewport GREW", () => {
    // Rotating to a taller viewport before the baseline resets.
    expect(computeKeyboardOpen(1000, BASELINE, 0)).toBe(false);
  });
});
