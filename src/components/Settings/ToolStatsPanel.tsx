import { createResource, createSignal, For, Show, type Resource } from "solid-js";
import { commands, unwrap } from "../../lib/invoke";
import type { ToolReliabilityRow } from "../../lib/bindings";
import { log } from "../../lib/log";

export type { ToolReliabilityRow };

/** Settings → AI → Tool Stats panel.
 *
 *  Surfaces per-tool approval / rejection rates from `:approval_log`.
 *  Tools the user rejects most show up first so the worst offenders
 *  are immediate. Backend already feeds these counts into the system
 *  prompt — tools above the rejection threshold get a "(low
 *  confidence)" qualifier inline with their description; this panel
 *  is the observability surface for that mechanism.
 *
 *  No row mutation here — removing an "always-approve" entry is the
 *  AgentAutonomy panel's job; resetting counts isn't exposed yet
 *  (would just be a `:rm` on approval_log, queued as a follow-up).
 */
export default function ToolStatsPanel() {
  const [window, setWindow] = createSignal(30);
  const [rows, { refetch }] = createResource(
    window,
    async (days) => {
      try {
        return await unwrap(commands.toolReliabilityStats(days));
      } catch (e) {
        log.warn("settings", "tool_reliability_stats failed", e);
        return [] as ToolReliabilityRow[];
      }
    },
    { initialValue: [] as ToolReliabilityRow[] },
  );

  return (
    <div class="space-y-3 border-t border-border pt-4">
      <div class="flex items-baseline justify-between gap-2">
        <div>
          <h4 class="text-text-secondary text-xs font-semibold uppercase tracking-wider">
            Which tools you trust
          </h4>
          <p class="text-text-muted text-[11px] mt-1 leading-relaxed">
            How often you say yes or no to each tool. Tools you reject a lot
            get flagged to Corvux so it tries them less in similar situations.
          </p>
        </div>
        <div class="flex items-center gap-1 shrink-0">
          <For each={[7, 30, 90]}>
            {(d) => (
              <button
                type="button"
                class={`text-[11px] px-2 py-0.5 rounded border ${
                  window() === d
                    ? "bg-purple/20 border-purple text-purple"
                    : "border-border text-text-muted hover:text-text-secondary"
                }`}
                onClick={() => setWindow(d)}
              >
                {d}d
              </button>
            )}
          </For>
          <button
            type="button"
            class="text-text-muted hover:text-text-secondary text-[11px] ml-1"
            onClick={() => refetch()}
            title="Refresh"
          >
            ↻
          </button>
        </div>
      </div>

      <ToolStatsBody rows={rows} />
    </div>
  );
}

function ToolStatsBody(props: { rows: Resource<ToolReliabilityRow[]> }) {
  return (
    <Show
      when={(props.rows()?.length ?? 0) > 0}
      fallback={
        <div class="text-text-muted text-xs italic">
          No approval decisions in this window yet. Use the agent and approve
          or reject a few write tools to populate this table.
        </div>
      }
    >
      <div class="border border-border rounded overflow-hidden">
        <table class="w-full text-[11px]">
          <thead class="bg-bg-secondary/50">
            <tr class="text-left text-text-muted">
              <th class="font-medium px-2 py-1">Tool</th>
              <th class="font-medium px-2 py-1 text-right">Asked</th>
              <th class="font-medium px-2 py-1 text-right">Approved</th>
              <th class="font-medium px-2 py-1 text-right">Rejected</th>
              <th class="font-medium px-2 py-1 text-right">Rate</th>
              <th class="font-medium px-2 py-1">Signal</th>
            </tr>
          </thead>
          <tbody>
            <For each={props.rows()}>
              {(row) => <ToolStatsRowView row={row} />}
            </For>
          </tbody>
        </table>
      </div>
    </Show>
  );
}

function ToolStatsRowView(props: { row: ToolReliabilityRow }) {
  const r = props.row;
  const decisive = r.total - r.forgotten;
  const approveTotal = r.approvals + r.always_approvals;
  // `rejection_rate` is `f64` on the Rust side → `number | null` in TS
  // (specta's NaN/Inf safety net). Coalesce to 0 for display arithmetic.
  const rate = r.rejection_rate ?? 0;
  const pct = (rate * 100).toFixed(0);

  // Mirror the agent loop's threshold (70% over ≥5 decisive decisions).
  // Keep these constants in sync with `chat.rs::LOW_CONF_*`.
  const lowConfidence = decisive >= 5 && rate >= 0.7;
  const everyApprove = r.rejections === 0 && approveTotal > 0;

  const rateColor = () => {
    if (lowConfidence) return "text-red-400 font-semibold";
    if (rate >= 0.4) return "text-yellow-400";
    return "text-text-secondary";
  };

  return (
    <tr class="border-t border-border hover:bg-bg-secondary/30">
      <td class="px-2 py-1 text-text-primary font-mono whitespace-nowrap">
        {r.tool}
      </td>
      <td class="px-2 py-1 text-text-secondary font-mono text-right">
        {r.total}
      </td>
      <td class="px-2 py-1 text-green-400 font-mono text-right">
        {approveTotal}
      </td>
      <td class="px-2 py-1 text-red-400 font-mono text-right">
        {r.rejections}
      </td>
      <td class={`px-2 py-1 font-mono text-right ${rateColor()}`}>{pct}%</td>
      <td class="px-2 py-1 text-[10px]">
        <Show
          when={lowConfidence}
          fallback={
            <Show
              when={everyApprove}
              fallback={
                <span class="text-text-muted">—</span>
              }
            >
              <span class="text-green-400">trusted</span>
            </Show>
          }
        >
          <span class="text-red-400" title="Corvux deprioritizes this tool for similar future requests">used less</span>
        </Show>
      </td>
    </tr>
  );
}
