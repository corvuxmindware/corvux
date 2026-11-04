import { createMemo } from "solid-js";
import { roostStore, type FileEntry } from "../stores/roost";
import { tabStore, GRAPH_TAB_PATH } from "../stores/tabs";
import { parseNote } from "../lib/frontmatter";
import type { NoteInfo } from "../components/Editor/extensions/link-autocomplete";

/// Recursively flatten the roost file tree into a flat list of leaves.
/// Used for autocomplete and wiki-link target resolution.
export function flattenFiles(entries: FileEntry[]): NoteInfo[] {
  const result: NoteInfo[] = [];
  for (const entry of entries) {
    if (entry.is_dir) {
      result.push(...flattenFiles(entry.children));
    } else {
      result.push({ name: entry.name, path: entry.path });
    }
  }
  return result;
}

/// Memoised list of all notes in the roost, suitable for the editor's
/// autocomplete provider. Recomputes only when `roostStore.files()` changes.
export function createAllNotes() {
  return createMemo(() => flattenFiles(roostStore.files()));
}

/// Memoised parse of the active tab's content into `{ frontmatter, body }`.
/// Used both for the Properties panel and the StatusBar word/char counts so
/// the YAML block isn't included in the count.
export function createParsedActive() {
  return createMemo(() => parseNote(tabStore.activeContent()));
}

export function createDocStats(parsed: ReturnType<typeof createParsedActive>) {
  return createMemo(() => {
    const content = tabStore.activeContent();
    if (!content) return { words: 0, chars: 0, lines: 0, properties: 0 };
    const body = parsed().body || content;
    const fm = parsed().frontmatter;
    return {
      words: body.split(/\s+/).filter(Boolean).length,
      chars: body.length,
      lines: body.split("\n").length,
      properties: fm ? Object.keys(fm).length : 0,
    };
  });
}

/// Build a breadcrumb of `{ roost, segments, path }` for the active tab.
/// `segments` is the relative path split into folder segments with the
/// `.md` extension stripped from the final entry — TopBar renders each
/// segment as its own span with `/` separators (mirrors Obsidian's
/// breadcrumb so the user sees the full roost → folder → folder → note
/// chain instead of `B... / Fit-ME/202...`). `path` kept for callers
/// that want the joined string (status bar, etc.). Returns `null` for
/// the Graph tab and when no tab is open.
export function createBreadcrumb() {
  return () => {
    const roost = roostStore.roostPath();
    const file = tabStore.activeTabPath();
    if (!roost || !file || file === GRAPH_TAB_PATH) return null;
    const roostName =
      roostStore.roostDisplayName() || roost.match(/[^/\\]+$/)?.[0] || "";
    const normRoost = roost.replace(/[/\\]+$/, "");
    let relative = file;
    if (file.startsWith(normRoost)) {
      relative = file.slice(normRoost.length).replace(/^[/\\]+/, "");
    }
    relative = relative.replace(/\\/g, "/");
    const rawSegments = relative.split("/").filter(Boolean);
    const segments = rawSegments.map((seg, i) =>
      i === rawSegments.length - 1 ? seg.replace(/\.md$/i, "") : seg,
    );
    return { roost: roostName, segments, path: relative };
  };
}
