/**
 * Tests for `src/lib/force-first-paint.ts` — the iOS-only WKWebView
 * "frozen until touched on cold launch" repaint nudge.
 *
 * The behavior itself (WebKit not compositing the first frame) can't be
 * reproduced in happy-dom, so these lock the two things that CAN regress:
 *   1. it's iOS-gated — a no-op on Android / desktop, where the stall
 *      doesn't exist and a stray opacity write would be pure waste;
 *   2. it ALWAYS restores #root's opacity — a broken restore would leave
 *      the whole app permanently dimmed.
 *
 * `isIOS()` is a module-load constant derived from the UA, so we set the
 * UA, then `vi.resetModules()` + import fresh (same pattern as
 * lib-platform.test.ts). rAF is hand-driven; only `setTimeout` is faked so
 * our manual rAF queue isn't clobbered by fake timers.
 */

import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";

const IPHONE_UA =
  "Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X) AppleWebKit/605.1.15 Safari/604.1";
const WINDOWS_UA =
  "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 Chrome/120.0.0.0 Safari/537.36";

function setNav(ua: string, maxTouchPoints = 0): void {
  Object.defineProperty(navigator, "userAgent", { value: ua, configurable: true });
  Object.defineProperty(navigator, "maxTouchPoints", {
    value: maxTouchPoints,
    configurable: true,
  });
}

async function freshNudge(): Promise<() => void> {
  vi.resetModules();
  return (await import("../../src/lib/force-first-paint")).nudgeFirstPaint;
}

describe("force-first-paint nudge", () => {
  let rafQueue: FrameRequestCallback[];
  let rafSpy: ReturnType<typeof vi.fn>;

  beforeEach(() => {
    document.body.innerHTML = '<div id="root"></div>';
    rafQueue = [];
    rafSpy = vi.fn((cb: FrameRequestCallback) => rafQueue.push(cb));
    // Only fake setTimeout so our manual rAF queue survives.
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] });
    vi.stubGlobal("requestAnimationFrame", rafSpy);
  });

  afterEach(() => {
    vi.unstubAllGlobals();
    vi.useRealTimers();
    vi.resetModules();
    document.body.innerHTML = "";
  });

  /** Run every callback currently queued for the next animation frame. */
  function runFrame(): void {
    const batch = rafQueue;
    rafQueue = [];
    batch.forEach((cb) => cb(0));
  }

  /** Drain all remaining frames + their inter-run setTimeout gaps. */
  function drain(): void {
    for (let i = 0; i < 50; i++) {
      runFrame();
      vi.advanceTimersByTime(200);
      if (rafQueue.length === 0 && vi.getTimerCount() === 0) break;
    }
    runFrame();
  }

  it("on iOS: dims #root then restores it, and brackets the cold start", async () => {
    setNav(IPHONE_UA, 5);
    const nudge = await freshNudge();
    const root = document.getElementById("root")!;

    nudge();
    expect(rafSpy).toHaveBeenCalledTimes(1); // first repaint frame scheduled

    runFrame(); // first flip runs
    expect(root.style.opacity).toBe("0.9999"); // layer dirtied → forces a paint

    drain();
    // Restored to the stylesheet default — the app is NEVER left dimmed.
    expect(root.style.opacity).toBe("");
    // 1 initial frame + one restore frame per flip = 1 + MAX_RUNS(4).
    // Locks the cadence so a future tweak is a conscious change.
    expect(rafSpy).toHaveBeenCalledTimes(5);
  });

  it("off iOS (desktop/Android): does nothing at all", async () => {
    setNav(WINDOWS_UA, 0);
    const nudge = await freshNudge();
    const root = document.getElementById("root")!;

    nudge();
    drain();

    expect(rafSpy).not.toHaveBeenCalled();
    expect(root.style.opacity).toBe(""); // untouched
  });

  it("on iOS with no #root present: no-op, never throws", async () => {
    setNav(IPHONE_UA, 5);
    document.body.innerHTML = ""; // tear the root out
    const nudge = await freshNudge();

    expect(() => nudge()).not.toThrow();
    expect(rafSpy).not.toHaveBeenCalled();
  });
});
