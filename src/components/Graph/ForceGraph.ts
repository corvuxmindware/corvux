/**
 * D3.js force-directed graph engine — Canvas2D renderer.
 *
 * Replaces the SVG DOM renderer to eliminate the 2K-node ceiling.
 * SVG creates one DOM element per node/edge/label, and every simulation
 * tick mutates 3000+ DOM attributes → layout recalculation stalls the
 * browser. Canvas2D draws the entire graph in a single draw call per
 * frame, scaling to 5K+ nodes without jank.
 *
 * Key changes from the SVG version:
 * - Nodes, edges, labels are drawn on a `<canvas>` instead of SVG elements
 * - Hit detection uses coordinate math (point-in-circle) instead of DOM events
 * - Label collision is the same AABB algorithm, applied in screen space
 * - Level-of-detail: at low zoom, skip labels and reduce edge rendering
 * - The D3 force simulation is unchanged — only the rendering layer moved
 */

import * as d3 from "d3";
import {
  type GraphNode,
  type GraphEdge,
  type GraphTheme,
  nodeColor,
  nodeRadius,
  labelPriority,
  labelOpacityFor,
  computeFitTransform as computeFitTransformPure,
} from "./graph-style";
import { type GraphSettings, GRAPH_SETTINGS_DEFAULTS } from "../../stores/graph-settings";

export type { GraphNode, GraphEdge } from "./graph-style";

function readGraphTheme(): GraphTheme {
  const cs = getComputedStyle(document.documentElement);
  const v = (name: string, fallback: string) => cs.getPropertyValue(name).trim() || fallback;
  return {
    note: v("--color-graph-note", "#7f849c"),
    hub: v("--color-graph-hub", "#89b4fa"),
    tag: v("--color-graph-tag", "#a6e3a1"),
    label: v("--color-graph-label", "#a6adc8"),
    link: v("--color-graph-link", "rgba(88, 91, 112, 0.5)"),
    linkDim: v("--color-graph-link-dim", "rgba(88, 91, 112, 0.1)"),
    linkFocus: v("--color-graph-link-focus", "rgba(205, 214, 244, 0.7)"),
    strokeFocus: v("--color-graph-stroke-focus", "#cdd6f4"),
    accent: v("--color-accent", "#cba6f7"),
    bg: v("--color-bg-primary", "#1e1e2e"),
  };
}

export interface ForceGraphHandle {
  destroy: () => void;
  highlight: (nodeId: string | null) => void;
  resetView: () => void;
  highlightPath: (nodeIds: string[] | null) => void;
  /** Snapshot of current node positions, keyed by node id (for layout continuity). */
  getPositions: () => Map<string, { x: number; y: number }>;
  /** Current zoom/pan transform as plain {x,y,k} (for layout continuity). */
  getTransform: () => { x: number; y: number; k: number };
  /** Apply new view settings live (force sliders, arrows, label density, scale). */
  updateSettings: (next: GraphSettings) => void;
}

/** Saved layout to seed a fresh render from (see graphStore.getGraphLayout). */
export interface GraphRestore {
  positions?: Map<string, { x: number; y: number }>;
  transform?: { x: number; y: number; k: number } | null;
}

