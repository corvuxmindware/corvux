import { createSignal, createMemo, For, Show, onMount, onCleanup } from "solid-js";
import { roostStore, type FileEntry } from "../../stores/roost";
import { tabStore } from "../../stores/tabs";

interface QuickSwitcherProps {
  onClose: () => void;
}

export default function QuickSwitcher(props: QuickSwitcherProps) {
  const [query, setQuery] = createSignal("");
  const [selectedIndex, setSelectedIndex] = createSignal(0);
  let inputRef!: HTMLInputElement;

  onMount(() => {
    inputRef.focus();
  });

  // Flatten file tree to a list of paths
  function flattenFiles(entries: FileEntry[]): { name: string; path: string }[] {
    const result: { name: string; path: string }[] = [];
    function walk(items: FileEntry[]) {
      for (const entry of items) {
        if (entry.is_dir) {
          walk(entry.children);
        } else {
          result.push({ name: entry.name, path: entry.path });
        }
      }
    }
    walk(entries);
    return result;
  }

  const allFiles = createMemo(() => flattenFiles(roostStore.files()));

  // Fuzzy match — simple substring with scoring
  function fuzzyMatch(
    needle: string,
    haystack: string
  ): { matches: boolean; score: number } {
    const lower = haystack.toLowerCase();
    const search = needle.toLowerCase();
    if (!search) return { matches: true, score: 0 };

    // Exact match at start
    if (lower.startsWith(search)) return { matches: true, score: 100 };
    // Exact match anywhere
    if (lower.includes(search)) return { matches: true, score: 50 };

    // Fuzzy: all characters in order
    let si = 0;
    let score = 0;
    let lastMatchIdx = -1;
    for (let i = 0; i < lower.length && si < search.length; i++) {
      if (lower[i] === search[si]) {
        score += 1;
        // Bonus for consecutive matches
        if (lastMatchIdx === i - 1) score += 2;
        lastMatchIdx = i;
        si++;
      }
    }

    if (si === search.length) return { matches: true, score };
    return { matches: false, score: 0 };
  }

  const filteredFiles = createMemo(() => {
    const q = query();
    const files = allFiles();
    const recent = tabStore.recentFiles();

    // Score all files
    const scored = files.map((f) => {
      const nameWithoutExt = f.name.replace(/\.md$/, "");
      const match = fuzzyMatch(q, nameWithoutExt);
      const recentIdx = recent.indexOf(f.path);
      // Recent files get a boost
      const recentBoost = recentIdx >= 0 ? 200 - recentIdx : 0;
      return {
        ...f,
        score: match.score + recentBoost,
        matches: match.matches,
      };
    });

    return scored
      .filter((f) => f.matches)
      .sort((a, b) => b.score - a.score)
      .slice(0, 20);
  });

  function openSelected() {
    const files = filteredFiles();
    const idx = selectedIndex();
    if (files[idx]) {
      tabStore.openTab(files[idx].path);
      props.onClose();
    }
  }

  function handleKeyDown(e: KeyboardEvent) {
    switch (e.key) {
      case "ArrowDown":
        e.preventDefault();
        setSelectedIndex((i) =>
          Math.min(i + 1, filteredFiles().length - 1)
        );
        break;
      case "ArrowUp":
        e.preventDefault();
        setSelectedIndex((i) => Math.max(i - 1, 0));
        break;
      case "Enter":
        e.preventDefault();
        openSelected();
        break;
      case "Escape":
        e.preventDefault();
        props.onClose();
        break;
    }
  }

  // Reset selection when query changes
  function handleInput(value: string) {
    setQuery(value);
    setSelectedIndex(0);
  }

  // Close on backdrop click
  function handleBackdropClick(e: MouseEvent) {
    if (e.target === e.currentTarget) {
      props.onClose();
    }
  }

  // Relative path from roost
  function relativePath(path: string): string {
    const roost = roostStore.roostPath();
    if (!roost) return path;
    return path.replace(roost + "/", "");
  }

  return (
    <div
      class="fixed inset-0 z-50 flex items-start justify-center pt-[15vh] bg-black/50"
      onClick={handleBackdropClick}
      role="dialog"
      aria-modal="true"
      aria-label="Quick switcher"
    >
      <div class="w-[560px] bg-bg-secondary rounded-lg border border-border shadow-2xl overflow-hidden">
        {/* Search input */}
        <div class="flex items-center gap-2 px-4 py-3 border-b border-border">
          <svg
            width="16"
            height="16"
            viewBox="0 0 24 24"
            fill="none"
            stroke="currentColor"
            stroke-width="2"
            class="text-text-muted shrink-0"
          >
            <circle cx="11" cy="11" r="8" />
            <path d="M21 21l-4.35-4.35" />
          </svg>
          <input
            ref={inputRef}
            type="text"
            placeholder="Type to search notes..."
            value={query()}
            onInput={(e) => handleInput(e.currentTarget.value)}
            onKeyDown={handleKeyDown}
            class="flex-1 bg-transparent text-text-primary text-sm outline-none placeholder:text-text-muted"
          />
          <Show when={query()}>
            <button
              class="text-text-muted hover:text-text-primary p-0.5"
              onClick={() => handleInput("")}
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
          </Show>
        </div>

        {/* Results */}
        <div class="max-h-[320px] overflow-y-auto py-1">
          <Show
            when={filteredFiles().length > 0}
            fallback={
              <div class="px-4 py-8 text-text-muted text-sm text-center">
                No matching notes
              </div>
            }
          >
            <For each={filteredFiles()}>
              {(file, idx) => {
                const isSelected = () => idx() === selectedIndex();
                const isOpen = () =>
                  tabStore.tabs().some((t) => t.path === file.path);
                return (
                  <div
                    class={`flex items-center gap-3 px-4 py-2 cursor-pointer transition-colors ${
                      isSelected()
                        ? "bg-accent/15 text-text-primary"
                        : "text-text-secondary hover:bg-bg-hover"
                    }`}
                    onClick={() => {
                      tabStore.openTab(file.path);
                      props.onClose();
                    }}
                    onMouseEnter={() => setSelectedIndex(idx())}
                  >
                    <svg
                      width="14"
                      height="14"
                      viewBox="0 0 24 24"
                      fill="none"
                      stroke="currentColor"
                      stroke-width="2"
                      class="shrink-0 opacity-50"
                    >
                      <path d="M14 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8z" />
                      <polyline points="14 2 14 8 20 8" />
                    </svg>
                    <div class="flex-1 min-w-0">
                      <div class="text-sm truncate">
                        {file.name.replace(/\.md$/, "")}
                      </div>
                      <div class="text-xs text-text-muted truncate">
                        {relativePath(file.path)}
                      </div>
                    </div>
                    <Show when={isOpen()}>
                      <span class="text-xs text-accent">open</span>
                    </Show>
                  </div>
                );
              }}
            </For>
          </Show>
        </div>

        {/* Footer hints */}
        <div class="flex items-center gap-4 px-4 py-2 border-t border-border text-text-muted text-xs">
          <span>
            <kbd class="px-1 py-0.5 rounded bg-bg-hover border border-border text-[10px]">
              Enter
            </kbd>{" "}
            open
          </span>
          <span>
            <kbd class="px-1 py-0.5 rounded bg-bg-hover border border-border text-[10px]">
              Esc
            </kbd>{" "}
            close
          </span>
          <div class="flex-1" />
          <span>{filteredFiles().length} results</span>
        </div>
      </div>
    </div>
  );
}
