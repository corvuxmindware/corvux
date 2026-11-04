/**
 * Regression guard for the welcome-note duplicate bug (2026-05-27).
 *
 * Before the fix, onboarding produced TWO welcome notes: `openRoost` auto-
 * seeded `Welcome to Corvux.md` into the empty roost, then the onboarding
 * step-3 checkbox seeded it again → `Welcome to Corvux 1.md`. Unchecking the
 * box also failed to yield an empty roost. The fix routes the auto-seed
 * decision through `shouldSeedWelcome`, which onboarding opts out of via
 * `skipWelcome`. These tests pin that predicate.
 */
import { describe, it, expect } from "vitest";
import { countMdFiles, shouldSeedWelcome, type FileEntry } from "../../src/stores/roost";

const file = (name: string): FileEntry => ({ name, path: `/v/${name}`, is_dir: false, children: [] });
const dir = (name: string, children: FileEntry[]): FileEntry => ({
  name,
  path: `/v/${name}`,
  is_dir: true,
  children,
});

describe("countMdFiles", () => {
  it("counts top-level markdown files", () => {
    expect(countMdFiles([file("a.md"), file("b.md"), file("c.txt")])).toBe(2);
  });

  it("recurses into nested directories", () => {
    expect(
      countMdFiles([dir("sub", [file("deep.md"), dir("deeper", [file("x.md")])]), file("top.md")]),
    ).toBe(3);
  });

  it("returns 0 for an empty tree", () => {
    expect(countMdFiles([])).toBe(0);
  });

  it("ignores non-markdown and empty dirs", () => {
    expect(countMdFiles([file("note.pdf"), dir("empty", [])])).toBe(0);
  });
});

describe("shouldSeedWelcome", () => {
  it("seeds an empty native roost", () => {
    expect(shouldSeedWelcome([], { isSaf: false, skipWelcome: false })).toBe(true);
  });

  it("does NOT seed when the roost already has a markdown file", () => {
    expect(shouldSeedWelcome([file("existing.md")], { isSaf: false, skipWelcome: false })).toBe(
      false,
    );
  });

  it("counts nested markdown — a note buried in a subfolder still blocks seeding", () => {
    expect(
      shouldSeedWelcome([dir("journal", [file("2026-05-27.md")])], {
        isSaf: false,
        skipWelcome: false,
      }),
    ).toBe(false);
  });

  it("does NOT seed a SAF (content://) roost — create unsupported", () => {
    expect(shouldSeedWelcome([], { isSaf: true, skipWelcome: false })).toBe(false);
  });

  // The core of the duplicate-note fix: onboarding passes skipWelcome so its
  // own consented checkbox is the sole writer. Even an empty native roost must
  // NOT auto-seed under skipWelcome.
  it("does NOT seed when skipWelcome is set, even for an empty native roost", () => {
    expect(shouldSeedWelcome([], { isSaf: false, skipWelcome: true })).toBe(false);
  });
});
