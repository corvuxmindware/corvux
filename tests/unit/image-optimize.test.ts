import { describe, it, expect } from "vitest";
import {
  shouldSkipOptimization,
  computeTargetDimensions,
  MAX_EDGE_PX,
  MIN_OPTIMIZE_BYTES,
} from "../../src/lib/image-optimize";

describe("shouldSkipOptimization", () => {
  it("skips non-images", () => {
    expect(shouldSkipOptimization({ type: "application/pdf", size: 5_000_000 })).toBe(true);
  });

  it("skips SVG (vector) and GIF (animation)", () => {
    expect(shouldSkipOptimization({ type: "image/svg+xml", size: 5_000_000 })).toBe(true);
    expect(shouldSkipOptimization({ type: "image/gif", size: 5_000_000 })).toBe(true);
  });

  it("skips images already under the size floor", () => {
    expect(shouldSkipOptimization({ type: "image/png", size: MIN_OPTIMIZE_BYTES - 1 })).toBe(true);
  });

  it("optimizes large PNG/JPEG/WebP", () => {
    expect(shouldSkipOptimization({ type: "image/png", size: 5_000_000 })).toBe(false);
    expect(shouldSkipOptimization({ type: "image/jpeg", size: 2_000_000 })).toBe(false);
    expect(shouldSkipOptimization({ type: "image/webp", size: 1_000_000 })).toBe(false);
  });
});

describe("computeTargetDimensions", () => {
  it("leaves images within the cap untouched", () => {
    const r = computeTargetDimensions(1600, 900);
    expect(r.resized).toBe(false);
    expect(r).toMatchObject({ width: 1600, height: 900 });
  });

  it("downscales the longest edge to the cap, preserving aspect ratio", () => {
    const r = computeTargetDimensions(4032, 3024); // 4:3 phone photo
    expect(r.resized).toBe(true);
    expect(Math.max(r.width, r.height)).toBe(MAX_EDGE_PX);
    // aspect ratio preserved within rounding
    expect(r.width / r.height).toBeCloseTo(4032 / 3024, 2);
  });

  it("caps by the taller edge for portrait images", () => {
    const r = computeTargetDimensions(1500, 6000);
    expect(r.resized).toBe(true);
    expect(r.height).toBe(MAX_EDGE_PX);
    expect(r.width).toBe(Math.round(1500 * (MAX_EDGE_PX / 6000)));
  });

  it("never returns zero dimensions and handles a degenerate 0 size", () => {
    expect(computeTargetDimensions(0, 0)).toMatchObject({ resized: false });
    const r = computeTargetDimensions(10000, 1);
    expect(r.width).toBe(MAX_EDGE_PX);
    expect(r.height).toBeGreaterThanOrEqual(1);
  });
});
