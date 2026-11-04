import { createSignal, createResource, For, Show, type Resource } from "solid-js";
import { commands, unwrap } from "../../lib/invoke";
import type { ApprovalHistoryRow } from "../../lib/bindings";
import { log } from "../../lib/log";

export type { ApprovalHistoryRow };

const DEFAULT_LIMIT = 100;
const MORE_LIMIT = 500;

/** Settings → AI → Approval History panel.
 *
 *  Reads the audit-log written by the approval gate (`ai/tools/approval.rs`).
 *  One row per decision (Approve / AlwaysApprove / Reject / Forgotten),
 *  newest first. Read-only — no row mutation here; the user removes an
 *  "always-approve" entry in the Agent Autonomy panel above.
 *
 *  Surfaces the audit trail the backend has been writing since the v5
 *  schema migration; without this panel, the data is invisible. */
export default function ApprovalHistoryPanel() {
  const [limit, setLimit] = createSignal(DEFAULT_LIMIT);
  const [rows, { refetch }] = createResource(
    limit,
    async (n) => {
      try {
        return await unwrap(commands.listApprovalLog(n));
      } catch (e) {
        log.warn("approvals", "list_approval_log failed", e);
        return [] as ApprovalHistoryRow[];
      }
    },
    { initialValue: [] as ApprovalHistoryRow[] },
  );

  return (
    <div class="space-y-3 border-t border-border pt-4">
      <div class="flex items-baseline justify-between">
        <div>
          <h4 class="text-text-secondary text-xs font-semibold uppercase tracking-wider">
            Decision history
          </h4>
          <p class="text-text-muted text-[11px] mt-1 leading-relaxed">
            Every time you approved or rejected something Corvux wanted to do.
            Newest first. The full details aren't stored — only a short
            summary, so your notes don't end up in this log.
          </p>
        </div>
        <button
          type="button"
          class="text-text-muted hover:text-text-secondary text-[11px]"
          onClick={() => refetch()}
          title="Refresh approval history"
        >
          Refresh
        </button>
      </div>

      <ApprovalHistoryBody rows={rows} />

      <Show when={(rows()?.length ?? 0) >= limit() && limit() < MORE_LIMIT}>
        <button
          type="button"
          class="text-accent hover:underline text-[11px]"
          onClick={() => setLimit(MORE_LIMIT)}
        >
          Load up to {MORE_LIMIT}
        </button>
      </Show>
    </div>
  );
}

function ApprovalHistoryBody(props: { rows: Resource<ApprovalHistoryRow[]> }) {
  return (
    <Show
      when={(props.rows()?.length ?? 0) > 0}
      fallback={
        <div class="text-text-muted text-xs italic">
          No approval decisions logged yet. Approve or reject a tool in chat to
          see entries appear here.
        </div>
      }
    >
      <div class="border border-border rounded overflow-hidden">
        <table class="w-full text-[11px]">
          <thead class="bg-bg-secondary/50">
            <tr class="text-left text-text-muted">
              <th class="font-medium px-2 py-1">When</th>
              <th class="font-medium px-2 py-1">Tool</th>
              <th class="font-medium px-2 py-1">Tier</th>
              <th class="font-medium px-2 py-1">Decision</th>
              <th class="font-medium px-2 py-1">Summary</th>
            </tr>
          </thead>
          <tbody>
            <For each={props.rows()}>
              {(row) => <ApprovalHistoryRowView row={row} />}
            </For>
          </tbody>
        </table>
      </div>
    </Show>
  );
}

function ApprovalHistoryRowView(props: { row: ApprovalHistoryRow }) {
  const tierColor = () => {
    switch (props.row.tier) {
      case "read":
        return "text-green-400";
      case "write":
        return "text-yellow-400";
      case "destructive":
        return "text-red-400";
      default:
        return "text-text-secondary";
    }
  };
  const decisionColor = () => {
    switch (props.row.decision) {
      case "approve":
      case "always_approve":
        return "text-green-400";
      case "reject":
        return "text-red-400";
      case "forgotten":
        return "text-text-muted italic";
      default:
        return "text-text-secondary";
    }
  };

  return (
    <tr class="border-t border-border hover:bg-bg-secondary/30">
      <td class="px-2 py-1 text-text-muted font-mono whitespace-nowrap">
        {formatTimestamp(props.row.timestamp ?? 0)}
      </td>
      <td class="px-2 py-1 text-text-primary font-mono whitespace-nowrap">
        {props.row.tool}
      </td>
      <td class={`px-2 py-1 font-mono uppercase ${tierColor()}`}>
        {props.row.tier}
      </td>
      <td class={`px-2 py-1 font-mono ${decisionColor()}`}>
        {decisionLabel(props.row.decision)}
      </td>
      <td
        class="px-2 py-1 text-text-secondary truncate max-w-xs"
        title={props.row.args_summary}
      >
        {props.row.args_summary || "—"}
      </td>
    </tr>
  );
}

function formatTimestamp(seconds: number): string {
  if (!seconds || !Number.isFinite(seconds)) return "—";
  const d = new Date(seconds * 1000);
  // Local short format. Avoids importing a date library for one panel.
  return d.toLocaleString(undefined, {
    month: "short",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
  });
}

function decisionLabel(decision: string): string {
  switch (decision) {
    case "approve":
      return "Approved";
    case "always_approve":
      return "Always-approved";
    case "reject":
      return "Rejected";
    case "forgotten":
      return "Closed";
    default:
      return decision;
  }
}
