/**
 * Gemini provider configuration. Extracted from AISection/index.tsx
 * (A3). Self-contained: 8 signals + 3 handlers + curated CHAT_MODELS
 * list. Parent renders inside a `<Show when={…llm_provider ===
 * "gemini"}>` so this component only exists while Gemini is active.
 *
 * Different from OpenRouter in that the input placeholder + key-shape
 * are Gemini-specific (AIza…), and saving the model is a separate
 * action from saving the key (key save → models endpoint test, model
 * save → just persist + flash).
 */

import { For, Show, createSignal, onMount } from "solid-js";
import { commands, unwrap } from "../../../lib/invoke";
import type { ConnectionTestResult } from "../../../lib/bindings";
import { errMsg } from "../../../lib/error";
import { log } from "../../../lib/log";
import { settingsStore } from "../../../stores/settings";

/** Curated Gemini models. The picker is for discoverability, not
 *  enforcement — the Rust backend forwards whatever model id string
 *  we send, so this list never has to be in lockstep with Google's
 *  release cadence. */
const CHAT_MODELS: { id: string; label: string; hint: string }[] = [
  {
    id: "gemini-3.1-flash-lite",
    label: "Gemini 3.1 Flash Lite",
    hint: "Cheapest, fastest — agentic-workflow tuned",
  },
  {
    id: "gemini-3.1-flash",
    label: "Gemini 3.1 Flash",
    hint: "Fast, balanced cost",
  },
  {
    id: "gemini-2.5-pro",
    label: "Gemini 2.5 Pro",
    hint: "Highest quality — slower, more expensive",
  },
  {
    id: "gemini-2.5-flash",
    label: "Gemini 2.5 Flash",
    hint: "Previous-gen default",
  },
];

/** Last-4 helper. Returns "•••" for keys shorter than 4 chars.
 *  Duplicated from OpenRouterProvider — 5 lines isn't worth a shared
 *  module yet. */
function last4(key: string | null | undefined): string {
  if (!key) return "";
  const trimmed = key.trim();
  if (trimmed.length <= 4) return "•••";
  return trimmed.slice(-4);
}

