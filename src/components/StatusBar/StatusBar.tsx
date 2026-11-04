import { Show } from "solid-js";
import BacklinksPopover from "../Sidebar/BacklinksPopover";
import { tabStore } from "../../stores/tabs";
import { graphStore } from "../../stores/graph";
import { roostStore } from "../../stores/roost";

interface StatusBarProps {
  /** Derived stats for the active note (words / chars / lines / property count). */
  stats: () => { words: number; chars: number; lines: number; properties: number };
}

/**
 * Bottom-of-editor status strip. Mobile (< 640 px) keeps only the three
 * signals a writer actually reads on a phone — backlinks (actionable),
 * word count, and save status. Tablet/desktop (`sm:` breakpoint) gets
 * the full detail strip (characters, line number, tab count, graph
 * stats, Local indicator).
 */
export default function StatusBar(props: StatusBarProps) {
  return (
    // Outer wrapper carries the bottom safe-area inset so the footer's
    // background extends through the home-indicator strip and its content
    // row sits ABOVE it. Without this the 28px bar sat under the home
    // indicator while the floating toolbar (positioned at safe-area + 40px)
    // floated above it — leaving a band of crisp editor text between the
    // two. Docking the bar into the safe area closes that gap.
    <div
      class="bg-bg-tertiary border-t border-border shrink-0"
      style={{ "padding-bottom": "env(safe-area-inset-bottom, 0px)" }}
    >
      <div class="h-7 flex items-center px-3 gap-3 sm:gap-4">
        <Show when={tabStore.activeTabPath()}>
        <BacklinksPopover />

        <Show when={props.stats().properties > 0}>
          <div class="hidden sm:flex items-center gap-1 text-text-muted text-xs">
            <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
              <path d="M4 6h16M4 12h16M4 18h7" />
            </svg>
            <span>{props.stats().properties} properties</span>
          </div>
        </Show>

        <div class="flex items-center gap-1 text-text-muted text-xs">
          <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
            <path d="M4 19.5A2.5 2.5 0 016.5 17H20" />
            <path d="M6.5 2H20v20H6.5A2.5 2.5 0 014 19.5v-15A2.5 2.5 0 016.5 2z" />
          </svg>
          <span>{props.stats().words.toLocaleString()} words</span>
        </div>

        <div class="hidden sm:block text-text-muted text-xs">
          {props.stats().chars.toLocaleString()} characters
        </div>

        <div class="hidden sm:block text-text-muted text-xs">
          Ln {props.stats().lines}
        </div>
      </Show>

      <div class="flex-1" />

      <Show when={tabStore.tabs().length > 0}>
        <span class="hidden sm:inline text-text-muted text-xs">
          {tabStore.tabs().length} tab{tabStore.tabs().length !== 1 ? "s" : ""}
        </span>
      </Show>

      <Show when={tabStore.activeTabPath()}>
        <span class="text-text-muted text-xs">
          {tabStore.isModified() ? "Auto-saving..." : "Saved"}
        </span>
      </Show>

      <Show when={graphStore.isReady()}>
        {/* Spelled out — the old "284N 581L" codes read as line noise to
            anyone who didn't write them. Title carries the long form. */}
        <span
          class="hidden sm:inline text-text-muted text-xs"
          title={`${graphStore.stats().notes} notes · ${graphStore.stats().links} links in this roost`}
        >
          {graphStore.stats().notes} notes · {graphStore.stats().links} links
        </span>
      </Show>

      <Show when={roostStore.roostPath()}>
        <div class="hidden sm:flex items-center gap-1 text-text-muted text-xs">
          <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
            <rect x="3" y="11" width="18" height="11" rx="2" ry="2" />
            <path d="M7 11V7a5 5 0 0110 0v4" />
          </svg>
          <span>Local</span>
        </div>
      </Show>
      </div>
    </div>
  );
}
