// Folder-destination logic for the "Move to folder…" action. Pure +
// dependency-free so it can be unit-tested without a Tauri runtime or a
// rendered tree — the gesture/IPC layer lives in MoveToSheet + Sidebar.

import type { FileEntry } from "./bindings";

export interface MoveTarget {
  /** Absolute folder path to move the entry INTO. */
  path: string;
  /** Display label — the folder name, or "Roost root" for the root. */
  label: string;
  /** Nesting depth for indentation. Root = 0, top-level folders = 1. */
  depth: number;
}

/** Parent directory of an absolute path, honouring either separator. */
function parentDir(p: string): string {
  return p.replace(/[\\/][^\\/]+$/, "");
}

/**
 * The folders a given entry can be moved into. Excludes, by construction:
 *  - the entry itself (a folder can't be moved inside itself),
 *  - any descendant of the entry (can't move a folder into its own subtree —
 *    that would orphan the subtree on disk),
 *  - the entry's current parent (moving there is a no-op).
 *
 * The roost root is offered as the first target unless the entry already
 * lives directly under it. We still *descend* into a skipped folder (a
 * deeper folder may be a valid destination) — except we never descend into
 * the entry's own subtree.
 *
 * `tree` is the same nested `FileEntry[]` the Sidebar renders; `roostRoot`
 * is the roost's absolute root path.
 */
export function eligibleMoveTargets(
  tree: FileEntry[],
  entry: FileEntry,
  roostRoot: string,
): MoveTarget[] {
  const sep = entry.path.includes("\\") ? "\\" : "/";
  const currentParent = parentDir(entry.path);
  const rootNorm = roostRoot.replace(/[\\/]+$/, "");

  const isSelfOrDescendant = (folderPath: string): boolean =>
    entry.is_dir &&
    (folderPath === entry.path || folderPath.startsWith(entry.path + sep));

  const targets: MoveTarget[] = [];

  // Root target — unless the entry is already directly under the root.
  if (currentParent !== rootNorm) {
    targets.push({ path: rootNorm, label: "Roost root", depth: 0 });
  }

  const walk = (entries: FileEntry[], depth: number): void => {
    for (const e of entries) {
      if (!e.is_dir) continue;
      const skip = isSelfOrDescendant(e.path) || e.path === currentParent;
      if (!skip) {
        targets.push({ path: e.path, label: e.name, depth });
      }
      // Descend unless this folder is the entry's own subtree.
      if (!isSelfOrDescendant(e.path)) {
        walk(e.children, depth + 1);
      }
    }
  };
  walk(tree, 1);

  return targets;
}
