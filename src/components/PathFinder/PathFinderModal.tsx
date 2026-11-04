import { createSignal, createMemo, For, Show, onMount, onCleanup } from "solid-js";
import { roostStore, type FileEntry } from "../../stores/roost";
import { tabStore } from "../../stores/tabs";
import { graphStore } from "../../stores/graph";
import { commands, unwrap } from "../../lib/invoke";
import { log } from "../../lib/log";

interface PathFinderModalProps {
  onClose: () => void;
}

interface NoteRow {
  name: string;
  path: string;
}

interface NoteIndexEntry {
  id: string;
  title: string;
  path: string;
}

// Local view: tauri-specta emits `cost: number | null` (f64 → null on NaN/Inf);
// we coalesce to a non-null number at the call site for display.
interface ShortestPathBetweenResult {
  cost: number;
  path: NoteIndexEntry[];
  elapsed_ms: number;
}

type Phase = "pickFrom" | "pickTo" | "running" | "result" | "noPath" | "error";

function flattenFiles(entries: FileEntry[]): NoteRow[] {
  const result: NoteRow[] = [];
  function walk(items: FileEntry[]) {
    for (const e of items) {
      if (e.is_dir) walk(e.children);
      else result.push({ name: e.name, path: e.path });
    }
  }
  walk(entries);
  return result;
}

function fuzzyScore(needle: string, haystack: string): { matches: boolean; score: number } {
  const lower = haystack.toLowerCase();
  const search = needle.toLowerCase();
  if (!search) return { matches: true, score: 0 };
  if (lower.startsWith(search)) return { matches: true, score: 100 };
  if (lower.includes(search)) return { matches: true, score: 50 };
  let si = 0;
  let score = 0;
  let last = -1;
  for (let i = 0; i < lower.length && si < search.length; i++) {
    if (lower[i] === search[si]) {
      score += 1;
      if (last === i - 1) score += 2;
      last = i;
      si++;
    }
  }
  return si === search.length ? { matches: true, score } : { matches: false, score: 0 };
}

