/// Edit History pane — Phase 2.2.
///
/// Reads `:edit_event` rows for the active note (via the `editHistory`
/// store), renders newest-first with actor badges, and offers per-row
/// revert. Click a row to expand the inline unified diff.
///
/// The diff is rendered as styled lines rather than via `@codemirror/merge`
/// — the unified text from the `similar` crate already encodes +/- per
/// line, and a sidebar pane is too narrow for a side-by-side view.

import { Show, For, createEffect, createSignal } from "solid-js";
import { tabStore } from "../../stores/tabs";
import { editHistoryStore, type EditEvent } from "../../stores/editHistory";

export default function EditHistoryPanel() {
  const [expanded, setExpanded] = createSignal<string | null>(null);
  const [confirmEventId, setConfirmEventId] = createSignal<string | null>(null);
  // Backend's error message for the conflict/divergence case. Shown in
  // the confirmation dialog so the user sees the exact reason ("later
  // edits would be lost" vs "file changed outside Corvux") instead of
  // a generic "Revert anyway?" prompt.
  const [confirmReason, setConfirmReason] = createSignal<string | null>(null);

  function targetPath(): string | null {
    const path = tabStore.activeTabPath();
    if (!path) return null;
    if (!(path.endsWith(".md") || path.endsWith(".markdown"))) return null;
    return path;
  }

  /// `createEffect` fires on mount with the current value AND on every
  /// subsequent tab switch — so opening the pane while a markdown tab is
  /// already active loads its history immediately.
  createEffect(() => {
    void editHistoryStore.load(targetPath());
  });

  function refresh() {
    void editHistoryStore.refresh();
  }

  async function attemptRevert(eventId: string, force: boolean) {
    try {
      await editHistoryStore.revert(eventId, force);
      setConfirmEventId(null);
      setConfirmReason(null);
    } catch (e) {
      const msg = String(e);
      // Both conflict variants (later-events overwrite + external
      // divergence) end the message with "Pass force=true to ...". Catch
      // both, surface the backend's reason so the dialog explains
      // exactly which case the user is overriding.
      if (msg.includes("force=true")) {
        setConfirmEventId(eventId);
        setConfirmReason(msg.replace(/^Error:\s*/, ""));
      }
    }
  }

  return (
    <div class="flex-1 min-h-0 flex flex-col">
      <Header onRefresh={refresh} />
      <Show when={editHistoryStore.error()}>
        <div class="px-3 py-2 text-xs text-red-400 border-b border-border bg-red-500/5 flex items-start gap-2">
          <span class="flex-1">{editHistoryStore.error()}</span>
          <button
            class="text-text-muted hover:text-text-primary"
            onClick={() => editHistoryStore.clearError()}
            aria-label="Dismiss error"
          >
            ×
          </button>
        </div>
      </Show>
      <div class="flex-1 overflow-y-auto">
        <Show
          when={editHistoryStore.loaded()}
          fallback={
            <div class="px-3 py-6 text-xs text-text-muted italic">Loading…</div>
          }
        >
          <Show
            when={editHistoryStore.activePath()}
            fallback={<IdleState />}
          >
            <Show
              when={editHistoryStore.events().length > 0}
              fallback={<EmptyState />}
            >
              <ul class="divide-y divide-border/60">
                <For each={editHistoryStore.events()}>
                  {(ev) => (
                    <EventRow
                      event={ev}
                      expanded={expanded() === ev.eventId}
                      reverting={editHistoryStore.reverting() === ev.eventId}
                      onToggleExpand={() =>
                        setExpanded((cur) => (cur === ev.eventId ? null : ev.eventId))
                      }
                      onRevert={() => void attemptRevert(ev.eventId, false)}
                    />
                  )}
                </For>
              </ul>
            </Show>
          </Show>
        </Show>
      </div>
      <Show when={confirmEventId()}>
        {(id) => (
          <ConfirmRevertDialog
            reason={confirmReason()}
            onConfirm={() => void attemptRevert(id(), true)}
            onCancel={() => {
              setConfirmEventId(null);
              setConfirmReason(null);
              editHistoryStore.clearError();
            }}
          />
        )}
      </Show>
    </div>
  );
}

function Header(props: { onRefresh: () => void }) {
  const count = () => editHistoryStore.events().length;
  return (
    <div class="px-3 py-2 border-b border-border flex items-center justify-between shrink-0">
      <div class="flex items-center gap-1.5 text-xs font-medium text-text-muted uppercase tracking-wider">
        <svg
          width="13"
          height="13"
          viewBox="0 0 24 24"
          fill="none"
          stroke="currentColor"
          stroke-width="2"
        >
          <circle cx="12" cy="12" r="10" />
          <polyline points="12 6 12 12 16 14" />
        </svg>
        <span>History</span>
        <Show when={count() > 0}>
          <span class="text-text-secondary normal-case">({count()})</span>
        </Show>
      </div>
      <button
        title="Refresh"
        aria-label="Refresh history"
        class="text-text-muted hover:text-text-primary p-1 rounded hover:bg-bg-hover transition-colors"
        onClick={props.onRefresh}
      >
        <svg
          width="14"
          height="14"
          viewBox="0 0 24 24"
          fill="none"
          stroke="currentColor"
          stroke-width="2"
        >
          <polyline points="23 4 23 10 17 10" />
          <path d="M20.49 15A9 9 0 1116 5L23 10" />
        </svg>
      </button>
    </div>
  );
}

