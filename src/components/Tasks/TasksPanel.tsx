/// Tasks pane — Phase 2.1 slice 2.
///
/// Read-only view over the `:task` relation. Groups every checkbox in the
/// roost by due-date bucket (Overdue / Today / This week / Later / No date /
/// Done). Click a task → opens its source note. Slice 3 will add the
/// click-✓-to-flip-markdown round-trip; slice 4 adds the quick-add field.

import { Show, For, onMount, createSignal, createMemo, createEffect } from "solid-js";
import { tabStore } from "../../stores/tabs";
import { tasksStore, type TaskRow, type TaskGroups } from "../../stores/tasks";
import { roostStore } from "../../stores/roost";
import { lsGet, lsSet } from "../../lib/storage";
import { isMobile } from "../../lib/platform";
import BottomSheet from "../BottomSheet/BottomSheet";
import { distinctFolders, taskMatchesFilter } from "./taskFilter";
import { composeTaskBody, todayLocalIso } from "./taskCompose";

/// Above this many undated tasks, surface the "exclude folders" nudge —
/// the "No date" bucket is where doc/template/archive checkboxes pile up.
const NUDGE_THRESHOLD = 50;
/// Persisted across restarts so a deliberate filter survives (persist-
/// view-state rule). Both the text query and folder scope ride in here.
const FILTER_KEY = "corvux:tasks:filter";

interface TasksPanelProps {
  /// Open the source note for a task and scroll/highlight a specific line.
  /// `line <= 0` (frontmatter tasks) opens the note without scrolling.
  onOpenAtLine: (path: string, line: number) => void | Promise<void>;
  /// Open Settings (the "too many undated tasks" nudge links to Tasks
  /// settings). Optional — the link only renders when provided.
  onOpenSettings?: () => void;
}

