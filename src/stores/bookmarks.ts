import { createSignal, createRoot } from "solid-js";
import { commands, unwrap } from "../lib/invoke";
import { log } from "../lib/log";
import { roostStore } from "./roost";

// Bookmarks are roost-relative paths persisted to `<roost>/.corvux/bookmarks.json`.
// Per-roost, no sync coupling — each device keeps its own list. Sync of
// bookmarks across devices is a separate feature: easy to add later, but
// not all users want their bookmark list shared, so default OFF.
//
// Storage format: `{ paths: string[] }`. A flat JSON array keeps the file
// trivially mergeable + readable when users browse `.corvux/` directly.
// Order is insertion order (newest at the top) — same semantics Obsidian
// uses for its Bookmarks pane.

interface BookmarksFile {
  paths: string[];
}

const BOOKMARKS_REL = ".corvux/bookmarks.json";

function createBookmarksStore() {
  const [paths, setPaths] = createSignal<string[]>([]);
  const [loaded, setLoaded] = createSignal(false);

  function bookmarksFile(roostPath: string): string {
    return `${roostPath.replace(/[/\\]+$/, "")}/${BOOKMARKS_REL}`;
  }

  async function load(roostPath: string) {
    setLoaded(false);
    try {
      const raw = await unwrap(commands.readFile(bookmarksFile(roostPath)));
      const parsed = JSON.parse(raw) as BookmarksFile;
      if (parsed && Array.isArray(parsed.paths)) {
        setPaths(parsed.paths.filter((p) => typeof p === "string"));
      } else {
        setPaths([]);
      }
    } catch {
      // File doesn't exist yet — fresh roost. Start with an empty list.
      setPaths([]);
    } finally {
      setLoaded(true);
    }
  }

  async function persist() {
    const roost = roostStore.roostPath();
    if (!roost) return;
    const body: BookmarksFile = { paths: paths() };
    try {
      await unwrap(
        commands.writeFile(bookmarksFile(roost), JSON.stringify(body, null, 2)),
      );
    } catch (e) {
      log.error("bookmarks", "persist failed", e);
    }
  }

  function isBookmarked(path: string): boolean {
    return paths().includes(path);
  }

  async function add(path: string) {
    if (isBookmarked(path)) return;
    // Newest at the top — matches the user's mental model of a recent list.
    setPaths((prev) => [path, ...prev.filter((p) => p !== path)]);
    await persist();
  }

  async function remove(path: string) {
    setPaths((prev) => prev.filter((p) => p !== path));
    await persist();
  }

  async function toggle(path: string) {
    if (isBookmarked(path)) await remove(path);
    else await add(path);
  }

  /** Drop entries that no longer exist on disk. Called on roost load
   *  AFTER `load()` so renamed/deleted notes don't ghost the list. */
  async function reconcileAgainstRoost(validPaths: Set<string>) {
    const filtered = paths().filter((p) => validPaths.has(p));
    if (filtered.length !== paths().length) {
      setPaths(filtered);
      await persist();
    }
  }

  return {
    paths,
    loaded,
    load,
    isBookmarked,
    add,
    remove,
    toggle,
    reconcileAgainstRoost,
  };
}

export const bookmarksStore = createRoot(createBookmarksStore);
