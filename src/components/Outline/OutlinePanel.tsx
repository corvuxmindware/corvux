import { createMemo, For, Show } from "solid-js";
import { tabStore } from "../../stores/tabs";

interface Heading {
  level: number;
  text: string;
  line: number;
}

interface OutlinePanelProps {
  onGoToLine: (line: number) => void;
}

export default function OutlinePanel(props: OutlinePanelProps) {
  const headings = createMemo((): Heading[] => {
    const content = tabStore.activeContent();
    if (!content) return [];

    const result: Heading[] = [];
    const lines = content.split("\n");
    let inFrontmatter = false;

    for (let i = 0; i < lines.length; i++) {
      const line = lines[i];

      // Skip YAML frontmatter
      if (i === 0 && line === "---") {
        inFrontmatter = true;
        continue;
      }
      if (inFrontmatter) {
        if (line === "---") inFrontmatter = false;
        continue;
      }

      // Match ATX headings (# through ######)
      const match = line.match(/^(#{1,6})\s+(.+)/);
      if (match) {
        result.push({
          level: match[1].length,
          text: match[2].replace(/\s*#+\s*$/, ""), // strip trailing #'s
          line: i + 1, // 1-indexed
        });
      }
    }

    return result;
  });

  const minLevel = createMemo(() => {
    const h = headings();
    if (h.length === 0) return 1;
    return Math.min(...h.map((h) => h.level));
  });

  return (
    <div class="flex flex-col h-full select-none">
      {/* Header */}
      <div class="h-10 flex items-center px-3 border-b border-border shrink-0">
        <span class="text-text-secondary font-medium text-sm tracking-wide">OUTLINE</span>
      </div>

      <div class="flex-1 overflow-y-auto py-2">
        <Show when={tabStore.activeTabPath()}>
          <Show
            when={headings().length > 0}
            fallback={
              <div class="px-4 py-8 text-center space-y-2">
                <svg width="24" height="24" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" class="mx-auto text-text-muted opacity-30">
                  <path d="M4 6h16M4 12h10M4 18h14" />
                </svg>
                <p class="text-text-muted text-xs">No headings found</p>
                <p class="text-text-muted text-[10px] opacity-50">
                  Use # syntax to create headings
                </p>
              </div>
            }
          >
            <For each={headings()}>
              {(heading) => (
                <button
                  class="w-full text-left px-3 py-1 rounded-sm text-sm text-text-secondary hover:text-text-primary hover:bg-bg-hover transition-colors truncate"
                  style={{ "padding-left": `${(heading.level - minLevel()) * 14 + 12}px` }}
                  onClick={() => props.onGoToLine(heading.line)}
                >
                  <span class={heading.level <= 2 ? "font-medium" : ""}>
                    {heading.text}
                  </span>
                </button>
              )}
            </For>
          </Show>
        </Show>
        <Show when={!tabStore.activeTabPath()}>
          <div class="px-4 py-8 text-center">
            <p class="text-text-muted text-xs">Open a note to see its outline</p>
          </div>
        </Show>
      </div>
    </div>
  );
}
