import { ErrorBoundary, type JSX } from "solid-js";
import { log } from "../lib/log";

/**
 * ENG-7: per-pane error isolation.
 *
 * The app root has one `<ErrorBoundary>` (see `index.tsx`), but it catches
 * at the whole-app level — a render exception in the editor replaces the
 * ENTIRE UI with the full-screen fallback. This wraps a single pane so a
 * crash there is contained: the rest of the app (other tabs, chat, the file
 * tree) keeps working while just the broken pane shows a small "reload this
 * pane" card. `reset()` re-runs the pane's render, which recovers cleanly for
 * transient errors (a bad note that the user then edits/closes).
 *
 * `label` names the pane in the fallback + the log line so a crash report
 * points at the right subsystem.
 */
export function PaneErrorBoundary(props: {
  label: string;
  children: JSX.Element;
}): JSX.Element {
  return (
    <ErrorBoundary
      fallback={(err, reset) => {
        log.error("pane", `${props.label} pane crashed`, err);
        return (
          <div class="h-full w-full flex items-center justify-center p-6">
            <div class="max-w-xs text-center space-y-3">
              <div class="text-2xl">⚠</div>
              <div class="text-sm font-medium text-text-primary">
                The {props.label} pane hit an error
              </div>
              <p class="text-xs text-text-muted">
                The rest of Corvux is still running and your files are safe on
                disk.
              </p>
              <pre class="text-[10px] text-left bg-bg-secondary rounded p-2 overflow-auto max-h-24 text-red-400">
                {(err as Error)?.message || "Unknown error"}
              </pre>
              <button
                onClick={reset}
                class="px-3 py-1.5 bg-accent text-bg-primary rounded text-xs font-medium hover:opacity-90 transition-opacity"
              >
                Reload this pane
              </button>
            </div>
          </div>
        );
      }}
    >
      {props.children}
    </ErrorBoundary>
  );
}
