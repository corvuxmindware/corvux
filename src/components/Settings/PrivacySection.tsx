import { For, Show, createMemo, createSignal } from "solid-js";
import { settingsStore } from "../../stores/settings";
import { commands, unwrap } from "../../lib/invoke";
import { toastStore } from "../Toast/Toast";

/**
 * Privacy panel — the "what leaves my device, what stays" surface.
 *
 * The license / docs make this claim; users don't read either. This
 * panel reflects the *actual current settings* into the trust model so
 * a user can answer in 5 seconds: "if I send a chat right now, does it
 * leave my machine? where does it go? what gets sent with it?"
 *
 * Rules of thumb baked into the rows below:
 * - Roost files always stay on disk; we never upload markdown wholesale.
 *   Chat sends only the tool-result snippets the agent retrieves, plus
 *   the user's typed prompt and message history.
 * - Search is full-text (FTS5) and runs entirely on-device; nothing is
 *   sent to a cloud embedding endpoint.
 * - Chat itself is currently cloud-only; the local LLM path is built
 *   but unwired (locked decision #4 — phase 1 was memory-bound).
 *   Surface this honestly so the welcome-note "AI is opt-in" copy
 *   doesn't outpace reality.
 *
 * Anything user-visible here should reflect *settings* and not be
 * hardcoded marketing copy. If the user switches provider or unsets a
 * key, this panel should update on the next render.
 */
