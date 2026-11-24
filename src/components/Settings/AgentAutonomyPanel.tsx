import { createSignal, For, Show, type JSX } from "solid-js";
import { settingsStore, type Settings } from "../../stores/settings";
import { isSecurityCancel } from "../../lib/error";
import { reportActionError } from "../../lib/action-error";
import { toastStore } from "../Toast/Toast";

/** Persist an autonomy change. Resolves `true` when it was saved.
 *
 *  Loosening a safety setting (auto-approve writes, auto-approving a tool,
 *  un-blocking a path) makes the backend show a native OS confirmation the
 *  webview cannot click. Saying no there is a choice, not a fault: we show a
 *  calm notice, and because the store only changes on a successful save,
 *  the panel keeps showing the real stored value. Any other failure is a
 *  real error and is toasted as one. */
async function saveAutonomy(updates: Partial<Settings>, failMessage: string): Promise<boolean> {
  try {
    await settingsStore.save(updates);
    return true;
  } catch (e) {
    if (isSecurityCancel(e)) {
      toastStore.show("Cancelled. Nothing was changed.", "info");
    } else {
      reportActionError(failMessage, e, {
        code: "AGENT_AUTONOMY_SAVE_FAIL",
        module: "ai",
        showReason: true,
      });
    }
    return false;
  }
}

/** Settings → Corvux Agent → Agent Autonomy panel.
 *
 *  Three controls:
 *  - `auto_approve_writes` — whole tier on/off (toggle switch)
 *  - `auto_approve_tools` — per-tool allowlist managed via × buttons
 *  - `use_native_tools` — wire format (toggle switch, experimental)
 *
 *  Reads always auto-run; intentionally not exposed as a toggle so users
 *  can't accidentally put every roost_search behind a confirmation dialog.
 *  Copy is plain-language by design — non-tech users should grasp what
 *  each switch does without reading the doc.
 */
export default function AgentAutonomyPanel() {
  const autonomy = () => settingsStore.settings().agent_autonomy;

  const setAutoApproveWrites = async (enabled: boolean) => {
    await saveAutonomy(
      {
        agent_autonomy: {
          ...autonomy(),
          auto_approve_writes: enabled,
        },
      },
      "Couldn't change auto-save for edits",
    );
  };

  const removeTool = async (tool: string) => {
    await saveAutonomy(
      {
        agent_autonomy: {
          ...autonomy(),
          auto_approve_tools: autonomy().auto_approve_tools.filter((t) => t !== tool),
        },
      },
      "Couldn't update trusted tools",
    );
  };

  const [pendingPattern, setPendingPattern] = createSignal("");

  const addBlockedPath = async () => {
    const next = pendingPattern().trim();
    if (!next) return;
    const existing = autonomy().agent_blocked_paths ?? [];
    if (existing.includes(next)) {
      setPendingPattern("");
      return;
    }
    const saved = await saveAutonomy(
      {
        agent_autonomy: {
          ...autonomy(),
          agent_blocked_paths: [...existing, next],
        },
      },
      "Couldn't block that path",
    );
    // Keep the typed pattern if the save failed so it isn't lost.
    if (saved) setPendingPattern("");
  };

  const removeBlockedPath = async (pattern: string) => {
    await saveAutonomy(
      {
        agent_autonomy: {
          ...autonomy(),
          agent_blocked_paths: (autonomy().agent_blocked_paths ?? []).filter(
            (p) => p !== pattern,
          ),
        },
      },
      "Couldn't unblock that path",
    );
  };

  return (
    <div class="space-y-4 border-t border-border pt-4">
      <div>
        <h4 class="text-text-secondary text-xs font-semibold uppercase tracking-wider">
          What Corvux can do on its own
        </h4>
        <p class="text-text-muted text-[11px] mt-1 leading-relaxed">
          Corvux always asks before changing or deleting your notes. Flip a
          switch below to let it work faster — at your own risk.
        </p>
      </div>

      <ToggleRow
        checked={autonomy().auto_approve_writes}
        onChange={setAutoApproveWrites}
        title="Let Corvux save edits without asking"
        description="It can create and update notes on its own. Deleting still asks every time."
      />

      <Show when={autonomy().auto_approve_tools.length > 0}>
        <div class="space-y-1.5">
          <div class="text-text-secondary text-xs">
            Tools you've trusted ({autonomy().auto_approve_tools.length})
          </div>
          <div class="flex flex-wrap gap-1.5">
            <For each={autonomy().auto_approve_tools}>
              {(tool) => (
                <span class="inline-flex items-center gap-1 bg-bg-secondary border border-border rounded px-2 py-0.5 text-[11px] font-mono">
                  {tool}
                  <button
                    class="text-text-muted hover:text-red-400 leading-none"
                    title={`Stop auto-approving ${tool}`}
                    onClick={() => void removeTool(tool)}
                  >
                    ×
                  </button>
                </span>
              )}
            </For>
          </div>
        </div>
      </Show>

      {/* Blocked paths — hard guardrails that fire BEFORE the approval
          card. Patterns match roost-relative paths; gitignore-style
          globs supported. A blocked write returns a structured error
          to the agent so the LLM is told why and can adjust or move
          on, instead of silently looping on the same path. */}
      <div class="space-y-2 pt-3 border-t border-border/50">
        <div>
          <div class="text-sm text-text-primary">
            Folders &amp; files Corvux must never touch
          </div>
          <div class="text-text-muted text-[11px] mt-0.5 leading-relaxed">
            Any pattern below is permanently off-limits for writes and
            deletes — even if "Let Corvux save edits without asking" is on.
            Reading is still allowed. Use{" "}
            <span class="font-mono">Journal/</span>,{" "}
            <span class="font-mono">**/secret.md</span>, or{" "}
            <span class="font-mono">Finance/*.md</span>-style globs.
          </div>
        </div>
        <div class="flex items-center gap-2">
          <input
            type="text"
            value={pendingPattern()}
            onInput={(e) => setPendingPattern(e.currentTarget.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter") {
                e.preventDefault();
                void addBlockedPath();
              }
            }}
            placeholder="Journal/  or  **/secret.md"
            class="flex-1 min-w-0 bg-bg-secondary border border-border rounded px-2 py-1 text-xs font-mono text-text-primary placeholder:text-text-muted/60 focus:outline-none focus:border-purple/60"
          />
          <button
            onClick={() => void addBlockedPath()}
            disabled={!pendingPattern().trim()}
            class="px-2.5 py-1 bg-bg-secondary border border-border rounded text-[11px] text-text-secondary hover:text-text-primary hover:border-purple/50 transition-colors disabled:opacity-40 disabled:cursor-not-allowed"
          >
            Add
          </button>
        </div>
        <Show when={(autonomy().agent_blocked_paths ?? []).length > 0}>
          <div class="flex flex-wrap gap-1.5">
            <For each={autonomy().agent_blocked_paths ?? []}>
              {(pattern) => (
                <span class="inline-flex items-center gap-1 bg-bg-secondary border border-border rounded px-2 py-0.5 text-[11px] font-mono">
                  {pattern}
                  <button
                    class="text-text-muted hover:text-red-400 leading-none"
                    title={`Stop blocking ${pattern}`}
                    onClick={() => void removeBlockedPath(pattern)}
                  >
                    ×
                  </button>
                </span>
              )}
            </For>
          </div>
        </Show>
      </div>

      {/* Native function-calling — wire-format toggle. Default-on now;
          kept as a toggle during the Phase 2.A burn-in. Copy is plain
          English so it doesn't read as developer-only debug switch. */}
      <div class="pt-3 border-t border-border/50">
        <ToggleRow
          checked={!!settingsStore.settings().use_native_tools}
          onChange={(v) =>
            saveAutonomy({ use_native_tools: v }, "Couldn't change the tool engine").then(() => undefined)
          }
          title="Use the new tool engine"
          description="Faster and fewer mistakes. Recommended. Restart the chat after switching."
          badge="Recommended"
        />
      </div>
    </div>
  );
}

