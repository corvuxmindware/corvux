import { createSignal, createRoot, createEffect } from "solid-js";
import { lsGet, lsSet } from "../lib/storage";

/**
 * Graph view settings — the "control" half of Phase 2. Persisted to
 * localStorage so the user's force/visibility tuning survives a restart
 * (persist-view-state rule). All values are plain primitives; the renderer
 * reads them as a snapshot and `GraphView` re-applies on change.
 *
 * Sliders are normalized "feel" multipliers (1 = default), NOT raw force
 * units — the renderer maps them onto its auto-tuned base values so the
 * defaults stay good across roost sizes.
 */
export interface GraphSettings {
  showTags: boolean;
  showOrphans: boolean;
  showArrows: boolean;
  /** Node radius multiplier. */
  nodeScale: number;
  /** Link-distance multiplier (spread). */
  linkDistance: number;
  /** Repel-force multiplier (charge). */
  repelForce: number;
  /** Label density multiplier — higher shows more labels at lower zoom. */
  labelDensity: number;
}

export const GRAPH_SETTINGS_DEFAULTS: GraphSettings = {
  showTags: true,
  showOrphans: true,
  showArrows: false,
  nodeScale: 1,
  linkDistance: 1,
  repelForce: 1,
  labelDensity: 1,
};

// Clamp ranges for the numeric sliders (min, max). Kept here so the UI and
// any validation share one source of truth.
export const GRAPH_SLIDER_RANGES = {
  nodeScale: [0.5, 2] as const,
  linkDistance: [0.5, 2] as const,
  repelForce: [0.5, 2.5] as const,
  labelDensity: [0.5, 2] as const,
};

const STORAGE_KEY = "corvux.graph.settings";

function clamp(n: number, lo: number, hi: number): number {
  return Math.min(hi, Math.max(lo, n));
}

/** Coerce arbitrary parsed JSON into a valid settings object. */
export function sanitizeSettings(raw: unknown): GraphSettings {
  const d = GRAPH_SETTINGS_DEFAULTS;
  if (!raw || typeof raw !== "object") return { ...d };
  const o = raw as Record<string, unknown>;
  const bool = (v: unknown, fb: boolean) => (typeof v === "boolean" ? v : fb);
  const num = (v: unknown, fb: number, lo: number, hi: number) =>
    typeof v === "number" && Number.isFinite(v) ? clamp(v, lo, hi) : fb;
  return {
    showTags: bool(o.showTags, d.showTags),
    showOrphans: bool(o.showOrphans, d.showOrphans),
    showArrows: bool(o.showArrows, d.showArrows),
    nodeScale: num(o.nodeScale, d.nodeScale, ...GRAPH_SLIDER_RANGES.nodeScale),
    linkDistance: num(o.linkDistance, d.linkDistance, ...GRAPH_SLIDER_RANGES.linkDistance),
    repelForce: num(o.repelForce, d.repelForce, ...GRAPH_SLIDER_RANGES.repelForce),
    labelDensity: num(o.labelDensity, d.labelDensity, ...GRAPH_SLIDER_RANGES.labelDensity),
  };
}

function load(): GraphSettings {
  const raw = lsGet(STORAGE_KEY);
  if (!raw) return { ...GRAPH_SETTINGS_DEFAULTS };
  try {
    return sanitizeSettings(JSON.parse(raw));
  } catch {
    return { ...GRAPH_SETTINGS_DEFAULTS };
  }
}

function createGraphSettingsStore() {
  const [settings, setSettings] = createSignal<GraphSettings>(load());

  // Persist on any change.
  createEffect(() => {
    lsSet(STORAGE_KEY, JSON.stringify(settings()));
  });

  function set<K extends keyof GraphSettings>(key: K, value: GraphSettings[K]) {
    setSettings((s) => ({ ...s, [key]: value }));
  }

  function reset() {
    setSettings({ ...GRAPH_SETTINGS_DEFAULTS });
  }

  return { settings, set, reset };
}

export const graphSettingsStore = createRoot(createGraphSettingsStore);