export default function TasksPanel(props: TasksPanelProps) {
  const [quickAddText, setQuickAddText] = createSignal("");
  const [quickAddDate, setQuickAddDate] = createSignal("");
  const [quickAddTime, setQuickAddTime] = createSignal("");
  const [adding, setAdding] = createSignal(false);

  // Mobile-only: the inline quick-add is too cramped in the narrow panel,
  // so mobile collapses it to a single "Add task" button that opens a
  // centered modal holding name + date + time + Confirm. Desktop keeps the
  // compact inline row (it's fine with a mouse). The modal binds straight
  // to the quick-add signals; closing without confirming resets them.
  const [showAddSheet, setShowAddSheet] = createSignal(false);

  // ── Filter row (display-only narrowing of the loaded list) ──────────
  const initialFilter = (() => {
    try {
      const raw = lsGet(FILTER_KEY);
      if (raw) {
        const p = JSON.parse(raw) as { query?: string; scope?: string };
        return { query: p.query ?? "", scope: p.scope ?? "" };
      }
    } catch {
      /* malformed — fall through to empty */
    }
    return { query: "", scope: "" };
  })();
  const [query, setQuery] = createSignal(initialFilter.query);
  const [scope, setScope] = createSignal(initialFilter.scope);

  // Persist the manual view choice (text + folder scope) across restarts.
  createEffect(() => {
    lsSet(FILTER_KEY, JSON.stringify({ query: query(), scope: scope() }));
  });

  const filterActive = createMemo(
    () => query().trim() !== "" || scope().trim() !== "",
  );

  // Folder-scope options derived from the loaded rows. Keep the persisted
  // scope visible even if no current task lives there (else the <select>
  // would silently show "All folders" while still filtering everything).
  const scopeOptions = createMemo(() => {
    const opts = distinctFolders(tasksStore.tasks(), roostStore.roostPath() ?? "");
    const cur = scope();
    if (cur && !opts.some((o) => o.toLowerCase() === cur.toLowerCase())) {
      return [cur, ...opts];
    }
    return opts;
  });

  // Apply the filter to each already-bucketized group. When inactive,
  // pass the store's groups straight through (no array churn).
  const filteredGroups = createMemo<TaskGroups>(() => {
    const g = tasksStore.groups();
    if (!filterActive()) return g;
    const f = { query: query(), scope: scope() };
    const vp = roostStore.roostPath() ?? "";
    const keep = (t: TaskRow) => taskMatchesFilter(t, f, vp);
    return {
      overdue: g.overdue.filter(keep),
      today: g.today.filter(keep),
      thisWeek: g.thisWeek.filter(keep),
      later: g.later.filter(keep),
      noDate: g.noDate.filter(keep),
      done: g.done.filter(keep),
    };
  });

  const filteredCount = createMemo(() => {
    const g = filteredGroups();
    return (
      g.overdue.length +
      g.today.length +
      g.thisWeek.length +
      g.later.length +
      g.noDate.length +
      g.done.length
    );
  });

  function clearFilter() {
    setQuery("");
    setScope("");
  }

  // Pull tasks on first mount. The save path also calls refresh() and
  // init_graph triggers it once on roost open, but a hot-reload or a
  // first-time pane open after the user has been editing for a while
  // would otherwise show stale or loading state. Refresh is cheap.
  onMount(() => {
    void tasksStore.refresh();
  });

  /// Open the mobile "Add task" modal.
  function openAddSheet() {
    setShowAddSheet(true);
  }

  /// Close the modal, discarding anything typed but not confirmed.
  function closeAddSheet() {
    setQuickAddText("");
    setQuickAddDate("");
    setQuickAddTime("");
    setShowAddSheet(false);
  }

  /// Submit from the mobile modal; close it only if a task was actually
  /// added (an empty name leaves the modal open).
  async function confirmAdd() {
    if (await submitQuickAdd()) setShowAddSheet(false);
  }

  /// Compose + add the task. Returns true when something was added (so the
  /// mobile modal knows whether to close). Shared by the desktop inline
  /// form (onSubmit) and the mobile modal.
  async function submitQuickAdd(e?: Event): Promise<boolean> {
    e?.preventDefault();
    const body = composeTaskBody(
      { text: quickAddText(), date: quickAddDate(), time: quickAddTime() },
      todayLocalIso(),
    );
    if (!body || adding()) return false;
    setAdding(true);
    try {
      await tasksStore.addTaskToToday(body);
      setQuickAddText("");
      setQuickAddDate("");
      setQuickAddTime("");
      return true;
    } finally {
      setAdding(false);
    }
  }

  function openSource(t: TaskRow) {
    if (t.sourceLine > 0) {
      void props.onOpenAtLine(t.notePath, t.sourceLine);
    } else {
      // Frontmatter tasks have no body line to land on; just open the note.
      tabStore.openTab(t.notePath);
    }
  }

  function refresh() {
    void tasksStore.refresh();
  }

  return (
    <div class="flex-1 min-h-0 flex flex-col">
      <Header onRefresh={refresh} />
      <Show
        when={isMobile()}
        fallback={
          <form
            onSubmit={submitQuickAdd}
            class="px-3 py-2 border-b border-border shrink-0 space-y-1.5"
          >
            <div class="flex items-center gap-1.5">
              <span class="text-text-muted shrink-0">
                <svg
                  width="14"
                  height="14"
                  viewBox="0 0 24 24"
                  fill="none"
                  stroke="currentColor"
                  stroke-width="2"
                >
                  <line x1="12" y1="5" x2="12" y2="19" />
                  <line x1="5" y1="12" x2="19" y2="12" />
                </svg>
              </span>
              <input
                type="text"
                value={quickAddText()}
                onInput={(e) => setQuickAddText(e.currentTarget.value)}
                placeholder="Add a task…"
                disabled={adding()}
                class="flex-1 min-w-0 bg-transparent text-sm placeholder-text-muted/70 focus:outline-none disabled:opacity-60"
              />
              <Show when={quickAddText().trim().length > 0}>
                <button
                  type="submit"
                  disabled={adding()}
                  class="text-[10px] text-text-muted hover:text-text-primary px-1.5 py-0.5 rounded hover:bg-bg-hover disabled:opacity-60 shrink-0"
                  title="Add to today's daily note"
                >
                  <kbd class="font-mono">Enter</kbd>
                </button>
              </Show>
            </div>
            <div class="flex items-center gap-1.5 pl-[18px]">
              <label
                class="flex items-center gap-1 text-[10px] text-text-muted hover:text-text-secondary"
                title="Due date — written as 📅 YYYY-MM-DD in the markdown"
              >
                <svg
                  width="11"
                  height="11"
                  viewBox="0 0 24 24"
                  fill="none"
                  stroke="currentColor"
                  stroke-width="2"
                >
                  <rect x="3" y="4" width="18" height="18" rx="2" />
                  <line x1="16" y1="2" x2="16" y2="6" />
                  <line x1="8" y1="2" x2="8" y2="6" />
                  <line x1="3" y1="10" x2="21" y2="10" />
                </svg>
                <input
                  type="date"
                  value={quickAddDate()}
                  onInput={(e) => setQuickAddDate(e.currentTarget.value)}
                  disabled={adding()}
                  class="bg-transparent border-none text-[11px] focus:outline-none accent-accent text-text-secondary px-0.5"
                />
              </label>
              <label
                class="flex items-center gap-1 text-[10px] text-text-muted hover:text-text-secondary"
                title="Due time — written as ⏰ HH:MM (requires a date; defaults to today if you only set a time)"
              >
                <svg
                  width="11"
                  height="11"
                  viewBox="0 0 24 24"
                  fill="none"
                  stroke="currentColor"
                  stroke-width="2"
                >
                  <circle cx="12" cy="12" r="10" />
                  <polyline points="12 6 12 12 16 14" />
                </svg>
                <input
                  type="time"
                  value={quickAddTime()}
                  onInput={(e) => setQuickAddTime(e.currentTarget.value)}
                  disabled={adding()}
                  class="bg-transparent border-none text-[11px] focus:outline-none accent-accent text-text-secondary px-0.5"
                />
              </label>
              <Show when={quickAddDate() || quickAddTime()}>
                <button
                  type="button"
                  onClick={() => {
                    setQuickAddDate("");
                    setQuickAddTime("");
                  }}
                  class="ml-auto text-[10px] text-text-muted hover:text-text-secondary"
                  title="Clear date / time"
                >
                  clear
                </button>
              </Show>
            </div>
          </form>
        }
      >
        {/* Mobile: one big button → centered Add-task modal (name+date+time). */}
        <div class="px-3 py-2 border-b border-border shrink-0">
          <button
            type="button"
            onClick={openAddSheet}
            disabled={adding()}
            class="w-full flex items-center justify-center gap-2 py-2.5 rounded-lg border border-border text-sm font-medium text-text-secondary hover:bg-bg-hover active:opacity-90 disabled:opacity-60 transition-colors"
          >
            <svg
              width="16"
              height="16"
              viewBox="0 0 24 24"
              fill="none"
              stroke="currentColor"
              stroke-width="2"
            >
              <line x1="12" y1="5" x2="12" y2="19" />
              <line x1="5" y1="12" x2="19" y2="12" />
            </svg>
            Add task
          </button>
        </div>
      </Show>
      <Show when={showAddSheet()}>
        <BottomSheet
          title="Add task"
          placement="center"
          onClose={closeAddSheet}
          handleAndroidBack
          footer={
            <div class="p-3">
              <button
                type="button"
                onClick={confirmAdd}
                disabled={adding() || quickAddText().trim().length === 0}
                class="w-full py-3 rounded-lg bg-accent text-bg-primary font-medium active:opacity-90 disabled:opacity-50 transition-opacity"
              >
                {adding() ? "Adding…" : "Add task"}
              </button>
            </div>
          }
        >
          <div class="p-4 space-y-4">
            <label class="block space-y-1.5">
              <span class="text-xs font-medium text-text-muted uppercase tracking-wider">
                Task
              </span>
              <input
                type="text"
                value={quickAddText()}
                onInput={(e) => setQuickAddText(e.currentTarget.value)}
                onKeyDown={(e) => {
                  if (e.key === "Enter") {
                    e.preventDefault();
                    void confirmAdd();
                  }
                }}
                placeholder="What needs doing?"
                disabled={adding()}
                class="w-full h-12 px-3 bg-bg-primary border border-border rounded-lg text-base text-text-primary placeholder-text-muted/70 focus:outline-none focus:border-accent/60"
              />
            </label>
            <label class="block space-y-1.5">
              <span class="text-xs font-medium text-text-muted uppercase tracking-wider">
                Date
              </span>
              <input
                type="date"
                value={quickAddDate()}
                onInput={(e) => setQuickAddDate(e.currentTarget.value)}
                disabled={adding()}
                class="w-full h-12 px-3 bg-bg-primary border border-border rounded-lg text-base text-text-primary focus:outline-none focus:border-accent/60 accent-accent"
              />
            </label>
            <label class="block space-y-1.5">
              <span class="text-xs font-medium text-text-muted uppercase tracking-wider">
                Time
              </span>
              <input
                type="time"
                value={quickAddTime()}
                onInput={(e) => setQuickAddTime(e.currentTarget.value)}
                disabled={adding()}
                class="w-full h-12 px-3 bg-bg-primary border border-border rounded-lg text-base text-text-primary focus:outline-none focus:border-accent/60 accent-accent"
              />
            </label>
            <p class="text-[11px] text-text-muted leading-snug">
              Time needs a date — if you set only a time, today's date is used.
            </p>
          </div>
        </BottomSheet>
      </Show>
      <Show when={tasksStore.totals().open + tasksStore.totals().done > 0}>
        <div class="px-3 py-1.5 border-b border-border shrink-0 flex items-center gap-1.5">
          <input
            type="text"
            value={query()}
            onInput={(e) => setQuery(e.currentTarget.value)}
            placeholder="Filter tasks…"
            aria-label="Filter tasks by text"
            class="flex-1 min-w-0 bg-bg-secondary border border-border rounded px-2 py-1 text-xs placeholder-text-muted/70 focus:outline-none focus:border-accent/60"
          />
          <Show when={scopeOptions().length > 0}>
            <select
              value={scope()}
              onChange={(e) => setScope(e.currentTarget.value)}
              aria-label="Filter tasks by folder"
              title="Filter by folder"
              class="shrink-0 max-w-[42%] bg-bg-secondary border border-border rounded px-1.5 py-1 text-xs text-text-secondary focus:outline-none focus:border-accent/60"
            >
              <option value="">All folders</option>
              <For each={scopeOptions()}>
                {(f) => <option value={f}>{f}</option>}
              </For>
            </select>
          </Show>
          <Show when={filterActive()}>
            <button
              type="button"
              onClick={clearFilter}
              class="shrink-0 text-[10px] text-text-muted hover:text-text-secondary px-1"
              title="Clear filter"
            >
              clear
            </button>
          </Show>
        </div>
      </Show>
      <div class="flex-1 overflow-y-auto">
        <Show
          when={tasksStore.loaded()}
          fallback={
            <div class="px-3 py-6 text-xs text-text-muted italic">Loading…</div>
          }
        >
          <Show when={tasksStore.error()}>
            <div class="px-3 py-2 text-xs text-red-400 border-b border-border bg-red-500/5">
              {tasksStore.error()}
            </div>
          </Show>
          <Show
            when={tasksStore.totals().open + tasksStore.totals().done > 0}
            // No EmptyState under an error banner — "No tasks yet, add a
            // checkbox" next to "couldn't load" reads as contradictory.
            fallback={tasksStore.error() ? null : <EmptyState />}
          >
            <Show when={!filterActive() && tasksStore.totals().noDate > NUDGE_THRESHOLD}>
              <NoDateNudge
                count={tasksStore.totals().noDate}
                onOpenSettings={props.onOpenSettings}
              />
            </Show>
            <Show
              when={filteredCount() > 0}
              fallback={<NoMatches onClear={clearFilter} />}
            >
              <Section
                label="Overdue"
                tone="overdue"
                tasks={filteredGroups().overdue}
                onOpen={openSource}
              />
              <Section
                label="Today"
                tone="today"
                tasks={filteredGroups().today}
                onOpen={openSource}
              />
              <Section
                label="This week"
                tone="week"
                tasks={filteredGroups().thisWeek}
                onOpen={openSource}
              />
              <Section
                label="Later"
                tone="later"
                tasks={filteredGroups().later}
                onOpen={openSource}
              />
              <Section
                label="No date"
                tone="muted"
                tasks={filteredGroups().noDate}
                onOpen={openSource}
              />
              <Section
                label="Done"
                tone="done"
                tasks={filteredGroups().done}
                onOpen={openSource}
                collapsedByDefault
              />
            </Show>
          </Show>
        </Show>
      </div>
    </div>
  );
}

