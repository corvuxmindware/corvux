import { Show, createSignal } from "solid-js";
import { updaterStore } from "../../stores/updater";
import { isMobile } from "../../lib/platform";

/**
 * Slim banner that appears at the top of the workspace when an update
 * is available, then morphs into a download / install progress strip
 * once the user opts in. Mobile + browser builds render nothing — the
 * updater plugin isn't wired on those targets.
 *
 * Two dismissal paths:
 *   - "✕" (snooze): hides for this session only. Next launch (or next
 *     hourly background check) re-surfaces the same banner.
 *   - "Skip" (persistent): writes the version id to localStorage so
 *     subsequent launches stop nagging until a newer version ships.
 */
export default function UpdateBanner() {
  const [snoozed, setSnoozed] = createSignal(false);

  if (!updaterStore.isSupported()) return null;
  if (isMobile()) return null;

  return (
    <Show when={!snoozed()}>
      <Show when={updaterStore.available()}>
        {(info) => (
          <div
            class="px-4 py-2 bg-accent/10 border-b border-accent/30 flex items-center gap-3 text-sm"
            role="status"
            aria-live="polite"
          >
            <svg
              width="16"
              height="16"
              viewBox="0 0 24 24"
              fill="none"
              stroke="currentColor"
              stroke-width="2"
              stroke-linecap="round"
              stroke-linejoin="round"
              class="text-accent shrink-0"
              aria-hidden="true"
            >
              <path d="M21 15v4a2 2 0 01-2 2H5a2 2 0 01-2-2v-4" />
              <polyline points="17 8 12 3 7 8" />
              <line x1="12" y1="3" x2="12" y2="15" />
            </svg>
            <span class="text-text-primary truncate flex-1 min-w-0">
              <span class="font-medium">Update available:</span>{" "}
              <span class="text-text-secondary">
                v{info().currentVersion} → v{info().version}
              </span>
            </span>
            <button
              onClick={() => updaterStore.installAndRelaunch()}
              class="px-3 py-1 rounded bg-accent text-white text-xs font-medium hover:bg-accent-hover transition-colors shrink-0"
            >
              Restart &amp; install
            </button>
            <button
              onClick={() => updaterStore.skipCurrent()}
              class="text-text-muted hover:text-text-secondary text-xs shrink-0"
              title="Don't show this version again"
            >
              Skip
            </button>
            <button
              onClick={() => setSnoozed(true)}
              class="text-text-muted hover:text-text-secondary p-1 rounded hover:bg-bg-hover transition-colors shrink-0"
              aria-label="Dismiss for now"
              title="Remind me next launch"
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
          </div>
        )}
      </Show>

      <Show when={updaterStore.isInstalling()}>
        <div
          class="px-4 py-2 bg-bg-secondary border-b border-border flex items-center gap-3 text-sm"
          role="status"
          aria-live="polite"
        >
          <span class="inline-block w-3.5 h-3.5 border-2 border-accent border-t-transparent rounded-full animate-spin shrink-0" />
          <Show
            when={updaterStore.downloading()}
            fallback={<span class="text-text-secondary">Installing update…</span>}
          >
            <span class="text-text-secondary truncate flex-1 min-w-0">
              Downloading update
              <Show when={updaterStore.downloadProgress() !== null}>
                {" · "}
                {Math.floor((updaterStore.downloadProgress() ?? 0) * 100)}%
              </Show>
              …
            </span>
          </Show>
        </div>
      </Show>

      <Show when={updaterStore.error()}>
        {(err) => (
          <div class="px-4 py-2 bg-red/10 border-b border-red/30 flex items-center gap-3 text-sm">
            <span class="text-red shrink-0">Update failed:</span>
            <span class="text-text-secondary truncate flex-1 min-w-0">
              {err().message}
            </span>
            <button
              onClick={() => setSnoozed(true)}
              class="text-text-muted hover:text-text-secondary text-xs shrink-0"
            >
              Dismiss
            </button>
          </div>
        )}
      </Show>
    </Show>
  );
}
