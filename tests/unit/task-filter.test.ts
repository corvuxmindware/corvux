import { describe, expect, it } from "vitest";
import {
  distinctFolders,
  taskFolderOf,
  taskMatchesFilter,
  type TaskPanelFilter,
} from "../../src/components/Tasks/taskFilter";

/**
 * Tasks pane filter row (tasks-noise redesign, Layer 3). Display-only
 * narrowing of the loaded task list — text + folder scope. These pure
 * helpers decide what the pane shows; the actual de-noising (what becomes
 * a task at all) is the `tasks: off` property + excluded-folders setting,
 * tested on the Rust side.
 */

const ROOST = "C:/Roost";

const task = (text: string, notePath: string, noteTitle = "n") => ({
  text,
  noteTitle,
  notePath,
});

describe("taskFolderOf", () => {
  it("returns the roost-relative parent folder, original case", () => {
    expect(taskFolderOf("C:/Roost/Daily Notes/2026-06-10.md", ROOST)).toBe(
      "Daily Notes",
    );
    expect(taskFolderOf("C:/Roost/Projects/Atlas/plan.md", ROOST)).toBe(
      "Projects/Atlas",
    );
  });

  it("returns empty for a note at the roost root", () => {
    expect(taskFolderOf("C:/Roost/Inbox.md", ROOST)).toBe("");
  });

  it("normalizes backslashes and a trailing-slash roost root", () => {
    expect(taskFolderOf("C:\\Roost\\Work\\a.md", "C:\\Roost\\")).toBe("Work");
  });
});

describe("distinctFolders", () => {
  it("collects unique parent folders, sorted, root omitted", () => {
    const rows = [
      task("a", "C:/Roost/Inbox.md"),
      task("b", "C:/Roost/Work/x.md"),
      task("c", "C:/Roost/Archive/y.md"),
      task("d", "C:/Roost/Work/z.md"), // dup folder
    ];
    expect(distinctFolders(rows, ROOST)).toEqual(["Archive", "Work"]);
  });

  it("keeps the first spelling of a case-variant folder", () => {
    const rows = [
      task("a", "C:/Roost/Work/x.md"),
      task("b", "C:/Roost/work/y.md"),
    ];
    expect(distinctFolders(rows, ROOST)).toEqual(["Work"]);
  });
});

describe("taskMatchesFilter", () => {
  const f = (over: Partial<TaskPanelFilter>): TaskPanelFilter => ({
    query: "",
    scope: "",
    ...over,
  });

  it("passes everything with an empty filter", () => {
    expect(taskMatchesFilter(task("anything", "C:/Roost/a.md"), f({}), ROOST)).toBe(
      true,
    );
  });

  it("matches query against task text (case-insensitive)", () => {
    const t = task("Reply to Ravi about API", "C:/Roost/a.md");
    expect(taskMatchesFilter(t, f({ query: "ravi" }), ROOST)).toBe(true);
    expect(taskMatchesFilter(t, f({ query: "invoice" }), ROOST)).toBe(false);
  });

  it("matches query against the note title too", () => {
    const t = task("standup", "C:/Roost/a.md", "Sprint Planning");
    expect(taskMatchesFilter(t, f({ query: "sprint" }), ROOST)).toBe(true);
  });

  it("keeps tasks inside the scoped folder and its subfolders", () => {
    const inWork = task("x", "C:/Roost/Work/a.md");
    const inWorkSub = task("y", "C:/Roost/Work/Q3/b.md");
    const elsewhere = task("z", "C:/Roost/Home/c.md");
    expect(taskMatchesFilter(inWork, f({ scope: "Work" }), ROOST)).toBe(true);
    expect(taskMatchesFilter(inWorkSub, f({ scope: "Work" }), ROOST)).toBe(true);
    expect(taskMatchesFilter(elsewhere, f({ scope: "Work" }), ROOST)).toBe(false);
  });

  it("does not treat a sibling sharing a name prefix as in-scope", () => {
    const sibling = task("x", "C:/Roost/Workshop/a.md");
    expect(taskMatchesFilter(sibling, f({ scope: "Work" }), ROOST)).toBe(false);
  });

  it("applies query and scope together (AND)", () => {
    const t = task("ship release", "C:/Roost/Work/a.md");
    expect(
      taskMatchesFilter(t, f({ query: "ship", scope: "Work" }), ROOST),
    ).toBe(true);
    expect(
      taskMatchesFilter(t, f({ query: "ship", scope: "Home" }), ROOST),
    ).toBe(false);
  });
});
