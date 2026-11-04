import { Show, For, createSignal, createMemo } from "solid-js";
import {
  chatStore,
  type PendingTool,
  type DiffPreview,
  type SecretWarning,
  type SecretKind,
} from "../../stores/chat";

/// Tool-approval card. Rendered in the chat panel for every entry in
/// `chatStore.pendingTools()`. Resolves via `decideTool(...)`, which fires
/// the backend `decide_tool` IPC and optimistically clears the card.
///
/// Three buttons:
/// - **Approve** — run this one tool call.
/// - **Reject**  — agent receives a "user rejected" tool result and can
///   adjust args or move on.
/// - **Always approve** — adds the tool name to `agent_autonomy.auto_approve_tools`
///   so future calls of THIS tool skip the gate. Hidden for `destructive`
///   tier — destructive deletes always require explicit confirmation.
///
/// For write tools (`edit_note`, `roost_write`) the backend pre-computes
/// a unified diff and ships it in `pending.preview`. The card renders the
/// diff above the args block so the user can see exactly what changes
/// before deciding.
export default function ToolCard(props: { pending: PendingTool }) {
  const tier = () => props.pending.tier;
  const tierColor = () => {
    switch (tier()) {
      case "read":
        return "border-green-400/50 bg-green-400/5";
      case "write":
        return "border-yellow-400/50 bg-yellow-400/5";
      case "destructive":
        return "border-red-400/50 bg-red-400/5";
    }
  };
  const tierBadge = () => {
    switch (tier()) {
      case "read":
        return "bg-green-400/15 text-green-400";
      case "write":
        return "bg-yellow-400/15 text-yellow-400";
      case "destructive":
        return "bg-red-400/15 text-red-400";
    }
  };

  // Hide `new_content`/`body` from the args block — those are already
  // visualized in the diff preview, repeating them as raw text just
  // bloats the card.
  const argEntries = () => {
    const hide = props.pending.preview
      ? new Set(["new_content", "body"])
      : new Set<string>();
    return Object.entries(props.pending.args)
      .filter(([k, v]) => !hide.has(k) && v !== null && v !== undefined && v !== "")
      .map(([k, v]) => [k, formatValue(v)] as const);
  };

  const onApprove = () => chatStore.decideTool(props.pending.request_id, "approve");
  const onReject = () => chatStore.decideTool(props.pending.request_id, "reject");
  const onAlways = () =>
    chatStore.decideTool(props.pending.request_id, "always_approve");

  return (
    <div class={`border rounded-lg p-3 my-2 ${tierColor()}`}>
      <div class="flex items-center justify-between mb-2">
        <div class="flex items-center gap-2 min-w-0">
          <span
            class={`text-[10px] uppercase font-semibold rounded px-1.5 py-0.5 ${tierBadge()}`}
          >
            {tier()}
          </span>
          <code class="text-xs font-mono text-text-secondary truncate">
            {props.pending.tool}
          </code>
        </div>
        <span class="text-text-muted text-[10px] italic">awaiting approval</span>
      </div>

      <div class="text-sm text-text-primary mb-2 truncate">
        {props.pending.summary}
      </div>

      <Show when={(props.pending.warnings ?? []).length > 0}>
        <SecretWarningsBanner warnings={props.pending.warnings ?? []} />
      </Show>

      <Show when={props.pending.preview}>
        {(preview) => <DiffPanel preview={preview()} />}
      </Show>

      <Show when={argEntries().length > 0}>
        <div class="bg-bg-secondary/50 rounded p-2 mb-2 space-y-1">
          <For each={argEntries()}>
            {([k, v]) => (
              <div class="flex gap-2 text-xs">
                <span class="text-text-muted font-mono shrink-0">{k}:</span>
                <span class="text-text-primary font-mono break-all whitespace-pre-wrap">
                  {v}
                </span>
              </div>
            )}
          </For>
        </div>
      </Show>

      <Show when={props.pending.file_path && !props.pending.preview}>
        <div class="text-text-muted text-[11px] mb-2">
          Affects: <code class="text-text-secondary">{props.pending.file_path}</code>
        </div>
      </Show>

      <div class="flex items-center gap-2">
        <button
          class="bg-green-500/15 hover:bg-green-500/25 text-green-400 border border-green-500/30 rounded px-4 sm:px-3 py-1 min-h-[44px] sm:min-h-0 inline-flex items-center justify-center text-xs font-medium"
          onClick={onApprove}
        >
          Approve
        </button>
        <button
          class="bg-bg-secondary hover:bg-bg-tertiary border border-border rounded px-4 sm:px-3 py-1 min-h-[44px] sm:min-h-0 inline-flex items-center justify-center text-xs"
          onClick={onReject}
        >
          Reject
        </button>
        <Show when={tier() !== "destructive"}>
          <button
            class="text-text-muted hover:text-text-secondary text-[11px] ml-auto inline-flex items-center min-h-[44px] sm:min-h-0"
            onClick={onAlways}
            title={`Add ${props.pending.tool} to your always-approve list`}
          >
            Always approve {props.pending.tool}
          </button>
        </Show>
      </div>
    </div>
  );
}

/// Render the unified diff with line-level coloring. Long diffs collapse
/// behind a "show all N lines" toggle to keep the card compact — the
/// trust gate is the *visibility* of the change, not necessarily seeing
/// every line in the chat scroll.
const VISIBLE_LINE_CAP = 24;

