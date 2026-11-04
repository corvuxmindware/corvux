// Settings → Corvux Agent → AI Learning panel.
//
// Surfaces the self-evolution loop's accumulated state: total signals
// recorded, accept/reject split, and the top boosted / penalized
// notes. Read-only display + a destructive Reset button. The signal
// data is produced by the approval gate (every Approve/Reject on a
// path-aware write tool writes one row to `:ai_signals`) and consumed
// by `rag::build_rag_context` to reweight retrievals.

import { createResource, For, Show, createSignal } from "solid-js";
import { commands, unwrap } from "../../lib/invoke";
import type { SignalSummary } from "../../lib/bindings";
import { log } from "../../lib/log";

export type { SignalSummary };

async function fetchSummary(): Promise<SignalSummary | null> {
  try {
    return await unwrap(commands.getAiLearningSummary(5));
  } catch (e) {
    log.warn("ai-learning", "get_ai_learning_summary failed", e);
    return null;
  }
}

export default function AILearningPanel() {
  const [summary, { refetch }] = createResource(fetchSummary);
  const [resetting, setResetting] = createSignal(false);
  const [confirmReset, setConfirmReset] = createSignal(false);

  const handleReset = async () => {
    setResetting(true);
    try {
      await unwrap(commands.resetAiLearning());
      await refetch();
      setConfirmReset(false);
    } catch (e) {
      log.warn("ai-learning", "reset failed", e);
    } finally {
      setResetting(false);
    }
  };

  return (
    <div class="space-y-3 border-t border-border pt-4">
      <div>
        <h4 class="text-text-secondary text-xs font-semibold uppercase tracking-wider">
          What Corvux has learned about your notes
        </h4>
        <p class="text-text-muted text-[11px] mt-1 leading-relaxed">
          Every time you Approve or Reject an agent action on a specific
          note, Corvux records a signal. Search results then float
          approved notes up and push rejected ones down — within a
          bounded range, so a strong match still wins.
        </p>
      </div>

      <Show
        when={!summary.loading && summary()}
        fallback={
          <div class="text-text-muted text-[11px] italic">Loading…</div>
        }
      >
        {(s) => (
          <>
            <div class="grid grid-cols-3 gap-2">
              <Tile label="Total signals" value={s().total_signals} />
              <Tile
                label="Approved"
                value={s().approve_signals}
                tone="positive"
              />
              <Tile
                label="Rejected"
                value={s().reject_signals}
                tone="negative"
              />
            </div>

            <Show when={s().total_signals === 0}>
              <div class="text-text-muted text-[11px] italic">
                No signals yet. Approve or reject an agent edit and they'll
                start showing up here.
              </div>
            </Show>

            <Show when={s().top_boosted.length > 0}>
              <NoteList
                title="Boosted in search"
                items={s().top_boosted}
                tone="positive"
              />
            </Show>

            <Show when={s().top_penalized.length > 0}>
              <NoteList
                title="Penalized in search"
                items={s().top_penalized}
                tone="negative"
              />
            </Show>

            <Show when={s().total_signals > 0}>
              <div class="pt-2 border-t border-border/50">
                <Show
                  when={confirmReset()}
                  fallback={
                    <button
                      onClick={() => setConfirmReset(true)}
                      class="text-[11px] text-red-400 hover:text-red-300 transition-colors"
                    >
                      Reset learning…
                    </button>
                  }
                >
                  <div class="flex items-center gap-2 text-[11px]">
                    <span class="text-text-muted">
                      Wipe all {s().total_signals} signals?
                    </span>
                    <button
                      onClick={() => void handleReset()}
                      disabled={resetting()}
                      class="px-2 py-0.5 rounded bg-red-500/15 text-red-300 border border-red-500/30 hover:bg-red-500/25 disabled:opacity-50"
                    >
                      {resetting() ? "Resetting…" : "Confirm"}
                    </button>
                    <button
                      onClick={() => setConfirmReset(false)}
                      disabled={resetting()}
                      class="text-text-muted hover:text-text-secondary"
                    >
                      Cancel
                    </button>
                  </div>
                </Show>
              </div>
            </Show>
          </>
        )}
      </Show>
    </div>
  );
}

function Tile(props: {
  label: string;
  value: number;
  tone?: "positive" | "negative";
}) {
  const toneClass = () =>
    props.tone === "positive"
      ? "text-green-400"
      : props.tone === "negative"
        ? "text-red-400"
        : "text-text-primary";
  return (
    <div class="bg-bg-secondary border border-border rounded px-2 py-1.5">
      <div class={`text-base font-semibold ${toneClass()}`}>{props.value}</div>
      <div class="text-[10px] text-text-muted uppercase tracking-wide">
        {props.label}
      </div>
    </div>
  );
}

function NoteList(props: {
  title: string;
  items: [string, number][];
  tone: "positive" | "negative";
}) {
  return (
    <div class="space-y-1">
      <div class="text-text-secondary text-xs">{props.title}</div>
      <div class="space-y-0.5">
        <For each={props.items}>
          {([note, sum]) => (
            <div class="flex items-center justify-between text-[11px] gap-2">
              <code class="text-text-primary truncate min-w-0">{note}</code>
              <span
                class={`font-mono shrink-0 ${
                  props.tone === "positive"
                    ? "text-green-400"
                    : "text-red-400"
                }`}
              >
                {sum > 0 ? `+${sum}` : sum}
              </span>
            </div>
          )}
        </For>
      </div>
    </div>
  );
}
