/**
 * Pure render-helper tests for the force graph (`components/Graph/graph-style.ts`).
 *
 * These functions drive node color/size, label priority, the fit
 * transform, and id/path canonicalization — all of which were previously
 * buried inside `ForceGraph.ts` with zero coverage. They're pure, so we
 * pin their behavior here directly (no canvas, no DOM, no simulation).
 */
import { describe, it, expect } from "vitest";
import {
  type GraphNode,
  type GraphTheme,
  hueFromString,
  nodeColor,
  nodeRadius,
  labelPriority,
  estimateLabelWidth,
  computeFitTransform,
  noteIdFor,
  prettyPath,
  labelForId,
  COMMUNITY_PALETTE,
  communityColor,
  computeCommunityRanks,
  labelOpacityFor,
  dedupeEdges,
  noteHoverPreview,
} from "../../src/components/Graph/graph-style";

const theme: GraphTheme = {
  note: "#note",
  hub: "#hub",
  tag: "#tag",
  label: "#label",
  link: "#link",
  linkDim: "#linkDim",
  linkFocus: "#linkFocus",
  strokeFocus: "#strokeFocus",
  accent: "#accent",
  bg: "#bg",
};

function note(over: Partial<GraphNode> = {}): GraphNode {
  return {
    id: "folder/my-note",
    title: "My Note",
    path: "/roost/folder/my-note.md",
    kind: "note",
    linkCount: 0,
    ...over,
  };
}

describe("hueFromString", () => {
  it("is deterministic and bounded to [0,360)", () => {
    const h = hueFromString("project");
    expect(h).toBe(hueFromString("project"));
    expect(h).toBeGreaterThanOrEqual(0);
    expect(h).toBeLessThan(360);
  });

  it("differs for different strings (no trivial collisions)", () => {
    expect(hueFromString("alpha")).not.toBe(hueFromString("beta"));
  });
});

describe("nodeColor", () => {
  it("colors tags by hashed hue, case/`#`-insensitive", () => {
    const a = nodeColor(note({ kind: "tag", id: "#Project" }), theme);
    const b = nodeColor(note({ kind: "tag", id: "project" }), theme);
    expect(a).toBe(b);
    expect(a).toMatch(/^hsl\(\d+(\.\d+)?, 55%, 60%\)$/);
  });

  it("colors notes with a community rank from the curated palette", () => {
    expect(nodeColor(note({ communityRank: 0 }), theme)).toBe(COMMUNITY_PALETTE[0]);
    expect(nodeColor(note({ communityRank: 2 }), theme)).toBe(COMMUNITY_PALETTE[2]);
  });

  it("treats missing/negative communityRank as 'no community' → link-count buckets", () => {
    expect(nodeColor(note({ communityRank: -1, linkCount: 6 }), theme)).toBe(theme.hub);
    expect(nodeColor(note({ linkCount: 2 }), theme)).toBe(theme.note);
  });

  it("uses hub color at >=5 links, note color below", () => {
    expect(nodeColor(note({ linkCount: 5 }), theme)).toBe(theme.hub);
    expect(nodeColor(note({ linkCount: 4 }), theme)).toBe(theme.note);
  });
});

describe("nodeRadius", () => {
  it("clamps note radius into [4,10]", () => {
    expect(nodeRadius(note({ linkCount: 0 }))).toBe(4);
    expect(nodeRadius(note({ linkCount: 10_000 }))).toBe(10);
  });

  it("clamps tag radius into [3,7]", () => {
    expect(nodeRadius(note({ kind: "tag", linkCount: 0 }))).toBe(3);
    expect(nodeRadius(note({ kind: "tag", linkCount: 10_000 }))).toBe(7);
  });

  it("prefers pagerank over link-count when present and positive", () => {
    const byRank = nodeRadius(note({ linkCount: 0, pagerank: 0.5 }));
    const byLinks = nodeRadius(note({ linkCount: 0 }));
    expect(byRank).toBeGreaterThan(byLinks);
  });
});

describe("labelPriority", () => {
  it("ranks active note above everything, tags above plain notes", () => {
    const active = labelPriority(note({ id: "x" }), "x");
    const tag = labelPriority(note({ kind: "tag", linkCount: 1 }));
    const plain = labelPriority(note({ linkCount: 50 }));
    expect(active).toBeGreaterThan(tag);
    expect(tag).toBeGreaterThan(plain);
  });
});

