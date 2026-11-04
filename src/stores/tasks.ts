/// Tasks store — slice 2 of Phase 2.1.
///
/// Read-only view over the `:task` rows the indexer derives from
/// `- [ ]` checkboxes and frontmatter `tasks:` arrays. The Tasks pane
/// subscribes to `tasks()` and `groups()`; both refresh whenever a save
/// re-runs `index_note` (wired in `graph.ts::indexNote`) or after the
/// initial roost open.
///
/// Slice 3 will add `complete_task` (markdown rewrite). For now the
/// pane is informational — clicking a task opens the source note.

import { createSignal, createMemo, createRoot } from "solid-js";
import { commands, unwrap } from "../lib/invoke";
import { log } from "../lib/log";

export interface TaskRow {
  noteId: string;
  notePath: string;
  noteTitle: string;
  sourceLine: number;
  text: string;
  /// "" when no due date is set; otherwise "YYYY-MM-DD" or "YYYY-MM-DD HH:MM".
  dueAt: string;
  completed: boolean;
  completedAt: string;
  linkedPerson: string;
  linkedProject: string;
}

export type TaskBucket =
  | "overdue"
  | "today"
  | "thisWeek"
  | "later"
  | "noDate"
  | "done";

export interface TaskGroups {
  overdue: TaskRow[];
  today: TaskRow[];
  thisWeek: TaskRow[];
  later: TaskRow[];
  noDate: TaskRow[];
  done: TaskRow[];
}

interface RawTaskRow {
  note_id: string;
  note_path: string;
  note_title: string;
  source_line: number;
  text: string;
  due_at: string;
  completed: boolean;
  completed_at: string;
  linked_person: string;
  linked_project: string;
}

/// A task is "timed" when its dueAt carries an HH:MM clock time
/// ("YYYY-MM-DD HH:MM"), not just a date. The backend scheduler
/// (`task_notify.rs`) only arms an OS alarm for timed tasks, so this is
/// exactly the set that needs POST_NOTIFICATIONS to have been granted.
/// Exported for unit testing. (Audit 2026-06-08, HIGH-2.)
export function hasTimedTask(
  rows: ReadonlyArray<{ dueAt: string; completed: boolean }>,
): boolean {
  return rows.some((r) => !r.completed && /\d{2}:\d{2}/.test(r.dueAt));
}

