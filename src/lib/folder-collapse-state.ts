/**
 * Persist the sidebar's collapsed-folder set across app restarts.
 *
 * The file tree defaults every folder to expanded (empty set = all open).
 * Without persistence, a user who collapses folders to navigate a large
 * roost finds the whole tree re-expanded on the next launch — unusable
 * once a roost has hundreds of files. We store the collapsed set per roost
 * (keyed by roost path) so each roost remembers its own view.
 *
 * Storage is localStorage (synchronous, survives restart, no IPC) via the
 * safe `lib/storage.ts` wrappers, matching how theme / typography / search
 * mode are already persisted.
 */
import { lsGet, lsSet } from "./storage";

const PREFIX = "corvux:folders-collapsed:";

export function collapseStorageKey(roostPath: string): string {
  return PREFIX + roostPath;
}

/**
 * Load the collapsed-folder set for a roost. Returns an empty set (= all
 * folders expanded, the default) for an unseen roost or any corrupt /
 * missing entry — so a bad value can never wedge the tree.
 */
export function loadCollapsedPaths(roostPath: string): Set<string> {
  const raw = lsGet(collapseStorageKey(roostPath));
  if (!raw) return new Set();
  try {
    const parsed: unknown = JSON.parse(raw);
    if (Array.isArray(parsed)) {
      return new Set(parsed.filter((x): x is string => typeof x === "string"));
    }
  } catch {
    /* corrupt JSON — fall through to the empty default */
  }
  return new Set();
}

/**
 * Persist the collapsed-folder set for a roost. No-op when there's no open
 * roost (nothing to key on). The set is stored as a JSON array.
 */
export function saveCollapsedPaths(
  roostPath: string | null,
  paths: Set<string>,
): void {
  if (!roostPath) return;
  lsSet(collapseStorageKey(roostPath), JSON.stringify([...paths]));
}