function Header(props: { onRefresh: () => void }) {
  const totals = () => tasksStore.totals();
  return (
    <div class="px-3 py-2 border-b border-border flex items-center justify-between shrink-0">
      <div class="flex items-center gap-1.5 text-xs font-medium text-text-muted uppercase tracking-wider">
        <svg
          width="13"
          height="13"
          viewBox="0 0 24 24"
          fill="none"
          stroke="currentColor"
          stroke-width="2"
        >
          <polyline points="9 11 12 14 22 4" />
          <path d="M21 12v7a2 2 0 01-2 2H5a2 2 0 01-2-2V5a2 2 0 012-2h11" />
        </svg>
        <span>Tasks</span>
        <span class="text-text-secondary normal-case">
          ({totals().open} open
          <Show when={totals().done > 0}>, {totals().done} done</Show>)
        </span>
      </div>
      <button
        title="Refresh"
        aria-label="Refresh tasks"
        class="text-text-muted hover:text-text-primary p-1 rounded hover:bg-bg-hover transition-colors"
        onClick={props.onRefresh}
      >
        <svg
          width="14"
          height="14"
          viewBox="0 0 24 24"
          fill="none"
          stroke="currentColor"
          stroke-width="2"
        >
          <polyline points="23 4 23 10 17 10" />
          <path d="M20.49 15A9 9 0 1116 5L23 10" />
        </svg>
      </button>
    </div>
  );
}

