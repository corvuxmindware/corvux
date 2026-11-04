import { createSignal, For, Show, onMount, onCleanup, type JSX } from "solid-js";
import { searchStore, type SearchResult } from "../../stores/search";
import { roostStore } from "../../stores/roost";

interface SearchPanelProps {
  onOpenResult: (path: string, line: number) => void;
}

/** Debounce window for the search-input → backend round-trip. Search is
 *  FTS5 (BM25) over SQLite; without debounce a 5-character typing burst
 *  fires 5 queries. 200 ms is the standard "feels instant after you stop
 *  typing" window — long enough to coalesce a burst, short enough that the
 *  user doesn't perceive it as lag. */
const SEARCH_DEBOUNCE_MS = 200;

export default function SearchPanel(props: SearchPanelProps) {
  const [inputValue, setInputValue] = createSignal("");
  let inputRef!: HTMLInputElement;
  let debounceTimer: number | undefined;

  onMount(() => {
    inputRef.focus();
  });

  onCleanup(() => {
    if (debounceTimer !== undefined) clearTimeout(debounceTimer);
  });

  function runSearchNow(value: string) {
    const roost = roostStore.roostPath();
    if (roost) searchStore.search(roost, value);
  }

  function handleInput(value: string) {
    setInputValue(value);
    if (debounceTimer !== undefined) clearTimeout(debounceTimer);
    if (!value) {
      // Empty input — clear immediately, don't wait the debounce window.
      searchStore.clear();
      return;
    }
    debounceTimer = window.setTimeout(() => runSearchNow(value), SEARCH_DEBOUNCE_MS);
  }

  function handleClear() {
    if (debounceTimer !== undefined) clearTimeout(debounceTimer);
    setInputValue("");
    searchStore.clear();
    inputRef.focus();
  }

  function relativePath(path: string): string {
    const roost = roostStore.roostPath();
    if (!roost) return path;
    return path.replace(roost + "/", "");
  }

  /** Highlight matching text in a line */
  function highlightMatch(text: string, query: string) {
    if (!query) return text;
    const lower = text.toLowerCase();
    const qLower = query.toLowerCase();
    const idx = lower.indexOf(qLower);
    if (idx === -1) return text;
    const before = text.slice(0, idx);
    const match = text.slice(idx, idx + query.length);
    const after = text.slice(idx + query.length);
    return (
      <>
        {before}
        <span class="bg-accent/30 text-accent rounded-sm px-0.5">{match}</span>
        {after}
      </>
    );
  }

  return (
    <div class="flex flex-col h-full select-none">
      {/* Header */}
      <div class="h-10 flex items-center px-3 border-b border-border shrink-0">
        <span class="text-text-secondary font-medium text-sm tracking-wide">SEARCH</span>
      </div>

      {/* Mode selector removed — search is full-text (FTS5) only now that
          the embeddings/semantic lane is gone. No lane to pick between. */}

      {/* Search input */}
      <div class="p-2">
        <div class="flex items-center gap-2 px-2 py-1.5 rounded-md bg-bg-tertiary border border-border focus-within:border-accent transition-colors">
          <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" class="text-text-muted shrink-0">
            <circle cx="11" cy="11" r="8" />
            <path d="M21 21l-4.35-4.35" />
          </svg>
          <input
            ref={inputRef}
            type="text"
            placeholder="Search in Roost..."
            value={inputValue()}
            onInput={(e) => handleInput(e.currentTarget.value)}
            class="flex-1 bg-transparent text-text-primary text-xs outline-none placeholder:text-text-muted min-w-0"
          />
          <Show when={inputValue()}>
            <button
              class="text-text-muted hover:text-text-primary p-0.5 shrink-0"
              onClick={handleClear}
            >
              <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
                <path d="M18 6L6 18M6 6l12 12" />
              </svg>
            </button>
          </Show>
        </div>
      </div>

      {/* Results */}
      <div class="flex-1 overflow-y-auto px-1">
        {/* Initial empty state — before any search */}
        <Show when={!inputValue() && !searchStore.isSearching()}>
          <div class="px-4 py-8 text-center space-y-3">
            <svg width="28" height="28" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" class="mx-auto text-text-muted opacity-40">
              <circle cx="11" cy="11" r="8" />
              <path d="M21 21l-4.35-4.35" />
            </svg>
            <p class="text-text-muted text-xs">Search across all notes in your roost</p>
            <div class="text-text-muted text-[10px] opacity-50 space-y-0.5">
              <p>Results update as you type</p>
              <p>Click a result to jump to the exact line</p>
            </div>
          </div>
        </Show>

        <Show when={searchStore.isSearching()}>
          <div class="px-3 py-4 text-text-muted text-xs text-center">
            Searching...
          </div>
        </Show>

        <Show when={!searchStore.isSearching() && searchStore.error()}>
          {(err) => (
            <div class="px-4 py-6 mx-2 my-3 rounded border border-red/30 bg-red/5 space-y-2">
              <div class="flex items-center gap-2 text-red text-xs font-medium">
                <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
                  <circle cx="12" cy="12" r="10" />
                  <path d="M12 8v4M12 16h.01" />
                </svg>
                Search failed
              </div>
              <p class="text-text-muted text-[11px] break-words font-mono">{err()}</p>
              <button
                class="text-[11px] text-accent hover:underline"
                onClick={() => {
                  const v = roostStore.roostPath();
                  if (v) searchStore.search(v, inputValue());
                }}
              >
                Try again
              </button>
            </div>
          )}
        </Show>

        <Show when={!searchStore.isSearching() && !searchStore.error() && inputValue() && searchStore.results().length === 0}>
          <div class="px-4 py-8 text-center space-y-2">
            <svg width="24" height="24" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" class="mx-auto text-text-muted opacity-40">
              <circle cx="11" cy="11" r="8" />
              <path d="M21 21l-4.35-4.35" />
              <path d="M8 11h6" />
            </svg>
            <p class="text-text-muted text-xs">No results for "{inputValue()}"</p>
            <p class="text-text-muted text-[10px] opacity-50">Try a different search term</p>
          </div>
        </Show>

        <Show when={!searchStore.isSearching() && searchStore.results().length > 0}>
          {/* Summary */}
          <div class="px-2 py-1 text-text-muted text-xs">
            {searchStore.totalMatches()} matches in {searchStore.results().length} files
          </div>

          <For each={searchStore.results()}>
            {(result) => (
              <SearchResultGroup
                result={result}
                query={inputValue()}
                relativePath={relativePath}
                highlightMatch={highlightMatch}
                onOpenResult={props.onOpenResult}
              />
            )}
          </For>
        </Show>
      </div>
    </div>
  );
}

