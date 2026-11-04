/**
 * OpenRouter provider configuration. Extracted from AISection/index.tsx
 * (A2). Self-contained: 7 signals + 2 handlers + curated model list +
 * structural-validity check. Parent renders inside a `<Show
 * when={…llm_provider === "openrouter"}>` so this component only
 * exists while OpenRouter is active.
 *
 * Two-layer key validation:
 *  - Server-side via `test_openrouter_connection` (live network probe)
 *  - Client-side regex against `sk-or-v1-…` shape (catches truncation
 *    before paying a network round-trip)
 */

import { For, Show, createSignal, onMount } from "solid-js";
import { commands, unwrap } from "../../../lib/invoke";
import type { ConnectionTestResult } from "../../../lib/bindings";
import { errMsg } from "../../../lib/error";
import { settingsStore } from "../../../stores/settings";

/** Curated suggestions for the picker; users can paste any
 *  `provider/model-name` id from openrouter.ai/models too. */
const OPENROUTER_MODELS: { id: string; label: string; hint: string }[] = [
  {
    id: "anthropic/claude-sonnet-4.6",
    label: "Claude Sonnet 4.6",
    hint: "Anthropic — strong reasoning, tool-use",
  },
  {
    id: "anthropic/claude-opus-4.8",
    label: "Claude Opus 4.8",
    hint: "Anthropic — deepest reasoning, slowest, most expensive",
  },
  {
    id: "openai/gpt-5.5",
    label: "GPT-5.5",
    hint: "OpenAI — current flagship",
  },
  {
    id: "google/gemini-2.5-flash",
    label: "Gemini 2.5 Flash (via OpenRouter)",
    hint: "Same model, routed through OpenRouter",
  },
];

/** Last-4 helper. Returns "•••" for keys shorter than 4 chars.
 *  Duplicated from index.tsx (also used by GeminiProvider); will
 *  consolidate in A3. */
function last4(key: string | null | undefined): string {
  if (!key) return "";
  const trimmed = key.trim();
  if (trimmed.length <= 4) return "•••";
  return trimmed.slice(-4);
}

