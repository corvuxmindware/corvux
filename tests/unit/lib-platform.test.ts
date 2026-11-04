/**
 * Tests for `src/lib/platform.ts` — the iPad-aware platform detection
 * that drives folder-picker routing (Onboarding, RoostSection,
 * ManageRoostsModal, CreateRoostModal) and the mobile-vs-desktop layout
 * switch in ~30 component sites.
 *
 * Platform is evaluated at module load time (compile-time constants
 * derived from `navigator.userAgent` + `navigator.maxTouchPoints`), so
 * we use `vi.resetModules` + import-fresh to verify each branch instead
 * of mutating a cached snapshot. Critical case: iPadOS 13+ WKWebView
 * reports `Macintosh` in the UA, distinguished only by maxTouchPoints.
 */

import { describe, it, expect, beforeEach, vi, afterEach } from "vitest";

interface Plat {
  isMobile: () => boolean;
  isIOS: () => boolean;
  isAndroid: () => boolean;
  isMacOS: () => boolean;
}

function setNav(ua: string, maxTouchPoints = 0): void {
  Object.defineProperty(navigator, "userAgent", {
    value: ua,
    configurable: true,
  });
  Object.defineProperty(navigator, "maxTouchPoints", {
    value: maxTouchPoints,
    configurable: true,
  });
}

async function freshPlatform(): Promise<Plat> {
  vi.resetModules();
  return (await import("../../src/lib/platform")) as Plat;
}

describe("lib/platform detection", () => {
  beforeEach(() => {
    vi.resetModules();
  });
  afterEach(() => {
    vi.resetModules();
  });

  it("iPhone UA → isIOS true, isMobile true, isMacOS false", async () => {
    setNav(
      "Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X) AppleWebKit/605.1.15 Safari/604.1",
      5,
    );
    const p = await freshPlatform();
    expect(p.isIOS()).toBe(true);
    expect(p.isMobile()).toBe(true);
    expect(p.isMacOS()).toBe(false);
    expect(p.isAndroid()).toBe(false);
  });

  it("iPad-masquerading-as-Mac (iPadOS 13+ default WKWebView UA) → isIOS true", async () => {
    // The bug that prompted the maxTouchPoints heuristic: iPad's default
    // WKWebView UA contains "Macintosh", not "iPad". maxTouchPoints>1 is
    // the cross-browser signal that distinguishes iPad from real Mac.
    setNav(
      "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.0 Safari/605.1.15",
      5,
    );
    const p = await freshPlatform();
    expect(p.isIOS()).toBe(true);
    expect(p.isMobile()).toBe(true);
    expect(p.isMacOS()).toBe(false); // critical: must NOT be macOS
  });

  it("real macOS → isMacOS true, isIOS false, isMobile false", async () => {
    setNav(
      "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.0 Safari/605.1.15",
      0,
    );
    const p = await freshPlatform();
    expect(p.isMacOS()).toBe(true);
    expect(p.isIOS()).toBe(false);
    expect(p.isMobile()).toBe(false);
  });

  it("Android UA → isAndroid true, isMobile true, isIOS false", async () => {
    setNav(
      "Mozilla/5.0 (Linux; Android 14; Pixel 7) AppleWebKit/537.36 Chrome/120.0.0.0 Mobile Safari/537.36",
      5,
    );
    const p = await freshPlatform();
    expect(p.isAndroid()).toBe(true);
    expect(p.isMobile()).toBe(true);
    expect(p.isIOS()).toBe(false);
    expect(p.isMacOS()).toBe(false);
  });

  it("Windows desktop UA → all mobile/Mac/iOS false", async () => {
    setNav(
      "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 Chrome/120.0.0.0 Safari/537.36",
      0,
    );
    const p = await freshPlatform();
    expect(p.isMobile()).toBe(false);
    expect(p.isIOS()).toBe(false);
    expect(p.isAndroid()).toBe(false);
    expect(p.isMacOS()).toBe(false);
  });

  it("Mac with touch input device (maxTouchPoints>1 but no Macintosh UA quirk) → still macOS, NOT iPad", async () => {
    // Edge case sanity check: maxTouchPoints alone shouldn't promote a
    // non-Macintosh UA to iOS. We require BOTH /Macintosh/ AND touch.
    setNav(
      "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36",
      5,
    );
    const p = await freshPlatform();
    expect(p.isIOS()).toBe(false);
    expect(p.isMobile()).toBe(false);
  });
});
