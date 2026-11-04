// Single source of truth for the mobile drawer's panel strip.
//
// Before this module the panel list was duplicated: App.tsx defined an
// (unused, dead) Files/Search/Bookmarks set while LeftPane.tsx rendered a
// different Files/Search/Tasks set — the product intent and the rendered UI
// disagreed, which the codex review flagged as "feels unfinished". Both
// surfaces now import from here.
//
// Hierarchy (codex finding #1): the top strip answers "where do I go?", so
// navigation destinations (Bookmarks) get the primary slots; feature panels
// (Tasks, Outline, …) live in the More overflow.

export interface MobilePanel {
  /** Matches the `activePanel` signal value LeftPane switches on. */
  id: string;
  label: string;
  /** Single SVG path string, rendered in a 24×24 viewBox, 1.8 stroke. */
  icon: string;
}

/** Primary tabs shown directly in the drawer strip: Files · Search · Bookmarks.
 *  The strip also renders a fixed "More" button (the overflow trigger) after
 *  these — it is not a panel, so it's not in this list. */
export const mobilePrimaryPanels: readonly MobilePanel[] = [
  {
    id: "files",
    label: "Files",
    icon: "M3 7v10a2 2 0 002 2h14a2 2 0 002-2V9a2 2 0 00-2-2h-6l-2-2H5a2 2 0 00-2 2z",
  },
  {
    id: "search",
    label: "Search",
    icon: "M21 21l-6-6m2-5a7 7 0 11-14 0 7 7 0 0114 0z",
  },
  {
    id: "bookmarks",
    label: "Bookmarks",
    icon: "M19 21l-7-5-7 5V5a2 2 0 012-2h10a2 2 0 012 2z",
  },
] as const;

/** Secondary feature panels behind the "More" overflow, ordered by intent. */
export const mobileOverflowPanels: readonly MobilePanel[] = [
  {
    id: "tasks",
    label: "Tasks",
    icon: "M9 11l3 3L22 4M21 12v7a2 2 0 01-2 2H5a2 2 0 01-2-2V5a2 2 0 012-2h11",
  },
  {
    id: "backlinks",
    label: "Links",
    icon: "M10 13a5 5 0 007.54.54l3-3a5 5 0 00-7.07-7.07l-1.72 1.71 M14 11a5 5 0 00-7.54-.54l-3 3a5 5 0 007.07 7.07l1.71-1.71",
  },
  {
    id: "outline",
    label: "Outline",
    icon: "M8 6h13M8 12h13M8 18h13M3 6h.01M3 12h.01M3 18h.01",
  },
  {
    id: "history",
    label: "History",
    icon: "M3 12a9 9 0 1 0 3-6.7L3 8 M3 3v5h5 M12 7v5l3 2",
  },
] as const;