describe("estimateLabelWidth", () => {
  it("scales with title length", () => {
    expect(estimateLabelWidth("abcd")).toBeGreaterThan(estimateLabelWidth("ab"));
  });
});

describe("computeFitTransform", () => {
  it("returns identity for an empty graph", () => {
    const t = computeFitTransform([], 800, 600);
    expect(t.k).toBe(1);
    expect(t.x).toBe(0);
    expect(t.y).toBe(0);
  });

  it("centers a single node in the viewport", () => {
    const t = computeFitTransform([note({ x: 100, y: 100 })], 800, 600);
    // The node's graph coords map to the viewport center.
    expect(100 * t.k + t.x).toBeCloseTo(400, 5);
    expect(100 * t.k + t.y).toBeCloseTo(300, 5);
  });

  it("keeps scale within the [0.1,2] clamp", () => {
    const spread: GraphNode[] = [
      note({ id: "a", x: -100000, y: -100000 }),
      note({ id: "b", x: 100000, y: 100000 }),
    ];
    expect(computeFitTransform(spread, 800, 600).k).toBeGreaterThanOrEqual(0.1);
    const tight = computeFitTransform([note({ x: 0, y: 0 })], 800, 600);
    expect(tight.k).toBeLessThanOrEqual(2);
  });
});

describe("noteIdFor", () => {
  it("is roost-relative, forward-slashed, lowercased, no extension", () => {
    expect(noteIdFor("C:\\Roost\\Fit-ME\\Puli-Fitness.md", "C:\\Roost")).toBe(
      "fit-me/puli-fitness",
    );
  });

  it("distinguishes same-named files in different folders", () => {
    const a = noteIdFor("/v/week-1/daily.md", "/v");
    const b = noteIdFor("/v/week-2/daily.md", "/v");
    expect(a).not.toBe(b);
  });

  it("handles a null roost path (passthrough, still normalized)", () => {
    expect(noteIdFor("/abs/Path/Note.md", null)).toBe("abs/path/note");
  });
});

describe("prettyPath", () => {
  it("preserves original case and drops the extension", () => {
    expect(prettyPath("/Roost/Folder/My-Note.md", "/Roost")).toBe("Folder/My-Note");
  });

  it("never lowercases (unlike noteIdFor)", () => {
    const abs = "/V/Mixed-Case/File.md";
    expect(prettyPath(abs, "/V")).not.toBe(noteIdFor(abs, "/V"));
  });
});

describe("labelForId", () => {
  it("returns the last path segment (filename stem), never the full id", () => {
    expect(labelForId("fit-me/2026-w11/puli-fitness")).toBe("puli-fitness");
    expect(labelForId("solo")).toBe("solo");
  });
});

describe("computeCommunityRanks", () => {
  const rows = (pairs: Array<[string, number]>) =>
    pairs.map(([note_id, community]) => ({ note_id, community }));

  it("assigns slot 0 to the largest community, descending by size", () => {
    // community 7 has 3 members, 2 has 2, 9 has 1.
    const ranks = computeCommunityRanks(
      rows([
        ["a", 7], ["b", 7], ["c", 7],
        ["d", 2], ["e", 2],
        ["f", 9],
      ]),
    );
    expect(ranks.get(7)).toBe(0);
    expect(ranks.get(2)).toBe(1);
    expect(ranks.get(9)).toBe(2);
  });

  it("breaks size ties by community id ascending (deterministic)", () => {
    const ranks = computeCommunityRanks(rows([["a", 5], ["b", 3]]));
    expect(ranks.get(3)).toBe(0); // both size 1 → lower id wins slot 0
    expect(ranks.get(5)).toBe(1);
  });

  it("ignores negative (unassigned) communities", () => {
    const ranks = computeCommunityRanks(rows([["a", -1], ["b", 4]]));
    expect(ranks.has(-1)).toBe(false);
    expect(ranks.get(4)).toBe(0);
  });

  it("is stable when community ids churn but sizes are preserved", () => {
    // Same size distribution (3,2,1), different raw ids → same color slots
    // for the size-ordered communities.
    const a = computeCommunityRanks(rows([["1", 1], ["2", 1], ["3", 1], ["4", 2], ["5", 2], ["6", 8]]));
    const b = computeCommunityRanks(rows([["1", 4], ["2", 4], ["3", 4], ["4", 9], ["5", 9], ["6", 0]]));
    // Largest in each gets rank 0.
    expect(a.get(1)).toBe(0);
    expect(b.get(4)).toBe(0);
  });
});

