import { For, Show, createMemo, createSignal, onMount } from "solid-js";
import { commands, unwrap } from "../../lib/invoke";
import { roostStore, type FileEntry } from "../../stores/roost";
import { roostSettingsStore } from "../../stores/roostSettings";
import { tasksStore } from "../../stores/tasks";
import { log } from "../../lib/log";

/**
 * Settings → Tasks.
 *
 * Excluded folders (tasks-noise redesign, Layer 2): checkboxes inside these
 * folders are treated as document structure, not to-dos — they're dropped
 * from the Tasks pane, the notification scheduler, and the AI agent. The
 * setting is device-local (graph DB) and applies on the next query, no
 * re-index. For a single note, prefer the `tasks: off` frontmatter property.
 */
export default function TasksSection() {
  const [excluded, setExcluded] = createSignal<string[]>([]);
  const [loaded, setLoaded] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);

  onMount(async () => {
    try {
      if (!roostStore.roostPath()) {
        // No Roost → nothing to read; the picker shows its own hint.
        return;
      }
      // The excluded-folders list lives in the graph DB, which is
      // lazy-initialized. Wake it first (same as tasksStore.refresh) so
      // opening Settings → Tasks before the Tasks pane doesn't error.
      const { graphStore } = await import("../../stores/graph");
      await graphStore.ensureGraphReady();
      setExcluded(await unwrap(commands.getExcludedTaskFolders()));
    } catch (e) {
      log.error("tasks-settings", "load excluded folders failed", e);
      setError(String(e));
    } finally {
      setLoaded(true);
    }
  });

  // Folders available to add: every roost folder not already excluded
  // (case-insensitive). Reactive on both the roost tree and the stored list.
  const available = createMemo(() => {
    const vp = roostStore.roostPath();
    if (!vp) return [] as string[];
    const have = new Set(excluded().map((f) => f.toLowerCase()));
    return collectFolders(roostStore.files(), vp).filter(
      (f) => !have.has(f.toLowerCase()),
    );
  });

  // DN-2: every roost folder (unlike `available()`, which drops the
  // already-excluded ones) — the datalist for the daily-notes picker.
  const allFolders = createMemo(() => {
    const vp = roostStore.roostPath();
    if (!vp) return [] as string[];
    return collectFolders(roostStore.files(), vp);
  });

  async function persistDailyFolder(value: string) {
    const folder = value.trim().replace(/^\/+|\/+$/g, "");
    if (!folder) return; // keep the existing value rather than blanking it
    await roostSettingsStore.save({ tasks: { daily_note_folder: folder } });
  }

  async function persist(next: string[]) {
    setError(null);
    const prev = excluded();
    setExcluded(next); // optimistic
    try {
      await unwrap(commands.setExcludedTaskFolders(next));
      // Pane + agent + scheduler read the new set on their next query.
      await tasksStore.refresh();
    } catch (e) {
      log.error("tasks-settings", "save excluded folders failed", e);
      setError(String(e));
      setExcluded(prev); // revert the optimistic change
    }
  }

  function addFolder(folder: string) {
    const f = folder.trim();
    if (!f || excluded().some((x) => x.toLowerCase() === f.toLowerCase())) return;
    void persist([...excluded(), f]);
  }

  function removeFolder(folder: string) {
    void persist(excluded().filter((f) => f !== folder));
  }

  return (
    <section>
      <h3 class="text-text-secondary text-xs font-semibold uppercase tracking-wider mb-3">
        Tasks
      </h3>

      <div class="space-y-3">
        {/* DN-2: daily-notes folder picker. The setting already existed
            (roostSettings.tasks.daily_note_folder) but was only editable by
            hand-editing settings.json — this surfaces it. Free text (the
            folder may not exist yet) with a datalist of existing folders for
            convenience. Honors Locked Decision #15: default, never hardcoded. */}
        <div>
          <div class="text-text-primary text-sm">Daily notes folder</div>
          <div class="text-text-muted text-xs leading-snug mb-1.5">
            Where today's daily note is created, and where quick-add writes its{" "}
            <code class="px-1 py-0 bg-bg-secondary rounded text-[10px]">- [ ]</code>{" "}
            line. Relative to the roost root.
          </div>
          <Show
            when={roostStore.roostPath()}
            fallback={
              <div class="text-text-muted text-xs italic">
                Open a roost to set this.
              </div>
            }
          >
            <input
              type="text"
              list="daily-note-folder-options"
              value={roostSettingsStore.settings().tasks.daily_note_folder}
              placeholder="Daily Notes"
              onChange={(e) => void persistDailyFolder(e.currentTarget.value)}
              class="w-full bg-bg-secondary border border-border text-text-primary rounded-md text-xs px-2 py-1.5 focus:outline-none focus:border-accent/60"
            />
            <datalist id="daily-note-folder-options">
              <For each={allFolders()}>
                {(folder) => <option value={folder} />}
              </For>
            </datalist>
          </Show>
        </div>

        <div>
          <div class="text-text-primary text-sm">Excluded folders</div>
          <div class="text-text-muted text-xs leading-snug">
            Checkboxes inside these folders won't show in the Tasks pane, fire
            reminders, or reach the AI agent — for templates, reference docs,
            and archives. To silence a single note instead, add{" "}
            <code class="px-1 py-0 bg-bg-secondary rounded text-[10px]">
              tasks: off
            </code>{" "}
            to its properties.
          </div>
        </div>

        <Show when={error()}>
          <div class="text-xs text-red-400 bg-red-500/5 border border-red-500/30 rounded-md px-3 py-2">
            {error()}
          </div>
        </Show>

        <Show
          when={loaded()}
          fallback={<div class="text-text-muted text-xs italic">Loading…</div>}
        >
          <Show
            when={excluded().length > 0}
            fallback={
              <div class="text-text-muted text-xs italic">
                No folders excluded — every checkbox in the roost is a task.
              </div>
            }
          >
            <ul class="flex flex-col gap-1.5">
              <For each={excluded()}>
                {(folder) => (
                  <li class="flex items-center justify-between gap-2 bg-bg-secondary border border-border rounded-md px-3 py-1.5">
                    <span class="text-text-primary text-xs truncate" title={folder}>
                      {folder}
                    </span>
                    <button
                      onClick={() => removeFolder(folder)}
                      class="shrink-0 text-text-muted hover:text-red-400 transition-colors"
                      aria-label={`Stop excluding ${folder}`}
                      title="Remove"
                    >
                      <svg
                        width="14"
                        height="14"
                        viewBox="0 0 24 24"
                        fill="none"
                        stroke="currentColor"
                        stroke-width="2"
                      >
                        <path d="M18 6L6 18M6 6l12 12" />
                      </svg>
                    </button>
                  </li>
                )}
              </For>
            </ul>
          </Show>

          <Show
            when={roostStore.roostPath()}
            fallback={
              <div class="text-text-muted text-xs italic">
                Open a roost to choose folders.
              </div>
            }
          >
            <Show
              when={available().length > 0}
              fallback={
                <div class="text-text-muted text-xs italic">
                  No more folders to exclude.
                </div>
              }
            >
              {/* Picking a folder adds it immediately; the select resets to
                  the placeholder via its bound empty value. */}
              <select
                value=""
                onChange={(e) => {
                  addFolder(e.currentTarget.value);
                  e.currentTarget.value = "";
                }}
                class="w-full bg-bg-secondary border border-border text-text-primary rounded-md text-xs px-2 py-1.5 focus:outline-none focus:border-accent/60"
              >
                <option value="">Add a folder…</option>
                <For each={available()}>
                  {(folder) => <option value={folder}>{folder}</option>}
                </For>
              </select>
            </Show>
          </Show>
        </Show>
      </div>
    </section>
  );
}

/// Flatten the roost tree into roost-relative folder paths (forward
/// slashes), skipping hidden technical folders (`.corvux`, `.attachments`,
/// `.corvux-trash`) the user would never file tasks under. Sorted
/// case-insensitively for a stable picker.
function collectFolders(entries: FileEntry[], roostPath: string): string[] {
  const prefix = roostPath.replace(/\\/g, "/").replace(/\/+$/, "") + "/";
  const out: string[] = [];
  const walk = (es: FileEntry[]) => {
    for (const e of es) {
      if (!e.is_dir) continue;
      const abs = e.path.replace(/\\/g, "/");
      const rel = abs.startsWith(prefix) ? abs.slice(prefix.length) : e.name;
      if (rel && !rel.split("/").some((seg) => seg.startsWith("."))) {
        out.push(rel);
      }
      walk(e.children);
    }
  };
  walk(entries);
  return out.sort((a, b) => a.localeCompare(b, undefined, { sensitivity: "base" }));
}
