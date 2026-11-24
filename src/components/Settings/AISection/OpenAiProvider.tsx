/**
 * OpenAI direct provider configuration. Mirrors OpenRouterProvider — the
 * OpenAI API is the same wire shape, so this is key + model + a live
 * `test_openai_connection` probe. Parent gates rendering behind a `<Show
 * when={…llm_provider === "openai"}>` so this only mounts while OpenAI is
 * the active provider.
 */

import { For, Show, createSignal, onMount } from "solid-js";
import { commands, unwrap } from "../../../lib/invoke";
import type { ConnectionTestResult } from "../../../lib/bindings";
import { errMsg } from "../../../lib/error";
import { confirmDialog } from "../../../lib/confirm";
import { settingsStore } from "../../../stores/settings";
import { reportProviderSaveFailure } from "./saveOutcome";

const DEFAULT_MODEL = "gpt-5.5";

/** Curated suggestions (current as of 2026-07); users can type any model
 *  id their key can reach. */
const OPENAI_MODELS: { id: string; label: string }[] = [
  { id: "gpt-5.5", label: "GPT-5.5 — current flagship" },
  { id: "gpt-5.5-pro", label: "GPT-5.5 Pro — highest accuracy, priciest" },
  { id: "gpt-5.4-nano", label: "GPT-5.4 nano — cheapest, high-volume" },
];

