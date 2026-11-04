import { Show, createMemo, createSignal, onCleanup, onMount } from "solid-js";
import { bugReportStore } from "../../stores/bugReport";
import { settingsStore } from "../../stores/settings";
import { isMobile } from "../../lib/platform";

/**
 * Bug-report capture modal. Always renders — opens itself when
 * `bugReportStore.open()` returns a context, hides otherwise.
 *
 * UX:
 *   - Single textarea for the user's narrative ("What were you doing?").
 *     Pre-filled for the post-crash path so the user only adds repro
 *     hints to the existing "Corvux closed unexpectedly" framing.
 *   - Visible hint about what gets included (diagnostic report + crash
 *     excerpt when applicable). Plain copy, no toggles — this isn't the
 *     UI for users who want to filter what goes in. They use Settings →
 *     Diagnostics → Copy diagnostic info instead and route manually.
 *   - Submit button labels reflect the configured endpoint:
 *       mailto:   "Open email"
 *       https://  "Open browser"
 *       (none)    "Copy report"
 *   - Result panel after submit confirms what happened: clipboard +
 *     channel, with a "Copy again" affordance in case the user closed
 *     the channel before pasting.
 */
export default function BugReportModal() {
  const [description, setDescription] = createSignal("");
  const [result, setResult] = createSignal<{
    copied: boolean;
    routed: "mailto" | "browser" | null;
    error?: string;
  } | null>(null);

  const ctx = () => bugReportStore.open();

  // Reset draft + result when the modal opens with a new context.
  onMount(() => {
    document.addEventListener("keydown", handleKey);
  });
  onCleanup(() => {
    document.removeEventListener("keydown", handleKey);
  });

  function handleKey(e: KeyboardEvent) {
    if (!ctx()) return;
    if (e.key === "Escape") {
      e.preventDefault();
      close();
    }
  }

  // Re-seed the description whenever the modal opens with a different
  // context. We can't use `onMount` because the modal stays mounted —
  // it just toggles visibility.
  let lastSeenCtx: ReturnType<typeof ctx> = null;
  function syncContext() {
    const c = ctx();
    if (c === lastSeenCtx) return;
    lastSeenCtx = c;
    if (c) {
      setDescription(c.initialDescription ?? "");
      setResult(null);
    }
  }

  // Run the sync as a side-effect during render — Solid will re-evaluate
  // whenever `ctx()` changes, which is exactly when we need to reset.
  const _seed = createMemo(() => {
    syncContext();
    return null;
  });

  const submitLabel = () => {
    const ep = settingsStore.settings().support_endpoint?.trim() ?? "";
    if (ep.startsWith("mailto:")) return "Open email";
    if (/^https?:\/\//i.test(ep)) return "Open browser";
    return "Copy report";
  };

  const helperText = () => {
    const c = ctx();
    if (c?.kind === "previous-crash") {
      return "Corvux didn't shut down cleanly last session. Tell us what you were doing right before it happened — even one sentence helps.";
    }
    if (c?.kind === "toast-error") {
      return "An error popped up. Add a sentence about what you were doing when it happened, then send.";
    }
    return "Describe what went wrong, what you expected, and what happened instead.";
  };

  async function handleSubmit() {
    const r = await bugReportStore.submit(description());
    setResult(r);
  }

  function close() {
    bugReportStore.dismiss();
    setDescription("");
    setResult(null);
  }

  // Force-evaluate the seed memo so the description pre-fills.
  // (Reading it from JSX avoids the `_seed` unused-var warning too.)
  return (
    <Show when={ctx()}>
      {(c) => (
        <div
          class="fixed inset-0 z-[110] flex items-center justify-center p-4"
          role="dialog"
          aria-modal="true"
          aria-label="Report a problem"
        >
          {_seed()}
          <div
            class="absolute inset-0 bg-black/60"
            onClick={close}
            aria-hidden="true"
          />
          <div
            class={`relative w-full ${
              isMobile() ? "max-w-full" : "max-w-md"
            } max-h-[90vh] overflow-y-auto bg-bg-primary border border-border rounded-xl shadow-2xl`}
          >
            <header class="flex items-center justify-between px-4 py-3 border-b border-border">
              <h2 class="text-text-primary font-semibold text-sm">
                {c().kind === "previous-crash"
                  ? "Send a crash report"
                  : "Report a problem"}
              </h2>
              <button
                onClick={close}
                class="text-text-muted hover:text-text-primary p-1 rounded hover:bg-bg-hover transition-colors"
                aria-label="Close"
              >
                <svg
                  width="16"
                  height="16"
                  viewBox="0 0 24 24"
                  fill="none"
                  stroke="currentColor"
                  stroke-width="2"
                >
                  <path d="M18 6L6 18M6 6l12 12" />
                </svg>
              </button>
            </header>

            <div class="p-4 space-y-3">
              <Show when={!result()}>
                <p class="text-text-muted text-[12px] leading-relaxed">
                  {helperText()}
                </p>

                <Show when={c().crashExcerpt}>
                  <details class="rounded-lg border border-border bg-bg-secondary/40 text-[11px]">
                    <summary class="px-3 py-2 cursor-pointer text-text-secondary hover:text-text-primary">
                      Crash excerpt — included automatically
                    </summary>
                    <pre class="px-3 py-2 max-h-32 overflow-y-auto whitespace-pre-wrap break-words font-mono text-text-muted">
                      {c().crashExcerpt}
                    </pre>
                  </details>
                </Show>

                <textarea
                  value={description()}
                  onInput={(e) => setDescription(e.currentTarget.value)}
                  placeholder="What were you doing? What did you expect? What happened?"
                  rows={5}
                  class="w-full px-3 py-2 rounded-lg border border-border bg-bg-secondary/60 text-text-primary text-sm placeholder:text-text-muted focus:outline-none focus:border-accent/60 resize-y"
                />

                <div class="text-text-muted text-[11px] leading-relaxed">
                  Sending will copy the full report (your description + app
                  version + recent log + crash trace) to your clipboard and
                  open
                  <Show
                    when={settingsStore.settings().support_endpoint?.trim()}
                    fallback={<span> nothing — paste it into your support channel manually.</span>}
                  >
                    {(ep) => {
                      const e = ep().trim();
                      if (e.startsWith("mailto:")) return <span> your email client.</span>;
                      if (/^https?:\/\//i.test(e)) return <span> a new browser tab.</span>;
                      return <span>.</span>;
                    }}
                  </Show>
                </div>

                <div class="flex justify-end gap-2 pt-1">
                  <button
                    onClick={close}
                    class="px-3 py-1.5 rounded-lg text-text-secondary hover:text-text-primary hover:bg-bg-hover text-sm transition-colors"
                  >
                    Cancel
                  </button>
                  <button
                    onClick={() => void handleSubmit()}
                    disabled={bugReportStore.submitting()}
                    class="px-3 py-1.5 rounded-lg bg-accent hover:bg-accent-hover text-white text-sm font-medium disabled:opacity-50 disabled:cursor-wait transition-colors"
                  >
                    {bugReportStore.submitting() ? "Building report…" : submitLabel()}
                  </button>
                </div>
              </Show>

              <Show when={result()}>
                {(r) => (
                  <>
                    <Show when={r().error}>
                      <div class="text-red text-sm">
                        Failed: {r().error}
                      </div>
                    </Show>
                    <Show when={!r().error}>
                      <div class="text-text-primary text-sm space-y-1">
                        <Show when={r().copied}>
                          <div class="flex items-center gap-2">
                            <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round" class="text-green shrink-0" aria-hidden="true">
                              <path d="M20 6L9 17l-5-5" />
                            </svg>
                            <span>Diagnostic report copied to clipboard.</span>
                          </div>
                        </Show>
                        <Show when={!r().copied}>
                          <div class="text-yellow">
                            Couldn't copy to clipboard — paste was blocked. Open Settings → Diagnostics → Copy diagnostic info as a fallback.
                          </div>
                        </Show>
                        <Show when={r().routed === "mailto"}>
                          <div class="text-text-secondary text-[12px]">
                            Email draft opened — paste below the divider line, then send.
                          </div>
                        </Show>
                        <Show when={r().routed === "browser"}>
                          <div class="text-text-secondary text-[12px]">
                            Browser tab opened — paste into the issue body, then submit.
                          </div>
                        </Show>
                        <Show when={r().routed === null}>
                          <div class="text-text-secondary text-[12px]">
                            No support endpoint configured. Paste the report into your support channel of choice (Discord, email, GitHub issue, etc.). Set a default in Settings → About.
                          </div>
                        </Show>
                      </div>
                    </Show>
                    <div class="flex justify-end pt-1">
                      <button
                        onClick={close}
                        class="px-3 py-1.5 rounded-lg bg-bg-secondary border border-border text-text-primary text-sm hover:bg-bg-hover transition-colors"
                      >
                        Close
                      </button>
                    </div>
                  </>
                )}
              </Show>
            </div>
          </div>
        </div>
      )}
    </Show>
  );
}
