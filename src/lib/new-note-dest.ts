// Where a new note gets created. Pure + dependency-free so it can be unit
// tested and shared between the roost store (which actually creates the
// file) and the Sidebar (which shows the destination in the New Note
// input). Rule chosen by the user: a new note lands in the folder of the
// note you currently have open ("create where I'm working"), falling back
// to the roost root when nothing — or a non-file tab like Graph — is open.

/** Sentinel tab paths (Graph, etc.) use this scheme, not a real file path. */
const SENTINEL_PREFIX = "corvux://";

/** Strip a trailing path separator (either platform's). */
function stripTrailingSep(p: string): string {
  return p.replace(/[\\/]+$/, "");
}

/**
 * Absolute folder a new note should be created in, given the currently
 * active tab path and the roost root. Returns the roost root when no note
 * is open, when the active tab is a sentinel (e.g. the Graph view), or when
 * the active path somehow sits outside the roost.
 */
export function newNoteDestDir(
  activeTabPath: string | null | undefined,
  roostRoot: string,
): string {
  const root = stripTrailingSep(roostRoot);
  if (!activeTabPath) return root;
  if (activeTabPath.startsWith(SENTINEL_PREFIX)) return root;
  if (!activeTabPath.startsWith(root)) return root;
  const parent = stripTrailingSep(activeTabPath.replace(/[\\/][^\\/]+$/, ""));
  return parent || root;
}

/**
 * Human label for a destination folder, relative to the roost root —
 * "Roost root" for the root itself, otherwise the in-roost path
 * (e.g. "Projects/Active"). Used in the New Note input so the user always
 * sees where the note will be created.
 */
export function relFolderLabel(destDir: string, roostRoot: string): string {
  const root = stripTrailingSep(roostRoot);
  const dir = stripTrailingSep(destDir);
  if (dir === root || dir === "") return "Roost root";
  const rel = dir.startsWith(root)
    ? dir.slice(root.length).replace(/^[\\/]+/, "")
    : dir;
  // Normalise Windows separators to forward slashes for display only.
  return rel.replace(/\\/g, "/") || "Roost root";
}
