/**
 * Pure render helpers for the force graph — extracted from `ForceGraph.ts`
 * so they can be unit-tested without a canvas, a DOM, or a running D3
 * simulation. Everything here is a referentially-transparent function of
 * its arguments: no `document`, no module state, no side effects. The
 * canvas renderer and `GraphView` both consume these.
 */

import * as d3 from "d3";
import { parseNote } from "../../lib/frontmatter";

/** A graph node as the simulation + renderer see it. */
export interface GraphNode extends d3.SimulationNodeDatum {
  id: string;
  title: string;
  path: string;
  kind: "note" | "tag";
  linkCount: number;
  community?: number;
  /**
   * Stable color slot for this node's community: 0 = largest community,
   * 1 = next, … (see `computeCommunityRanks`). Decoupling color from the
   * raw Louvain id is what keeps cluster colors from shuffling on every
   * recompute — the ids are unstable, the size ordering is much less so.
   */
  communityRank?: number;
  pagerank?: number;
}

/** A directed link between two node ids (or resolved node refs once D3 binds). */
export interface GraphEdge extends d3.SimulationLinkDatum<GraphNode> {
  source: string | GraphNode;
  target: string | GraphNode;
}

/** Theme tokens read once from CSS custom properties (see `readGraphTheme`). */
export interface GraphTheme {
  note: string;
  hub: string;
  tag: string;
  label: string;
  link: string;
  linkDim: string;
  linkFocus: string;
  strokeFocus: string;
  accent: string;
  bg: string;
}

/** FNV-1a → hue in [0,360). Stable per string, used for tag coloring. */
export function hueFromString(s: string): number {
  let h = 2166136261 >>> 0;
  for (let i = 0; i < s.length; i++) {
    h ^= s.charCodeAt(i);
    h = Math.imul(h, 16777619) >>> 0;
  }
  return h % 360;
}

/**
 * Curated community palette — 14 hand-picked, mid-tone hues that stay
 * legible on both the dark (Mocha-ish `#1e1e2e`) and light (Latte-ish
 * `#eff1f5`) backgrounds we ship. Replaces the old golden-angle HSL at
 * 38% saturation, which washed out to gray fog and, because it was keyed
 * to the raw Louvain id, reshuffled on every recompute. Colors are
 * assigned by community SIZE RANK (see `communityColor`) so the largest
 * cluster keeps slot 0 across recomputes.
 */
export const COMMUNITY_PALETTE = [
  "#5b8def", // blue
  "#e0567a", // rose
  "#3f9d6b", // green
  "#e08e3c", // orange
  "#9b6ef3", // violet
  "#d9a441", // gold
  "#3fae9d", // teal
  "#d169b5", // magenta
  "#c0566a", // maroon
  "#4aa3d4", // sky
  "#7d83e0", // indigo
  "#8aae42", // olive
  "#d06a9c", // pink
  "#5aa0a0", // cyan
] as const;

/**
 * Filter edges to those whose endpoints both survive node filtering, and
 * drop exact-duplicate directed edges (a doubled A→B adds a second spring
 * to the link force and distorts layout). Reciprocal A→B / B→A pairs are
 * KEPT — they're distinct directed edges the arrow rendering relies on.
 */
export function dedupeEdges(
  raw: Array<{ source: string; target: string }>,
  nodeIds: Set<string>,
): GraphEdge[] {
  const seen = new Set<string>();
  const out: GraphEdge[] = [];
  for (const e of raw) {
    if (!nodeIds.has(e.source) || !nodeIds.has(e.target)) continue;
    // JSON key is unambiguous even when an id contains the separator —
    // note ids are roost-relative paths and can contain spaces/slashes.
    const key = JSON.stringify([e.source, e.target]);
    if (seen.has(key)) continue;
    seen.add(key);
    out.push({ source: e.source, target: e.target });
  }
  return out;
}

/** Color for a community size-rank, wrapping around the palette. */
export function communityColor(rank: number): string {
  const n = COMMUNITY_PALETTE.length;
  return COMMUNITY_PALETTE[((rank % n) + n) % n];
}

