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
import { confirmDialog } from "../../../lib/confirm";
import { settingsStore } from "../../../stores/settings";
import { reportProviderSaveFailure } from "./saveOutcome";

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

export default function OpenRouterProvider() {
  const [orKeyInput, setOrKeyInput] = createSignal("");
  const [orModelInput, setOrModelInput] = createSignal("");
  const [editingOR, setEditingOR] = createSignal(false);
  const [orSaved, setOrSaved] = createSignal(false);
  const [orTesting, setOrTesting] = createSignal(false);
  const [orTest, setOrTest] = createSignal<ConnectionTestResult | null>(null);
  const [orSaveError, setOrSaveError] = createSignal<string | null>(null);

  // The saved key never reaches the webview — only "is one saved?" does.
  const hasSavedKey = () => settingsStore.keyStatus().openrouter;
  // Test is meaningful with a typed key, or with a saved one (the backend
  // falls back to the saved key when we send an empty string).
  const canTest = () => orKeyInput().trim().length > 0 || hasSavedKey();

  /** Saves the model, and the key too when one was typed. An empty key box
   *  leaves the saved key alone (`null` = unchanged) — removing a key is the
   *  explicit Remove action. */
  async function handleSaveOpenRouter() {
    const key = orKeyInput().trim();
    const model = orModelInput().trim() || "anthropic/claude-sonnet-4.6";
    setOrSaveError(null);
    try {
      await settingsStore.save({
        ...(key ? { openrouter_api_key: key } : {}),
        openrouter_model: model,
      });
      // Don't keep the key in the component once the backend has it.
      setOrKeyInput("");
      setOrSaved(true);
      setEditingOR(false);
      setOrTest(null);
      setTimeout(() => setOrSaved(false), 2000);
    } catch (e) {
      // A declined native confirmation is a choice, not a failure.
      reportProviderSaveFailure(e, setOrSaveError);
    }
  }

  async function handleTestOpenRouter() {
    setOrTesting(true);
    setOrTest(null);
    try {
      // Empty input => `""` => the backend tests the SAVED key.
      const result = await unwrap(commands.testOpenrouterConnection(orKeyInput().trim()));
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

  /** Delete the saved key. Sends `""` — `null` now means "leave it alone". */
  async function handleRemoveKey() {
    if (!(await confirmDialog("Remove the saved OpenRouter API key from this device?"))) return;
    setOrSaveError(null);
    try {
      await settingsStore.save({ openrouter_api_key: "" });
      setOrKeyInput("");
      setEditingOR(false);
      setOrTest(null);
    } catch (e) {
      reportProviderSaveFailure(e, setOrSaveError);
    }
  }

  onMount(() => {
    // Seed the model from current settings — parent's onMount already loaded
    // them before we mounted (Show gates us on llm_provider === "openrouter").
    // The key box always starts empty: the saved key is never in the webview.
    const s = settingsStore.settings();
    setOrModelInput(s.openrouter_model || "anthropic/claude-sonnet-4.6");
  });

  return (
    <>
      <div>
        <label class="text-text-primary text-sm block mb-1">OpenRouter API Key</label>

        <Show
          when={!editingOR() && hasSavedKey()}
          fallback={
            <div class="flex gap-2">
              <input
                type="password"
                placeholder={hasSavedKey() ? "Paste a new key to replace the saved one" : "sk-or-v1-..."}
                value={orKeyInput()}
                onInput={(e) => setOrKeyInput(e.currentTarget.value)}
                onKeyDown={(e) => {
                  if (e.key === "Enter") handleSaveOpenRouter();
                }}
                class="flex-1 bg-bg-secondary border border-border rounded-lg px-3 py-2 text-sm text-text-primary placeholder-text-muted focus:outline-none focus:border-accent"
              />
              <button
                onClick={handleSaveOpenRouter}
                disabled={!orKeyInput().trim()}
                class="px-4 py-2 bg-accent text-bg-primary rounded-lg text-sm font-medium hover:opacity-90 disabled:opacity-50 disabled:cursor-not-allowed transition-opacity"
              >
                Save
              </button>
              <button
                onClick={handleTestOpenRouter}
                disabled={orTesting() || !canTest()}
                class="px-3 py-2 border border-border bg-bg-secondary text-text-primary rounded-lg text-sm hover:bg-bg-hover disabled:opacity-50 disabled:cursor-not-allowed transition-colors"
                title="Verify the key against OpenRouter's models endpoint"
              >
                {orTesting() ? "Testing…" : "Test"}
              </button>
              <Show when={editingOR()}>
                <button
                  onClick={() => {
                    setOrKeyInput("");
                    setEditingOR(false);
                  }}
                  class="px-3 py-2 border border-border bg-bg-secondary text-text-secondary rounded-lg text-sm hover:bg-bg-hover transition-colors"
                >
                  Cancel
                </button>
              </Show>
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
              Key saved
            </span>
            <button
              onClick={() => {
                // Start empty — the saved key can't be shown or prefilled.
                setOrKeyInput("");
                setEditingOR(true);
                setOrTest(null);
              }}
              class="px-2 py-1 text-xs rounded border border-border text-text-secondary hover:bg-bg-hover hover:text-text-primary transition-colors"
            >
              Replace
            </button>
            <button
              onClick={handleTestOpenRouter}
              disabled={orTesting()}
              class="px-2 py-1 text-xs rounded border border-border text-text-secondary hover:bg-bg-hover hover:text-text-primary disabled:opacity-50 disabled:cursor-wait transition-colors"
            >
              {orTesting() ? "Testing…" : "Test"}
            </button>
            <button
              onClick={() => void handleRemoveKey()}
              class="px-2 py-1 text-xs rounded border border-border text-text-secondary hover:bg-bg-hover hover:text-red transition-colors"
            >
              Remove
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
            prefix before paying a network round-trip. It checks the key being
            TYPED: the saved key is never in the webview, so it can't be
            re-checked after the fact. */}
        {(() => {
          const typed = orKeyInput().trim();
          if (!typed) return null;
          const looksValid = /^sk-or-v1-[a-zA-Z0-9_-]{40,}$/.test(typed);
          if (looksValid) return null;
          return (
            <div class="mt-1.5 text-xs text-red-400">
              This key is {typed.length} chars and{" "}
              {typed.startsWith("sk-or-v1-")
                ? "looks too short."
                : "is missing the `sk-or-v1-` prefix."}{" "}
              Paste the FULL key from openrouter.ai/keys.
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
