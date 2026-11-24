/**
 * Tests for `src/lib/roost-entry-open.ts` — the single "is this roost really
 * open" rule shared by Switch Roost and Manage Roosts (SAF device row A1).
 */

import { describe, expect, it } from "vitest";
import { isRoostEntryOpen } from "../../src/lib/roost-entry-open";

const lab = { id: "lab", path: "content://lab/tree/root" };
const docs = { id: "docs", path: "content://docs/tree/x" };

describe("isRoostEntryOpen", () => {
  it("is true only for the active entry when its path is the open roost", () => {
    expect(isRoostEntryOpen(lab, "lab", lab.path)).toBe(true);
  });

  it("is false for the active entry when nothing is open (failed startup open)", () => {
    expect(isRoostEntryOpen(lab, "lab", null)).toBe(false);
  });

  it("is false for the active entry when a different roost is open", () => {
    expect(isRoostEntryOpen(lab, "lab", docs.path)).toBe(false);
  });

  it("is false for a non-active entry even if its path matches", () => {
    expect(isRoostEntryOpen(docs, "lab", docs.path)).toBe(false);
  });

  it("is false when no entry is active", () => {
    expect(isRoostEntryOpen(lab, null, lab.path)).toBe(false);
  });
});
