import { createSignal, createMemo, For, Show, onMount } from "solid-js";
import { roostStore, type FileEntry } from "../../stores/roost";
import { tabStore } from "../../stores/tabs";

export interface PaletteCommand {
  id: string;
  label: string;
  category: "command" | "file";
  shortcut?: string;
  action: () => void;
}

interface CommandPaletteProps {
  onClose: () => void;
  commands: PaletteCommand[];
}

export default function CommandPalette(props: CommandPaletteProps) {
  const [query, setQuery] = createSignal("");
  const [selectedIndex, setSelectedIndex] = createSignal(0);
  let inputRef!: HTMLInputElement;

  onMount(() => {
    inputRef.focus();
  });

  // Flatten roost files into palette commands.
  //
  // AUDIT-030: cached on the tree identity (`roostStore.files()`), so a
  // 5K-file roost walks the tree once per file-list change instead of
  // once per keystroke. Mirrors the pattern QuickSwitcher already uses.
  const fileCommandsCache = createMemo<PaletteCommand[]>(() => {
    const files: PaletteCommand[] = [];
    function walk(entries: FileEntry[]) {
      for (const entry of entries) {
        if (entry.is_dir) {
          walk(entry.children);
        } else {
          const name = entry.name.replace(/\.md$/, "");
          files.push({
            id: `file:${entry.path}`,
            label: name,
            category: "file",
            action: () => {
              tabStore.openTab(entry.path);
              props.onClose();
            },
          });
        }
      }
    }
    walk(roostStore.files());
    return files;
  });

  // Fuzzy match
  function fuzzyMatch(needle: string, haystack: string): { matches: boolean; score: number } {
    const lower = haystack.toLowerCase();
    const search = needle.toLowerCase();
    if (!search) return { matches: true, score: 0 };
    if (lower.startsWith(search)) return { matches: true, score: 100 };
    if (lower.includes(search)) return { matches: true, score: 50 };

    let si = 0;
    let score = 0;
    let lastIdx = -1;
    for (let i = 0; i < lower.length && si < search.length; i++) {
      if (lower[i] === search[si]) {
        score += 1;
        if (lastIdx === i - 1) score += 2;
        lastIdx = i;
        si++;
      }
    }
    if (si === search.length) return { matches: true, score };
    return { matches: false, score: 0 };
  }

  const allItems = createMemo(() => {
    const q = query();
    // If query starts with >, show only commands. Otherwise, mix files + commands.
    const isCommandMode = q.startsWith(">");
    const searchQ = isCommandMode ? q.slice(1).trim() : q;

    let items: PaletteCommand[];
    if (isCommandMode) {
      items = props.commands;
    } else {
      // Files first, then commands
      items = [...fileCommandsCache(), ...props.commands];
    }

    const recent = tabStore.recentFiles();

    const scored = items.map((item) => {
      const match = fuzzyMatch(searchQ, item.label);
      const recentIdx = item.category === "file"
        ? recent.indexOf(item.id.replace("file:", ""))
        : -1;
      const recentBoost = recentIdx >= 0 ? 200 - recentIdx : 0;
      const categoryBoost = !q && item.category === "command" ? -50 : 0;
      return {
        ...item,
        score: match.score + recentBoost + categoryBoost,
        matches: match.matches,
      };
    });

    return scored
      .filter((i) => i.matches)
      .sort((a, b) => b.score - a.score)
      .slice(0, 30);
  });

  function executeSelected() {
    const items = allItems();
    const idx = selectedIndex();
    if (items[idx]) {
      items[idx].action();
    }
  }

  function handleKeyDown(e: KeyboardEvent) {
    switch (e.key) {
      case "ArrowDown":
        e.preventDefault();
        setSelectedIndex((i) => Math.min(i + 1, allItems().length - 1));
        break;
      case "ArrowUp":
        e.preventDefault();
        setSelectedIndex((i) => Math.max(i - 1, 0));
        break;
      case "Enter":
        e.preventDefault();
        executeSelected();
        break;
      case "Escape":
        e.preventDefault();
        props.onClose();
        break;
    }
  }

  function handleInput(value: string) {
    setQuery(value);
    setSelectedIndex(0);
  }

  function handleBackdropClick(e: MouseEvent) {
    if (e.target === e.currentTarget) {
      props.onClose();
    }
  }

  function categoryIcon(category: string) {
    if (category === "command") {
      return (
        <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" class="shrink-0 opacity-50">
          <path d="M4 17l6-6-6-6M12 19h8" />
        </svg>
      );
    }
    return (
      <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" class="shrink-0 opacity-50">
        <path d="M14 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8z" />
        <polyline points="14 2 14 8 20 8" />
      </svg>
    );
  }

  return (
    <div
      class="fixed inset-0 z-50 flex items-start justify-center pt-[15vh] bg-black/50"
      onClick={handleBackdropClick}
      role="dialog"
      aria-modal="true"
      aria-label="Command palette"
    >
      <div class="w-[560px] bg-bg-secondary rounded-lg border border-border shadow-2xl overflow-hidden">
        {/* Input */}
        <div class="flex items-center gap-2 px-4 py-3 border-b border-border">
          <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" class="text-text-muted shrink-0">
            <path d="M4 17l6-6-6-6M12 19h8" />
          </svg>
          <input
            ref={inputRef}
            type="text"
            placeholder="Type a command or search files... (> for commands only)"
            value={query()}
            onInput={(e) => handleInput(e.currentTarget.value)}
            onKeyDown={handleKeyDown}
            class="flex-1 bg-transparent text-text-primary text-sm outline-none placeholder:text-text-muted"
          />
        </div>

        {/* Results */}
        <div class="max-h-[360px] overflow-y-auto py-1">
          <Show
            when={allItems().length > 0}
            fallback={
              <div class="px-4 py-8 text-text-muted text-sm text-center">
                No matching items
              </div>
            }
          >
            <For each={allItems()}>
              {(item, idx) => {
                const isSelected = () => idx() === selectedIndex();
                return (
                  <div
                    class={`flex items-center gap-3 px-4 py-2 cursor-pointer transition-colors ${
                      isSelected()
                        ? "bg-accent/15 text-text-primary"
                        : "text-text-secondary hover:bg-bg-hover"
                    }`}
                    onClick={() => item.action()}
                    onMouseEnter={() => setSelectedIndex(idx())}
                  >
                    {categoryIcon(item.category)}
                    <span class="flex-1 text-sm truncate">{item.label}</span>
                    <Show when={item.shortcut}>
                      <kbd class="px-1.5 py-0.5 rounded bg-bg-hover border border-border text-[10px] text-text-muted shrink-0">
                        {item.shortcut}
                      </kbd>
                    </Show>
                    <Show when={item.category === "command"}>
                      <span class="text-[10px] text-text-muted uppercase">cmd</span>
                    </Show>
                  </div>
                );
              }}
            </For>
          </Show>
        </div>

        {/* Footer */}
        <div class="flex items-center gap-4 px-4 py-2 border-t border-border text-text-muted text-xs">
          <span>
            <kbd class="px-1 py-0.5 rounded bg-bg-hover border border-border text-[10px]">Enter</kbd>{" "}
            run
          </span>
          <span>
            <kbd class="px-1 py-0.5 rounded bg-bg-hover border border-border text-[10px]">Esc</kbd>{" "}
            close
          </span>
          <span>
            <kbd class="px-1 py-0.5 rounded bg-bg-hover border border-border text-[10px]">&gt;</kbd>{" "}
            commands only
          </span>
          <div class="flex-1" />
          <span>{allItems().length} items</span>
        </div>
      </div>
    </div>
  );
}
