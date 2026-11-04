import { For, Show } from "solid-js";
import BottomSheet from "../BottomSheet/BottomSheet";
import type { FileEntry } from "../../lib/bindings";
import { eligibleMoveTargets } from "../../lib/move-targets";

/**
 * "Move to folder…" picker. A folder-list sheet — tap a destination and
 * the entry is moved there. Chosen over touch drag-and-drop because HTML5
 * drag events don't fire in the iOS/Android WebView, and a tap-to-pick
 * list is more reliable + accessible on a small scrolling tree (this is
 * also what Obsidian mobile does). Reuses the existing `move_entry` IPC via
 * the `onPick` callback in Sidebar, which also re-indexes the graph.
 */
export default function MoveToSheet(props: {
  entry: FileEntry;
  tree: FileEntry[];
  roostRoot: string;
  onClose: () => void;
  /** Called with the absolute destination folder path. */
  onPick: (destDir: string) => void;
}) {
  const targets = () =>
    eligibleMoveTargets(props.tree, props.entry, props.roostRoot);

  return (
    <BottomSheet title={`Move "${props.entry.name}" to…`} onClose={props.onClose}>
      <Show
        when={targets().length > 0}
        fallback={
          <div class="px-4 py-8 text-center text-text-muted text-sm">
            No other folder to move this into.
          </div>
        }
      >
        <ul class="py-1">
          <For each={targets()}>
            {(t) => (
              <li>
                <button
                  class="w-full flex items-center gap-2 px-4 py-2.5 text-left text-text-primary text-[15px] sm:text-sm hover:bg-bg-hover transition-colors"
                  style={{ "padding-left": `${16 + t.depth * 14}px` }}
                  onClick={() => {
                    props.onPick(t.path);
                    props.onClose();
                  }}
                >
                  <svg
                    width="16"
                    height="16"
                    viewBox="0 0 24 24"
                    fill="none"
                    stroke="currentColor"
                    stroke-width="1.75"
                    stroke-linecap="round"
                    stroke-linejoin="round"
                    class="shrink-0 text-text-muted"
                  >
                    <path d="M3 7v10a2 2 0 002 2h14a2 2 0 002-2V9a2 2 0 00-2-2h-6l-2-2H5a2 2 0 00-2 2z" />
                  </svg>
                  <span class="truncate">{t.label}</span>
                </button>
              </li>
            )}
          </For>
        </ul>
      </Show>
    </BottomSheet>
  );
}