export default function OpenRouterProvider() {
  const [orKeyInput, setOrKeyInput] = createSignal("");
  const [orModelInput, setOrModelInput] = createSignal("");
  const [editingOR, setEditingOR] = createSignal(false);
  const [orSaved, setOrSaved] = createSignal(false);
  const [orTesting, setOrTesting] = createSignal(false);
  const [orTest, setOrTest] = createSignal<ConnectionTestResult | null>(null);
  const [orSaveError, setOrSaveError] = createSignal<string | null>(null);

  async function handleSaveOpenRouter() {
    const key = orKeyInput().trim();
    const model = orModelInput().trim() || "anthropic/claude-sonnet-4.6";
    setOrSaveError(null);
    try {
      await settingsStore.save({
        openrouter_api_key: key || null,
        openrouter_model: model,
      });
      setOrSaved(true);
      setEditingOR(false);
      setOrTest(null);
      setTimeout(() => setOrSaved(false), 2000);
    } catch (e) {
      const msg = errMsg(e);
      setOrSaveError(msg);
    }
  }

  async function handleTestOpenRouter() {
    setOrTesting(true);
    setOrTest(null);
    try {
      const candidate =
        orKeyInput().trim() || (settingsStore.settings().openrouter_api_key ?? "");
      const result = await unwrap(commands.testOpenrouterConnection(candidate));
      setOrTest(result);
    } catch (e) {
      const msg = errMsg(e);
      setOrTest({
        ok: false,
        latency_ms: 0,
        summary: "Test failed",
        error_message: msg,
      });
    } finally {
      setOrTesting(false);
    }
  }

  onMount(() => {
    // Seed from current settings — parent's onMount already loaded them
    // before we mounted (Show gates us on llm_provider === "openrouter").
    const s = settingsStore.settings();
    if (s.openrouter_api_key) setOrKeyInput(s.openrouter_api_key);
    setOrModelInput(s.openrouter_model || "anthropic/claude-sonnet-4.6");
  });

  return (
    <>
      <div>
        <label class="text-text-primary text-sm block mb-1">OpenRouter API Key</label>

        <Show
          when={
            !editingOR() &&
            (settingsStore.settings().openrouter_api_key ?? "").length > 0
          }
          fallback={
            <div class="flex gap-2">
              <input
                type="password"
                placeholder="sk-or-v1-..."
                value={orKeyInput()}
                onInput={(e) => setOrKeyInput(e.currentTarget.value)}
                onKeyDown={(e) => {
                  if (e.key === "Enter") handleSaveOpenRouter();
                }}
                class="flex-1 bg-bg-secondary border border-border rounded-lg px-3 py-2 text-sm text-text-primary placeholder-text-muted focus:outline-none focus:border-accent"
              />
              <button
                onClick={handleSaveOpenRouter}
                class="px-4 py-2 bg-accent text-bg-primary rounded-lg text-sm font-medium hover:opacity-90 transition-opacity"
              >
                Save
              </button>
              <button
                onClick={handleTestOpenRouter}
                disabled={orTesting()}
                class="px-3 py-2 border border-border bg-bg-secondary text-text-primary rounded-lg text-sm hover:bg-bg-hover disabled:opacity-50 disabled:cursor-wait transition-colors"
                title="Verify the key against OpenRouter's models endpoint"
              >
                {orTesting() ? "Testing…" : "Test"}
              </button>
            </div>
          }
        >
          <div class="flex items-center gap-2 px-3 py-2 rounded-lg border border-border bg-bg-secondary">
            <svg
              width="14"
              height="14"
              viewBox="0 0 24 24"
              fill="none"
              stroke="currentColor"
              stroke-width="2.5"
              stroke-linecap="round"
              stroke-linejoin="round"
              class="text-green shrink-0"
              aria-hidden="true"
            >
              <path d="M20 6L9 17l-5-5" />
            </svg>
            <span class="flex-1 text-sm text-text-primary truncate min-w-0">
              Saved · ends in{" "}
              <span class="font-mono text-text-secondary">
                •••{last4(settingsStore.settings().openrouter_api_key)}
              </span>
            </span>
            <button
              onClick={() => {
                setOrKeyInput(settingsStore.settings().openrouter_api_key ?? "");
                setEditingOR(true);
                setOrTest(null);
              }}
              class="px-2 py-1 text-xs rounded border border-border text-text-secondary hover:bg-bg-hover hover:text-text-primary transition-colors"
            >
              Edit
            </button>
            <button
              onClick={handleTestOpenRouter}
              disabled={orTesting()}
              class="px-2 py-1 text-xs rounded border border-border text-text-secondary hover:bg-bg-hover hover:text-text-primary disabled:opacity-50 disabled:cursor-wait transition-colors"
            >
              {orTesting() ? "Testing…" : "Test"}
            </button>
          </div>
        </Show>

        <div class="flex items-center justify-between mt-1.5">
          <a
            href="https://openrouter.ai/keys"
            target="_blank"
            rel="noopener"
            class="text-xs text-accent hover:underline"
          >
            Create a key at openrouter.ai
          </a>
          <Show when={orSaved()}>
            <span class="text-xs text-green">Saved</span>
          </Show>
        </div>

        <Show when={orSaveError()}>
          {(err) => (
            <div class="mt-1.5 text-xs text-red">
              Save failed: {err()}
            </div>
          )}
        </Show>

        <Show when={orTest()}>
          {(r) => (
            <div
              class={`mt-1.5 text-xs flex items-start gap-1.5 ${
                r().ok ? "text-green" : "text-red"
              }`}
              role="status"
              aria-live="polite"
            >
              <span class="font-semibold">{r().ok ? "✓" : "✗"}</span>
              <span class="flex-1">
                {r().summary} · {r().latency_ms} ms
                <Show when={!r().ok && r().error_message}>
                  <div class="text-text-muted mt-0.5 break-words font-mono text-[10px]">
                    {r().error_message}
                  </div>
                </Show>
              </span>
            </div>
          )}
        </Show>

        {/* Structural validator — catches paste-truncation and missing
            prefix before paying a network round-trip. */}
        {(() => {
          const saved = settingsStore.settings().openrouter_api_key ?? "";
          if (!saved) return null;
          const looksValid = /^sk-or-v1-[a-zA-Z0-9_-]{40,}$/.test(saved);
          if (looksValid) return null;
          return (
            <div class="mt-1.5 text-xs text-red-400">
              Saved key is {saved.length} chars and{" "}
              {saved.startsWith("sk-or-v1-")
                ? "looks too short."
                : "is missing the `sk-or-v1-` prefix."}{" "}
              Re-paste the FULL key from openrouter.ai/keys.
            </div>
          );
        })()}
      </div>

      <div>
        <label class="text-text-primary text-sm block mb-1">OpenRouter Model</label>
        <div class="flex gap-2">
          <input
            type="text"
            placeholder="anthropic/claude-sonnet-4.6"
            value={orModelInput()}
            onInput={(e) => setOrModelInput(e.currentTarget.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter") handleSaveOpenRouter();
            }}
            class="flex-1 bg-bg-secondary border border-border rounded-lg px-3 py-2 text-sm text-text-primary placeholder-text-muted focus:outline-none focus:border-accent font-mono"
          />
          <button
            onClick={handleSaveOpenRouter}
            class="px-4 py-2 bg-accent text-bg-primary rounded-lg text-sm font-medium hover:opacity-90 transition-opacity"
          >
            Save
          </button>
        </div>
        <div class="flex items-center gap-3 mt-1.5">
          <select
            class="text-text-primary text-xs bg-bg-secondary px-2 py-1 rounded-lg border border-border focus:outline-none focus:ring-2 focus:ring-accent/40"
            value=""
            onChange={(e) => {
              if (e.currentTarget.value) {
                setOrModelInput(e.currentTarget.value);
                void handleSaveOpenRouter();
              }
              e.currentTarget.value = "";
            }}
          >
            <option value="">Pick a curated model…</option>
            <For each={OPENROUTER_MODELS}>
              {(m) => <option value={m.id}>{m.label}</option>}
            </For>
          </select>
          <div class="text-xs text-text-muted">
            or paste any id from <span class="font-mono">openrouter.ai/models</span>
          </div>
        </div>
        <div class="text-xs text-text-muted mt-1">
          Active: <span class="font-mono text-text-secondary">{settingsStore.settings().openrouter_model}</span>
        </div>
      </div>
    </>
  );
}
