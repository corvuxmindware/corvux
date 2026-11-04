import { createRoot } from "solid-js";
import { createAutoHideOnScroll } from "@/lib/auto-hide-on-scroll";

describe("createAutoHideOnScroll", () => {
  beforeEach(() => vi.useFakeTimers());
  afterEach(() => vi.useRealTimers());

  it("starts visible (not hidden)", () => {
    createRoot((dispose) => {
      const { hidden } = createAutoHideOnScroll(250);
      expect(hidden()).toBe(false);
      dispose();
    });
  });

  it("hides on scroll, then reveals after the idle window elapses", () => {
    createRoot((dispose) => {
      const { hidden, onScroll } = createAutoHideOnScroll(250);
      onScroll();
      expect(hidden()).toBe(true);
      vi.advanceTimersByTime(249);
      expect(hidden()).toBe(true); // still within idle window
      vi.advanceTimersByTime(1);
      expect(hidden()).toBe(false); // idle reached → revealed
      dispose();
    });
  });

  it("debounces continuous scrolling — each event resets the idle timer", () => {
    createRoot((dispose) => {
      const { hidden, onScroll } = createAutoHideOnScroll(250);
      onScroll();
      vi.advanceTimersByTime(200);
      onScroll(); // reset before the first window expires
      vi.advanceTimersByTime(200);
      expect(hidden()).toBe(true); // only 200ms since last scroll → still hidden
      vi.advanceTimersByTime(50);
      expect(hidden()).toBe(false); // now 250ms idle → revealed
      dispose();
    });
  });

  it("clears its pending timer on cleanup (no reveal after dispose)", () => {
    let hiddenAccessor: (() => boolean) | undefined;
    createRoot((dispose) => {
      const { hidden, onScroll } = createAutoHideOnScroll(250);
      hiddenAccessor = hidden;
      onScroll();
      expect(hidden()).toBe(true);
      dispose();
    });
    // Timer was cleared by onCleanup; advancing must not flip the (now
    // disposed) signal back. It stays at its last value.
    vi.advanceTimersByTime(500);
    expect(hiddenAccessor!()).toBe(true);
  });
});
