import { For, onMount, Show } from "solid-js";
import { settingsStore } from "../../stores/settings";
import AgentAutonomyPanel from "./AgentAutonomyPanel";
import AILearningPanel from "./AILearningPanel";
import ApprovalHistoryPanel from "./ApprovalHistoryPanel";
import ToolStatsPanel from "./ToolStatsPanel";
import RoostInstructionsPanel from "./RoostInstructionsPanel";

/**
 * Settings → Corvux Agent.
 *
 * Behavior-side companion to AI Models. Anything about HOW the agent
 * acts — approval policy, roost conventions, tool reliability, audit
 * history — lives here. Wiring (provider, keys, model, embedding
 * backend) stays in AI Models so the user has one tab they set once
 * and forget.
 */
export default function CorvuxAgentSection() {
  // Lazy-load profiles on first render — chat panel does this too but
  // the user may open Settings before they ever open the chat panel.
  onMount(() => {
    if (settingsStore.agentProfiles().length === 0) {
      settingsStore.loadAgentProfiles();
    }
  });

  const activeProfileId = () =>
    settingsStore.settings().agent_profile ?? "default";
  const activeProfile = () =>
    settingsStore.agentProfiles().find((p) => p.id === activeProfileId());

  return (
    <section>
      <h3 class="text-text-secondary text-xs font-semibold uppercase tracking-wider mb-1">
        Corvux Agent
      </h3>
      <p class="text-text-muted text-[11px] mb-4 leading-relaxed">
        How the agent behaves day-to-day — when it asks before acting,
        what conventions your roost follows, and which tools have proven
        themselves over time.
      </p>

      <div class="space-y-3">
        {/* Default profile — picks the bundle of tools the agent uses
            and how long it's allowed to work on a single answer.
            Moved here from the chat panel on 2026-05-20 (no one
            switched per-chat). */}
        <Show when={settingsStore.agentProfiles().length > 0}>
          <div class="space-y-1.5">
            <div class="flex items-baseline justify-between gap-3">
              <div class="min-w-0">
                <div class="text-text-primary text-sm">Default profile</div>
                <div class="text-text-muted text-[11px]">
                  Sets the tradeoff between speed, depth, and what kinds of
                  tasks the agent is shaped for.
                </div>
              </div>
              <select
                value={activeProfileId()}
                onChange={(e) =>
                  void settingsStore.setAgentProfile(e.currentTarget.value)
                }
                class="shrink-0 bg-bg-secondary border border-border rounded px-2 py-1 text-xs text-text-primary focus:outline-none focus:border-purple"
              >
                <For each={settingsStore.agentProfiles()}>
                  {(p) => <option value={p.id}>{p.label}</option>}
                </For>
              </select>
            </div>
            <Show when={activeProfile()?.summary}>
              <div class="text-text-muted text-[11px] italic pl-0.5">
                {activeProfile()?.summary}
              </div>
            </Show>
          </div>
        </Show>

        {/* Autonomy — when the agent pauses for approval, always-approve
            list, native function-calling toggle. */}
        <AgentAutonomyPanel />

        {/* Roost instructions — AGENTS.md surface. */}
        <RoostInstructionsPanel />

        {/* AI Learning — accumulated per-note signal from approval
            decisions. Drives the RAG retrieval re-rank: notes you've
            approved float up, rejected ones sink. Phase 2.4
            self-evolution loop. */}
        <AILearningPanel />

        {/* Tool reliability — per-tool approval rates. Drives the
            "low confidence" qualifier the agent loop adds to the
            system prompt for tools the user rejects often. */}
        <ToolStatsPanel />

        {/* Approval history — read-only audit log of every decision
            the user made on tool cards. */}
        <ApprovalHistoryPanel />
      </div>
    </section>
  );
}