describe("communityColor", () => {
  it("wraps around the palette and is safe for negative ranks", () => {
    expect(communityColor(0)).toBe(COMMUNITY_PALETTE[0]);
    expect(communityColor(COMMUNITY_PALETTE.length)).toBe(COMMUNITY_PALETTE[0]);
    expect(communityColor(-1)).toBe(COMMUNITY_PALETTE[COMMUNITY_PALETTE.length - 1]);
  });
});

describe("dedupeEdges", () => {
  const ids = (...xs: string[]) => new Set(xs);

  it("drops edges whose endpoints aren't in the node set", () => {
    const out = dedupeEdges(
      [
        { source: "a", target: "b" },
        { source: "a", target: "gone" },
      ],
      ids("a", "b"),
    );
    expect(out).toEqual([{ source: "a", target: "b" }]);
  });

  it("dedups exact-duplicate directed edges", () => {
    const out = dedupeEdges(
      [
        { source: "a", target: "b" },
        { source: "a", target: "b" },
      ],
      ids("a", "b"),
    );
    expect(out).toHaveLength(1);
  });

  it("keeps reciprocal A→B / B→A pairs (distinct directed edges)", () => {
    const out = dedupeEdges(
      [
        { source: "a", target: "b" },
        { source: "b", target: "a" },
      ],
      ids("a", "b"),
    );
    expect(out).toHaveLength(2);
  });

  it("disambiguates ids that contain the separator character", () => {
    // "a b" → "c" must NOT collide with "a" → "b c".
    const out = dedupeEdges(
      [
        { source: "a b", target: "c" },
        { source: "a", target: "b c" },
      ],
      ids("a b", "c", "a", "b c"),
    );
    expect(out).toHaveLength(2);
  });
});

describe("noteHoverPreview", () => {
  it("extracts frontmatter array tags (# stripped) and a body excerpt", () => {
    const md = [
      "---",
      "tags: [project, ml]",
      "---",
      "# Heading",
      "",
      "First real line.",
      "Second line.",
    ].join("\n");
    const p = noteHoverPreview(md);
    expect(p.tags).toContain("project");
    expect(p.tags).toContain("ml");
    expect(p.excerpt).toBe("First real line. Second line.");
  });

  it("splits a frontmatter tags string and picks up inline #tags", () => {
    const md = ["---", "tags: alpha beta", "---", "Body with #gamma here."].join("\n");
    const p = noteHoverPreview(md);
    expect(p.tags).toEqual(expect.arrayContaining(["alpha", "beta", "gamma"]));
  });

  it("skips headings, rules, and blanks; strips list/quote markers", () => {
    const md = ["## H", "---", "", "- bullet one", "> quoted two", "plain three", "four"].join("\n");
    const p = noteHoverPreview(md);
    // No frontmatter here, so the leading `## H` / `---` are body lines:
    // `## H` is a heading (skipped), `---` is a rule (skipped).
    expect(p.excerpt).toBe("bullet one quoted two plain three");
  });

  it("handles a note with no frontmatter (empty tags)", () => {
    const p = noteHoverPreview("Just text, no metadata.");
    expect(p.tags).toEqual([]);
    expect(p.excerpt).toBe("Just text, no metadata.");
  });

  it("caps tags at 8", () => {
    const md = ["---", `tags: [${Array.from({ length: 20 }, (_, i) => `t${i}`).join(", ")}]`, "---", "x"].join("\n");
    expect(noteHoverPreview(md).tags.length).toBe(8);
  });
});

describe("labelOpacityFor", () => {
  it("is always-on when minZoom <= 0", () => {
    expect(labelOpacityFor(0.1, 0)).toBe(1);
  });

  it("is 0 at/below the floor and ramps to 1 across the fade band", () => {
    expect(labelOpacityFor(1.0, 1.0)).toBe(0);
    expect(labelOpacityFor(1.0 + 0.225, 1.0, 0.45)).toBeCloseTo(0.5, 5);
    expect(labelOpacityFor(2.0, 1.0, 0.45)).toBe(1); // well above → clamped to 1
  });
});
