import { Show, For, createSignal, onCleanup, onMount } from "solid-js";
import { graphStore, type BacklinkEntry } from "../../stores/graph";
import { tabStore } from "../../stores/tabs";

/**
 * Status-bar backlinks indicator with a click-to-open popover card.
 * The card floats directly above the trigger so it's visible even when
 * the sidebar is closed.
 */
export default function BacklinksPopover() {
  const backlinks = () => graphStore.backlinks();
  const [open, setOpen] = createSignal(false);

  let containerRef: HTMLDivElement | undefined;

  function handleNavigate(entry: BacklinkEntry) {
    tabStore.openTab(entry.path);
    setOpen(false);
  }

  function handleDocumentClick(e: MouseEvent) {
    if (!open()) return;
    const target = e.target as Node | null;
    if (!target) return;
    if (containerRef && containerRef.contains(target)) return;
    setOpen(false);
  }

  function handleKey(e: KeyboardEvent) {
    if (e.key === "Escape") setOpen(false);
  }

  onMount(() => {
    document.addEventListener("click", handleDocumentClick);
    document.addEventListener("keydown", handleKey);
  });

  onCleanup(() => {
    document.removeEventListener("click", handleDocumentClick);
    document.removeEventListener("keydown", handleKey);
  });

  return (
    <div ref={containerRef} class="relative">
      {/* Floating card — anchored above the trigger */}
      <Show when={open()}>
        <div
          class="absolute left-0 bottom-full mb-1 z-50 w-56 rounded-md border border-border bg-bg-secondary shadow-lg p-2 max-h-64 overflow-y-auto"
          role="dialog"
          aria-label="Backlinks"
        >
          <div class="text-[10px] font-medium text-text-muted uppercase tracking-wider px-1 pb-1.5 mb-1 border-b border-border">
            Linked from
          </div>
          <Show
            when={backlinks().length > 0}
            fallback={
              <div class="text-xs text-text-muted italic py-2 px-1 text-center">
                No backlinks
              </div>
            }
          >
            <div class="space-y-0.5">
              <For each={backlinks()}>
                {(entry) => (
                  <button
                    type="button"
                    class="w-full text-left px-2 py-1.5 rounded text-sm text-text-secondary hover:text-text-primary hover:bg-bg-hover transition-colors flex items-center gap-2 cursor-pointer"
                    onClick={() => handleNavigate(entry)}
                  >
                    <svg
                      width="13"
                      height="13"
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
      </Show>

      {/* Trigger — replaces the static "X backlinks" span */}
      <button
        type="button"
        class={`flex items-center gap-1 text-xs px-1.5 py-0.5 -mx-1.5 rounded hover:bg-bg-hover transition-colors cursor-pointer ${
          open() ? "text-text-primary bg-bg-hover" : "text-text-muted hover:text-text-secondary"
        }`}
        onClick={(e) => {
          e.stopPropagation();
          setOpen((v) => !v);
        }}
        aria-expanded={open()}
        title="Show backlinks"
      >
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
        <span>{backlinks().length} backlinks</span>
      </button>
    </div>
  );
}
