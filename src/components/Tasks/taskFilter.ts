/// Pure helpers for the Tasks pane filter row (tasks-noise redesign,
/// Layer 3). Display-only narrowing of the already-loaded task list — it
/// does NOT touch what the indexer, scheduler, or agent see (that's the
/// `tasks: off` property + excluded-folders setting). Kept pure and
/// dependency-free so the filter logic is unit-testable without SolidJS.

import type { TaskRow } from "../../stores/tasks";

/// Minimal shape the filter needs — lets tests pass bare objects.
type TaskLike = Pick<TaskRow, "text" | "noteTitle" | "notePath">;

export interface TaskPanelFilter {
  /// Free text, matched case-insensitively against the task text + its
  /// note title. Empty = no text filter.
  query: string;
  /// Roost-relative folder path; keeps only tasks in that folder or a
  /// subfolder. Empty = all folders.
  scope: string;
}

export const EMPTY_FILTER: TaskPanelFilter = { query: "", scope: "" };

/// Roost-relative parent folder of a task's note (original case, forward
/// slashes). "" when the note sits at the roost root or the path can't be
/// made relative to `roostPath`.
export function taskFolderOf(notePath: string, roostPath: string): string {
  const abs = notePath.replace(/\\/g, "/");
  const root = roostPath
    ? roostPath.replace(/\\/g, "/").replace(/\/+$/, "") + "/"
    : "";
  const rel = root && abs.startsWith(root) ? abs.slice(root.length) : abs;
  const slash = rel.lastIndexOf("/");
  return slash >= 0 ? rel.slice(0, slash) : "";
}

/// Distinct, case-insensitively-sorted parent folders across the rows, for
/// the scope dropdown. Root-level notes (folder "") are omitted; the first
/// spelling of each folder wins for display.
export function distinctFolders(
  tasks: ReadonlyArray<TaskLike>,
  roostPath: string,
): string[] {
  const seen = new Map<string, string>(); // lowercase → first-seen original
  for (const t of tasks) {
    const f = taskFolderOf(t.notePath, roostPath);
    if (f && !seen.has(f.toLowerCase())) seen.set(f.toLowerCase(), f);
  }
  return [...seen.values()].sort((a, b) =>
    a.localeCompare(b, undefined, { sensitivity: "base" }),
  );
}

/// True when a task passes the panel filter. Text matches the task text or
/// its note title (case-insensitive substring); scope keeps tasks in the
/// chosen folder or any subfolder (case-insensitive).
export function taskMatchesFilter(
  task: TaskLike,
  filter: TaskPanelFilter,
  roostPath: string,
): boolean {
  const q = filter.query.trim().toLowerCase();
  if (q) {
    const hay = `${task.text}\n${task.noteTitle}`.toLowerCase();
    if (!hay.includes(q)) return false;
  }
  const scope = filter.scope.trim().toLowerCase();
  if (scope) {
    const folder = taskFolderOf(task.notePath, roostPath).toLowerCase();
    if (folder !== scope && !folder.startsWith(scope + "/")) return false;
  }
  return true;
}