export default function PrivacySection() {
  const settings = settingsStore.settings;

  // EH-3: edit-history purge / export controls. Both are roost-wide and
  // device-local (history never syncs), so a purge is a real erase.
  const [clearingHistory, setClearingHistory] = createSignal(false);
  const [exportingHistory, setExportingHistory] = createSignal(false);

  async function handleExportEditHistory() {
    setExportingHistory(true);
    try {
      // `null` = whole roost.
      const json = await unwrap(commands.exportEditHistory(null));
      await navigator.clipboard.writeText(json);
      toastStore.show(
        "Edit history exported as JSON to your clipboard.",
        "success",
      );
    } catch (e) {
      toastStore.show(`Export failed: ${e}`, "error");
    } finally {
      setExportingHistory(false);
    }
  }

  async function handleClearEditHistory() {
    if (
      !confirm(
        "Clear ALL edit history for this roost?\n\n" +
          "This permanently deletes the revertable edit/diff log kept on this " +
          "device. Your notes themselves are NOT changed, but past versions " +
          "can no longer be reverted from the History pane. This cannot be undone.",
      )
    ) {
      return;
    }
    setClearingHistory(true);
    try {
      const removed = await unwrap(commands.purgeRoostEditHistory());
      toastStore.show(
        `Edit history cleared — ${removed} event${removed === 1 ? "" : "s"} removed.`,
        "success",
      );
    } catch (e) {
      toastStore.show(`Clear failed: ${e}`, "error");
    } finally {
      setClearingHistory(false);
    }
  }

  const chatProvider = () => settings().llm_provider;
  const chatModel = () => {
    switch (chatProvider()) {
      case "openrouter":
        return settings().openrouter_model;
      case "openai":
        return settings().openai_model;
      case "anthropic":
        return settings().anthropic_model;
      case "ollama":
        return settings().ollama_model;
      default:
        return settings().model;
    }
  };
  const hasGeminiKey = () => !!settings().gemini_api_key;
  const hasOpenrouterKey = () => !!settings().openrouter_api_key;
  const hasOpenaiKey = () => !!settings().openai_api_key;
  const hasAnthropicKey = () => !!settings().anthropic_api_key;

  // AUDIT-047: every `LlmProvider` variant must have an arm. The prior
  // switch covered gemini + openrouter only; ollama fell through to
  // `undefined` and `chatDestination().label` threw on access.
  const chatDestination = createMemo(() => {
    switch (chatProvider()) {
      case "gemini":
        return {
          kind: "cloud" as const,
          label: "Google Gemini API",
          host: "generativelanguage.googleapis.com",
          keyConfigured: hasGeminiKey(),
        };
      case "openrouter":
        return {
          kind: "cloud" as const,
          label: "OpenRouter",
          host: "openrouter.ai",
          keyConfigured: hasOpenrouterKey(),
        };
      case "openai":
        return {
          kind: "cloud" as const,
          label: "OpenAI API",
          host: "api.openai.com",
          keyConfigured: hasOpenaiKey(),
        };
      case "anthropic":
        return {
          kind: "cloud" as const,
          label: "Anthropic (Claude) API",
          host: "api.anthropic.com",
          keyConfigured: hasAnthropicKey(),
        };
      case "ollama":
        return {
          kind: "local" as const,
          label: "Your machine (local Ollama)",
          host: "127.0.0.1:11434",
          // Ollama has no key concept; treat as always-configured so the
          // "missing API key" banner doesn't show.
          keyConfigured: true,
        };
    }
  });

  const storedKeys = createMemo(() => {
    const rows: { label: string; present: boolean; storage: string }[] = [];
    rows.push({
      label: "Gemini API key",
      present: hasGeminiKey(),
      storage:
        "OS keychain (desktop) / device-tied app-private storage (mobile — encrypted under a per-device seed; not hardware-backed yet)",
    });
    rows.push({
      label: "OpenRouter API key",
      present: hasOpenrouterKey(),
      storage:
        "OS keychain (desktop) / device-tied app-private storage (mobile — encrypted under a per-device seed; not hardware-backed yet)",
    });
    rows.push({
      label: "OpenAI API key",
      present: hasOpenaiKey(),
      storage:
        "OS keychain (desktop) / device-tied app-private storage (mobile — encrypted under a per-device seed; not hardware-backed yet)",
    });
    rows.push({
      label: "Anthropic API key",
      present: hasAnthropicKey(),
      storage:
        "OS keychain (desktop) / device-tied app-private storage (mobile — encrypted under a per-device seed; not hardware-backed yet)",
    });
    return rows;
  });

  return (
    <section class="space-y-6">
      <header>
        <h3 class="text-text-secondary text-xs font-semibold uppercase tracking-wider mb-1">
          Privacy
        </h3>
        <p class="text-text-muted text-xs leading-relaxed">
          What leaves your device right now, given your current settings.
          Switch providers in <strong class="text-text-secondary">AI Models</strong> to
          change.
        </p>
      </header>

      {/* Chat */}
      <div class="border border-border rounded-lg p-3 space-y-2">
        <div class="flex items-center justify-between gap-2">
          <span class="text-sm font-medium text-text-primary">Chat</span>
          <DataFlowBadge kind={chatDestination()?.kind ?? "cloud"} />
        </div>
        <p class="text-xs text-text-secondary leading-relaxed">
          Each message you send goes to{" "}
          <strong class="text-text-primary">{chatDestination()?.label}</strong>
          {chatDestination()?.kind === "cloud" ? " over HTTPS" : ""}, using the model{" "}
          <code class="text-text-primary font-mono text-[11px]">{chatModel()}</code>.
          Sent with the message: your prompt, the chat history of this session, and any
          note snippets the agent's tools retrieved (capped — never the whole roost).
        </p>
        <Show when={!chatDestination()?.keyConfigured}>
          <p class="text-[11px] text-amber-400">
            No API key set yet. Chat won't work until you add one in AI Models.
          </p>
        </Show>
        <p class="text-[11px] text-text-muted">
          Endpoint: <code class="font-mono">{chatDestination()?.host}</code>
        </p>
      </div>

      {/* Search */}
      <div class="border border-border rounded-lg p-3 space-y-2">
        <div class="flex items-center justify-between gap-2">
          <span class="text-sm font-medium text-text-primary">
            Search &amp; indexing
          </span>
          <DataFlowBadge kind="local" />
        </div>
        <p class="text-xs text-text-secondary leading-relaxed">
          Search is full-text (SQLite FTS5) and runs entirely on this device.
          Your notes are never sent to a cloud endpoint to be indexed.
        </p>
      </div>

      {/* Stays local */}
      <div class="border border-border rounded-lg p-3 space-y-2">
        <div class="flex items-center justify-between gap-2">
          <span class="text-sm font-medium text-text-primary">Stays on this device</span>
          <DataFlowBadge kind="local" />
        </div>
        <ul class="text-xs text-text-secondary leading-relaxed space-y-1 list-disc pl-5">
          <li>Your roost — all markdown files, attachments, daily notes.</li>
          <li>The graph database (CozoDB) — links, tags, tasks.</li>
          <li>Full-text search index (SQLite FTS5).</li>
          <li>Chat session history (saved as files inside your roost).</li>
          <li>Edit history with revertable diffs.</li>
        </ul>
      </div>

      {/* EH-3 / EH-4: edit-history controls + on-device disclosure. */}
      <div class="border border-border rounded-lg p-3 space-y-2">
        <div class="flex items-center justify-between gap-2">
          <span class="text-sm font-medium text-text-primary">Edit history</span>
          <DataFlowBadge kind="local" />
        </div>
        <p class="text-xs text-text-secondary leading-relaxed">
          Every save is recorded as a revertable diff so you can roll a note
          back from the History pane. This log is stored{" "}
          <strong class="text-text-primary">only on this device and never syncs</strong>{" "}
          — up to 500 edits are kept per note. Export it as JSON for your own
          records, or clear it entirely.
        </p>
        <div class="flex flex-wrap gap-2 pt-1">
          <button
            onClick={handleExportEditHistory}
            disabled={exportingHistory()}
            class="flex items-center justify-center gap-2 px-3 py-2 rounded-lg border border-border bg-bg-secondary/60 text-text-secondary text-sm hover:bg-bg-hover hover:text-text-primary disabled:opacity-50 disabled:cursor-wait transition-colors"
            aria-label="Export all edit history as JSON to the clipboard"
          >
            {exportingHistory() ? "Exporting…" : "Export edit history"}
          </button>
          <button
            onClick={handleClearEditHistory}
            disabled={clearingHistory()}
            class="flex items-center justify-center gap-2 px-3 py-2 rounded-lg border border-red-400/40 bg-red-400/5 text-red-400 text-sm hover:bg-red-400/10 disabled:opacity-50 disabled:cursor-wait transition-colors"
            aria-label="Permanently clear all edit history for this roost"
          >
            {clearingHistory() ? "Clearing…" : "Clear edit history"}
          </button>
        </div>
      </div>

      {/* Stored keys */}
      <div class="border border-border rounded-lg p-3 space-y-2">
        <div class="flex items-center justify-between gap-2">
          <span class="text-sm font-medium text-text-primary">Secrets stored locally</span>
        </div>
        <div class="text-xs text-text-secondary space-y-1.5">
          <For each={storedKeys()}>
            {(row) => (
              <div class="flex items-center justify-between gap-2">
                <span class="truncate">{row.label}</span>
                <span
                  class={`text-[11px] font-mono shrink-0 ${
                    row.present ? "text-green-400" : "text-text-muted"
                  }`}
                >
                  {row.present ? "configured" : "not set"}
                </span>
              </div>
            )}
          </For>
          <p class="text-[11px] text-text-muted pt-1">
            Storage: {storedKeys()[0]?.storage}
          </p>
        </div>
      </div>

      {/* What we never do */}
      <div class="border border-border/50 rounded-lg p-3 space-y-2 bg-bg-secondary/30">
        <span class="text-sm font-medium text-text-primary">We never</span>
        <ul class="text-xs text-text-secondary leading-relaxed space-y-1 list-disc pl-5">
          <li>Run telemetry or analytics. There is no backend collecting usage data.</li>
          <li>Listen to your microphone in the background. Voice input is push-to-talk only.</li>
          <li>
            Store readable copies of your notes on any Corvux-operated server. The
            optional store-and-forward queue holds <em>ciphertext only</em> (E2E-encrypted
            change packets, ACK-and-delete, 30-day TTL); the server cannot decrypt them.
            You can disable the queue and use strict P2P (Tailscale / always-on peer) in Sync settings.
          </li>
          <li>Send your notes to a third party automatically. The agent only sees what its tools fetch in response to your prompt.</li>
        </ul>
      </div>
    </section>
  );
}

function DataFlowBadge(props: { kind: "cloud" | "local" }) {
  const cls = () =>
    props.kind === "cloud"
      ? "bg-amber-400/15 text-amber-400 border-amber-400/30"
      : "bg-green-400/15 text-green-400 border-green-400/30";
  const label = () => (props.kind === "cloud" ? "Leaves device" : "Stays local");
  return (
    <span class={`text-[10px] uppercase font-semibold rounded px-1.5 py-0.5 border ${cls()}`}>
      {label()}
    </span>
  );
}