/// Toggle-switch row — pill-shaped track with a sliding thumb,
/// matching the visual language of macOS / Notion / Slack settings.
/// The whole row is the click target (label-wrapped); keyboard users
/// get the native focus ring on the underlying checkbox.
///
/// `onChange` may return a promise (a save). The row is controlled by
/// `checked`: once the handler settles we put the native checkbox back to
/// `props.checked`. If the save was declined or failed, the stored value did
/// not change, so Solid would never re-sync the DOM on its own and the
/// hidden checkbox would keep claiming the new state.
function ToggleRow(props: {
  checked: boolean;
  onChange: (v: boolean) => void | Promise<void>;
  title: string;
  description: string;
  badge?: string;
}): JSX.Element {
  return (
    <label class="flex items-start gap-3 cursor-pointer group">
      <span class="relative inline-flex shrink-0 mt-0.5">
        <input
          type="checkbox"
          checked={props.checked}
          onChange={(e) => {
            const el = e.currentTarget;
            void Promise.resolve(props.onChange(el.checked)).finally(() => {
              el.checked = props.checked;
            });
          }}
          class="sr-only peer"
        />
        <span
          class={`w-9 h-5 rounded-full transition-colors ${
            props.checked
              ? "bg-purple"
              : "bg-bg-tertiary border border-border"
          } peer-focus-visible:ring-2 peer-focus-visible:ring-purple/40`}
          aria-hidden="true"
        />
        <span
          class={`absolute top-0.5 left-0.5 w-4 h-4 rounded-full bg-white shadow transition-transform ${
            props.checked ? "translate-x-4" : "translate-x-0"
          }`}
          aria-hidden="true"
        />
      </span>
      <div class="flex-1 min-w-0">
        <div class="flex items-center gap-2">
          <div class="text-sm text-text-primary">{props.title}</div>
          <Show when={props.badge}>
            <span class="text-[10px] px-1.5 py-0.5 rounded-full bg-purple/15 text-purple border border-purple/30 font-medium">
              {props.badge}
            </span>
          </Show>
        </div>
        <div class="text-text-muted text-[11px] leading-relaxed mt-0.5">
          {props.description}
        </div>
      </div>
    </label>
  );
}
