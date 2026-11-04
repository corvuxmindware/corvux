/**
 * Anthropic (Claude) direct provider configuration. Key + model + a live
 * `test_anthropic_connection` probe (native Messages API auth). Parent
 * gates rendering behind `<Show when={…llm_provider === "anthropic"}>`.
 *
 * Note: thinking is disabled server-side for now (see
 * `ai/tool_translator/anthropic.rs`), so Claude Fable 5 / Mythos 5 aren't
 * usable here yet — the curated list sticks to the tool-loop-robust models.
 */

import { For, Show, createSignal, onMount } from "solid-js";
import { commands, unwrap } from "../../../lib/invoke";
import type { ConnectionTestResult } from "../../../lib/bindings";
import { errMsg } from "../../../lib/error";
import { settingsStore } from "../../../stores/settings";

const DEFAULT_MODEL = "claude-sonnet-5";

/** Curated suggestions; users can type any model id their key can reach. */
const ANTHROPIC_MODELS: { id: string; label: string }[] = [
  { id: "claude-sonnet-5", label: "Claude Sonnet 5 — balanced default" },
  { id: "claude-opus-4-8", label: "Claude Opus 4.8 — deepest reasoning" },
  { id: "claude-haiku-4-5", label: "Claude Haiku 4.5 — fastest, cheapest" },
];

function last4(key: string | null | undefined): string {
  if (!key) return "";
  const trimmed = key.trim();
  if (trimmed.length <= 4) return "•••";
  return trimmed.slice(-4);
}

export default function AnthropicProvider() {
  const [keyInput, setKeyInput] = createSignal("");
  const [modelInput, setModelInput] = createSignal("");
  const [editing, setEditing] = createSignal(false);
  const [saved, setSaved] = createSignal(false);
  const [testing, setTesting] = createSignal(false);
  const [test, setTest] = createSignal<ConnectionTestResult | null>(null);
  const [saveError, setSaveError] = createSignal<string | null>(null);

  async function handleSave() {
    const key = keyInput().trim();
    const model = modelInput().trim() || DEFAULT_MODEL;
    setSaveError(null);
    try {
      await settingsStore.save({
        anthropic_api_key: key || null,
        anthropic_model: model,
      });
      setSaved(true);
      setEditing(false);
      setTest(null);
      setTimeout(() => setSaved(false), 2000);
    } catch (e) {
      setSaveError(errMsg(e));
    }
  }

  async function handleTest() {
    setTesting(true);
    setTest(null);
    try {
      const candidate = keyInput().trim() || (settingsStore.settings().anthropic_api_key ?? "");
      const result = await unwrap(commands.testAnthropicConnection(candidate));
      setTest(result);
    } catch (e) {
      setTest({ ok: false, latency_ms: 0, summary: "Test failed", error_message: errMsg(e) });
    } finally {
      setTesting(false);
    }
  }

  onMount(() => {
    const s = settingsStore.settings();
    if (s.anthropic_api_key) setKeyInput(s.anthropic_api_key);
    setModelInput(s.anthropic_model || DEFAULT_MODEL);
  });

  return (
    <>
      <div>
        <label class="text-text-primary text-sm block mb-1">Anthropic API Key</label>

        <Show
          when={!editing() && (settingsStore.settings().anthropic_api_key ?? "").length > 0}
          fallback={
            <div class="flex gap-2">
              <input
                type="password"
                placeholder="sk-ant-..."
                value={keyInput()}
                onInput={(e) => setKeyInput(e.currentTarget.value)}
                onKeyDown={(e) => {
                  if (e.key === "Enter") handleSave();
                }}
                class="flex-1 bg-bg-secondary border border-border rounded-lg px-3 py-2 text-sm text-text-primary placeholder-text-muted focus:outline-none focus:border-accent"
              />
              <button
                onClick={handleSave}
                class="px-4 py-2 bg-accent text-bg-primary rounded-lg text-sm font-medium hover:opacity-90 transition-opacity"
              >
                Save
              </button>
              <button
                onClick={handleTest}
                disabled={testing()}
                class="px-3 py-2 border border-border bg-bg-secondary text-text-primary rounded-lg text-sm hover:bg-bg-hover disabled:opacity-50 disabled:cursor-wait transition-colors"
                title="Verify the key against Anthropic's models endpoint"
              >
                {testing() ? "Testing…" : "Test"}
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
                •••{last4(settingsStore.settings().anthropic_api_key)}
              </span>
            </span>
            <button
              onClick={() => {
                setKeyInput(settingsStore.settings().anthropic_api_key ?? "");
                setEditing(true);
                setTest(null);
              }}
              class="px-2 py-1 text-xs rounded border border-border text-text-secondary hover:bg-bg-hover hover:text-text-primary transition-colors"
            >
              Edit
            </button>
            <button
              onClick={handleTest}
              disabled={testing()}
              class="px-2 py-1 text-xs rounded border border-border text-text-secondary hover:bg-bg-hover hover:text-text-primary disabled:opacity-50 disabled:cursor-wait transition-colors"
            >
              {testing() ? "Testing…" : "Test"}
            </button>
          </div>
        </Show>

        <div class="flex items-center justify-between mt-1.5">
          <a
            href="https://console.anthropic.com/settings/keys"
            target="_blank"
            rel="noopener"
            class="text-xs text-accent hover:underline"
          >
            Create a key at console.anthropic.com
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
        <label class="text-text-primary text-sm block mb-1">Claude Model</label>
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
            <For each={ANTHROPIC_MODELS}>{(m) => <option value={m.id}>{m.label}</option>}</For>
          </select>
          <div class="text-xs text-text-muted">or type any Claude model id</div>
        </div>
        <div class="text-xs text-text-muted mt-1">
          Active: <span class="font-mono text-text-secondary">{settingsStore.settings().anthropic_model}</span>
        </div>
      </div>
    </>
  );
}
