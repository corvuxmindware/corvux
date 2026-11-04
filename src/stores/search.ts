import { createSignal, createRoot } from "solid-js";
import { commands, unwrap } from "../lib/invoke";
import { errMsg } from "../lib/error";
import { log } from "../lib/log";
import { graphStore } from "./graph";
import type { SearchMatch, SearchResult } from "../lib/bindings";

// Re-exported from the generated tauri-specta bindings (source of truth
// in `commands/search.rs`).
export type { SearchMatch, SearchResult };

function createSearchStore() {
  const [results, setResults] = createSignal<SearchResult[]>([]);
  const [query, setQuery] = createSignal("");
  const [isSearching, setIsSearching] = createSignal(false);
  // Distinct from "no results" — a real error talking to the backend.
  // Without this, a failed search showed the same empty-state UI as a
  // legit zero-match query, which lied to the user about what happened.
  const [error, setError] = createSignal<string | null>(null);

  let debounceTimer: ReturnType<typeof setTimeout> | null = null;
  let searchGeneration = 0;

  async function search(roostPath: string, q: string) {
    const generation = ++searchGeneration;
    setQuery(q);

    if (!q.trim()) {
      setResults([]);
      setIsSearching(false);
      setError(null);
      return;
    }

    // Debounce — wait 150ms after the user stops typing
    if (debounceTimer) clearTimeout(debounceTimer);

    return new Promise<void>((resolve) => {
      debounceTimer = setTimeout(async () => {
        setIsSearching(true);
        setError(null);
        try {
          // Lazily initialise the FTS5 search index on first query.
          // On warm start this is <100ms (DB already populated).
          await graphStore.ensureSearchReady();

          const res = await unwrap(commands.searchRoost(roostPath, q));
          // Only update if the query hasn't changed since we started
          if (
            generation === searchGeneration &&
            query() === q
          ) {
            setResults(res);
          }
        } catch (e) {
          // Stale-query guard: if the user kept typing past this attempt,
          // don't write its error over the freshly-running query's state.
          if (
            generation !== searchGeneration ||
            query() !== q
          ) {
            return;
          }
          const msg = errMsg(e);
          log.error("search", "search failed", e);
          setResults([]);
          setError(msg);
        } finally {
          // AUDIT-029: only flip the spinner off if this attempt is still
          // the current query. The prior code flipped unconditionally — a
          // stale attempt's finally would clobber the fresh attempt's
          // `setIsSearching(true)` so the spinner vanished mid-request.
          if (
            generation === searchGeneration &&
            query() === q
          ) {
            setIsSearching(false);
          }
          resolve();
        }
      }, 150);
    });
  }

  function clear() {
    searchGeneration += 1;
    setQuery("");
    setResults([]);
    setIsSearching(false);
    setError(null);
    if (debounceTimer) clearTimeout(debounceTimer);
  }

  /** Total match count across all files */
  function totalMatches(): number {
    return results().reduce((sum, r) => sum + r.matches.length, 0);
  }

  return {
    results,
    query,
    isSearching,
    error,
    search,
    clear,
    totalMatches,
  };
}

export const searchStore = createRoot(createSearchStore);
