import { createResource, createSignal, For, Show } from "solid-js";
import { commands, unwrap } from "../../lib/invoke";
import type { AgentsMdStatus } from "../../lib/bindings";
import { log } from "../../lib/log";
import { tabStore } from "../../stores/tabs";

/** Settings → AI → Roost Instructions panel.
 *
 *  Surfaces the status of `{roost}/AGENTS.md` — the user-curated
 *  markdown file the system prompt loads every turn. Pattern follows
 *  Claude Code's `CLAUDE.md` and Cursor's `.cursorrules`. See
 *  `agent-hardening-and-self-improvement.md` § 5.
 *
 *  Three states:
 *  - **Not created** — render virtual empty state with a single
 *    "Create AGENTS.md" button. Per § 5.13, NEVER auto-create.
 *  - **Present and clean** — show file path + size + "Open in editor".
 *  - **Present with warnings** — same plus a red banner if the loader
 *    stripped sanitized tags or truncated the body.
 */
const EMPTY_STATUS: AgentsMdStatus = {
  exists: false,
  path: null,
  body_bytes: null,
  truncated: false,
  stripped_tags: [],
};

export default function RoostInstructionsPanel() {
  const [status, { refetch }] = createResource(
    async () => {
      try {
        return await unwrap(commands.agentsMdStatus());
      } catch (e) {
        log.warn("settings", "agents_md_status failed", e);
        return EMPTY_STATUS;
      }
    },
    { initialValue: EMPTY_STATUS },
  );
  const [creating, setCreating] = createSignal(false);
  const [createError, setCreateError] = createSignal<string | null>(null);

  const handleCreate = async () => {
    setCreating(true);
    setCreateError(null);
    try {
      const path = await unwrap(commands.createAgentsMd());
      // Refresh the status so the UI flips into "Present" state.
      await refetch();
      // Open the new file in a tab so the user can edit it immediately.
      if (path) {
        await tabStore.openTab(path);
      }
    } catch (e) {
      setCreateError(String(e));
    } finally {
      setCreating(false);
    }
  };

  const handleOpen = async () => {
    const s = status();
    if (s?.path) {
      await tabStore.openTab(s.path);
    }
  };

  return (
    <div class="space-y-3 border-t border-border pt-4">
      <div>
        <h4 class="text-text-secondary text-xs font-semibold uppercase tracking-wider">
          Roost instructions
        </h4>
        <p class="text-text-muted text-[11px] mt-1 leading-relaxed">
          Teach the agent how your roost works once, instead of repeating
          yourself every chat. One line per rule, plain English — folder
          layout, tag conventions, daily-note format, tone preferences.
          The agent reads it before every reply.
        </p>
        <p class="text-text-muted/70 text-[10px] mt-1">
          Stored at <code class="text-text-muted">AGENTS.md</code> in your
          roost root. Up to 4 KB.
        </p>
      </div>

      <Show
        when={status()?.exists}
        fallback={
          <div class="space-y-2">
            <div class="text-xs text-text-muted italic">
              No AGENTS.md in this roost yet.
            </div>
            <button
              type="button"
              class="bg-purple/15 hover:bg-purple/25 text-purple border border-purple/30 rounded px-3 py-1 text-xs font-medium disabled:opacity-50"
              disabled={creating()}
              onClick={() => void handleCreate()}
            >
              {creating() ? "Creating…" : "Create AGENTS.md"}
            </button>
            <Show when={createError()}>
              <div class="text-red-400 text-[11px]">{createError()}</div>
            </Show>
          </div>
        }
      >
        <div class="space-y-2">
          <div
            class="flex items-baseline gap-2 text-xs"
            title={status()?.path ?? ""}
          >
            <span class="text-green-400" aria-hidden="true">●</span>
            <span class="text-text-secondary">
              Active — {formatBytes(status()?.body_bytes ?? 0)} loaded each turn
            </span>
          </div>

          <Show when={status()?.truncated}>
            <div class="text-yellow-400 text-[11px]">
              ⚠ Your AGENTS.md is over 4 KB; only the first 4 KB is loaded.
              Shorten it to be sure the agent sees everything.
            </div>
          </Show>

          <Show when={(status()?.stripped_tags?.length ?? 0) > 0}>
            <div class="border border-red-400/40 bg-red-500/10 rounded p-2 text-[11px]">
              <div class="text-red-400 font-semibold mb-0.5">
                Sanitizer stripped tag families before load
              </div>
              <div class="text-text-secondary font-mono">
                <For each={status()?.stripped_tags ?? []}>
                  {(t) => <span class="mr-2">{t}</span>}
                </For>
              </div>
              <div class="text-text-muted mt-1">
                These tag families are reserved for the agent harness. Their
                contents in your file are still loaded (only the brackets
                were dropped).
              </div>
            </div>
          </Show>

          <div class="flex items-center gap-2">
            <button
              type="button"
              class="inline-flex items-center gap-1.5 bg-purple/15 hover:bg-purple/25 text-purple border border-purple/30 rounded px-3 py-1.5 text-xs font-medium transition-colors"
              onClick={() => void handleOpen()}
              title="Open AGENTS.md in a tab"
            >
              <svg
                width="12"
                height="12"
                viewBox="0 0 24 24"
                fill="none"
                stroke="currentColor"
                stroke-width="2"
                stroke-linecap="round"
                stroke-linejoin="round"
                aria-hidden="true"
              >
                <path d="M11 4H4a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h14a2 2 0 0 0 2-2v-7" />
                <path d="M18.5 2.5a2.121 2.121 0 0 1 3 3L12 15l-4 1 1-4 9.5-9.5z" />
              </svg>
              Edit instructions
            </button>
            <button
              type="button"
              class="text-text-muted hover:text-text-secondary text-[11px] underline-offset-2 hover:underline"
              onClick={() => void refetch()}
              title="Re-read AGENTS.md from disk"
            >
              Refresh
            </button>
          </div>
        </div>
      </Show>
    </div>
  );
}

function formatBytes(n: number): string {
  if (!n) return "0 B";
  if (n < 1024) return `${n} B`;
  return `${(n / 1024).toFixed(1)} KB`;
}