function EmptyState() {
  return (
    <div class="px-3 py-6 text-xs text-text-muted space-y-2">
      <p class="italic">No tasks yet.</p>
      <p class="opacity-80 leading-relaxed">
        Add a task by typing a checkbox in any note:
      </p>
      <pre class="font-mono text-[10px] bg-bg-hover px-2 py-1.5 rounded leading-snug whitespace-pre-wrap">
{`- [ ] Reply to Ravi 📅 2026-05-08
- [ ] Stand-up 📅 2026-05-08 ⏰ 10:00`}
      </pre>
      <p class="opacity-70 text-[11px]">
        Save the note → it shows up here.
      </p>
    </div>
  );
}

/// Shown when the "No date" bucket is large — the tell-tale sign of
/// document/template/archive checkboxes leaking in. Points at the two
/// de-noising tools: the per-note `tasks: off` property and the
/// excluded-folders setting.
function NoDateNudge(props: { count: number; onOpenSettings?: () => void }) {
  return (
    <div class="mx-3 my-2 px-3 py-2 rounded-md bg-yellow/5 border border-yellow/30 text-[11px] text-text-secondary leading-snug">
      <span class="font-medium text-text-primary">
        {props.count} undated tasks.
      </span>{" "}
      Planning docs or templates leaking in? Add{" "}
      <code class="px-1 bg-bg-secondary rounded text-[10px]">tasks: off</code> to
      a note, or{" "}
      <Show
        when={props.onOpenSettings}
        fallback={<span>exclude folders in Settings → Tasks.</span>}
      >
        <button
          type="button"
          onClick={() => props.onOpenSettings?.()}
          class="text-accent hover:underline"
        >
          exclude folders in Settings → Tasks.
        </button>
      </Show>
    </div>
  );
}

