/**
 * `eligibleMoveTargets` — the folder-destination list behind the "Move to
 * folder…" picker. Pure function: no Tauri, no rendered tree. Locks down
 * the exclusion rules (self, descendants, current-parent) so a regression
 * can't silently offer an illegal destination (e.g. moving a folder into
 * its own subtree, which would orphan files on disk).
 */

import { describe, it, expect } from "vitest";
import { eligibleMoveTargets } from "../../src/lib/move-targets";
import type { FileEntry } from "../../src/lib/bindings";

const dir = (path: string, name: string, children: FileEntry[] = []): FileEntry => ({
  path,
  name,
  is_dir: true,
  children,
});
const file = (path: string, name: string): FileEntry => ({
  path,
  name,
  is_dir: false,
  children: [],
});

// /roost
//   Projects/
//     Active/
//     archive.md
//   Journal/
//   inbox.md
const ROOT = "/roost";
const tree: FileEntry[] = [
  dir("/roost/Projects", "Projects", [
    dir("/roost/Projects/Active", "Active"),
    file("/roost/Projects/archive.md", "archive.md"),
  ]),
  dir("/roost/Journal", "Journal"),
  file("/roost/inbox.md", "inbox.md"),
];

describe("eligibleMoveTargets", () => {
  it("offers root + all folders for a file at the roost root, minus nothing extra", () => {
    // inbox.md lives at the root, so 'Roost root' is excluded (no-op) but
    // every folder is a valid destination.
    const targets = eligibleMoveTargets(tree, file("/roost/inbox.md", "inbox.md"), ROOT);
    expect(targets.map((t) => t.path)).toEqual([
      "/roost/Projects",
      "/roost/Projects/Active",
      "/roost/Journal",
    ]);
  });

  it("excludes the file's current parent folder (moving there is a no-op)", () => {
    const targets = eligibleMoveTargets(
      tree,
      file("/roost/Projects/archive.md", "archive.md"),
      ROOT,
    );
    const paths = targets.map((t) => t.path);
    expect(paths).toContain("/roost"); // root now offered (file is not at root)
    expect(paths).toContain("/roost/Projects/Active");
    expect(paths).toContain("/roost/Journal");
    expect(paths).not.toContain("/roost/Projects"); // its current parent
  });

  it("excludes the folder itself and its whole subtree", () => {
    // Moving "Projects" — it must not be offered itself, nor "Active"
    // (its child), and "Roost root" is excluded since Projects is already
    // directly under root.
    const targets = eligibleMoveTargets(
      tree,
      dir("/roost/Projects", "Projects", [dir("/roost/Projects/Active", "Active")]),
      ROOT,
    );
    const paths = targets.map((t) => t.path);
    expect(paths).not.toContain("/roost/Projects");
    expect(paths).not.toContain("/roost/Projects/Active");
    expect(paths).not.toContain("/roost"); // already under root
    expect(paths).toEqual(["/roost/Journal"]);
  });

  it("labels the root target 'Roost root' at depth 0", () => {
    const targets = eligibleMoveTargets(
      tree,
      file("/roost/Projects/archive.md", "archive.md"),
      ROOT,
    );
    const root = targets.find((t) => t.path === "/roost");
    expect(root).toEqual({ path: "/roost", label: "Roost root", depth: 0 });
  });

  it("tolerates a trailing slash on the roost root", () => {
    const targets = eligibleMoveTargets(
      tree,
      file("/roost/Projects/archive.md", "archive.md"),
      "/roost/",
    );
    expect(targets.some((t) => t.path === "/roost")).toBe(true);
  });

  it("handles Windows-style backslash paths", () => {
    const winTree: FileEntry[] = [
      dir("C:\\roost\\A", "A", [dir("C:\\roost\\A\\sub", "sub")]),
      dir("C:\\roost\\B", "B"),
    ];
    const targets = eligibleMoveTargets(
      winTree,
      dir("C:\\roost\\A", "A", [dir("C:\\roost\\A\\sub", "sub")]),
      "C:\\roost",
    );
    // A and its subtree excluded; A is under root so root excluded; only B.
    expect(targets.map((t) => t.path)).toEqual(["C:\\roost\\B"]);
  });
});