export default function OpenAiProvider() {
  const [keyInput, setKeyInput] = createSignal("");
  const [modelInput, setModelInput] = createSignal("");
  const [editing, setEditing] = createSignal(false);
  const [saved, setSaved] = createSignal(false);
  const [testing, setTesting] = createSignal(false);
  const [test, setTest] = createSignal<ConnectionTestResult | null>(null);
  const [saveError, setSaveError] = createSignal<string | null>(null);

  // The saved key never reaches the webview — only "is one saved?" does.
  const hasSavedKey = () => settingsStore.keyStatus().openai;
  // Test is meaningful with a typed key, or with a saved one (the backend
  // falls back to the saved key when we send an empty string).
  const canTest = () => keyInput().trim().length > 0 || hasSavedKey();

  /** Saves the model, and the key too when one was typed. An empty key box
   *  leaves the saved key alone (`null` = unchanged) — removing a key is the
   *  explicit Remove action. */
  async function handleSave() {
    const key = keyInput().trim();
    const model = modelInput().trim() || DEFAULT_MODEL;
    setSaveError(null);
    try {
      await settingsStore.save({
        ...(key ? { openai_api_key: key } : {}),
        openai_model: model,
      });
      // Don't keep the key in the component once the backend has it.
      setKeyInput("");
      setSaved(true);
      setEditing(false);
      setTest(null);
      setTimeout(() => setSaved(false), 2000);
    } catch (e) {
      // A declined native confirmation is a choice, not a failure.
      reportProviderSaveFailure(e, setSaveError);
    }
  }

  async function handleTest() {
    setTesting(true);
    setTest(null);
    try {
      // Empty input => `""` => the backend tests the SAVED key.
      const result = await unwrap(commands.testOpenaiConnection(keyInput().trim()));
      setTest(result);
    } catch (e) {
      setTest({ ok: false, latency_ms: 0, summary: "Test failed", error_message: errMsg(e) });
    } finally {
      setTesting(false);
    }
  }

  /** Delete the saved key. Sends `""` — `null` now means "leave it alone". */
  async function handleRemoveKey() {
    if (!(await confirmDialog("Remove the saved OpenAI API key from this device?"))) return;
    setSaveError(null);
    try {
      await settingsStore.save({ openai_api_key: "" });
      setKeyInput("");
      setEditing(false);
      setTest(null);
    } catch (e) {
      reportProviderSaveFailure(e, setSaveError);
    }
  }

  onMount(() => {
    // The key box always starts empty: the saved key is never in the webview.
    const s = settingsStore.settings();
    setModelInput(s.openai_model || DEFAULT_MODEL);
  });

  return (
    <>
      <div>
        <label class="text-text-primary text-sm block mb-1">OpenAI API Key</label>

        <Show
          when={!editing() && hasSavedKey()}
          fallback={
            <div class="flex gap-2">
              <input
                type="password"
                placeholder={hasSavedKey() ? "Paste a new key to replace the saved one" : "sk-..."}
                value={keyInput()}
                onInput={(e) => setKeyInput(e.currentTarget.value)}
                onKeyDown={(e) => {
                  if (e.key === "Enter") handleSave();
                }}
                class="flex-1 bg-bg-secondary border border-border rounded-lg px-3 py-2 text-sm text-text-primary placeholder-text-muted focus:outline-none focus:border-accent"
              />
              <button
                onClick={handleSave}
                disabled={!keyInput().trim()}
                class="px-4 py-2 bg-accent text-bg-primary rounded-lg text-sm font-medium hover:opacity-90 disabled:opacity-50 disabled:cursor-not-allowed transition-opacity"
              >
                Save
              </button>
              <button
                onClick={handleTest}
                disabled={testing() || !canTest()}
                class="px-3 py-2 border border-border bg-bg-secondary text-text-primary rounded-lg text-sm hover:bg-bg-hover disabled:opacity-50 disabled:cursor-not-allowed transition-colors"
                title="Verify the key against OpenAI's models endpoint"
              >
                {testing() ? "Testing…" : "Test"}
              </button>
              <Show when={editing()}>
                <button
                  onClick={() => {
                    setKeyInput("");
                    setEditing(false);
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
                setKeyInput("");
                setEditing(true);
                setTest(null);
              }}
              class="px-2 py-1 text-xs rounded border border-border text-text-secondary hover:bg-bg-hover hover:text-text-primary transition-colors"
            >
              Replace
            </button>
            <button
              onClick={handleTest}
              disabled={testing()}
              class="px-2 py-1 text-xs rounded border border-border text-text-secondary hover:bg-bg-hover hover:text-text-primary disabled:opacity-50 disabled:cursor-wait transition-colors"
            >
              {testing() ? "Testing…" : "Test"}
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
            href="https://platform.openai.com/api-keys"
            target="_blank"
            rel="noopener"
            class="text-xs text-accent hover:underline"
          >
            Create a key at platform.openai.com
          </a>
          <Show when={saved()}>
            <span class="text-xs text-green">Saved</span>
          </Show>
        </div>

        <Show when={saveError()}>
          {(err) => <div class="mt-1.5 text-xs text-red">Save failed: {err()}</div>}
        </Show>

        <Show when={test()}>
          {(r) => (
            <div
              class={`mt-1.5 text-xs flex items-start gap-1.5 ${r().ok ? "text-green" : "text-red"}`}
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
        <label class="text-text-primary text-sm block mb-1">OpenAI Model</label>
        <div class="flex gap-2">
          <input
            type="text"
            placeholder={DEFAULT_MODEL}
            value={modelInput()}
            onInput={(e) => setModelInput(e.currentTarget.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter") handleSave();
            }}
            class="flex-1 bg-bg-secondary border border-border rounded-lg px-3 py-2 text-sm text-text-primary placeholder-text-muted focus:outline-none focus:border-accent font-mono"
          />
          <button
            onClick={handleSave}
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
                setModelInput(e.currentTarget.value);
                void handleSave();
              }
              e.currentTarget.value = "";
            }}
          >
            <option value="">Pick a model…</option>
            <For each={OPENAI_MODELS}>{(m) => <option value={m.id}>{m.label}</option>}</For>
          </select>
          <div class="text-xs text-text-muted">or type any model id your key can reach</div>
        </div>
        <div class="text-xs text-text-muted mt-1">
          Active: <span class="font-mono text-text-secondary">{settingsStore.settings().openai_model}</span>
        </div>
      </div>
    </>
  );
}