function SearchResultGroup(props: {
  result: SearchResult;
  query: string;
  relativePath: (path: string) => string;
  highlightMatch: (text: string, query: string) => JSX.Element;
  onOpenResult: (path: string, line: number) => void;
}) {
  const [expanded, setExpanded] = createSignal(true);

  return (
    <div class="mb-1">
      {/* File header */}
      <button
        class="w-full flex items-center gap-1.5 px-2 py-1.5 rounded text-left hover:bg-bg-hover transition-colors group"
        onClick={() => setExpanded(!expanded())}
      >
        <svg
          width="10"
          height="10"
          viewBox="0 0 24 24"
          fill="none"
          stroke="currentColor"
          stroke-width="2"
          class="shrink-0 text-text-muted transition-transform"
          style={{ transform: expanded() ? "rotate(90deg)" : "rotate(0deg)" }}
        >
          <path d="M9 18l6-6-6-6" />
        </svg>
        <svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" class="shrink-0 text-text-muted">
          <path d="M14 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8z" />
          <polyline points="14 2 14 8 20 8" />
        </svg>
        <span class="text-xs text-text-secondary truncate flex-1">
          {props.relativePath(props.result.path)}
        </span>
        <span class="text-xs text-text-muted shrink-0">
          {props.result.matches.length}
        </span>
      </button>

      {/* Match lines */}
      <Show when={expanded()}>
        <div class="ml-4">
          <For each={props.result.matches.slice(0, 10)}>
            {(match) => (
              <button
                class="w-full text-left px-2 py-1 rounded text-xs text-text-secondary hover:bg-bg-hover hover:text-text-primary transition-colors flex items-start gap-2 group"
                onClick={() => props.onOpenResult(props.result.path, match.line)}
              >
                <span class="text-text-muted shrink-0 w-6 text-right tabular-nums">
                  {match.line}
                </span>
                <span class="truncate min-w-0">
                  {props.highlightMatch(match.text, props.query)}
                </span>
              </button>
            )}
          </For>
          <Show when={props.result.matches.length > 10}>
            <div class="px-2 py-1 text-xs text-text-muted italic">
              +{props.result.matches.length - 10} more matches
            </div>
          </Show>
        </div>
      </Show>
    </div>
  );
}