export default function PathFinderModal(props: PathFinderModalProps) {
  const [phase, setPhase] = createSignal<Phase>("pickFrom");
  const [fromNote, setFromNote] = createSignal<NoteRow | null>(null);
  const [toNote, setToNote] = createSignal<NoteRow | null>(null);
  const [query, setQuery] = createSignal("");
  const [selectedIndex, setSelectedIndex] = createSignal(0);
  const [result, setResult] = createSignal<ShortestPathBetweenResult | null>(null);
  const [errorMsg, setErrorMsg] = createSignal<string | null>(null);
  let inputRef!: HTMLInputElement;

  // Pre-fill "from" with the active tab — most common case is "from where I
  // am right now to somewhere else." Saves the user one fuzzy search.
  onMount(() => {
    const active = tabStore.activeTabPath();
    if (active) {
      const all = flattenFiles(roostStore.files());
      const match = all.find((f) => f.path === active);
      if (match) {
        setFromNote(match);
        setPhase("pickTo");
      }
    }
    requestAnimationFrame(() => inputRef?.focus());
  });

  const allFiles = createMemo(() => flattenFiles(roostStore.files()));

  const filtered = createMemo(() => {
    const q = query();
    const files = allFiles();
    const exclude =
      phase() === "pickTo" ? fromNote()?.path : phase() === "pickFrom" ? toNote()?.path : null;

    const scored = files
      .filter((f) => f.path !== exclude)
      .map((f) => {
        const stem = f.name.replace(/\.md$/, "");
        const m = fuzzyScore(q, stem);
        return { ...f, score: m.score, matches: m.matches };
      });
    return scored.filter((f) => f.matches).sort((a, b) => b.score - a.score).slice(0, 20);
  });

  function relativePath(p: string): string {
    const v = roostStore.roostPath();
    if (!v) return p;
    return p.replace(v + "/", "").replace(v + "\\", "");
  }

  async function runQuery(from: NoteRow, to: NoteRow) {
    setPhase("running");
    setErrorMsg(null);
    try {
      const r = await unwrap(commands.graphShortestPathBetween(from.path, to.path));
      if (!r || r.path.length === 0) {
        setPhase("noPath");
        setResult(null);
        return;
      }
      setResult({ ...r, cost: r.cost ?? 0 });
      setPhase("result");
    } catch (e) {
      const msg = typeof e === "string" ? e : (e as Error)?.message ?? "Unknown error";
      log.error("path-finder", "graph_shortest_path_between failed", msg);
      setErrorMsg(msg);
      setPhase("error");
    }
  }

  function pickCurrent() {
    const list = filtered();
    const idx = selectedIndex();
    const choice = list[idx];
    if (!choice) return;
    const row: NoteRow = { name: choice.name, path: choice.path };
    if (phase() === "pickFrom") {
      setFromNote(row);
      setQuery("");
      setSelectedIndex(0);
      setPhase("pickTo");
      requestAnimationFrame(() => inputRef?.focus());
    } else if (phase() === "pickTo") {
      setToNote(row);
      const from = fromNote();
      if (from) void runQuery(from, row);
    }
  }

  function reset() {
    setPhase("pickFrom");
    setFromNote(null);
    setToNote(null);
    setResult(null);
    setErrorMsg(null);
    setQuery("");
    setSelectedIndex(0);
    requestAnimationFrame(() => inputRef?.focus());
  }

  function handleKey(e: KeyboardEvent) {
    if (e.key === "Escape") {
      e.preventDefault();
      props.onClose();
      return;
    }
    if (phase() === "pickFrom" || phase() === "pickTo") {
      switch (e.key) {
        case "ArrowDown":
          e.preventDefault();
          setSelectedIndex((i) => Math.min(i + 1, filtered().length - 1));
          break;
        case "ArrowUp":
          e.preventDefault();
          setSelectedIndex((i) => Math.max(i - 1, 0));
          break;
        case "Enter":
          e.preventDefault();
          pickCurrent();
          break;
      }
    }
  }

  // Document-level esc capture so the modal closes even when result links
  // are focused (no input to listen on).
  onMount(() => document.addEventListener("keydown", handleKey));
  onCleanup(() => document.removeEventListener("keydown", handleKey));

  function openNote(path: string) {
    void tabStore.openTab(path);
    props.onClose();
  }

  // Replay the found path in the graph view: full mode (the path may span
  // the whole roost, so a local window could miss nodes) + the path
  // pre-highlighted. Plumbing already exists on the graph store.
  function showInGraph() {
    const r = result();
    if (!r || r.path.length === 0) return;
    graphStore.requestGraphMode("full");
    graphStore.requestHighlightPath(r.path.map((node) => node.id));
    tabStore.openGraphTab();
    props.onClose();
  }

  function handleBackdrop(e: MouseEvent) {
    if (e.target === e.currentTarget) props.onClose();
  }

  const headerLabel = () => {
    switch (phase()) {
      case "pickFrom":
        return "Find path — pick the starting note";
      case "pickTo":
        return "Find path — pick the destination note";
      case "running":
        return "Finding shortest path…";
      case "result":
        return "Shortest path";
      case "noPath":
        return "No path found";
      case "error":
        return "Error";
    }
  };

  return (
    <div
      class="fixed inset-0 z-50 flex items-start justify-center pt-[12vh] bg-black/50"
      onClick={handleBackdrop}
      role="dialog"
      aria-modal="true"
      aria-label="Find path between notes"
    >
      <div class="w-[600px] max-w-[calc(100vw-32px)] bg-bg-secondary rounded-lg border border-border shadow-2xl overflow-hidden">
        {/* Header */}
        <div class="px-4 py-3 border-b border-border flex items-center gap-3">
          <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" class="text-accent shrink-0">
            <circle cx="6" cy="6" r="2.5" />
            <circle cx="18" cy="18" r="2.5" />
            <path d="M8 7l8 10" stroke-dasharray="2 2" />
          </svg>
          <div class="flex-1 text-sm text-text-primary font-medium">{headerLabel()}</div>
          <button
            class="text-text-muted hover:text-text-primary p-1"
            onClick={props.onClose}
            aria-label="Close"
          >
            <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
              <path d="M18 6L6 18M6 6l12 12" />
            </svg>
          </button>
        </div>

        {/* Selected breadcrumbs (the two endpoints chosen so far) */}
        <Show when={fromNote() || toNote()}>
          <div class="px-4 py-2 border-b border-border flex flex-wrap gap-2 text-xs">
            <Show when={fromNote()}>
              <span class="px-2 py-0.5 rounded bg-bg-tertiary text-text-secondary">
                <span class="text-text-muted">From:</span>{" "}
                {fromNote()!.name.replace(/\.md$/, "")}
              </span>
            </Show>
            <Show when={toNote()}>
              <span class="px-2 py-0.5 rounded bg-bg-tertiary text-text-secondary">
                <span class="text-text-muted">To:</span>{" "}
                {toNote()!.name.replace(/\.md$/, "")}
              </span>
            </Show>
          </div>
        </Show>

        {/* Picker phase */}
        <Show when={phase() === "pickFrom" || phase() === "pickTo"}>
          <div class="flex items-center gap-2 px-4 py-3 border-b border-border">
            <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" class="text-text-muted shrink-0">
              <circle cx="11" cy="11" r="8" />
              <path d="M21 21l-4.35-4.35" />
            </svg>
            <input
              ref={inputRef}
              type="text"
              placeholder="Type to search notes..."
              value={query()}
              onInput={(e) => {
                setQuery(e.currentTarget.value);
                setSelectedIndex(0);
              }}
              class="flex-1 bg-transparent text-text-primary text-sm outline-none placeholder:text-text-muted"
            />
          </div>
          <div class="max-h-[280px] overflow-y-auto py-1">
            <Show
              when={filtered().length > 0}
              fallback={
                <div class="px-4 py-8 text-text-muted text-sm text-center">
                  No matching notes
                </div>
              }
            >
              <For each={filtered()}>
                {(file, idx) => {
                  const isSelected = () => idx() === selectedIndex();
                  return (
                    <div
                      class={`flex items-center gap-3 px-4 py-2 cursor-pointer transition-colors ${
                        isSelected()
                          ? "bg-accent/15 text-text-primary"
                          : "text-text-secondary hover:bg-bg-hover"
                      }`}
                      onClick={() => {
                        setSelectedIndex(idx());
                        pickCurrent();
                      }}
                      onMouseEnter={() => setSelectedIndex(idx())}
                    >
                      <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" class="shrink-0 opacity-50">
                        <path d="M14 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8z" />
                        <polyline points="14 2 14 8 20 8" />
                      </svg>
                      <div class="flex-1 min-w-0">
                        <div class="text-sm truncate">{file.name.replace(/\.md$/, "")}</div>
                        <div class="text-xs text-text-muted truncate">{relativePath(file.path)}</div>
                      </div>
                    </div>
                  );
                }}
              </For>
            </Show>
          </div>
        </Show>

        {/* Running phase */}
        <Show when={phase() === "running"}>
          <div class="px-4 py-12 flex items-center justify-center text-text-muted text-sm">
            <svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" class="animate-spin mr-2">
              <path d="M21 12a9 9 0 1 1-6.2-8.5" />
            </svg>
            Searching the graph…
          </div>
        </Show>

        {/* Result phase — clickable breadcrumbs */}
        <Show when={phase() === "result" && result()}>
          <div class="px-4 py-4">
            <div class="flex flex-wrap items-center gap-1.5 text-sm leading-relaxed">
              <For each={result()!.path}>
                {(node, idx) => (
                  <>
                    <Show when={idx() > 0}>
                      <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" class="text-text-muted shrink-0">
                        <path d="M5 12h14M13 6l6 6-6 6" />
                      </svg>
                    </Show>
                    <button
                      class="px-2 py-1 rounded bg-bg-tertiary hover:bg-accent/15 hover:text-accent text-text-primary text-sm transition-colors"
                      onClick={() => openNote(node.path)}
                      title={relativePath(node.path)}
                    >
                      {node.title}
                    </button>
                  </>
                )}
              </For>
            </div>
            <div class="mt-3 flex items-center gap-3 text-xs text-text-muted">
              <span>{result()!.path.length} hop{result()!.path.length === 1 ? "" : "s"}</span>
              <span>·</span>
              <span>{result()!.elapsed_ms} ms</span>
            </div>
          </div>
        </Show>

        {/* No-path phase */}
        <Show when={phase() === "noPath"}>
          <div class="px-4 py-8 text-center text-sm">
            <div class="text-text-secondary mb-1">No path between these notes.</div>
            <div class="text-text-muted text-xs">
              They aren't connected through wiki-links. Add a link or pick different endpoints.
            </div>
          </div>
        </Show>

        {/* Error phase */}
        <Show when={phase() === "error"}>
          <div class="px-4 py-6 text-center text-sm">
            <div class="text-red-400 mb-1">Couldn't run the query.</div>
            <div class="text-text-muted text-xs break-words">{errorMsg()}</div>
          </div>
        </Show>

        {/* Footer */}
        <div class="flex items-center gap-3 px-4 py-2 border-t border-border text-text-muted text-xs">
          <Show when={phase() === "pickFrom" || phase() === "pickTo"}>
            <span>
              <kbd class="px-1 py-0.5 rounded bg-bg-hover border border-border text-[10px]">Enter</kbd> pick
            </span>
            <span>
              <kbd class="px-1 py-0.5 rounded bg-bg-hover border border-border text-[10px]">↑↓</kbd> navigate
            </span>
          </Show>
          <Show when={phase() === "result" || phase() === "noPath" || phase() === "error"}>
            <button class="text-text-secondary hover:text-text-primary" onClick={reset}>
              ← Pick different notes
            </button>
          </Show>
          <Show when={phase() === "result"}>
            <button
              class="flex items-center gap-1 text-accent hover:text-accent-hover"
              onClick={showInGraph}
              title="Show this path in the graph view"
            >
              <svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                <circle cx="5" cy="6" r="2" />
                <circle cx="19" cy="18" r="2" />
                <path d="M6.5 7.5l11 9" />
              </svg>
              Show in graph
            </button>
          </Show>
          <div class="flex-1" />
          <span>
            <kbd class="px-1 py-0.5 rounded bg-bg-hover border border-border text-[10px]">Esc</kbd> close
          </span>
        </div>
      </div>
    </div>
  );
}