export function createForceGraph(
  container: HTMLDivElement,
  nodes: GraphNode[],
  edges: GraphEdge[],
  onNodeClick: (node: GraphNode) => void,
  activeNoteId?: string,
  onBackgroundClick?: () => void,
  restore?: GraphRestore,
  initialSettings?: GraphSettings,
  // Desktop hover preview: fired with the node under the cursor (or null
  // on leave) + screen coords. When provided, the renderer's own minimal
  // tooltip is suppressed — the caller owns the richer hover card.
  onHover?: (node: GraphNode | null, x: number, y: number) => void,
): ForceGraphHandle {
  // Re-read on every resize: dragging the window between monitors with
  // different pixel ratios changes devicePixelRatio, and a stale dpr
  // leaves the canvas blurry / mis-scaled until reload.
  let dpr = window.devicePixelRatio || 1;
  let width = container.clientWidth || 800;
  let height = container.clientHeight || 600;
  let theme = readGraphTheme();
  // Live view settings (force sliders, arrows, scale, label density).
  // `showTags` / `showOrphans` are applied upstream by GraphView (they
  // change which nodes exist), so the renderer ignores them here.
  let settings: GraphSettings = initialSettings ?? GRAPH_SETTINGS_DEFAULTS;

  // Effective radius = base radius × user node-size multiplier.
  const effRadius = (d: GraphNode) => nodeRadius(d) * settings.nodeScale;

  container.innerHTML = "";

  const canvas = document.createElement("canvas");
  canvas.style.width = "100%";
  canvas.style.height = "100%";
  canvas.style.display = "block";
  canvas.style.touchAction = "none";
  canvas.style.cursor = "grab";
  canvas.setAttribute("role", "img");
  canvas.setAttribute("aria-label", "Knowledge graph visualization");
  container.appendChild(canvas);

  const ctx = canvas.getContext("2d")!;

  function resizeCanvas(): boolean {
    const nextWidth = container.clientWidth;
    const nextHeight = container.clientHeight;
    if (nextWidth <= 0 || nextHeight <= 0) return false;
    dpr = window.devicePixelRatio || 1;
    width = nextWidth;
    height = nextHeight;
    canvas.width = width * dpr;
    canvas.height = height * dpr;
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    return true;
  }
  if (!resizeCanvas()) {
    width = 800;
    height = 600;
    canvas.width = width * dpr;
    canvas.height = height * dpr;
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
  }

  // Overlay div for tooltip (HTML renders rich text better than canvas).
  // Structure built with DOM nodes — never innerHTML — so a note title
  // containing markup can never escape into executable UI.
  const tooltip = document.createElement("div");
  tooltip.style.cssText = `
    position:absolute;pointer-events:none;background:var(--color-bg-secondary);
    border:1px solid var(--color-border);border-radius:6px;padding:6px 10px;
    font-size:12px;color:var(--color-text-primary);opacity:0;transition:opacity 0.15s;
    z-index:10;white-space:nowrap;
  `;
  const tooltipTitle = document.createElement("strong");
  const tooltipMeta = document.createElement("span");
  tooltipMeta.style.color = "var(--color-text-secondary)";
  tooltip.appendChild(tooltipTitle);
  tooltip.appendChild(document.createElement("br"));
  tooltip.appendChild(tooltipMeta);
  container.appendChild(tooltip);
  // Don't override the container's `position` — GraphView mounts this with
  // `position: absolute` + top/right/bottom/left:0 to fill its flex parent.
  // Setting it to `relative` collapses the container to its content size,
  // which kills the "fill available space" behavior and breaks ResizeObserver
  // reactions when the user resizes the AI panel or sidebar. `position:
  // absolute` is itself a positioning context, so the absolute tooltip and
  // zoomSvg overlays anchor against this container correctly without help.

  // Zoom state (manual tracking since there's no SVG zoom)
  let currentTransform = d3.zoomIdentity;
  let highlightedNode: string | null = null;
  let highlightedPath: string[] | null = null;

  // ── Eased visual transitions (Phase 2) ──────────────────────────
  // `dimAmount` eases 0→1 when a highlight/path engages (non-focus nodes
  // dim) and 1→0 when it clears — so dimming doesn't snap. The highlight
  // target is KEPT alive until dimAmount fades back to ~0 (see the
  // animation loop), which is what lets the fade-OUT still know which
  // nodes were focused. `hoverAmount` eases the hovered node's growth.
  let dimAmount = 0;
  let dimTarget = 0;
  let hoverId: string | null = null;
  let hoverActive = false;
  let hoverAmount = 0;
  const lerp = (a: number, b: number, t: number) => a + (b - a) * t;

  // Neighbor cache for highlight
  const neighborMap = new Map<string, Set<string>>();
  for (const e of edges) {
    const src = typeof e.source === "string" ? e.source : (e.source as GraphNode).id;
    const tgt = typeof e.target === "string" ? e.target : (e.target as GraphNode).id;
    if (!neighborMap.has(src)) neighborMap.set(src, new Set());
    if (!neighborMap.has(tgt)) neighborMap.set(tgt, new Set());
    neighborMap.get(src)!.add(tgt);
    neighborMap.get(src)!.add(src);
    neighborMap.get(tgt)!.add(src);
    neighborMap.get(tgt)!.add(tgt);
  }

  function getNeighbors(nodeId: string): Set<string> {
    return neighborMap.get(nodeId) ?? new Set([nodeId]);
  }

  // Force layout setup (identical to SVG version). Base values are
  // auto-tuned to roost size; the user's slider multipliers ride on top
  // so the defaults stay good and the sliders just nudge feel.
  const area = Math.max(width * height, 1);
  const n = Math.max(nodes.length, 1);
  const idealDistance = Math.sqrt(area / n) * 0.55;
  const baseLinkDistance = Math.max(45, Math.min(160, idealDistance));
  const baseChargeStrength = -baseLinkDistance * (n <= 5 ? 5 : n <= 20 ? 3.5 : 2.5);
  const linkDistance = baseLinkDistance * settings.linkDistance;
  const chargeStrength = baseChargeStrength * settings.repelForce;

  // Seed positions. Prefer a saved position from the previous render
  // (layout continuity — no re-randomize on tab switch / stats reload),
  // then fall back to the radial seed for genuinely new nodes.
  const savedPositions = restore?.positions;
  let restoredCount = 0;
  const seedRadius = Math.min(width, height) * 0.3;
  nodes.forEach((node, i) => {
    if (node.x == null || node.y == null) {
      const saved = savedPositions?.get(node.id);
      if (saved) {
        node.x = saved.x;
        node.y = saved.y;
        restoredCount++;
        return;
      }
      const angle = (i / n) * Math.PI * 2;
      const jitter = (Math.random() - 0.5) * 30;
      node.x = width / 2 + Math.cos(angle) * seedRadius + jitter;
      node.y = height / 2 + Math.sin(angle) * seedRadius + jitter;
    }
  });

  const gravityFor = (d: GraphNode) => (d.linkCount === 0 ? 0.35 : 0.12);
  const simulation = d3
    .forceSimulation<GraphNode, GraphEdge>(nodes)
    .force("link", d3.forceLink<GraphNode, GraphEdge>(edges).id((d) => d.id).distance(linkDistance))
    .force("charge", d3.forceManyBody().strength(chargeStrength))
    .force("center", d3.forceCenter(width / 2, height / 2))
    .force("x", d3.forceX<GraphNode>(width / 2).strength(gravityFor))
    .force("y", d3.forceY<GraphNode>(height / 2).strength(gravityFor))
    .force("collision", d3.forceCollide<GraphNode>().radius((d) => effRadius(d) + 8))
    .on("tick", draw);

  // Layout continuity: if most nodes were seeded from a saved layout (a
  // tab switch / stats reload, not a genuinely new graph), start the sim
  // cool so positions barely shift instead of re-settling from alpha=1.
  // A restored transform is also a strong "we're continuing" signal.
  const mostlyRestored = n > 0 && restoredCount / n >= 0.5;
  if (mostlyRestored || (restore?.transform && restoredCount > 0)) {
    simulation.alpha(0.12);
  }

  // Cached active-note ref — avoids an O(N) `nodes.find()` every ripple
  // frame. Node identity is stable for this render's lifetime.
  const activeNode = activeNoteId ? nodes.find((nd) => nd.id === activeNoteId) : undefined;

  // measureText width cache — keyed by title, measured ONCE at the base
  // label font. Replaces the `length × 6.5` heuristic with real metrics
  // (variable-width fonts make char-count a poor proxy), and the cache
  // keeps it off the per-frame hot path.
  const labelWidthCache = new Map<string, number>();
  function measureLabelWidth(title: string): number {
    const cached = labelWidthCache.get(title);
    if (cached !== undefined) return cached;
    ctx.save();
    ctx.font = "11px Inter, system-ui, sans-serif";
    const w = ctx.measureText(title).width + 8;
    ctx.restore();
    labelWidthCache.set(title, w);
    return w;
  }

  // Spatial index for hit-testing (tooltip, hover, drag-vs-pan gate).
  // Rebuilt lazily + throttled rather than per-frame: a slightly stale
  // tree is fine for "what's under the cursor" and rebuilding every tick
  // would cost as much as the O(N) scan it replaces.
  const maxBaseRadius = nodes.reduce((m, d) => Math.max(m, nodeRadius(d)), 0);
  let quadtree = d3
    .quadtree<GraphNode>()
    .x((d) => d.x ?? 0)
    .y((d) => d.y ?? 0)
    .addAll(nodes);
  let quadDirty = false;
  let lastQuadBuild = 0;
  function ensureQuadtree() {
    if (!quadDirty) return;
    const now = Date.now();
    if (now - lastQuadBuild < 80) return;
    quadtree = d3
      .quadtree<GraphNode>()
      .x((d) => d.x ?? 0)
      .y((d) => d.y ?? 0)
      .addAll(nodes);
    lastQuadBuild = now;
    quadDirty = false;
  }

  // Throttled label-collision cache. computeVisibleLabels (sort + AABB
  // sweep) ran on EVERY draw — every tick, every zoom frame, every ripple
  // frame. Recompute only on a meaningful zoom/pan change, a highlight
  // change, or every ~150ms (covers slow position drift during settle).
  let labelCache = new Map<string, number>();
  let labelCacheTs = 0;
  let labelCacheK = -1;
  let labelCacheX = Number.NaN;
  let labelCacheY = Number.NaN;
  let labelCacheHL: string | null = null;
  function visibleLabelsThrottled(): Map<string, number> {
    const now = Date.now();
    const k = currentTransform.k;
    const x = currentTransform.x;
    const y = currentTransform.y;
    const movedEnough =
      Math.abs(k - labelCacheK) > 1e-3 ||
      Math.abs(x - labelCacheX) > 0.5 ||
      Math.abs(y - labelCacheY) > 0.5;
    if (movedEnough || labelCacheHL !== highlightedNode || now - labelCacheTs > 150) {
      labelCache = computeVisibleLabels();
      labelCacheTs = now;
      labelCacheK = k;
      labelCacheX = x;
      labelCacheY = y;
      labelCacheHL = highlightedNode;
    }
    return labelCache;
  }

  // Returns id → label opacity (0..1). Labels fade in over a zoom band
  // above their per-node floor instead of popping. The floor is divided
  // by the user's label-density multiplier (higher density ⇒ labels
  // appear at lower zoom). Collision placement is unchanged — a label
  // only claims space once it's begun fading in.
  function computeVisibleLabels(): Map<string, number> {
    const shown = new Map<string, number>();
    if (highlightedNode) return shown;
    const k = currentTransform.k;
    const sorted = nodes
      .slice()
      .sort((a, b) => labelPriority(b, activeNoteId) - labelPriority(a, activeNoteId));
    const placed: Array<{ x1: number; y1: number; x2: number; y2: number }> = [];

    // Small graphs (subgraph search, local-view) — show every label
    // regardless of link-count. The link-count thresholds below were tuned
    // for ~1000-node full graphs where most nodes need to fall away or the
    // canvas turns into a wall of text; in a 6-node subgraph that same
    // rule starves all but the focus.
    const showAllLabels = nodes.length <= 50;
    const density = settings.labelDensity || 1;

    for (const d of sorted) {
      let minZoom: number;
      if (showAllLabels) minZoom = 0;
      else if (d.id === activeNoteId) minZoom = 0;
      else if (d.kind === "tag") minZoom = d.linkCount >= 5 ? 0.35 : 0.8;
      else if (d.linkCount >= 15) minZoom = 0.5;
      else if (d.linkCount >= 8) minZoom = 0.9;
      else if (d.linkCount >= 4) minZoom = 1.3;
      else if (d.linkCount >= 2) minZoom = 1.8;
      else minZoom = 2.5;

      // Density shifts the floor: more density → lower effective floor.
      const effMinZoom = minZoom > 0 ? minZoom / density : 0;
      const opacity = labelOpacityFor(k, effMinZoom);
      if (opacity <= 0) continue;

      const sx = (d.x ?? 0) * k + currentTransform.x;
      const sy = ((d.y ?? 0) + effRadius(d) + 14) * k + currentTransform.y;
      const halfW = measureLabelWidth(d.title) / 2;
      const halfH = 8;
      const box = { x1: sx - halfW, y1: sy - halfH, x2: sx + halfW, y2: sy + halfH };

      let collides = false;
      for (const p of placed) {
        if (!(box.x2 < p.x1 || box.x1 > p.x2 || box.y2 < p.y1 || box.y1 > p.y2)) {
          collides = true;
          break;
        }
      }
      if (collides) continue;

      placed.push(box);
      shown.set(d.id, opacity);
    }
    return shown;
  }

  // ── Canvas draw ──────────────────────────────────────────────────
  function draw() {
    ctx.save();
    ctx.clearRect(0, 0, width, height);
    ctx.fillStyle = theme.bg;
    ctx.fillRect(0, 0, width, height);

    const k = currentTransform.k;
    const tx = currentTransform.x;
    const ty = currentTransform.y;

    const isHighlighting = highlightedNode !== null;
    const isPathHighlighting = highlightedPath !== null;
    const visibleLabels = visibleLabelsThrottled();
    // Positions just changed (this draw came from a tick or interaction);
    // mark the hit-test index stale so the next hit-test rebuilds it.
    quadDirty = true;

    // Hoist highlight/path lookups OUT of the per-edge / per-node loops.
    // The old code rebuilt `new Set(highlightedPath)` and ran `indexOf`
    // for every edge AND every node on every frame — thousands of allocs
    // per frame during the path ripple. Build them once per draw.
    const pathSet = isPathHighlighting ? new Set(highlightedPath!) : null;
    const pathIndex = isPathHighlighting
      ? new Map(highlightedPath!.map((id, i) => [id, i] as const))
      : null;
    const neighborSet =
      isHighlighting && highlightedNode ? getNeighbors(highlightedNode) : null;

    // LOD: at very low zoom, skip edges entirely for performance
    const drawEdges = k > 0.1;
    // LOD: at low zoom, use thinner edges
    const edgeWidth = k < 0.5 ? 0.5 : 1.5;
    // Arrows only above a zoom threshold (they're noise when zoomed out)
    // and only when the user enabled them.
    const drawArrows = settings.showArrows && k > 0.7;

    // ── Draw edges ──
    if (drawEdges) {
      for (const e of edges) {
        const src = e.source as GraphNode;
        const tgt = e.target as GraphNode;
        const sx = (src.x ?? 0) * k + tx;
        const sy = (src.y ?? 0) * k + ty;
        const ex = (tgt.x ?? 0) * k + tx;
        const ey = (tgt.y ?? 0) * k + ty;

        // Cull off-screen edges
        if (sx < -50 && ex < -50) continue;
        if (sx > width + 50 && ex > width + 50) continue;
        if (sy < -50 && ey < -50) continue;
        if (sy > height + 50 && ey > height + 50) continue;

        let focusEdge = !pathIndex && !neighborSet; // bright by default
        let color = theme.link;
        let w = edgeWidth;

        if (pathIndex) {
          const i1 = pathIndex.get(src.id);
          const i2 = pathIndex.get(tgt.id);
          focusEdge = i1 !== undefined && i2 !== undefined && Math.abs(i1 - i2) === 1;
          if (focusEdge) {
            color = theme.linkFocus;
            w = 2.5;
          }
        } else if (neighborSet && highlightedNode) {
          focusEdge = src.id === highlightedNode || tgt.id === highlightedNode;
          if (focusEdge) {
            color = theme.linkFocus;
            w = 2.5;
          }
        }

        // Non-focus edges ease toward (and back from) dim with dimAmount,
        // so edges fade in step with the nodes instead of snapping.
        ctx.globalAlpha = focusEdge ? 1 : lerp(1, 0.18, dimAmount);
        ctx.beginPath();
        ctx.moveTo(sx, sy);
        ctx.lineTo(ex, ey);
        ctx.strokeStyle = color;
        ctx.lineWidth = w;
        ctx.stroke();

        // Directed arrowhead near the target, offset by the target radius.
        if (drawArrows) {
          const dxl = ex - sx;
          const dyl = ey - sy;
          const len = Math.hypot(dxl, dyl) || 1;
          const ux = dxl / len;
          const uy = dyl / len;
          const tipDist = effRadius(tgt) * k + 2;
          const tipX = ex - ux * tipDist;
          const tipY = ey - uy * tipDist;
          const ah = Math.max(4, 5 * Math.min(k, 1.5));
          ctx.beginPath();
          ctx.moveTo(tipX, tipY);
          ctx.lineTo(tipX - ux * ah - uy * ah * 0.5, tipY - uy * ah + ux * ah * 0.5);
          ctx.lineTo(tipX - ux * ah + uy * ah * 0.5, tipY - uy * ah - ux * ah * 0.5);
          ctx.closePath();
          ctx.fillStyle = color;
          ctx.fill();
        }
        ctx.globalAlpha = 1;
      }
    }

    // ── Draw nodes ──
    for (const d of nodes) {
      const nx = (d.x ?? 0) * k + tx;
      const ny = (d.y ?? 0) * k + ty;
      // Hovered node grows ~1.3× (eased). effRadius applies the user's
      // node-size multiplier.
      const hoverScale = d.id === hoverId ? lerp(1, 1.3, hoverAmount) : 1;
      const r = effRadius(d) * k * hoverScale;

      // Cull off-screen nodes
      if (nx + r < 0 || nx - r > width || ny + r < 0 || ny - r > height) continue;

      const baseOpacity = d.linkCount === 0 ? 0.5 : 1;
      let relevant = true; // bright when not dimmed by a highlight
      let strokeColor = "transparent";
      let strokeWidth = 0;

      if (pathSet) {
        relevant = pathSet.has(d.id);
        if (relevant) {
          strokeColor = theme.strokeFocus;
          strokeWidth = 3 * k;
        }
      } else if (neighborSet) {
        relevant = neighborSet.has(d.id);
        if (d.id === highlightedNode) {
          strokeColor = theme.strokeFocus;
          strokeWidth = 3 * k;
        } else if (relevant) {
          strokeColor = nodeColor(d, theme);
          strokeWidth = 2 * k;
        }
      } else if (d.id === activeNoteId) {
        strokeColor = theme.accent;
        strokeWidth = 2.5 * k;
      }

      // Non-relevant nodes ease toward the dim floor (and back out when the
      // highlight clears) instead of snapping — dimAmount drives the lerp.
      const opacity = relevant ? baseOpacity : lerp(baseOpacity, 0.12, dimAmount);

      ctx.globalAlpha = opacity;
      ctx.beginPath();
      ctx.arc(nx, ny, r, 0, Math.PI * 2);
      ctx.fillStyle = nodeColor(d, theme);
      ctx.fill();

      if (strokeWidth > 0) {
        ctx.strokeStyle = strokeColor;
        ctx.lineWidth = strokeWidth;
        ctx.stroke();
      }
      ctx.globalAlpha = 1;
    }

    // ── Draw ripple on active note ──
    if (activeNode && !isHighlighting && !isPathHighlighting) {
      const ax = (activeNode.x ?? 0) * k + tx;
      const ay = (activeNode.y ?? 0) * k + ty;
      const baseR = effRadius(activeNode) * k;
      const pulsePhase = (Date.now() % 1800) / 1800;
      const pulseR = baseR * (1 + pulsePhase * 2);
      const pulseOpacity = 0.6 * (1 - pulsePhase);
      ctx.beginPath();
      ctx.arc(ax, ay, pulseR, 0, Math.PI * 2);
      ctx.strokeStyle = theme.accent;
      ctx.lineWidth = 2 * k;
      ctx.globalAlpha = pulseOpacity;
      ctx.stroke();
      ctx.globalAlpha = 1;
    }

    // ── Draw labels ──
    if (k > 0.35) {
      ctx.font = `${11 * Math.min(k, 1.5)}px Inter, system-ui, sans-serif`;
      ctx.textAlign = "center";
      ctx.textBaseline = "top";
      // Halo: stroke the label in the background color before filling so
      // text stays readable where it crosses edges, instead of smearing.
      ctx.lineWidth = 3;
      ctx.strokeStyle = theme.bg;
      ctx.lineJoin = "round";

      for (const d of nodes) {
        // In highlight/path mode, draw ONLY the relevant labels (path
        // nodes, or the focused node + its neighbors). The old code drew
        // EVERY label at 10% opacity for non-neighbors — text soup plus a
        // wasted draw per node on a 1K-node graph. Skip them outright.
        let labelColor = theme.label;
        let labelOpacity = 1;
        if (pathSet) {
          if (!pathSet.has(d.id)) continue;
          labelColor = theme.strokeFocus;
        } else if (neighborSet) {
          if (!neighborSet.has(d.id)) continue;
          labelColor = d.id === highlightedNode ? theme.strokeFocus : theme.label;
        } else {
          // Normal mode: respect the collision/zoom-floor visibility set;
          // the value is the fade-in opacity.
          const o = visibleLabels.get(d.id);
          if (o === undefined) continue;
          labelOpacity = o;
        }

        const nx = (d.x ?? 0) * k + tx;
        const ny = (d.y ?? 0) * k + ty;
        const labelY = ny + effRadius(d) * k + 4 * k;

        ctx.globalAlpha = labelOpacity;
        ctx.strokeText(d.title, nx, labelY);
        ctx.fillStyle = labelColor;
        ctx.fillText(d.title, nx, labelY);
        ctx.globalAlpha = 1;
      }
    }

    ctx.restore();
  }

  // ── Hit detection ────────────────────────────────────────────────
  // Quadtree-backed: O(log N) nearest-candidate lookup instead of an O(N)
  // scan on every mousemove. Search radius bounds the largest possible
  // node (max base radius × node-scale + padding) in graph space; the
  // candidate is then confirmed against its own (scaled) hit radius.
  function findNodeAt(mx: number, ny: number): GraphNode | null {
    const k = currentTransform.k;
    // Transform screen coords to graph coords
    const gx = (mx - currentTransform.x) / k;
    const gy = (ny - currentTransform.y) / k;

    ensureQuadtree();
    const searchR = maxBaseRadius * settings.nodeScale + 6;
    const cand = quadtree.find(gx, gy, searchR);
    if (!cand) return null;
    const dx = (cand.x ?? 0) - gx;
    const dy = (cand.y ?? 0) - gy;
    const r = nodeRadius(cand) * settings.nodeScale + 4; // scaled hit area
    return dx * dx + dy * dy <= r * r ? cand : null;
  }

  // ── Zoom + Pan (d3-zoom on a transparent overlay) ────────────────
  // We use an invisible SVG overlay just for d3.zoom gesture handling,
  // then apply the transform to the canvas draw. This gives us all the
  // smooth zoom/pan/inertia behavior for free.
  const zoomSvg = d3
    .select(container)
    .append("svg")
    .attr("width", "100%")
    .attr("height", "100%")
    .style("position", "absolute")
    .style("top", "0")
    .style("left", "0")
    .style("pointer-events", "all")
    .style("cursor", "grab")
    .style("z-index", "1")
    // iOS WebKit interprets finger drags on this overlay as native
    // scroll/zoom and cancels the pointer stream mid-gesture, which
    // tears down d3-drag's listener chain. `touch-action: none` opts
    // out of native gesture handling so pointer events flow all the
    // way through to d3-drag and d3-zoom. The canvas already sets this
    // for the same reason, but the SVG sits on top and was missing it.
    .style("touch-action", "none");

  const zoomBehavior = d3
    .zoom<SVGSVGElement, unknown>()
    .scaleExtent([0.04, 25])
    // Opt zoom OUT when a gesture starts on a node, so d3-drag can move
    // the node. Without this, d3-zoom's pointerdown handler fires first
    // and calls stopImmediatePropagation, which silently kills the drag
    // listener registered later on the same SVG — nodes become
    // un-draggable as soon as the simulation settles past alphaMin.
    //
    // d3 v7 dispatches pointer events (`pointerdown`) internally on
    // every platform; the legacy `mousedown` / `touchstart` types only
    // appear on older browsers. iOS WebKit reports `pointerdown`, so we
    // MUST include it or the gate never fires on iPhone and zoom eats
    // every tap as a pan.
    .filter((event) => {
      if (event.button) return false;
      if (event.ctrlKey && event.type !== "wheel") return false;
      if (
        event.type === "mousedown" ||
        event.type === "touchstart" ||
        event.type === "pointerdown"
      ) {
        const rect = container.getBoundingClientRect();
        let cx: number, cy: number;
        if (event.type === "touchstart") {
          const t = (event as TouchEvent).touches[0];
          if (!t) return true;
          cx = t.clientX;
          cy = t.clientY;
        } else {
          // pointerdown + mousedown both expose clientX/clientY directly.
          cx = (event as PointerEvent | MouseEvent).clientX;
          cy = (event as PointerEvent | MouseEvent).clientY;
        }
        if (findNodeAt(cx - rect.left, cy - rect.top)) return false;
      }
      return true;
    })
    .wheelDelta((event) => {
      const base = -event.deltaY * (event.deltaMode === 1 ? 0.05 : event.deltaMode ? 1 : 0.005);
      return base * (event.ctrlKey ? 10 : 1);
    })
    .on("start", () => zoomSvg.style("cursor", "grabbing"))
    .on("zoom", (event) => {
      currentTransform = event.transform;
      draw();
    })
    .on("end", () => zoomSvg.style("cursor", "grab"));

  zoomSvg.call(zoomBehavior).on("dblclick.zoom", null);

  // ── Drag (native pointer events) ─────────────────────────────────
  // We don't use d3-drag here. d3-drag v3 binds to touch + mouse events
  // separately, and on iOS WKWebView the touch path is fragile — touch
  // events can be cancelled by the system before d3-drag's `touchmove`
  // listener fires, so node drag silently never starts. PointerEvents
  // unify mouse/touch/pen on every platform we ship to and give us
  // setPointerCapture so the gesture survives the finger leaving the
  // SVG (or temporarily losing focus). Less moving parts, less to break.
  const TAP_THRESHOLD_PX = 6;
  type DragSession = {
    pointerId: number;
    node: GraphNode;
    startClientX: number;
    startClientY: number;
    lastClientX: number;
    lastClientY: number;
    didMove: boolean;
    shiftKey: boolean;
    pointerType: string;
  };
  let dragSession: DragSession | null = null;
  const zoomSvgEl = zoomSvg.node() as SVGSVGElement;

  function onPointerDown(event: PointerEvent) {
    if (event.button !== 0 && event.pointerType !== "touch") return;
    const rect = container.getBoundingClientRect();
    const mx = event.clientX - rect.left;
    const my = event.clientY - rect.top;
    const node = findNodeAt(mx, my);
    if (!node) return; // Empty-space gesture → let d3-zoom pan.

    // We're claiming this gesture for drag. Stop d3-zoom from also
    // grabbing the simulated mouse / touch event, and prevent the
    // browser from interpreting the gesture as a native scroll.
    event.preventDefault();
    event.stopPropagation();

    try {
      zoomSvgEl.setPointerCapture(event.pointerId);
    } catch {
      // setPointerCapture can throw if the element isn't connected.
      // Safe to ignore — without capture we still get pointermove/up
      // events as long as the finger stays over the SVG.
    }

    simulation.alphaTarget(0.3).restart();
    node.fx = node.x;
    node.fy = node.y;

    dragSession = {
      pointerId: event.pointerId,
      node,
      startClientX: event.clientX,
      startClientY: event.clientY,
      lastClientX: event.clientX,
      lastClientY: event.clientY,
      didMove: false,
      shiftKey: event.shiftKey,
      pointerType: event.pointerType,
    };
  }

  function onPointerMove(event: PointerEvent) {
    if (!dragSession || event.pointerId !== dragSession.pointerId) return;
    const session = dragSession;
    const dxClient = event.clientX - session.lastClientX;
    const dyClient = event.clientY - session.lastClientY;
    session.lastClientX = event.clientX;
    session.lastClientY = event.clientY;

    if (!session.didMove) {
      const total = Math.hypot(
        event.clientX - session.startClientX,
        event.clientY - session.startClientY,
      );
      if (total > TAP_THRESHOLD_PX) session.didMove = true;
    }

    const k = currentTransform.k;
    const dx = dxClient / k;
    const dy = dyClient / k;
    session.node.fx = (session.node.fx ?? session.node.x ?? 0) + dx;
    session.node.fy = (session.node.fy ?? session.node.y ?? 0) + dy;
    // Keep the simulation warm so the rest of the graph reacts in
    // real time to the dragged node — same as d3-drag's behaviour.
    simulation.alphaTarget(0.3);
  }

  function onPointerUp(event: PointerEvent) {
    if (!dragSession || event.pointerId !== dragSession.pointerId) return;
    const session = dragSession;
    dragSession = null;

    try {
      zoomSvgEl.releasePointerCapture(event.pointerId);
    } catch {
      // Non-fatal.
    }

    simulation.alphaTarget(0);

    if (!session.shiftKey) {
      session.node.fx = null;
      session.node.fy = null;
    }

    // Tap = pointerdown + pointerup with no significant movement.
    // Route through the same click semantics the desktop click
    // handler uses, regardless of pointer type — works for finger,
    // mouse, and pen.
    if (!session.didMove) {
      const wantsHighlight = session.shiftKey || session.node.kind === "tag";
      if (wantsHighlight) {
        if (highlightedNode === session.node.id) {
          highlight(null);
        } else {
          highlight(session.node.id);
        }
      } else {
        onNodeClick(session.node);
      }
    }
  }

  function onPointerCancel(event: PointerEvent) {
    // OS / browser pulled the gesture from us (e.g. multi-touch
    // upgrade to pinch-zoom). Treat like pointerup but never fire a
    // click — the user didn't complete a deliberate gesture.
    if (!dragSession || event.pointerId !== dragSession.pointerId) return;
    const session = dragSession;
    dragSession = null;
    simulation.alphaTarget(0);
    if (!session.shiftKey) {
      session.node.fx = null;
      session.node.fy = null;
    }
  }

  zoomSvgEl.addEventListener("pointerdown", onPointerDown);
  zoomSvgEl.addEventListener("pointermove", onPointerMove);
  zoomSvgEl.addEventListener("pointerup", onPointerUp);
  zoomSvgEl.addEventListener("pointercancel", onPointerCancel);

  // ── Click + hover ────────────────────────────────────────────────
  // Node clicks (open + highlight) are handled inside the pointer-event
  // drag/tap chain above. The legacy click handler still runs after a
  // background tap (empty-space click that finished d3-zoom's pan),
  // and we keep just the background branch here to clear the
  // highlight. Acting on a node here too would double-fire onNodeClick
  // because pointerup already invoked it.
  zoomSvg.on("click", (event) => {
    const rect = container.getBoundingClientRect();
    const mx = event.clientX - rect.left;
    const my = event.clientY - rect.top;
    const node = findNodeAt(mx, my);
    if (!node) {
      highlight(null);
      onBackgroundClick?.();
    }
  });

  zoomSvg.on("mousemove", (event) => {
    const rect = container.getBoundingClientRect();
    const mx = event.clientX - rect.left;
    const my = event.clientY - rect.top;
    const node = findNodeAt(mx, my);

    // Hover growth applies to note nodes only (tags stay put).
    const hoverableId = node && node.kind === "note" ? node.id : null;
    if (hoverableId !== null) {
      if (hoverableId !== hoverId) {
        hoverId = hoverableId;
        hoverAmount = 0; // fresh grow-in for the newly hovered node
      }
      hoverActive = true;
    } else {
      hoverActive = false; // keep hoverId until it eases back to 0
    }
    ensureLoop();

    // Cursor feedback always; tooltip/hover-card depends on ownership.
    canvas.style.cursor = node ? (node.kind === "tag" ? "default" : "pointer") : "grab";
    if (onHover) {
      // Caller owns the hover UI (rich preview card).
      onHover(node, mx, my);
    } else if (node) {
      tooltipTitle.textContent = node.title;
      tooltipMeta.textContent = `${node.linkCount} connections`;
      tooltip.style.opacity = "1";
      tooltip.style.left = (mx + 12) + "px";
      tooltip.style.top = (my - 10) + "px";
    } else {
      tooltip.style.opacity = "0";
    }
  });

  zoomSvg.on("mouseleave", () => {
    tooltip.style.opacity = "0";
    onHover?.(null, 0, 0);
    hoverActive = false;
    ensureLoop();
  });

  // ── Highlight logic ──────────────────────────────────────────────
  // Engaging a highlight sets dimTarget=1 (non-focus nodes ease toward
  // the dim floor). Clearing sets dimTarget=0 but KEEPS the focus set
  // (highlightedNode / highlightedPath) until the animation loop sees
  // dimAmount fade back to ~0 — that's what lets the fade-OUT still know
  // which nodes were focused.
  function highlight(nodeId: string | null) {
    if (nodeId) {
      highlightedNode = nodeId;
      highlightedPath = null;
      dimTarget = 1;
    } else {
      dimTarget = 0;
    }
    ensureLoop();
    draw();
  }

  function highlightPath(nodeIds: string[] | null) {
    if (nodeIds && nodeIds.length > 0) {
      highlightedPath = nodeIds;
      highlightedNode = null;
      dimTarget = 1;
    } else {
      dimTarget = 0;
    }
    ensureLoop();
    draw();
  }

  // ── Fit transform ────────────────────────────────────────────────
  function computeFitTransform(): d3.ZoomTransform {
    return computeFitTransformPure(
      nodes,
      container.clientWidth || width,
      container.clientHeight || height,
    );
  }

  // ── Theme observer ───────────────────────────────────────────────
  function applyTheme() {
    theme = readGraphTheme();
    draw();
  }

  const themeObserver = new MutationObserver(() => applyTheme());
  themeObserver.observe(document.documentElement, {
    attributes: true,
    attributeFilter: ["data-theme", "class"],
  });

  // ── Resize handler ───────────────────────────────────────────────
  // ResizeObserver fires once immediately on observe(), even when the size
  // hasn't changed. Reheating the simulation to alpha=0.3 on that initial
  // fire kneecaps the layout settle (alpha=1 → 0.3 loses 70% of settling
  // budget), which made the graph appear to "load slowly" — nodes never
  // got to spread out from seed positions. Track the last observed size
  // and skip the work when nothing actually changed.
  let lastObservedW = width;
  let lastObservedH = height;
  let resizeFitTimer: ReturnType<typeof setTimeout> | undefined;
  const observer = new ResizeObserver(() => {
    const resized = resizeCanvas();
    if (!resized) return;
    if (width === lastObservedW && height === lastObservedH) return;
    lastObservedW = width;
    lastObservedH = height;

    const center = simulation.force("center") as d3.ForceCenter<GraphNode>;
    if (center) center.x(width / 2).y(height / 2);
    const fx = simulation.force("x") as d3.ForceX<GraphNode>;
    const fy = simulation.force("y") as d3.ForceY<GraphNode>;
    if (fx) fx.x(width / 2);
    if (fy) fy.y(height / 2);
    // Only reheat if the simulation has already cooled — a fresh sim is
    // already at alpha=1 and resetting to 0.3 only steals momentum.
    if (simulation.alpha() < 0.3) simulation.alpha(0.3).restart();

    // Split-pane changes, especially the AI panel width transition, change the
    // viewport without changing the graph data. Refit after the transition has
    // settled so the canvas does not keep a transform computed for the old pane.
    clearTimeout(resizeFitTimer);
    resizeFitTimer = setTimeout(() => {
      zoomSvg
        .interrupt()
        .call(zoomBehavior.transform, computeFitTransform());
    }, 220);
  });
  observer.observe(container);

  // Unified animation loop. Runs only while there's something to animate
  // — the active-note ripple, an easing dim transition, or a hover
  // grow/shrink — then parks itself. A 60Hz no-op loop is real battery
  // cost on mobile, so `ensureLoop` re-arms it on demand from
  // highlight()/highlightPath()/mousemove and it stops when settled.
  let rafId: number | null = null;
  let lastTs = 0;
  function rippleActive(): boolean {
    return !!activeNode && !highlightedNode && !highlightedPath;
  }
  function transitionsSettling(): boolean {
    if (Math.abs(dimAmount - dimTarget) > 0.005) return true;
    if (hoverActive) return hoverAmount < 0.999;
    return hoverId !== null && hoverAmount > 0.005;
  }
  function needsLoop(): boolean {
    return rippleActive() || transitionsSettling();
  }
  function tick(ts: number) {
    const dt = lastTs ? Math.min(64, ts - lastTs) : 16;
    lastTs = ts;
    // Exponential smoothing, tau≈70ms → ~120ms visual settle.
    const f = 1 - Math.exp(-dt / 70);
    dimAmount += (dimTarget - dimAmount) * f;
    const hoverGoal = hoverActive ? 1 : 0;
    hoverAmount += (hoverGoal - hoverAmount) * f;

    // Highlight fully faded out → drop the retained focus set so labels +
    // collision return to normal.
    if (dimTarget === 0 && dimAmount < 0.01) {
      dimAmount = 0;
      highlightedNode = null;
      highlightedPath = null;
    }
    // Hover fully shrunk back → release the node.
    if (!hoverActive && hoverAmount < 0.01) {
      hoverAmount = 0;
      hoverId = null;
    }

    draw();
    if (needsLoop()) {
      rafId = requestAnimationFrame(tick);
    } else {
      rafId = null;
      lastTs = 0;
    }
  }
  function ensureLoop() {
    if (rafId == null && needsLoop()) {
      lastTs = 0;
      rafId = requestAnimationFrame(tick);
    }
  }
  ensureLoop();

  // Initial transform. If we have a saved transform from the previous
  // render (layout continuity), restore it verbatim so zoom/pan survives
  // the reload; otherwise fit-to-content. Applied THROUGH the zoom
  // behavior (not just the local var) so d3-zoom's internal `__zoom`
  // state matches — without this the next user gesture would jump.
  const initialTransform =
    restore?.transform
      ? d3.zoomIdentity
          .translate(restore.transform.x, restore.transform.y)
          .scale(restore.transform.k)
      : computeFitTransform();
  zoomSvg.call(zoomBehavior.transform, initialTransform);

  // Apply new settings live. Force-affecting changes (spread, repel,
  // node size) re-tune the simulation forces and gently reheat; purely
  // visual changes (arrows, label density) just redraw.
  function applySettings(next: GraphSettings) {
    const prev = settings;
    settings = next;
    const forceChanged =
      next.linkDistance !== prev.linkDistance ||
      next.repelForce !== prev.repelForce ||
      next.nodeScale !== prev.nodeScale;
    if (forceChanged) {
      const link = simulation.force("link") as
        | d3.ForceLink<GraphNode, GraphEdge>
        | undefined;
      link?.distance(baseLinkDistance * next.linkDistance);
      const charge = simulation.force("charge") as d3.ForceManyBody<GraphNode> | undefined;
      charge?.strength(baseChargeStrength * next.repelForce);
      const coll = simulation.force("collision") as d3.ForceCollide<GraphNode> | undefined;
      coll?.radius((d) => effRadius(d) + 8);
      simulation.alpha(Math.max(simulation.alpha(), 0.25)).restart();
    } else {
      draw();
    }
  }

  return {
    destroy() {
      if (rafId != null) cancelAnimationFrame(rafId);
      clearTimeout(resizeFitTimer);
      observer.disconnect();
      themeObserver.disconnect();
      simulation.stop();
      container.innerHTML = "";
    },
    highlight,
    highlightPath,
    updateSettings: applySettings,
    getPositions() {
      const m = new Map<string, { x: number; y: number }>();
      for (const nd of nodes) {
        if (nd.x != null && nd.y != null) m.set(nd.id, { x: nd.x, y: nd.y });
      }
      return m;
    },
    getTransform() {
      return { x: currentTransform.x, y: currentTransform.y, k: currentTransform.k };
    },
    resetView() {
      nodes.forEach((nd) => {
        nd.fx = null;
        nd.fy = null;
      });
      simulation.alpha(0.6).restart();

      zoomSvg
        .transition()
        .duration(400)
        .call(zoomBehavior.transform, computeFitTransform());

      let settleTimer: ReturnType<typeof setTimeout> | undefined;
      const onTick = () => {
        if (simulation.alpha() < 0.05) {
          clearTimeout(settleTimer);
          simulation.on("tick.fit", null);
          zoomSvg
            .transition()
            .duration(500)
            .call(zoomBehavior.transform, computeFitTransform());
        }
      };
      simulation.on("tick.fit", onTick);
      settleTimer = setTimeout(() => {
        simulation.on("tick.fit", null);
        zoomSvg
          .transition()
          .duration(400)
          .call(zoomBehavior.transform, computeFitTransform());
      }, 1500);
    },
  };
}
