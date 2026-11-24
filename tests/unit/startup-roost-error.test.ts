/**
 * Tests for `src/lib/startup-roost-error.ts` — the message shown when the
 * last roost fails to re-open at app start (SAF device matrix row A1).
 *
 * Contract: always says which roost failed and why (paths scrubbed), and
 * tells the user what happens next — either how to retry, or that the dead
 * entry was dropped from the list without touching any files.
 */

import { describe, expect, it } from "vitest";
import { startupRoostErrorMessage } from "../../src/lib/startup-roost-error";

const A1_ERROR =
  "SAF roost is no longer accessible: Directory listing incomplete: provider could not list '/'";

describe("startupRoostErrorMessage", () => {
  it("names the roost, gives the reason and the retry path for a recoverable failure", () => {
    const msg = startupRoostErrorMessage("FaultLab", A1_ERROR, false);
    expect(msg).toBe(
      `Could not open "FaultLab": ${A1_ERROR}. To try again, open the sidebar, tap Switch Roost and pick it.`,
    );
  });

  it("says the entry was removed and no files were deleted for a dead path", () => {
    const msg = startupRoostErrorMessage("Notes", new Error("Path does not exist: /x"), true);
    expect(msg).toContain('Could not open "Notes": Path does not exist');
    expect(msg).toContain("removed from your roost list. No files were deleted.");
    expect(msg).not.toContain("try again");
  });

  it("scrubs provider and storage paths from the visible line", () => {
    const msg = startupRoostErrorMessage(
      "Lab",
      "cannot list content://com.example.provider/tree/root or /storage/emulated/0/Notes",
      false,
    );
    expect(msg).not.toContain("content://");
    expect(msg).not.toContain("/storage/");
    expect(msg).toContain("<path>");
  });

  it("falls back to a generic label when the name is missing or blank", () => {
    expect(startupRoostErrorMessage(null, "locked", false)).toMatch(/^Could not open your last roost: locked\./);
    expect(startupRoostErrorMessage("   ", "locked", false)).toMatch(/^Could not open your last roost/);
  });

  it("does not double the final full stop and copes with an empty reason", () => {
    expect(startupRoostErrorMessage("A", "Roost is locked.", false)).toMatch(/^Could not open "A": Roost is locked\. To try/);
    expect(startupRoostErrorMessage("A", "", false)).toMatch(/^Could not open "A"\. To try/);
  });
});
