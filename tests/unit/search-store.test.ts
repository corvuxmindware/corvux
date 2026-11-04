/**
 * Search store — verifies FTS5 search plumbing without firing the actual
 * Tauri command. Covers: empty-query short-circuit, totalMatches, clear.
 * (Search is full-text only now — the hybrid/semantic mode toggle was
 * removed when embeddings were torn out.)
 */

import { describe, it, expect, beforeEach } from "vitest";
import { mockInvoke } from "./setup";
import { searchStore } from "../../src/stores/search";

describe("search store — FTS", () => {
  beforeEach(() => {
    searchStore.clear();
  });

  it("empty query produces empty results immediately", async () => {
    let calls = 0;
    mockInvoke.set("search_roost", () => {
      calls++;
      return [];
    });
    await searchStore.search("/roost", "");
    expect(searchStore.results()).toEqual([]);
    expect(searchStore.isSearching()).toBe(false);
    expect(calls).toBe(0);
  });

  it("totalMatches sums across all result rows", async () => {
    mockInvoke.set("search_roost", () => [
      {
        path: "/roost/a.md",
        name: "a.md",
        snippet: "...x...",
        matches: [
          { line: 1, text: "x", col: 0 },
          { line: 5, text: "x", col: 2 },
        ],
        score: 0.8,
        match_type: "fts",
      },
      {
        path: "/roost/b.md",
        name: "b.md",
        snippet: "...x...",
        matches: [{ line: 3, text: "x", col: 0 }],
        score: 0.5,
        match_type: "fts",
      },
    ]);
    mockInvoke.set("init_search", () => 0);
    mockInvoke.set("init_graph", () => ({ notes: 0, links: 0, tags: 0 }));
    await searchStore.search("/roost", "x");
    expect(searchStore.totalMatches()).toBe(3);
  });

  it("clear() resets query, results, and isSearching", async () => {
    mockInvoke.set("search_roost", () => [
      {
        path: "/roost/a.md",
        name: "a.md",
        snippet: "x",
        matches: [],
        score: 0.5,
        match_type: "fts" as const,
      },
    ]);
    mockInvoke.set("init_search", () => 0);
    mockInvoke.set("init_graph", () => ({ notes: 0, links: 0, tags: 0 }));
    await searchStore.search("/roost", "anything");
    searchStore.clear();
    expect(searchStore.query()).toBe("");
    expect(searchStore.results()).toEqual([]);
    expect(searchStore.isSearching()).toBe(false);
  });
});
