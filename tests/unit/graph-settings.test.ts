/**
 * Graph settings — sanitize / persistence round-trip.
 *
 * `sanitizeSettings` is the load-path guard: it coerces whatever came back
 * from localStorage (or a corrupt/old blob) into a valid GraphSettings,
 * clamping sliders and falling back to defaults for bad types. Testing it
 * pins the persistence contract without depending on effect-flush timing.
 */
import { describe, it, expect } from "vitest";
import {
  GRAPH_SETTINGS_DEFAULTS,
  GRAPH_SLIDER_RANGES,
  sanitizeSettings,
} from "../../src/stores/graph-settings";

describe("sanitizeSettings", () => {
  it("returns a copy of defaults for empty/garbage input", () => {
    expect(sanitizeSettings(null)).toEqual(GRAPH_SETTINGS_DEFAULTS);
    expect(sanitizeSettings("nope")).toEqual(GRAPH_SETTINGS_DEFAULTS);
    expect(sanitizeSettings(42)).toEqual(GRAPH_SETTINGS_DEFAULTS);
    expect(sanitizeSettings({})).toEqual(GRAPH_SETTINGS_DEFAULTS);
  });

  it("round-trips a valid custom object through JSON unchanged", () => {
    const custom = {
      showTags: false,
      showOrphans: false,
      showArrows: true,
      nodeScale: 1.7,
      linkDistance: 0.8,
      repelForce: 2.2,
      labelDensity: 1.4,
    };
    expect(sanitizeSettings(JSON.parse(JSON.stringify(custom)))).toEqual(custom);
  });

  it("clamps out-of-range slider values to their allowed range", () => {
    const out = sanitizeSettings({
      nodeScale: 99,
      linkDistance: -5,
      repelForce: 1000,
      labelDensity: 0,
    });
    expect(out.nodeScale).toBe(GRAPH_SLIDER_RANGES.nodeScale[1]);
    expect(out.linkDistance).toBe(GRAPH_SLIDER_RANGES.linkDistance[0]);
    expect(out.repelForce).toBe(GRAPH_SLIDER_RANGES.repelForce[1]);
    expect(out.labelDensity).toBe(GRAPH_SLIDER_RANGES.labelDensity[0]);
  });

  it("falls back per-field for wrong types and non-finite numbers", () => {
    const out = sanitizeSettings({
      showTags: "yes",
      nodeScale: Number.NaN,
      repelForce: Infinity,
    });
    expect(out.showTags).toBe(GRAPH_SETTINGS_DEFAULTS.showTags);
    expect(out.nodeScale).toBe(GRAPH_SETTINGS_DEFAULTS.nodeScale);
    expect(out.repelForce).toBe(GRAPH_SETTINGS_DEFAULTS.repelForce);
  });
});
