/**
 * Geometry tests for the in-place mermaid pinch-zoom (mermaid-zoom.ts).
 *
 * The DOM/pointer wiring (mermaid-zoom-dom.ts) is imperative glue; the
 * invariants that actually keep the diagram usable are pure math:
 *  - scale clamps to [MIN, MAX]
 *  - pan can never expose empty space around the scaled content
 *  - zoom (pinch or wheel) keeps the focal point pinned under the gesture
 */

import { describe, it, expect } from "vitest";
import {
  MIN_SCALE,
  MAX_SCALE,
  clamp,
  clampPan,
  computePinch,
  distance,
  isZoomed,
  zoomAt,
  IDENTITY,
  type PinchStart,
} from "../../src/components/Editor/extensions/mermaid-zoom";

describe("clamp", () => {
  it("bounds within range", () => {
    expect(clamp(5, 0, 10)).toBe(5);
    expect(clamp(-1, 0, 10)).toBe(0);
    expect(clamp(99, 0, 10)).toBe(10);
  });
});

describe("distance", () => {
  it("is the euclidean norm", () => {
    expect(distance(0, 0, 3, 4)).toBe(5);
  });
});

describe("clampPan", () => {
  const BOX = 400;

  it("pins translate to (0,0) at fit scale", () => {
    const out = clampPan({ scale: 1, tx: -50, ty: 80 }, BOX, BOX);
    expect(out.tx).toBe(0);
    expect(out.ty).toBe(0);
  });

  it("never lets content edges pull inside the viewport", () => {
    // At scale 2 the content is 800px; legal tx ∈ [-400, 0].
    const tooFarLeft = clampPan({ scale: 2, tx: -1000, ty: 0 }, BOX, BOX);
    expect(tooFarLeft.tx).toBe(-400);
    const tooFarRight = clampPan({ scale: 2, tx: 50, ty: 0 }, BOX, BOX);
    expect(tooFarRight.tx).toBe(0);
  });

  it("leaves a legal translate untouched", () => {
    const out = clampPan({ scale: 2, tx: -200, ty: -100 }, BOX, BOX);
    expect(out).toEqual({ scale: 2, tx: -200, ty: -100 });
  });
});

describe("computePinch", () => {
  const start: PinchStart = {
    scale0: 1,
    tx0: 0,
    ty0: 0,
    dist0: 100,
    fx0: 200,
    fy0: 150,
  };

  it("scales proportionally to the pinch distance ratio", () => {
    // Fingers spread 2x → scale 2, focal unchanged.
    const out = computePinch(start, 200, 200, 150);
    expect(out.scale).toBeCloseTo(2, 5);
  });

  it("keeps the focal point pinned under the fingers", () => {
    const out = computePinch(start, 200, 200, 150);
    // Content point that was under (200,150) at scale 1 is (200,150);
    // after 2x it must still sit under (200,150): tx = 200 - 200*2 = -200.
    expect(out.tx).toBeCloseTo(-200, 5);
    expect(out.ty).toBeCloseTo(-150, 5);
  });

  it("clamps to MAX_SCALE on an extreme spread", () => {
    const out = computePinch(start, 100000, 200, 150);
    expect(out.scale).toBe(MAX_SCALE);
  });

  it("clamps to MIN_SCALE on a pinch-in below fit", () => {
    const out = computePinch(start, 1, 200, 150);
    expect(out.scale).toBe(MIN_SCALE);
  });

  it("treats midpoint drift as a two-finger pan", () => {
    // Same distance (no zoom) but midpoint moved right 30px.
    const out = computePinch(start, 100, 230, 150);
    expect(out.scale).toBeCloseTo(1, 5);
    expect(out.tx).toBeCloseTo(30, 5);
  });
});

describe("zoomAt", () => {
  it("zooms toward the cursor, keeping that point fixed", () => {
    const out = zoomAt({ scale: 1, tx: 0, ty: 0 }, 2, 100, 100);
    // Content point under cursor stays under cursor: tx = 100 - 100*2.
    expect(out.scale).toBe(2);
    expect(out.tx).toBeCloseTo(-100, 5);
    expect(out.ty).toBeCloseTo(-100, 5);
  });

  it("clamps and cannot zoom out past fit", () => {
    const out = zoomAt(IDENTITY, 0.5, 50, 50);
    expect(out.scale).toBe(MIN_SCALE);
  });
});

describe("isZoomed", () => {
  it("is false at fit, true once scaled", () => {
    expect(isZoomed(IDENTITY)).toBe(false);
    expect(isZoomed({ scale: 1.5, tx: 0, ty: 0 })).toBe(true);
  });
});