function DiffPanel(props: { preview: DiffPreview }) {
  const [expanded, setExpanded] = createSignal(false);
  const lines = createMemo(() => parseUnifiedDiff(props.preview.unified_diff));
  const visible = () =>
    expanded() ? lines() : lines().slice(0, VISIBLE_LINE_CAP);
  const overflow = () => lines().length - VISIBLE_LINE_CAP;
  const verb = () => (props.preview.kind === "create" ? "Create" : "Edit");

  return (
    <div class="border border-border rounded mb-2 overflow-hidden">
      <div class="flex items-center gap-2 px-2 py-1 bg-bg-secondary/50 text-[11px] text-text-secondary">
        <span class="font-mono uppercase tracking-wide">{verb()}</span>
        <code class="text-text-primary truncate flex-1">{props.preview.path}</code>
        <span class="text-green-400 font-mono shrink-0">
          +{props.preview.lines_added}
        </span>
        <span class="text-red-400 font-mono shrink-0">
          −{props.preview.lines_removed}
        </span>
      </div>
      <div class="font-mono text-[11px] leading-snug whitespace-pre overflow-x-auto bg-bg-primary/40">
        <For each={visible()}>
          {(line) => (
            <div class={diffLineClass(line.tag)}>
              <span class="inline-block w-4 text-text-muted shrink-0">
                {diffMarker(line.tag)}
              </span>
              <span>{line.text}</span>
            </div>
          )}
        </For>
        <Show when={overflow() > 0 && !expanded()}>
          <button
            class="block w-full text-left px-2 py-1 text-text-muted hover:text-text-secondary border-t border-border bg-bg-secondary/30"
            onClick={() => setExpanded(true)}
          >
            …show {overflow()} more line{overflow() === 1 ? "" : "s"}
          </button>
        </Show>
      </div>
    </div>
  );
}

interface DiffLine {
  tag: "add" | "del" | "ctx" | "hunk";
  text: string;
}

/// Parse the unified-diff string into typed lines. Skips the `--- before`
/// / `+++ after` header rows since they don't carry information the user
/// needs (the path is already shown in the panel header). Treats `@@`
/// hunks as a separator row so the user can see "this region changed,
/// then this one."
function parseUnifiedDiff(diff: string): DiffLine[] {
  const out: DiffLine[] = [];
  for (const raw of diff.split("\n")) {
    if (raw.startsWith("--- ") || raw.startsWith("+++ ")) continue;
    if (raw.startsWith("@@")) {
      out.push({ tag: "hunk", text: raw });
    } else if (raw.startsWith("+")) {
      out.push({ tag: "add", text: raw.slice(1) });
    } else if (raw.startsWith("-")) {
      out.push({ tag: "del", text: raw.slice(1) });
    } else if (raw.length === 0) {
      // similar's unified_diff emits a trailing blank line; drop it.
      continue;
    } else {
      // Context lines start with a leading space; trim it.
      out.push({ tag: "ctx", text: raw.startsWith(" ") ? raw.slice(1) : raw });
    }
  }
  return out;
}

function diffLineClass(tag: DiffLine["tag"]): string {
  switch (tag) {
    case "add":
      return "px-2 bg-green-500/10 text-green-300";
    case "del":
      return "px-2 bg-red-500/10 text-red-300 line-through-subtle";
    case "hunk":
      return "px-2 bg-bg-secondary/50 text-text-muted italic";
    case "ctx":
      return "px-2 text-text-secondary";
  }
}

function diffMarker(tag: DiffLine["tag"]): string {
  switch (tag) {
    case "add":
      return "+";
    case "del":
      return "−";
    case "hunk":
      return "@";
    case "ctx":
      return " ";
  }
}

/// Render secret-scanner hits as a red warning banner above the diff.
/// One row per warning kind (deduped by the backend already); shows the
/// dotted args path + masked preview so the user can spot a smuggled
/// credential without it being readable in plaintext. T6 / T7 defense
/// per `agent-hardening-and-self-improvement.md` § 3.4.3.
function SecretWarningsBanner(props: { warnings: SecretWarning[] }) {
  return (
    <div class="border border-red-400/50 bg-red-500/10 rounded p-2 mb-2">
      <div class="flex items-center gap-1.5 text-red-400 text-[11px] font-semibold uppercase tracking-wide mb-1">
        <svg width="11" height="11" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
          <path d="M12 9v2m0 4h.01M5 12a7 7 0 1014 0 7 7 0 00-14 0z" />
        </svg>
        Possible secret in this call
      </div>
      <For each={props.warnings}>
        {(w) => (
          <div class="flex items-baseline gap-2 text-[11px] font-mono py-0.5">
            <span class="text-red-300 shrink-0">{secretLabel(w.kind)}</span>
            <span class="text-text-muted shrink-0">at</span>
            <span class="text-text-secondary truncate">{w.field}</span>
            <span class="text-text-muted">·</span>
            <span class="text-red-200 truncate">{w.preview}</span>
          </div>
        )}
      </For>
      <div class="text-[10px] text-text-muted mt-1">
        Review carefully — if you didn't intend to share this, click Reject.
      </div>
    </div>
  );
}

function secretLabel(kind: SecretKind): string {
  switch (kind) {
    case "aws_access_key_id":
      return "AWS key shape";
    case "gemini_api_key":
      return "Gemini key shape";
    case "jwt_like":
      return "JWT shape";
    case "base64_blob":
      return "long base64";
    case "url_credentials":
      return "URL w/ creds";
    case "long_hex":
      return "long hex blob";
  }
}

/// Render any JSON value as a short display string. Strings pass through;
/// arrays / objects get one-line JSON; anything else uses its serialized
/// form. Trims to 200 chars so the card stays compact even when the LLM
/// passes a giant blob.
function formatValue(v: unknown): string {
  if (typeof v === "string") {
    return v.length > 200 ? `${v.slice(0, 200)}…` : v;
  }
  try {
    const s = JSON.stringify(v);
    return s.length > 200 ? `${s.slice(0, 200)}…` : s;
  } catch {
    return String(v);
  }
}
