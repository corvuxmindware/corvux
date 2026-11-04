/**
 * AUDIT-053: contract tests for the bundled `save_roost_note` IPC and
 * its TS wrapper. Pins the wire shape so the Rust `SaveOutcome` struct
 * and the TS `SaveOutcome` interface can't drift apart silently.
 *
 * Component rendering isn't in vitest's scope (see vitest.config.ts),
 * so we exercise the helper module directly rather than driving the
 * editor save flow end-to-end (that lives in the Playwright suite).
 */

import { describe, it, expect } from "vitest";
import { mockInvoke } from "./setup";
import { saveRoostNote, type SaveOutcome } from "../../src/lib/save";

describe("saveRoostNote wrapper (AUDIT-053)", () => {
  it("hands path + content to save_roost_note and returns the bundled outcome", async () => {
    const fixture: SaveOutcome = {
      stats: { notes: 42, links: 17, tags: 8 },
      tasks: [
        {
          note_id: "notes/today",
          note_path: "/v/notes/today.md",
          note_title: "today",
          source_line: 5,
          text: "go to gym",
          due_at: "2026-05-25T14:30:00",
          completed: false,
          completed_at: "",
          linked_person: "",
          linked_project: "",
        },
      ],
      backlinks: [{ title: "yesterday", path: "/v/notes/yesterday.md" }],
      fts5: { kind: "indexed" },
    };
    const calls: Array<Record<string, unknown>> = [];
    mockInvoke.set("save_roost_note", (args) => {
      calls.push(args);
      return fixture;
    });

    const outcome = await saveRoostNote("/v/notes/today.md", "# Today\n\nGo to gym");

    expect(calls).toHaveLength(1);
    expect(calls[0]).toEqual({
      path: "/v/notes/today.md",
      content: "# Today\n\nGo to gym",
    });
    expect(outcome).toEqual(fixture);
  });

  it("propagates the FTS5-failed envelope so the caller can surface a non-blocking warning", async () => {
    const fixture: SaveOutcome = {
      stats: { notes: 1, links: 0, tags: 0 },
      tasks: [],
      backlinks: [],
      fts5: { kind: "failed", message: "search DB not initialized" },
    };
    mockInvoke.set("save_roost_note", () => fixture);

    const outcome = await saveRoostNote("/v/notes/today.md", "x");

    // The save itself didn't reject — the bundled outcome carries the
    // fail-soft. Caller's responsibility (effects.ts) to log / toast.
    expect(outcome.fts5.kind).toBe("failed");
    if (outcome.fts5.kind === "failed") {
      expect(outcome.fts5.message).toContain("not initialized");
    }
  });

  it("propagates a hard backend error (non-markdown path, no roost) by rejecting", async () => {
    mockInvoke.set("save_roost_note", () => {
      throw "save_roost_note rejects 'settings.json' — not a markdown file. Use write_file for non-markdown writes.";
    });
    await expect(saveRoostNote("settings.json", "{}")).rejects.toMatch(/not a markdown file/);
  });
});