/**
 * Map each Louvain community id → a stable color slot ordered by size
 * (largest community → 0). Ties broken by community id ascending so the
 * mapping is deterministic. This is the "size-rank" stability trick: the
 * raw community ids churn between recomputes, but the relative SIZE of
 * the big clusters rarely does, so colors stay put for the eye.
 */
export function computeCommunityRanks(
  communities: Array<{ note_id: string; community: number }>,
): Map<number, number> {
  const size = new Map<number, number>();
  for (const c of communities) {
    if (c.community < 0) continue;
    size.set(c.community, (size.get(c.community) ?? 0) + 1);
  }
  const sorted = [...size.entries()].sort((a, b) => b[1] - a[1] || a[0] - b[0]);
  const rank = new Map<number, number>();
  sorted.forEach(([cid], i) => rank.set(cid, i));
  return rank;
}

export function nodeColor(node: GraphNode, theme: GraphTheme): string {
  if (node.kind === "tag") {
    const key = node.id.replace(/^#/, "").toLowerCase();
    return `hsl(${hueFromString(key)}, 55%, 60%)`;
  }
  if (typeof node.communityRank === "number" && node.communityRank >= 0) {
    return communityColor(node.communityRank);
  }
  return node.linkCount >= 5 ? theme.hub : theme.note;
}

export function nodeRadius(node: GraphNode): number {
  if (node.kind === "tag") {
    return Math.max(3, Math.min(7, 3 + Math.sqrt(node.linkCount) * 1.1));
  }
  if (typeof node.pagerank === "number" && node.pagerank > 0) {
    return Math.max(4, Math.min(10, 4 + Math.sqrt(node.pagerank * 100) * 2.5));
  }
  return Math.max(4, Math.min(10, 4 + Math.sqrt(node.linkCount) * 1.6));
}

/**
 * Label draw priority: the active note always wins, then tags by
 * connectedness, then notes by link count. Used to order the AABB
 * collision pass so the most important labels claim space first.
 */
export function labelPriority(d: GraphNode, activeNoteId?: string): number {
  if (d.id === activeNoteId) return 1e9;
  if (d.kind === "tag") return 1e6 + d.linkCount;
  return d.linkCount;
}

/** Cheap label-width estimate (avoids a `measureText` per label per frame). */
export function estimateLabelWidth(title: string): number {
  return title.length * 6.5 + 8;
}

/**
 * Label fade-in: returns 0 below `minZoom`, ramping to 1 over `fadeBand`
 * zoom units above it. Replaces the old hard pop-in at the zoom floor so
 * labels ease into view instead of snapping. `minZoom <= 0` ⇒ always-on.
 */
export function labelOpacityFor(k: number, minZoom: number, fadeBand = 0.45): number {
  if (minZoom <= 0) return 1;
  if (k <= minZoom) return 0;
  return Math.min(1, (k - minZoom) / fadeBand);
}

/**
 * Fit transform that frames all nodes within `cw`×`ch` with padding.
 * Pure: depends only on node positions + dimensions, so it's testable
 * without a canvas. Returns a D3 zoom transform (`d3.zoomIdentity` is
 * itself side-effect free).
 */
export function computeFitTransform(
  nodes: GraphNode[],
  cw: number,
  ch: number,
): d3.ZoomTransform {
  if (nodes.length === 0) return d3.zoomIdentity;

  let minX = Infinity, minY = Infinity, maxX = -Infinity, maxY = -Infinity;
  for (const nd of nodes) {
    const x = nd.x ?? cw / 2;
    const y = nd.y ?? ch / 2;
    const r = nodeRadius(nd) + 16;
    if (x - r < minX) minX = x - r;
    if (y - r < minY) minY = y - r;
    if (x + r > maxX) maxX = x + r;
    if (y + r > maxY) maxY = y + r;
  }

  const dx = maxX - minX || 1;
  const dy = maxY - minY || 1;
  const padding = 0.92;
  const scale = Math.max(0.1, Math.min(2, Math.min(cw / dx, ch / dy) * padding));
  const ttx = cw / 2 - ((minX + maxX) / 2) * scale;
  const tty = ch / 2 - ((minY + maxY) / 2) * scale;
  return d3.zoomIdentity.translate(ttx, tty).scale(scale);
}

/**
 * Canonical internal note id — mirrors Rust's `note_id_from_abs_path`:
 * roost-relative, forward-slash, lowercased, no extension. Basename alone
 * would collapse same-named files in different folders into one node, so
 * search/path lookups must use the full relative path.
 *
 * INTERNAL ONLY — never show this to the user (it's lowercased + path-y).
 * Use `prettyPath` for anything user-facing.
 */
export function noteIdFor(absPath: string, roostPath: string | null): string {
  const absNorm = absPath.replace(/\\/g, "/");
  let rel = absNorm;
  if (roostPath) {
    const rootNorm = roostPath.replace(/\\/g, "/");
    if (absNorm.toLowerCase().startsWith(rootNorm.toLowerCase())) {
      rel = absNorm.slice(rootNorm.length);
    }
  }
  return rel.replace(/^[/\\]+/, "").replace(/\.md$/i, "").toLowerCase();
}

/** Display path: roost-relative, ORIGINAL case, no `.md`. User-facing. */
export function prettyPath(absPath: string, roostPath: string | null): string {
  const absNorm = absPath.replace(/\\/g, "/");
  let rel = absNorm;
  if (roostPath) {
    const rootNorm = roostPath.replace(/\\/g, "/");
    if (absNorm.toLowerCase().startsWith(rootNorm.toLowerCase())) {
      rel = absNorm.slice(rootNorm.length);
    }
  }
  return rel.replace(/^[/\\]+/, "").replace(/\.md$/i, "");
}

/**
 * Best-effort human label for an internal node id when no in-memory node
 * carries its title (e.g. a shortest path that routes through nodes
 * outside the current window). Drops the folder path, keeps the filename
 * stem. NEVER renders the full lowercased id to the user.
 */
export function labelForId(id: string): string {
  const seg = id.split("/").pop() ?? id;
  return seg || id;
}

/** Hover-preview payload for a note: its tags + a short body excerpt. */
export interface NotePreview {
  tags: string[];
  excerpt: string;
}

/**
 * Build a hover-card preview from raw note markdown: frontmatter +
 * inline tags, plus the first few meaningful body lines. Pure (parseNote
 * is pure). Inline-tag scanning is capped to the first ~2KB so a huge
 * note doesn't cost more than a hover should.
 */
export function noteHoverPreview(content: string, maxLines = 3): NotePreview {
  const { frontmatter, body } = parseNote(content);
  const tags = new Set<string>();

  const ft = frontmatter?.tags;
  if (Array.isArray(ft)) {
    for (const t of ft) {
      if (typeof t === "string" && t.trim()) tags.add(t.trim().replace(/^#/, ""));
    }
  } else if (typeof ft === "string") {
    for (const t of ft.split(/[,\s]+/)) {
      if (t.trim()) tags.add(t.trim().replace(/^#/, ""));
    }
  }

  // Inline #tags from the start of the body (bounded scan).
  const head = body.slice(0, 2000);
  const inline = head.match(/(?:^|\s)#([A-Za-z0-9_][\w/-]*)/g);
  if (inline) {
    for (const m of inline) tags.add(m.trim().replace(/^#/, ""));
  }

  // Excerpt: first `maxLines` non-empty body lines that aren't headings or
  // horizontal rules; strip leading list/quote markers.
  const lines: string[] = [];
  for (const raw of body.split(/\r?\n/)) {
    const line = raw.trim();
    if (!line) continue;
    if (/^#{1,6}\s/.test(line)) continue;
    if (/^[-*=_]{3,}$/.test(line)) continue;
    lines.push(line.replace(/^[>\-*+]\s+/, ""));
    if (lines.length >= maxLines) break;
  }

  return { tags: [...tags].slice(0, 8), excerpt: lines.join(" ") };
}