export default function GeminiProvider() {
  const [apiKeyInput, setApiKeyInput] = createSignal("");
  const [geminiModelInput, setGeminiModelInput] = createSignal("");
  const [chatModelSaved, setChatModelSaved] = createSignal(false);
  const [saved, setSaved] = createSignal(false);
  const [editingGemini, setEditingGemini] = createSignal(false);
  const [geminiTesting, setGeminiTesting] = createSignal(false);
  const [geminiTest, setGeminiTest] = createSignal<ConnectionTestResult | null>(null);
  const [geminiSaveError, setGeminiSaveError] = createSignal<string | null>(null);

  async function handleSaveApiKey() {
    const key = apiKeyInput().trim();
    setGeminiSaveError(null);
    try {
      await settingsStore.save({ gemini_api_key: key || null });
      setSaved(true);
      setEditingGemini(false);
      // A new save invalidates any prior test result — the user might
      // have changed the key, so make them re-test before assuming
      // green status still holds.
      setGeminiTest(null);
      setTimeout(() => setSaved(false), 2000);
    } catch (e) {
      const msg = errMsg(e);
      setGeminiSaveError(msg);
    }
  }

  /** Probe the saved Gemini key against `models.list`. We send the
   *  CURRENT input (apiKeyInput) rather than the persisted settings
   *  because users typically click Test right after typing — letting
   *  them validate before pressing Save. If the input is empty, we
   *  fall back to the persisted key. */
  async function handleTestGemini() {
    setGeminiTesting(true);
    setGeminiTest(null);
    try {
      const candidate =
        apiKeyInput().trim() || (settingsStore.settings().gemini_api_key ?? "");
      const result = await unwrap(commands.testGeminiConnection(candidate));
      setGeminiTest(result);
    } catch (e) {
      const msg = errMsg(e);
      setGeminiTest({
        ok: false,
        latency_ms: 0,
        summary: "Test failed",
        error_message: msg,
      });
    } finally {
      setGeminiTesting(false);
    }
  }

  async function handleSaveChatModel() {
    const model = geminiModelInput().trim() || "gemini-3.1-flash-lite";
    try {
      await settingsStore.save({ model });
      setChatModelSaved(true);
      setTimeout(() => setChatModelSaved(false), 2000);
    } catch (e) {
      log.error("ai", "failed to set chat model", e);
    }
  }

  onMount(() => {
    const s = settingsStore.settings();
    if (s.gemini_api_key) setApiKeyInput(s.gemini_api_key);
    setGeminiModelInput(s.model || "gemini-3.1-flash-lite");
  });

  return (
    <>
      <div>
        <label class="text-text-primary text-sm block mb-1">Gemini API Key</label>

        {/* Saved-state chip — shown when there's a persisted key AND
            the user isn't editing. Hides the actual value so a
            shoulder-surfer / screenshot doesn't leak it; "ends in
            XXXX" is enough to confirm "yes, the right key is in
            place" without exposing the full string. */}
        <Show
          when={
            !editingGemini() &&
            (settingsStore.settings().gemini_api_key ?? "").length > 0
          }
          fallback={
            <div class="flex gap-2">
              <input
                type="password"
                placeholder="AIza..."
                value={apiKeyInput()}
                onInput={(e) => setApiKeyInput(e.currentTarget.value)}
                onKeyDown={(e) => {
                  if (e.key === "Enter") handleSaveApiKey();
                }}
                class="flex-1 bg-bg-secondary border border-border rounded-lg px-3 py-2 text-sm text-text-primary placeholder-text-muted focus:outline-none focus:border-accent"
              />
              <button
                onClick={handleSaveApiKey}
                class="px-4 py-2 bg-accent text-bg-primary rounded-lg text-sm font-medium hover:opacity-90 transition-opacity"
              >
                Save
              </button>
              <button
                onClick={handleTestGemini}
                disabled={geminiTesting()}
                class="px-3 py-2 border border-border bg-bg-secondary text-text-primary rounded-lg text-sm hover:bg-bg-hover disabled:opacity-50 disabled:cursor-wait transition-colors"
                title="Verify the key against Gemini's models endpoint"
              >
                {geminiTesting() ? "Testing…" : "Test"}
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
                •••{last4(settingsStore.settings().gemini_api_key)}
              </span>
            </span>
            <button
              onClick={() => {
                setApiKeyInput(settingsStore.settings().gemini_api_key ?? "");
                setEditingGemini(true);
                setGeminiTest(null);
              }}
              class="px-2 py-1 text-xs rounded border border-border text-text-secondary hover:bg-bg-hover hover:text-text-primary transition-colors"
            >
              Edit
            </button>
            <button
              onClick={handleTestGemini}
              disabled={geminiTesting()}
              class="px-2 py-1 text-xs rounded border border-border text-text-secondary hover:bg-bg-hover hover:text-text-primary disabled:opacity-50 disabled:cursor-wait transition-colors"
            >
              {geminiTesting() ? "Testing…" : "Test"}
            </button>
          </div>
        </Show>

        <div class="flex items-center justify-between mt-1.5">
          <a
            href="https://aistudio.google.com/apikey"
            target="_blank"
            rel="noopener"
            class="text-xs text-accent hover:underline"
          >
            Get a free API key from Google AI Studio
          </a>
          <Show when={saved()}>
            <span class="text-xs text-green">Saved</span>
          </Show>
        </div>

        {/* Inline save error — replaces the silent console.error */}
        <Show when={geminiSaveError()}>
          {(err) => (
            <div class="mt-1.5 text-xs text-red">
              Save failed: {err()}
            </div>
          )}
        </Show>

        {/* Connection-test result — green ✓ for ok, red for fail */}
        <Show when={geminiTest()}>
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
      </div>

      <div>
        <label class="text-text-primary text-sm block mb-1">Chat Model</label>
        <div class="flex gap-2">
          <input
            type="text"
            placeholder="gemini-3.1-flash-lite"
            value={geminiModelInput()}
            onInput={(e) => setGeminiModelInput(e.currentTarget.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter") void handleSaveChatModel();
            }}
            class="flex-1 bg-bg-secondary border border-border rounded-lg px-3 py-2 text-sm text-text-primary placeholder-text-muted focus:outline-none focus:border-accent font-mono"
          />
          <button
            onClick={handleSaveChatModel}
            class="px-4 py-2 bg-accent text-bg-primary rounded-lg text-sm font-medium hover:opacity-90 transition-opacity"
          >
            Save
          </button>
        </div>
        <Show when={chatModelSaved()}>
          <div class="text-xs text-accent mt-1">Saved.</div>
        </Show>
        <div class="flex items-center gap-3 mt-1.5">
          <select
            class="text-text-primary text-xs bg-bg-secondary px-2 py-1 rounded-lg border border-border focus:outline-none focus:ring-2 focus:ring-accent/40"
            value=""
            onChange={(e) => {
              if (e.currentTarget.value) {
                setGeminiModelInput(e.currentTarget.value);
                void handleSaveChatModel();
              }
              e.currentTarget.value = "";
            }}
          >
            <option value="">Pick a curated model…</option>
            <For each={CHAT_MODELS}>
              {(m) => <option value={m.id}>{m.label}</option>}
            </For>
          </select>
          <div class="text-xs text-text-muted">
            or paste any id from <span class="font-mono">ai.google.dev/gemini-api/docs/models</span>
          </div>
        </div>
        <div class="text-xs text-text-muted mt-1">
          Active: <span class="font-mono text-text-secondary">{settingsStore.settings().model}</span>
        </div>
      </div>
    </>
  );
}
