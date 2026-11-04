import { roostStore } from "../stores/roost";
import { tabStore } from "../stores/tabs";
import { flattenFiles } from "../app/derived";

/// Resolve a `[[wiki-link]]` target to a roost path and open it as a
/// tab. Shared between the editor's Ctrl+Click navigation and the chat
/// panel's rendered wiki-link clicks so the two surfaces use the same
/// matching rules — otherwise the same `[[Title]]` would resolve to
/// different notes in different parts of the app.
///
/// Resolution priority:
///   1. Exact roost-relative path match (`[[folder/note]]`)
///   2. Basename match (`[[note]]` → any note with that filename), and when
///      several notes share the basename, the one whose folder is CLOSEST to
///      the source note — matching the backend graph resolver so a click
///      opens the same note the backlink/graph edge points at (LR-2).
/// Both case-insensitive. Path separators normalised. `.md` stripped. Any
/// `#heading` / `^block` anchor is stripped before matching (LR-1).
///
/// When no match exists and `createIfMissing` is true, falls back to
/// `roostStore.createNewNote(...)` (with the anchor stripped, so a
/// mistyped `[[Note#Section]]` never mints a junk note literally named
/// "Note#Section"). The chat surface passes `createIfMissing: false` — we
/// don't want clicking a wiki-link in the agent's reply to silently
/// create a note the user didn't ask for.
///
/// `sourcePath` is the note the link lives in; it anchors the folder-
/// proximity tiebreak. Defaults to the active tab (correct for the editor's
/// Ctrl+Click); chat clicks can pass it explicitly.
export async function navigateToWikiLink(
  target: string,
  options: { createIfMissing?: boolean; sourcePath?: string } = {},
): Promise<boolean> {
  const roost = roostStore.roostPath();
  if (!roost) return false;

  const notes = flattenFiles(roostStore.files());
  const roostRoot = roost.replace(/\\/g, "/").replace(/\/+$/, "");

  // LR-1: drop any in-note anchor before matching. `#heading`, `^block`, and
  // `#^block` all address a location WITHIN the target note, not a different
  // note. The backend resolver strips these too (normalize_target); doing it
  // here keeps click-navigation in parity and stops the editor creating a
  // note named "Note#Section".
  const withoutAnchor = target.split(/[#^]/)[0].trim();
  const targetLower = withoutAnchor.toLowerCase().replace(/\\/g, "/").replace(/\.md$/, "");
  // A bare same-note anchor (`[[#Heading]]`) has no note part — nothing to
  // open, and nothing to create.
  if (!targetLower) return false;

  const toRel = (p: string): string => {
    const norm = p.replace(/\\/g, "/").replace(/\.md$/i, "");
    return norm.startsWith(roostRoot + "/") ? norm.slice(roostRoot.length + 1) : norm;
  };

  // 1. Full roost-relative path match.
  let match = notes.find((n) => toRel(n.path).toLowerCase() === targetLower);

  // 2. Basename match, folder-proximity ranked.
  if (!match) {
    const targetBase = targetLower.split("/").pop() ?? targetLower;
    const candidates = notes.filter(
      (n) => n.name.replace(/\.md$/i, "").toLowerCase() === targetBase,
    );
    if (candidates.length === 1) {
      match = candidates[0];
    } else if (candidates.length > 1) {
      const sourcePath = options.sourcePath ?? tabStore.activeTabPath() ?? "";
      const sourceDir = dirSegments(toRel(sourcePath).toLowerCase());
      // Mirror the backend: most shared folder segments wins; then shallower
      // path; then lexicographically-smallest relative path. Deterministic.
      match = candidates
        .map((n) => {
          const rel = toRel(n.path).toLowerCase();
          return {
            n,
            rel,
            shared: sharedPrefix(dirSegments(rel), sourceDir),
            depth: rel.split("/").length,
          };
        })
        .sort(
          (a, b) =>
            b.shared - a.shared ||
            a.depth - b.depth ||
            (a.rel < b.rel ? -1 : a.rel > b.rel ? 1 : 0),
        )[0].n;
    }
  }

  if (match) {
    await tabStore.openTab(match.path);
    return true;
  }

  if (options.createIfMissing) {
    const path = await roostStore.createNewNote(withoutAnchor);
    if (path) {
      // Following a link to a note that didn't exist yet means the user
      // intends to fill it in — open the empty note in write mode.
      await tabStore.openTab(path, { readOnly: false });
      return true;
    }
  }
  return false;
}

/// Folder segments of a roost-relative note path (the path minus its
/// filename), empty segments dropped.
function dirSegments(rel: string): string[] {
  const idx = rel.lastIndexOf("/");
  const dir = idx >= 0 ? rel.slice(0, idx) : "";
  return dir.split("/").filter(Boolean);
}

/// Count of shared leading segments between two folder-segment lists.
function sharedPrefix(a: string[], b: string[]): number {
  let i = 0;
  while (i < a.length && i < b.length && a[i] === b[i]) i++;
  return i;
}