function IdleState() {
  return (
    <div class="px-3 py-6 text-xs text-text-muted space-y-2">
      <p class="italic">Open a note to see its edit history.</p>
      <p class="opacity-80 leading-relaxed">
        Every save, task toggle, or agent write is recorded here so you can
        review and revert changes.
      </p>
    </div>
  );
}

function EmptyState() {
  return (
    <div class="px-3 py-6 text-xs text-text-muted space-y-2">
      <p class="italic">No history yet for this note.</p>
      <p class="opacity-80 leading-relaxed">
        Edits will appear here the next time you save.
      </p>
    </div>
  );
}

function EventRow(props: {
  event: EditEvent;
  expanded: boolean;
  reverting: boolean;
  onToggleExpand: () => void;
  onRevert: () => void;
}) {
  const ev = () => props.event;
  const intentLabel = () => formatIntent(ev().intent);
  const ageLabel = () => formatRelativeAge(ev().timestamp);
  const isReverted = () => ev().revertedBy.length > 0;
  // External edits (MCP / other app / sync) are visibility-only — the diff is
  // shown but there's no revert (the backend refuses it; the "before" isn't
  // revert-grade). See Actor::External in edit_events.rs.
  const isExternal = () => ev().actorType === "external";

  return (
    <li>
      <div
        class={`px-3 py-2 hover:bg-bg-hover cursor-pointer flex items-start gap-2 ${
          isReverted() ? "opacity-60" : ""
        }`}
        onClick={props.onToggleExpand}
      >
        <ActorBadge actor={ev().actorType} />
        <div class="flex-1 min-w-0 flex flex-col gap-0.5">
          <div class="flex items-center gap-1.5 text-xs">
            <span class="text-text-primary font-medium">{intentLabel()}</span>
            <Show when={isReverted()}>
              <span
                class="text-[10px] text-text-muted italic"
                title={`Reverted by ${ev().revertedBy}`}
              >
                (reverted)
              </span>
            </Show>
            {/* EH-1: reverting this event would roll past newer edits or an
                external change. Not a block — a heads-up that Revert will ask
                to confirm before discarding that work. */}
            <Show when={!isReverted() && ev().needsForce}>
              <span
                class="text-[10px] text-amber-500 italic"
                title="Reverting this will discard newer changes — you'll be asked to confirm"
              >
                (discards newer)
              </span>
            </Show>
            <span class="ml-auto text-[10px] text-text-muted">{ageLabel()}</span>
          </div>
          <div class="flex items-center gap-2 text-[10px] text-text-muted">
            <Show when={ev().linesAdded > 0}>
              <span class="text-green-500">+{ev().linesAdded}</span>
            </Show>
            <Show when={ev().linesRemoved > 0}>
              <span class="text-red-500">−{ev().linesRemoved}</span>
            </Show>
            <span class="opacity-70">{ev().actorId}</span>
          </div>
        </div>
        {/* External edits are visibility-only: no Revert affordance, just a
            muted tag so the row reads as "changed outside the editor". */}
        <Show
          when={!isExternal()}
          fallback={
            <span
              class="text-[10px] text-text-muted italic px-1.5 py-0.5 shrink-0"
              title="Changed outside the editor — shown for visibility, not revertible from here"
            >
              external
            </span>
          }
        >
          <button
            type="button"
            onClick={(e) => {
              e.stopPropagation();
              props.onRevert();
            }}
            disabled={
              props.reverting ||
              ev().revertConflicts ||
              isReverted() ||
              ev().intent === "create"
            }
            title={
              isReverted()
                ? "This event has already been reverted"
                : ev().intent === "create"
                  ? "Reverting a create would empty the file — use delete instead"
                  : ev().needsForce
                    ? "Restore this event's prior content (will ask to confirm — newer changes would be discarded)"
                    : "Restore this event's prior content"
            }
            class="text-[10px] text-text-muted hover:text-accent disabled:opacity-40 disabled:hover:text-text-muted px-1.5 py-0.5 rounded hover:bg-bg-hover transition-colors shrink-0"
          >
            <Show when={props.reverting} fallback={<span>Revert</span>}>
              <span class="italic">…</span>
            </Show>
          </button>
        </Show>
      </div>
      <Show when={props.expanded}>
        <DiffView diff={ev().diff} />
      </Show>
    </li>
  );
}

