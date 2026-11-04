/**
 * `resolveBookmark` — picks the security-scoped bookmark to open a roost
 * with. Regression for ROOST_OPEN_FAIL on iOS: RoostSwitcher (and other
 * re-open paths) opened a scoped roost by path + name only, dropping the
 * bookmark, so the backend stat'd a raw iCloud path and the OS denied it
 * (EPERM). The fix resolves the bookmark centrally from the registry when
 * a caller doesn't pass one.
 */

import { describe, it, expect } from "vitest";
import { resolveBookmark, type RoostEntry } from "../../src/stores/roosts";

const entry = (path: string, bookmark?: string): RoostEntry => ({
  id: path,
  path,
  displayName: path,
  type: bookmark ? "ios-scoped" : "native",
  lastOpened: 0,
  bookmark,
});

const REGISTRY: RoostEntry[] = [
  entry("/icloud/Obsidian/Brain-Dump", "BM_OBSIDIAN"),
  entry("/local/Notes"), // native, no bookmark
];

describe("resolveBookmark", () => {
  it("returns the explicit bookmark when one is passed (caller wins)", () => {
    expect(resolveBookmark("BM_EXPLICIT", REGISTRY, "/icloud/Obsidian/Brain-Dump")).toBe(
      "BM_EXPLICIT",
    );
  });

  it("falls back to the registry bookmark when the caller passes none", () => {
    // This is the RoostSwitcher case: openRoost(path, name) with no bookmark.
    expect(resolveBookmark(undefined, REGISTRY, "/icloud/Obsidian/Brain-Dump")).toBe(
      "BM_OBSIDIAN",
    );
  });

  it("returns undefined for a native roost with no stored bookmark", () => {
    expect(resolveBookmark(undefined, REGISTRY, "/local/Notes")).toBeUndefined();
  });

  it("returns undefined when the path isn't in the registry", () => {
    expect(resolveBookmark(undefined, REGISTRY, "/unknown/path")).toBeUndefined();
  });
});
