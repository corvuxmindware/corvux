/**
 * Graph store — layout continuity + stats-equality guard.
 *
 * Two Phase-1 behaviors that have nothing to do with IPC:
 *  1. `stats` uses structural equality so a no-op save (content changed,
 *     link/tag counts unchanged) does NOT notify subscribers — that's
 *     what stops GraphView's 5s timer from re-settling the whole layout
 *     on every keystroke-triggered autosave.
 *  2. `saveGraphLayout` / `getGraphLayout` round-trip the position +
 *     transform snapshot that keeps the graph from re-randomizing across
 *     remounts.
 *
 * The equality guard is verified by reference identity: an equal update
 * is suppressed, so the getter keeps handing back the prior object.
 */
import { describe, it, expect } from "vitest";
import type { SaveOutcome } from "../../src/lib/bindings";
import { graphStore } from "../../src/stores/graph";

function outcome(notes: number, links: number, tags: number): SaveOutcome {
  return {
    stats: { notes, links, tags },
    tasks: [],
    backlinks: [],
    fts5: { status: "ok" },
  } as unknown as SaveOutcome;
}

describe("graph store — stats equality guard", () => {
  it("suppresses structurally-equal stats updates (reference unchanged)", () => {
    graphStore.applySaveOutcome(outcome(5, 5, 5));
    const ref1 = graphStore.stats();

    // Identical counts → setter is a no-op, getter returns the SAME object.
    graphStore.applySaveOutcome(outcome(5, 5, 5));
    expect(graphStore.stats()).toBe(ref1);
  });

  it("propagates when any count changes", () => {
    graphStore.applySaveOutcome(outcome(5, 5, 5));
    const ref1 = graphStore.stats();

    graphStore.applySaveOutcome(outcome(6, 5, 5));
    const ref2 = graphStore.stats();
    expect(ref2).not.toBe(ref1);
    expect(ref2.notes).toBe(6);

    graphStore.applySaveOutcome(outcome(6, 5, 7));
    expect(graphStore.stats()).not.toBe(ref2);
  });
});

describe("graph store — revision (CO-3)", () => {
  it("bumps the graph revision even when stat counts are unchanged", () => {
    // A count-neutral edit — retargeting a link (`[[X]]`→`[[Y]]`) or renaming
    // a tag — leaves notes/links/tags identical. The stats-equality guard
    // (above) correctly suppresses the stats signal, but the graph DID change,
    // so the revision must still advance. GraphView's overlay cache + reload
    // timer key on this, so this is what makes a count-neutral edit refresh
    // the PageRank/community overlays.
    graphStore.applySaveOutcome(outcome(5, 5, 5));
    const r1 = graphStore.graphRevision();

    graphStore.applySaveOutcome(outcome(5, 5, 5)); // identical counts
    expect(graphStore.graphRevision()).toBe(r1 + 1);

    graphStore.applySaveOutcome(outcome(5, 5, 5));
    expect(graphStore.graphRevision()).toBe(r1 + 2);
  });

  it("bumps when the backend warm healer reports a graph change", () => {
    const before = graphStore.graphRevision();
    graphStore.backendGraphChanged();
    expect(graphStore.graphRevision()).toBe(before + 1);
  });
});

describe("graph store — layout cache", () => {
  it("round-trips positions and transform", () => {
    const positions = new Map([
      ["a", { x: 1, y: 2 }],
      ["b", { x: -3, y: 4 }],
    ]);
    graphStore.saveGraphLayout(positions, { x: 10, y: 20, k: 1.5 });

    const got = graphStore.getGraphLayout();
    expect(got.positions.get("a")).toEqual({ x: 1, y: 2 });
    expect(got.positions.get("b")).toEqual({ x: -3, y: 4 });
    expect(got.transform).toEqual({ x: 10, y: 20, k: 1.5 });
  });

  it("overwrites the prior snapshot on the next save", () => {
    graphStore.saveGraphLayout(new Map([["x", { x: 0, y: 0 }]]), { x: 0, y: 0, k: 1 });
    graphStore.saveGraphLayout(new Map([["y", { x: 9, y: 9 }]]), { x: 5, y: 5, k: 2 });

    const got = graphStore.getGraphLayout();
    expect(got.positions.has("x")).toBe(false);
    expect(got.positions.get("y")).toEqual({ x: 9, y: 9 });
    expect(got.transform).toEqual({ x: 5, y: 5, k: 2 });
  });
});
