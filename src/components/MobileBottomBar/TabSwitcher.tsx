import { For, Show, createMemo } from "solid-js";
import {
  tabStore,
  GRAPH_TAB_PATH,
  isFileTab,
  type Tab,
} from "../../stores/tabs";
import BottomSheet from "../BottomSheet/BottomSheet";

export interface TabSwitcherProps {
  onClose: () => void;
  /** Open the new-note flow — usually wired to the parent's same prompt. */
  onNewNote: () => void;
}

const PREVIEW_CHARS = 240;
const PREVIEW_LINES_MAX = 6;

/**
 * Extract a short, preview-friendly slice of a note for the card body.
 * - Strips YAML frontmatter if present (between `---` fences at top).
 * - Strips leading H1 if it duplicates the tab title.
 * - Keeps newlines but collapses runs of blank lines.
 * - Caps at PREVIEW_CHARS chars / PREVIEW_LINES_MAX lines, whichever first.
 */
function previewFor(tab: Tab): string {
  if (tab.kind === "graph") return "Graph view — all notes + tags";
  let text = tab.content ?? "";

  // Strip YAML frontmatter
  if (text.startsWith("---")) {
    const end = text.indexOf("\n---", 3);
    if (end !== -1) text = text.slice(end + 4).replace(/^\s*\n/, "");
  }

  // Strip leading H1 if it roughly matches the file name — otherwise the
  // preview just repeats the card title.
  const cleanName = tab.name.replace(/\.md$/i, "").toLowerCase();
  const firstLine = text.split("\n", 1)[0]?.trim() ?? "";
  if (/^#\s+/.test(firstLine)) {
    const h1 = firstLine.replace(/^#\s+/, "").toLowerCase();
    if (h1 === cleanName) {
      text = text.split("\n").slice(1).join("\n").replace(/^\s*\n/, "");
    }
  }

  // Collapse blank-line runs to single blanks
  text = text.replace(/\n{3,}/g, "\n\n");

  // Cap by lines then by chars — whichever is more aggressive
  const lines = text.split("\n").slice(0, PREVIEW_LINES_MAX);
  text = lines.join("\n");
  if (text.length > PREVIEW_CHARS) text = text.slice(0, PREVIEW_CHARS) + "…";

  return text.trim();
}

function tabDisplayName(tab: Tab): string {
  if (tab.kind === "graph") return "Graph view";
  return tab.name.replace(/\.md$/i, "");
}

/**
 * Obsidian-style tab-switcher card grid. Each open tab renders as a
 * bordered card with a content-preview snippet above and the tab name
 * below. Tap a card to switch; tap the X to close a tab. Active tab
 * is outlined in the accent colour.
 *
 * Footer hosts the "new note" button, a live tab-count label, and a
 * "Done" action that dismisses the switcher.
 */
export default function TabSwitcher(props: TabSwitcherProps) {
  const tabs = createMemo(() => tabStore.tabs());
  const activePath = () => tabStore.activeTabPath();

  function handleSwitch(path: string) {
    tabStore.switchToTab(path);
    props.onClose();
  }

  function handleClose(e: MouseEvent, path: string) {
    e.stopPropagation();
    tabStore.closeTab(path);
    // If that was the last tab, drop the switcher — nothing left to show.
    if (tabStore.tabs().length === 0) props.onClose();
  }

  const footer = (
    <div
      class="flex items-center justify-between px-3 py-2 gap-2 bg-bg-secondary"
      style={{ "padding-bottom": "calc(env(safe-area-inset-bottom, 0px) + 8px)" }}
    >
      <button
        onClick={() => {
          props.onClose();
          props.onNewNote();
        }}
        class="w-10 h-10 flex items-center justify-center rounded-full text-text-secondary hover:text-text-primary hover:bg-bg-hover transition-colors"
        aria-label="New note"
      >
        <svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round">
          <path d="M12 5v14M5 12h14" />
        </svg>
      </button>
      <span class="text-text-secondary text-sm tabular-nums">
        {tabs().length} {tabs().length === 1 ? "tab" : "tabs"}
      </span>
      <button
        onClick={props.onClose}
        class="px-4 h-10 flex items-center rounded-full text-accent font-medium text-sm hover:bg-bg-hover transition-colors"
      >
        Done
      </button>
    </div>
  );

  return (
    <BottomSheet
      onClose={props.onClose}
      variant="fullscreen"
      ariaLabel="Open tabs"
      footer={footer}
    >
      <Show
        when={tabs().length > 0}
        fallback={
          <div class="h-full flex items-center justify-center text-text-muted text-sm px-6 text-center">
            No tabs open — tap the + button to create one.
          </div>
        }
      >
        <div
          class="grid grid-cols-2 gap-3 p-3"
          style={{ "padding-top": "calc(env(safe-area-inset-top, 0px) + 12px)" }}
          role="list"
          aria-label="Open tabs"
        >
          <For each={tabs()}>
            {(tab) => {
              const isActive = () => tab.path === activePath();
              const name = () => tabDisplayName(tab);
              const preview = createMemo(() => previewFor(tab));
              return (
                <div role="listitem" class="flex flex-col gap-1.5">
                  <button
                    onClick={() => handleSwitch(tab.path)}
                    aria-label={`Switch to ${name()}`}
                    aria-current={isActive() ? "true" : undefined}
                    class={`relative aspect-[3/4] w-full overflow-hidden rounded-xl text-left transition-colors ${
                      isActive()
                        ? "border-2 border-accent bg-bg-secondary"
                        : "border border-border bg-bg-secondary/60 hover:bg-bg-secondary"
                    }`}
                  >
                    {/* Close button — persistent, not hover-gated, so it's
                        reachable on touch. The × always shows; desktop
                        hover states are aesthetic only. */}
                    <Show when={tab.path !== GRAPH_TAB_PATH || tabs().length > 1}>
                      <button
                        onClick={(e) => handleClose(e, tab.path)}
                        aria-label={`Close ${name()}`}
                        class="absolute top-1 right-1 z-10 w-9 h-9 flex items-center justify-center rounded-full bg-bg-primary/70 backdrop-blur-sm text-text-secondary hover:text-text-primary hover:bg-bg-primary transition-colors"
                      >
                        <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
                          <path d="M18 6L6 18M6 6l12 12" />
                        </svg>
                      </button>
                    </Show>

                    {/* Modified indicator — top-left dot */}
                    <Show when={isFileTab(tab) && tab.isModified}>
                      <span
                        class="absolute top-2.5 left-2.5 z-10 w-1.5 h-1.5 rounded-full bg-accent"
                        aria-label="Unsaved changes"
                      />
                    </Show>

                    <div class="h-full p-3 pt-8 text-[11px] leading-snug text-text-secondary whitespace-pre-wrap break-words overflow-hidden">
                      {preview()}
                    </div>
                  </button>
                  {/* Tab name BELOW the card, matching Obsidian's pattern */}
                  <div
                    class={`text-[12px] text-center truncate px-1 ${
                      isActive()
                        ? "text-accent font-medium"
                        : "text-text-secondary"
                    }`}
                    title={name()}
                  >
                    {name()}
                  </div>
                </div>
              );
            }}
          </For>
        </div>
      </Show>
    </BottomSheet>
  );
}
