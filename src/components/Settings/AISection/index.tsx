import GeminiProvider from "./GeminiProvider";
import OllamaProvider from "./OllamaProvider";
import OpenRouterProvider from "./OpenRouterProvider";
import OpenAiProvider from "./OpenAiProvider";
import AnthropicProvider from "./AnthropicProvider";
import { For, Show, createMemo, onMount } from "solid-js";
import { settingsStore, type LlmProvider } from "../../../stores/settings";
import { isMobile } from "../../../lib/platform";
import { log } from "../../../lib/log";

// Behavior panels moved to CorvuxAgentSection.tsx as of 2026-05-20.
// AISection is the orchestrator: a compact provider dropdown + the
// self-contained per-provider subcomponents that own everything else.
//
// UX note (2026-07-05): the provider picker was five stacked cards. At
// five providers that pushed the actual key/model config below the fold,
// so it's now a dropdown + a single status line for the selected
// provider. The at-a-glance "which providers have keys" overview the
// cards gave is traded for vertical compactness; the status line keeps
// the readiness signal for the one that matters (the active one).

interface ProviderOption {
  id: LlmProvider;
  label: string;
  blurb: string;
  /** Ollama is a local desktop server — hidden on mobile. */
  desktopOnly?: boolean;
}

const PROVIDERS: ProviderOption[] = [
  { id: "gemini", label: "Google Gemini", blurb: "Free tier available. Fast, good default." },
  { id: "openrouter", label: "OpenRouter", blurb: "One key for Claude, GPT, Gemini, and 100+ others." },
  { id: "openai", label: "OpenAI", blurb: "Your own OpenAI key — GPT models, direct (no gateway)." },
  { id: "anthropic", label: "Anthropic (Claude)", blurb: "Your own Anthropic key — Claude models, direct." },
  { id: "ollama", label: "Ollama (local)", blurb: "Run models on your machine. No API key needed.", desktopOnly: true },
];

export default function AISection() {
  // Provider state, key inputs, test results, model lists, download
  // progress, and the failed-embed surface all live inside the
  // subcomponents. This orchestrator only needs to ensure settings
  // are loaded before any subcomponent renders.
  onMount(async () => {
    await settingsStore.load();
  });

  async function setProvider(provider: LlmProvider) {
    try {
      await settingsStore.save({ llm_provider: provider });
      // Switching providers mounts the corresponding subcomponent;
      // each one handles its own onMount-time seeding.
    } catch (e) {
      log.error("ai", "failed to set provider", e);
    }
  }

  const options = createMemo(() =>
    PROVIDERS.filter((p) => !p.desktopOnly || !isMobile()),
  );

  const active = createMemo(() =>
    options().find((p) => p.id === settingsStore.settings().llm_provider),
  );

  // Readiness for the *active* provider. `hasApiKey()` already knows the
  // per-provider rule (key present, or a model chosen for keyless Ollama).
  const ready = createMemo(() => settingsStore.hasApiKey());
  const readyLabel = createMemo(() => {
    const p = settingsStore.settings().llm_provider;
    if (p === "ollama") return ready() ? "Model selected" : "Pick a model below";
    return ready() ? "Key saved" : "Needs an API key";
  });

  return (
    <section>
      <h3 class="text-text-secondary text-xs font-semibold uppercase tracking-wider mb-1">
        AI Models
      </h3>
      <p class="text-text-muted text-[11px] mb-4 leading-relaxed">
        Pick your provider, paste an API key, and choose the chat model.
        For how the agent <em>behaves</em> (approvals, roost conventions,
        audit history), see Corvux Agent.
      </p>
      <div class="space-y-3">
        <div class="space-y-1.5">
          <label
            for="ai-provider-select"
            class="text-text-primary text-sm block"
          >
            Which AI runs your chats
          </label>
          <select
            id="ai-provider-select"
            class="w-full text-text-primary text-sm bg-bg-secondary px-3 py-2 rounded-lg border border-border focus:outline-none focus:ring-2 focus:ring-accent/40"
            value={settingsStore.settings().llm_provider}
            onChange={(e) => setProvider(e.currentTarget.value as LlmProvider)}
          >
            <For each={options()}>
              {(p) => <option value={p.id}>{p.label}</option>}
            </For>
          </select>

          {/* Selected-provider status line — replaces the per-card dots. */}
          <div class="flex items-center gap-2 text-xs">
            <span
              class={`w-1.5 h-1.5 rounded-full shrink-0 ${
                ready() ? "bg-green-400" : "bg-amber-400"
              }`}
              aria-hidden="true"
            />
            <span class={ready() ? "text-green-400" : "text-amber-400"}>
              {readyLabel()}
            </span>
            <span class="text-text-muted truncate min-w-0">
              · {active()?.blurb}
            </span>
          </div>
        </div>

        {/* Per-provider config — only the active provider's subcomponent
            mounts. Each owns its own state, handlers, and IPC calls. */}
        <Show when={settingsStore.settings().llm_provider === "gemini"}>
          <GeminiProvider />
        </Show>
        <Show when={settingsStore.settings().llm_provider === "openrouter"}>
          <OpenRouterProvider />
        </Show>
        <Show when={settingsStore.settings().llm_provider === "openai"}>
          <OpenAiProvider />
        </Show>
        <Show when={settingsStore.settings().llm_provider === "anthropic"}>
          <AnthropicProvider />
        </Show>
        <Show when={!isMobile() && settingsStore.settings().llm_provider === "ollama"}>
          <OllamaProvider />
        </Show>
      </div>
    </section>
  );
}
