import { describe, it, expect, beforeEach } from "vitest";
import { shouldShowFormatBar } from "../../src/components/MobileFormatBar/visibility";
import {
  formatBarInset,
  setFormatBarInset,
} from "../../src/lib/format-bar-inset";

/** Editable note, keyboard down, not scrolling — the parked state. */
const parked = {
  mobile: true,
  fileTab: true,
  readOnly: false,
  keyboardUp: false,
  chromeHidden: false,
};

describe("shouldShowFormatBar", () => {
  it("shows the buttons before the user types (the whole point)", () => {
    // Regression: the bar used to require keyboardUp, so a non-technical
    // user had to already be typing markdown to discover the checkbox and
    // bullet buttons that exist to spare them typing markdown.
    expect(shouldShowFormatBar(parked)).toBe(true);
  });

  it("shows while typing, docked to the keyboard", () => {
    expect(shouldShowFormatBar({ ...parked, keyboardUp: true })).toBe(true);
  });

  it("hides on desktop — that chrome lives in the tab bar", () => {
    expect(shouldShowFormatBar({ ...parked, mobile: false })).toBe(false);
  });

  it("hides in read mode", () => {
    expect(shouldShowFormatBar({ ...parked, readOnly: true })).toBe(false);
  });

  it("hides on the graph tab", () => {
    expect(shouldShowFormatBar({ ...parked, fileTab: false })).toBe(false);
  });

  it("parks out of the way while the user scrolls", () => {
    expect(shouldShowFormatBar({ ...parked, chromeHidden: true })).toBe(false);
  });

  it("never disappears mid-edit, even if a scroll fires", () => {
    // Typing scrolls the editor. If scroll-hide applied with the keyboard
    // up, the bar would flicker away under the user's thumb.
    expect(
      shouldShowFormatBar({ ...parked, keyboardUp: true, chromeHidden: true }),
    ).toBe(true);
  });

  it("read mode wins over every showing condition", () => {
    expect(
      shouldShowFormatBar({ ...parked, readOnly: true, keyboardUp: true }),
    ).toBe(false);
  });
});

describe("formatBarInset", () => {
  beforeEach(() => setFormatBarInset(0));

  it("publishes the measured height to signal and CSS var together", () => {
    setFormatBarInset(52);
    expect(formatBarInset()).toBe(52);
    expect(
      document.documentElement.style.getPropertyValue(
        "--corvux-format-bar-inset",
      ),
    ).toBe("52px");
  });

  it("rounds sub-pixel measurements", () => {
    setFormatBarInset(52.4);
    expect(formatBarInset()).toBe(52);
  });

  it("clamps negatives to 0 rather than shrinking the editor padding", () => {
    setFormatBarInset(-10);
    expect(formatBarInset()).toBe(0);
  });

  it("resets to 0 when the bar goes away", () => {
    setFormatBarInset(52);
    setFormatBarInset(0);
    expect(formatBarInset()).toBe(0);
    expect(
      document.documentElement.style.getPropertyValue(
        "--corvux-format-bar-inset",
      ),
    ).toBe("0px");
  });
});
