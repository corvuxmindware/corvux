import { For, Show } from "solid-js";
import { bookmarksStore } from "../../stores/bookmarks";

interface BookmarksPanelProps {
  /** Open the bookmarked note in the editor. */
  onOpen: (path: string) => void;
}

// Filename without `.md`. Mirrors the visual the TabBar uses for tabs so
// the bookmark list reads identically to a tab strip.
function basename(p: string): string {
  const last = p.split(/[\\/]/).pop() ?? p;
  return last.replace(/\.md$/i, "");
}

// Folder above the file, for the muted "where it lives" line under the
// title. Empty string when the file is at the roost root.
function parentDir(p: string): string {
  const segs = p.split(/[\\/]/);
  segs.pop();
  return segs.filter(Boolean).join(" / ");
}

export default function BookmarksPanel(props: BookmarksPanelProps) {
  return (
    <div class="flex-1 min-h-0 overflow-y-auto">
      <Show
        when={bookmarksStore.paths().length > 0}
        fallback={
          <div class="px-4 py-6 text-xs text-text-muted italic leading-relaxed">
            No bookmarks yet. Click the bookmark icon next to a note's title
            to pin it here.
          </div>
        }
      >
        <For each={bookmarksStore.paths()}>
          {(path) => (
            <div
              class="group flex items-center gap-2 px-3 py-1.5 cursor-pointer hover:bg-bg-hover transition-colors"
              onClick={() => props.onOpen(path)}
              title={path}
            >
              <svg
                width="13"
                height="13"
                viewBox="0 0 24 24"
                fill="currentColor"
                class="shrink-0 text-accent opacity-80"
                aria-hidden="true"
              >
                <path d="M19 21l-7-5-7 5V5a2 2 0 012-2h10a2 2 0 012 2z" />
              </svg>
              <div class="flex-1 min-w-0">
                <div class="text-text-primary text-sm truncate">{basename(path)}</div>
                <Show when={parentDir(path)}>
                  <div class="text-text-muted text-[11px] truncate">{parentDir(path)}</div>
                </Show>
              </div>
              <button
                onClick={(e) => {
                  e.stopPropagation();
                  void bookmarksStore.remove(path);
                }}
                class="shrink-0 w-6 h-6 flex items-center justify-center rounded opacity-0 group-hover:opacity-60 hover:!opacity-100 hover:bg-bg-hover text-text-muted hover:text-text-secondary transition-opacity"
                title="Remove bookmark"
                aria-label={`Remove ${basename(path)} from bookmarks`}
              >
                <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
                  <path d="M18 6L6 18M6 6l12 12" />
                </svg>
              </button>
            </div>
          )}
        </For>
      </Show>
    </div>
  );
}