function createTasksStore() {
const [tasks, setTasks] = createSignal<TaskRow[]>([]);
const [loaded, setLoaded] = createSignal(false);
const [error, setError] = createSignal<string | null>(null);

/// Track the in-session permission state so we don't re-prompt on every
/// quick-add. The OS already remembers across launches — this just
/// avoids spamming the IPC call mid-session.
let notificationPermissionChecked = false;
let notificationPermissionGranted = false;

/// Set once we've attempted to prompt off the back of a loaded task set
/// this session (regardless of outcome), so a routine task-list refresh
/// that still contains timed tasks doesn't re-prompt every time.
let notifPromptTriggeredForTasks = false;
let refreshGeneration = 0;

/// Make sure the OS has granted POST_NOTIFICATIONS (Android 13+) /
/// equivalent on other platforms. First call triggers the system
/// permission prompt; subsequent calls in the same session are no-ops
/// once we know the answer. Silent on platforms that auto-grant
/// (desktop). If the user denies, we surface a one-line error in the
/// pane so the next save makes it obvious why the toast won't fire.
async function ensureNotificationPermission(): Promise<void> {
  if (notificationPermissionChecked && notificationPermissionGranted) return;
  try {
    const { isPermissionGranted, requestPermission } = await import(
      "@tauri-apps/plugin-notification"
    );
    let granted = await isPermissionGranted();
    if (!granted) {
      const state = await requestPermission();
      granted = state === "granted";
      if (!granted) {
        setError(
          "Notifications denied. Tasks will still save, but timed reminders won't fire until you allow notifications in Settings → Apps → Corvux.",
        );
      }
    }
    notificationPermissionChecked = true;
    notificationPermissionGranted = granted;
  } catch (e) {
    // Plugin call failures are non-fatal — the task still saves; only
    // the future-fire path is at risk. Log and move on.
    log.warn("tasks", "notification permission check failed", e);
  }
}

/// Replace the in-memory task list with rows the backend already
/// queried. Used by `graphStore.indexNote` so the save path doesn't
/// fire a second `list_tasks` IPC after the indexer has just produced
/// the same data. Decoupled from `refresh()` so it skips the
/// `ensureGraphReady` round-trip — the caller already proved the graph
/// is ready by getting these rows back from `index_note`.
function applyTasksFromBackend(raw: RawTaskRow[]) {
  const rows: TaskRow[] = raw.map((r) => ({
    noteId: r.note_id,
    notePath: r.note_path,
    noteTitle: r.note_title,
    sourceLine: r.source_line,
    text: r.text,
    dueAt: r.due_at,
    completed: r.completed,
    completedAt: r.completed_at,
    linkedPerson: r.linked_person,
    linkedProject: r.linked_project,
  }));
  setTasks(rows);
  setLoaded(true);
  setError(null);
  maybeRequestNotifPermissionForTimedTasks(rows);
}

/// Editor, sync, and quick-add all funnel their tasks through the task
/// list — every refresh and every post-save re-index lands here. The
/// backend scheduler arms an OS alarm for each *timed* task it indexes,
/// so if the freshly-loaded set contains any timed task and we haven't
/// asked yet, request POST_NOTIFICATIONS now. Without this, a reminder
/// typed straight into a note (the common Obsidian-style workflow, not
/// quick-add) would schedule an alarm the OS then silently drops on
/// Android 13+ because the permission was never requested. Fire-and-
/// forget — never blocks the task-list update. (Audit 2026-06-08, HIGH-2.)
function maybeRequestNotifPermissionForTimedTasks(rows: TaskRow[]) {
  if (notifPromptTriggeredForTasks || notificationPermissionGranted) return;
  if (!hasTimedTask(rows)) return;
  notifPromptTriggeredForTasks = true;
  void ensureNotificationPermission();
}

async function refresh() {
  const generation = refreshGeneration;
  try {
    // The `:task` relation is populated by the indexer, which only runs
    // after `init_graph` has scanned the roost. `init_graph` is lazy —
    // it fires on first demand (graph view / search / chat / now: tasks).
    // Without this, opening the Tasks pane on a fresh roost load would
    // show "no tasks" until something else — typically the graph button —
    // happened to wake the indexer up first.
    //
    // Dynamic import to break the tasks → graph → tasks module cycle.
    const { graphStore } = await import("./graph");
    await graphStore.ensureGraphReady();

    // `?? []` — boundary guard: a backend/mock hiccup that resolves to
    // undefined must degrade to the empty state, not throw inside
    // `applyTasksFromBackend` and surface a raw TypeError in the panel.
    const raw = await unwrap(commands.listTasks());
    if (generation !== refreshGeneration) return;
    applyTasksFromBackend(raw ?? []);
  } catch (e) {
    if (generation !== refreshGeneration) return;
    log.error("tasks", "refresh failed", e);
    // Human copy only — the technical detail is in the log (and the
    // Diagnostics panel). Raw `TypeError: ... (reading 'map')` in the
    // pane reads as "the app is broken", not "tasks didn't load".
    setError("Couldn't load tasks. Tap ⟳ to retry.");
    setTasks([]);
    setLoaded(true);
  }
}

function clear() {
  refreshGeneration += 1;
  setTasks([]);
  setLoaded(false);
  setError(null);
}

/// Toggle a task's completion state by rewriting the source markdown.
/// Backend returns the new file content; we hand it to the tab store so an
/// open editor reloads from disk (skipping when the user has unsaved
/// changes — local edits win in that case). Refreshes `tasks()` after.
///
/// Frontmatter tasks (sourceLine ≤ 0) route through the YAML rewrite
/// path inside the same `complete_task` IPC — the source_line sign
/// drives backend dispatch. The optimistic flip below works for both.
async function completeTask(task: TaskRow, completed: boolean): Promise<void> {
  // Optimistic UI: flip the row in-memory so the click feels instant.
  // If the backend errors, we re-pull on refresh and the row reverts.
  setTasks((prev) =>
    prev.map((t) =>
      t.noteId === task.noteId && t.sourceLine === task.sourceLine
        ? { ...t, completed }
        : t,
    ),
  );
  try {
    await unwrap(
      commands.completeTask(task.notePath, task.sourceLine, completed, task.text.slice(0, 30)),
    );
    // Tell the tab store to reload from disk in case the file is open.
    // Dynamic import avoids a circular module-load (tabs.ts → graph.ts → tasks.ts).
    const { tabStore } = await import("./tabs");
    await tabStore.reloadTabFromDisk(task.notePath);
    await refresh();
  } catch (e) {
    log.error("tasks", "complete_task failed", e);
    setError(String(e));
    await refresh();
  }
}

/// Bucket every task by due-date relative to today's local date.
/// Date math is intentionally coarse — we operate on YYYY-MM-DD strings
/// and let lexicographic compares do the work. Times within a date are
/// ignored for bucketing (a 23:00 task on today's date is still "today").
function bucketize(all: TaskRow[]): TaskGroups {
  const today = isoDateLocal(new Date());
  const weekEnd = isoDateLocal(addDays(new Date(), 6));

  const groups: TaskGroups = {
    overdue: [],
    today: [],
    thisWeek: [],
    later: [],
    noDate: [],
    done: [],
  };

  for (const t of all) {
    if (t.completed) {
      groups.done.push(t);
      continue;
    }
    if (!t.dueAt) {
      groups.noDate.push(t);
      continue;
    }
    const dateOnly = t.dueAt.slice(0, 10);
    if (dateOnly < today) {
      groups.overdue.push(t);
    } else if (dateOnly === today) {
      groups.today.push(t);
    } else if (dateOnly <= weekEnd) {
      groups.thisWeek.push(t);
    } else {
      groups.later.push(t);
    }
  }

  return groups;
}

function isoDateLocal(d: Date): string {
  const y = d.getFullYear();
  const m = String(d.getMonth() + 1).padStart(2, "0");
  const day = String(d.getDate()).padStart(2, "0");
  return `${y}-${m}-${day}`;
}

function addDays(d: Date, n: number): Date {
  const c = new Date(d);
  c.setDate(c.getDate() + n);
  return c;
}

/// Memoized so the IconRail badge — which reads `totals().overdue +
/// totals().today` — doesn't re-bucketize the entire task list on every
/// render. The non-memoized version was an O(n) hot path on Android.
const groups = createMemo(() => bucketize(tasks()));

const totals = createMemo(() => {
  const g = groups();
  return {
    overdue: g.overdue.length,
    today: g.today.length,
    thisWeek: g.thisWeek.length,
    later: g.later.length,
    noDate: g.noDate.length,
    done: g.done.length,
    open: g.overdue.length + g.today.length + g.thisWeek.length + g.later.length + g.noDate.length,
  };
});

/// Quick-add: append a `- [ ] {text}` line to today's daily note and
/// re-pull rows. The daily note folder is read from roost settings
/// (`tasks.daily_note_folder`, default `Daily Notes` per Locked Decision
/// #15). `create_daily_note` is idempotent — it returns today's path
/// whether or not the file existed before.
///
/// Notification permission is requested lazily here when the quick-add
/// text has a ⏰ time marker — the earliest possible prompt, before the
/// task is even written. Editor- and sync-created timed tasks are caught
/// separately by `maybeRequestNotifPermissionForTimedTasks` on the next
/// task-list load. Users who never schedule timed tasks never see the
/// permission dialog.
async function addTaskToToday(text: string): Promise<void> {
  const trimmed = text.trim();
  if (!trimmed) return;
  setError(null);

  if (trimmed.includes("⏰")) {
    await ensureNotificationPermission();
  }

  try {
    const { roostStore } = await import("./roost");
    const { roostSettingsStore } = await import("./roostSettings");
    const roostPath = roostStore.roostPath();
    if (!roostPath) {
      throw new Error("No Roost is open.");
    }
    const folder = roostSettingsStore.settings().tasks.daily_note_folder;
    const notePath = await unwrap(commands.createDailyNote(folder));
    await unwrap(commands.addTaskLine(notePath, trimmed));
    // Refresh the file tree so a brand-new daily note appears in the
    // sidebar. Use `refreshFiles` (cheap re-read) instead of `openRoost`
    // — the latter resets `startupStore.phase` to "roost" and on a
    // same-path re-open the post-open effect doesn't refire to clear it,
    // leaving the TopBar spinner stuck on "Opening roost…" forever.
    // Force past the focus-event debounce — we just created a file and
    // need it visible immediately, not 2s later.
    await roostStore.refreshFiles({ force: true });
    // Reload the in-memory tab if the daily note is open.
    const { tabStore } = await import("./tabs");
    await tabStore.reloadTabFromDisk(notePath);
    await refresh();
  } catch (e) {
    log.error("tasks", "addTaskToToday failed", e);
    setError(String(e));
    await refresh();
  }
}

return {
  tasks,
  groups,
  totals,
  loaded,
  error,
  refresh,
  applyTasksFromBackend,
  completeTask,
  addTaskToToday,
  clear,
};
}

/// Wrap in `createRoot` so the memos and signals get a stable owner that
/// survives module re-evaluation under HMR — matches the convention used
/// by every other store (roost, graph, tabs, etc.).
export const tasksStore = createRoot(createTasksStore);
