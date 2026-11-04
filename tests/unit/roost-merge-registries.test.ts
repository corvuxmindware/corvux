/**
 * NEW-M2: tests for `mergeRegistries` in `src/stores/roosts.ts`.
 *
 * The bug this fixes: `hydrateFromManifest` used to bail if the live registry
 * was non-empty, so if `restoreLastRoost` registered ONE roost while the async
 * manifest read was in flight, every OTHER manifest roost was dropped. Merging
 * by path (live wins) recovers the rest without clobbering the live entry.
 */
import { describe, it, expect } from "vitest";
import { mergeRegistries, type RoostEntry } from "../../src/stores/roosts";

function entry(id: string, path: string, extra?: Partial<RoostEntry>): RoostEntry {
  return {
    id,
    path,
    displayName: id,
    type: "native",
    lastOpened: 0,
    ...extra,
  };
}

describe("mergeRegistries", () => {
  it("recovers manifest roosts missing from a partial live list (the race)", () => {
    // restoreLastRoost registered only roost A; the manifest still holds A,B,C.
    const live = [entry("a", "/r/a")];
    const manifest = [entry("a2", "/r/a"), entry("b", "/r/b"), entry("c", "/r/c")];
    const merged = mergeRegistries(live, manifest);
    expect(merged.map((e) => e.path)).toEqual(["/r/a", "/r/b", "/r/c"]);
    // The live entry (id "a") wins the /r/a collision, not the manifest's "a2".
    expect(merged.find((e) => e.path === "/r/a")?.id).toBe("a");
  });

  it("recovers the full list when the live registry is empty (localStorage wipe)", () => {
    const merged = mergeRegistries([], [entry("a", "/r/a"), entry("b", "/r/b")]);
    expect(merged.map((e) => e.path)).toEqual(["/r/a", "/r/b"]);
  });

  it("adds nothing when the manifest is a subset of the live list", () => {
    const live = [entry("a", "/r/a"), entry("b", "/r/b")];
    const merged = mergeRegistries(live, [entry("a2", "/r/a")]);
    expect(merged).toHaveLength(2);
    expect(merged.map((e) => e.id)).toEqual(["a", "b"]); // live entries preserved
  });

  it("dedups by path across both inputs", () => {
    const merged = mergeRegistries([entry("a", "/r/a")], [entry("a2", "/r/a")]);
    expect(merged).toHaveLength(1);
    expect(merged[0].id).toBe("a");
  });
});
