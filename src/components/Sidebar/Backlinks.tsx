import { Show, For } from "solid-js";
import { graphStore, type BacklinkEntry } from "../../stores/graph";
import { tabStore } from "../../stores/tabs";

export default function Backlinks() {
  const backlinks = () => graphStore.backlinks();

  function handleClick(entry: BacklinkEntry) {
    tabStore.openTab(entry.path);
  }

  return (
    <div class="px-3 py-2">
      <div class="flex items-center gap-1.5 text-xs font-medium text-text-muted uppercase tracking-wider mb-2">
        <svg
          width="12"
          height="12"
          viewBox="0 0 24 24"
          fill="none"
          stroke="currentColor"
          stroke-width="2"
        >
          <path d="M10 13a5 5 0 007.54.54l3-3a5 5 0 00-7.07-7.07l-1.72 1.71" />
          <path d="M14 11a5 5 0 00-7.54-.54l-3 3a5 5 0 007.07 7.07l1.71-1.71" />
        </svg>
        <span>Backlinks ({backlinks().length})</span>
      </div>

      <Show
        when={backlinks().length > 0}
        fallback={
          <div class="text-xs text-text-muted py-2 px-1 space-y-1">
            <p class="italic">No backlinks yet</p>
            <p class="opacity-70">
              Use{" "}
              <code class="font-mono not-italic text-[10px] bg-bg-hover px-1 rounded">
                [[note name]]
              </code>{" "}
              in another note to link to this one.
            </p>
          </div>
        }
      >
        <div class="space-y-0.5">
          <For each={backlinks()}>
            {(entry) => (
              <button
                class="w-full text-left px-2 py-1.5 rounded text-sm text-text-secondary hover:text-text-primary hover:bg-bg-hover transition-colors flex items-center gap-2 group"
                onClick={() => handleClick(entry)}
              >
                <svg
                  width="14"
                  height="14"
                  viewBox="0 0 24 24"
                  fill="none"
                  stroke="currentColor"
                  stroke-width="2"
                  class="text-text-muted shrink-0"
                >
                  <path d="M14 2H6a2 2 0 00-2 2v16a2 2 0 002 2h12a2 2 0 002-2V8z" />
                  <polyline points="14 2 14 8 20 8" />
                </svg>
                <span class="truncate">{entry.title}</span>
              </button>
            )}
          </For>
        </div>
      </Show>
    </div>
  );
}