/// Shown when a filter is active but nothing matches.
function NoMatches(props: { onClear: () => void }) {
  return (
    <div class="px-3 py-6 text-xs text-text-muted space-y-2">
      <p class="italic">No tasks match your filter.</p>
      <button
        type="button"
        onClick={props.onClear}
        class="text-accent hover:underline"
      >
        Clear filter
      </button>
    </div>
  );
}

function Section(props: {
  label: string;
  tone: "overdue" | "today" | "week" | "later" | "muted" | "done";
  tasks: TaskRow[];
  onOpen: (t: TaskRow) => void;
  collapsedByDefault?: boolean;
}) {
  const count = () => props.tasks.length;

  const dotClass = () => {
    switch (props.tone) {
      case "overdue":
        return "bg-red-500";
      case "today":
        return "bg-accent";
      case "week":
        return "bg-yellow";
      case "later":
        return "bg-blue-500";
      case "muted":
        return "bg-text-muted/60";
      case "done":
        return "bg-green-500/70";
    }
  };

  // <Show> keeps the empty-section guard reactive. Solid components run
  // their body exactly once, so an early `if (count() === 0) return null`
  // would lock the section as null at first render — adding a task to a
  // previously-empty bucket wouldn't surface until the whole pane
  // unmounted and remounted (i.e. close + reopen the panel).
  return (
    <Show when={count() > 0}>
      <details open={!props.collapsedByDefault} class="group border-b border-border/60 last:border-b-0">
        <summary class="flex items-center gap-2 px-3 py-2 cursor-pointer select-none hover:bg-bg-hover">
          <span class={`w-1.5 h-1.5 rounded-full ${dotClass()}`} />
          <span class="text-xs font-medium text-text-secondary">{props.label}</span>
          <span class="text-[10px] text-text-muted">({count()})</span>
          <svg
            class="ml-auto w-3 h-3 text-text-muted transition-transform group-open:rotate-90"
            viewBox="0 0 24 24"
            fill="none"
            stroke="currentColor"
            stroke-width="2"
          >
            <polyline points="9 18 15 12 9 6" />
          </svg>
        </summary>
        <div class="pb-1">
          <For each={props.tasks}>
            {(t) => <TaskItem task={t} onOpen={() => props.onOpen(t)} />}
          </For>
        </div>
      </details>
    </Show>
  );
}

