/**
 * Pure pinch/pan math for the mermaid in-place zoom (mermaid-render.ts wires
 * it to pointer events). Kept dependency-free and DOM-free so the geometry is
 * unit-testable without simulating touch gestures.
 *
 * Model: the diagram fits the box width at `scale === 1` (the baseline). The
 * transform `translate(tx, ty) scale(s)` is applied to an inner wrapper with
 * `transform-origin: 0 0`, so (tx, ty) are in viewport (box) pixels and the
 * content's natural size equals the box size at scale 1.
 */

export const MIN_SCALE = 1;
export const MAX_SCALE = 6;

export interface Transform {
  scale: number;
  tx: number;
  ty: number;
}

export function clamp(v: number, min: number, max: number): number {
  return v < min ? min : v > max ? max : v;
}

export function distance(
  ax: number,
  ay: number,
  bx: number,
  by: number,
): number {
  const dx = ax - bx;
  const dy = ay - by;
  return Math.hypot(dx, dy);
}

/**
 * Clamp the pan so the scaled content always covers the viewport — you can
 * never drag the diagram off into empty space. At scale 1 the only legal
 * translate is (0, 0). Box dimensions are the viewport (box) size in px.
 */
export function clampPan(
  t: Transform,
  boxW: number,
  boxH: number,
): Transform {
  // content size = box * scale; legal tx ∈ [box*(1-scale), 0].
  const minX = boxW * (1 - t.scale);
  const minY = boxH * (1 - t.scale);
  return {
    scale: t.scale,
    tx: clamp(t.tx, minX, 0),
    ty: clamp(t.ty, minY, 0),
  };
}

/** Gesture anchor captured when a pinch (or wheel-zoom) begins. */
export interface PinchStart {
  /** Transform at gesture start. */
  scale0: number;
  tx0: number;
  ty0: number;
  /** Distance between the two pointers at start (px). 1 for wheel. */
  dist0: number;
  /** Focal point (pinch midpoint / cursor) at start, in box coords. */
  fx0: number;
  fy0: number;
}

/**
 * Given the start anchor and the live pinch state (new pointer distance + new
 * midpoint), produce the next transform. Zooms toward the focal point and lets
 * the midpoint drift act as a two-finger pan. Result is NOT pan-clamped — the
 * caller clamps with box dimensions.
 */
export function computePinch(
  s: PinchStart,
  newDist: number,
  fx: number,
  fy: number,
): Transform {
  const scale = clamp((s.scale0 * newDist) / s.dist0, MIN_SCALE, MAX_SCALE);
  // Content point that sat under the start focal, kept under the live focal.
  const contentX = (s.fx0 - s.tx0) / s.scale0;
  const contentY = (s.fy0 - s.ty0) / s.scale0;
  return {
    scale,
    tx: fx - contentX * scale,
    ty: fy - contentY * scale,
  };
}

/**
 * Discrete zoom step (desktop wheel / +- buttons). `factor` > 1 zooms in.
 * Zooms toward (fx, fy) in box coords. NOT pan-clamped — caller clamps.
 */
export function zoomAt(
  t: Transform,
  factor: number,
  fx: number,
  fy: number,
): Transform {
  const scale = clamp(t.scale * factor, MIN_SCALE, MAX_SCALE);
  const contentX = (fx - t.tx) / t.scale;
  const contentY = (fy - t.ty) / t.scale;
  return {
    scale,
    tx: fx - contentX * scale,
    ty: fy - contentY * scale,
  };
}

export const IDENTITY: Transform = { scale: 1, tx: 0, ty: 0 };

export function isZoomed(t: Transform): boolean {
  return t.scale > MIN_SCALE + 1e-3;
}