function ActorBadge(props: { actor: string }) {
  const tone = () => {
    switch (props.actor) {
      case "agent":
        return {
          bg: "bg-purple/20",
          text: "text-purple",
          label: "AI",
          title: "Edit by an AI agent",
        };
      case "system":
        return {
          bg: "bg-text-muted/20",
          text: "text-text-muted",
          label: "SYS",
          title: "Edit by the system (sync, watcher, …)",
        };
      case "external":
        return {
          bg: "bg-sky-500/20",
          text: "text-sky-500",
          label: "EXT",
          title:
            "Changed outside the editor (MCP, another app, or sync). Shown for visibility — not revertible from here.",
        };
      default:
        return {
          bg: "bg-accent/20",
          text: "text-accent",
          label: "YOU",
          title: "Edit by you",
        };
    }
  };
  const t = tone();
  return (
    <span
      class={`inline-flex items-center justify-center text-[9px] font-semibold uppercase tracking-wider rounded px-1.5 py-0.5 mt-0.5 shrink-0 ${t.bg} ${t.text}`}
      title={t.title}
    >
      {t.label}
    </span>
  );
}

/// Inline unified-diff renderer. Splits the diff text into lines and
/// styles each by its leading character. Hunk headers (`@@ ... @@`) get
/// a muted style. We keep the diff readable in a narrow pane by allowing
/// horizontal scroll on long lines rather than wrapping.
function DiffView(props: { diff: string }) {
  const lines = () => props.diff.split("\n");
  return (
    <pre class="text-[11px] font-mono leading-snug bg-bg-tertiary/50 border-y border-border/60 px-3 py-2 overflow-x-auto whitespace-pre">
      <For each={lines()}>
        {(line) => {
          if (line.startsWith("@@")) {
            return <div class="text-text-muted italic">{line || " "}</div>;
          }
          if (line.startsWith("+++") || line.startsWith("---")) {
            return <div class="text-text-muted opacity-70">{line || " "}</div>;
          }
          if (line.startsWith("+")) {
            return (
              <div class="text-green-500 bg-green-500/5">{line || " "}</div>
            );
          }
          if (line.startsWith("-")) {
            return <div class="text-red-500 bg-red-500/5">{line || " "}</div>;
          }
          return <div class="text-text-secondary">{line || " "}</div>;
        }}
      </For>
    </pre>
  );
}

function ConfirmRevertDialog(props: {
  /** Backend's error string explaining why the first attempt was rejected.
   *  When present we show it verbatim so the user knows whether they're
   *  overriding "later edits" or "external file changes". When null we
   *  fall back to the generic copy. */
  reason?: string | null;
  onConfirm: () => void;
  onCancel: () => void;
}) {
  // Strip the trailing "Pass force=true ..." sentence — the dialog's
  // Revert button is itself the "force=true" choice; restating it in
  // body text is noise.
  const cleanReason = () => {
    const r = props.reason;
    if (!r) return null;
    return r.replace(/\s*Pass force=true[^.]*\.?\s*$/i, "").trim();
  };
  return (
    <div class="absolute inset-0 z-30 flex items-center justify-center bg-black/40">
      <div class="bg-bg-secondary border border-border rounded-md shadow-lg w-80 p-4 space-y-3">
        <div class="text-sm font-medium text-text-primary">Revert anyway?</div>
        <div class="text-xs text-text-muted leading-relaxed">
          <Show
            when={cleanReason()}
            fallback={
              <>
                Edits made after this point will be lost. The revert will be
                recorded in the history so you can undo it later.
              </>
            }
          >
            {(r) => (
              <>
                {r()} The revert will be recorded in the history so you can
                undo it later.
              </>
            )}
          </Show>
        </div>
        <div class="flex items-center justify-end gap-2">
          <button
            class="text-xs px-2 py-1 rounded text-text-muted hover:text-text-primary hover:bg-bg-hover"
            onClick={props.onCancel}
          >
            Cancel
          </button>
          <button
            class="text-xs px-2 py-1 rounded text-bg-primary bg-accent hover:bg-accent/90"
            onClick={props.onConfirm}
          >
            Revert
          </button>
        </div>
      </div>
    </div>
  );
}

function formatIntent(intent: string): string {
  switch (intent) {
    case "edit":
      return "Edit";
    case "create":
      return "Create";
    case "delete":
      return "Delete";
    case "toggle_task":
      return "Toggle task";
    case "add_task":
      return "Add task";
    case "rename":
      return "Rename";
    case "agent_write":
      return "Agent write";
    case "revert":
      return "↩ Reverted";
    default:
      return intent;
  }
}

/// Format a unix-seconds timestamp as a relative age. Coarse — the
/// History pane is a low-frequency surface, so "5 min ago" is enough.
function formatRelativeAge(epochSeconds: number): string {
  const now = Date.now() / 1000;
  const delta = Math.max(0, now - epochSeconds);
  if (delta < 60) return "just now";
  if (delta < 3600) return `${Math.floor(delta / 60)} min ago`;
  if (delta < 86400) return `${Math.floor(delta / 3600)} h ago`;
  if (delta < 86400 * 7) return `${Math.floor(delta / 86400)} d ago`;
  const d = new Date(epochSeconds * 1000);
  return d.toLocaleDateString();
}