function TaskItem(props: { task: TaskRow; onOpen: () => void }) {
  const t = () => props.task;

  function toggleComplete(e: MouseEvent) {
    // Stop the click from bubbling to the row's open-source handler.
    e.stopPropagation();
    // Frontmatter tasks (sourceLine ≤ 0) route through the YAML
    // rewrite path on the backend; the same `completeTask` call works
    // for both forms.
    void tasksStore.completeTask(t(), !t().completed);
  }

  return (
    <div
      class="w-full text-left px-3 py-1.5 hover:bg-bg-hover transition-colors flex items-start gap-2 group/task cursor-pointer"
      onClick={props.onOpen}
      title={`Open ${t().noteTitle}`}
    >
      <button
        type="button"
        onClick={toggleComplete}
        title={t().completed ? "Mark as not done" : "Mark as done"}
        aria-label={t().completed ? "Mark as not done" : "Mark as done"}
        class={`mt-0.5 w-3.5 h-3.5 shrink-0 rounded border transition-colors flex items-center justify-center ${
          t().completed
            ? "bg-green-500/80 border-green-500/80 hover:bg-green-500"
            : "border-text-muted/60 hover:border-accent hover:bg-accent/10"
        }`}
      >
        <Show when={t().completed}>
          <svg
            width="9"
            height="9"
            viewBox="0 0 24 24"
            fill="none"
            stroke="currentColor"
            stroke-width="3"
            class="text-bg-primary"
          >
            <polyline points="20 6 9 17 4 12" />
          </svg>
        </Show>
      </button>
      <span class="flex-1 min-w-0 flex flex-col gap-0.5">
        <span
          class={`text-sm leading-snug truncate ${
            t().completed ? "line-through text-text-muted" : "text-text-primary"
          }`}
        >
          {t().text}
        </span>
        <span class="flex items-center gap-1.5 text-[10px] text-text-muted">
          <Show when={t().dueAt}>
            <span class="inline-flex items-center gap-0.5">
              <svg
                width="9"
                height="9"
                viewBox="0 0 24 24"
                fill="none"
                stroke="currentColor"
                stroke-width="2"
              >
                <rect x="3" y="4" width="18" height="18" rx="2" />
                <line x1="16" y1="2" x2="16" y2="6" />
                <line x1="8" y1="2" x2="8" y2="6" />
                <line x1="3" y1="10" x2="21" y2="10" />
              </svg>
              {t().dueAt}
            </span>
          </Show>
          <Show when={t().linkedProject}>
            <span class="text-accent">#{t().linkedProject}</span>
          </Show>
          <Show when={t().linkedPerson}>
            <span class="text-purple">@{t().linkedPerson}</span>
          </Show>
          <span class="ml-auto truncate opacity-70 group-hover/task:opacity-100">
            {t().noteTitle}
          </span>
        </span>
      </span>
    </div>
  );
}
