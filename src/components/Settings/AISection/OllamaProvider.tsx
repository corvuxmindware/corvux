/**
 * Ollama provider configuration. Extracted from AISection/index.tsx
 * (A1). Self-contained: owns its own signals, handlers, and onMount
 * lifecycle. Parent renders inside a `<Show when={…llm_provider ===
 * "ollama"}>` so this component only exists while Ollama is the
 * active provider — no need to gate internally.
 *
 * Differs from the cloud-provider components in three ways:
 *  - Base URL is configurable (defaults to localhost:11434)
 *  - No mandatory API key (auth proxy is the rare case)
 *  - Model picker is a dropdown of `ollama list` output, with a
 *    freeform fallback when Ollama isn't reachable
 */

import { For, Show, createSignal, onMount } from "solid-js";
import { commands, unwrap } from "../../../lib/invoke";
import type { ConnectionTestResult, OllamaModelInfo } from "../../../lib/bindings";
import { errMsg } from "../../../lib/error";
import { settingsStore } from "../../../stores/settings";

function formatBytes(bytes: number): string {
  if (bytes === 0) return "0 B";
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  if (bytes < 1024 * 1024 * 1024) return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
  return `${(bytes / 1024 / 1024 / 1024).toFixed(2)} GB`;
}

export default function OllamaProvider() {
  const [ollamaBaseInput, setOllamaBaseInput] = createSignal("");
  const [ollamaModelInput, setOllamaModelInput] = createSignal("");
  const [ollamaKeyInput, setOllamaKeyInput] = createSignal("");
  const [ollamaTesting, setOllamaTesting] = createSignal(false);
  const [ollamaTest, setOllamaTest] = createSignal<ConnectionTestResult | null>(null);
  const [ollamaSaved, setOllamaSaved] = createSignal(false);
  const [ollamaSaveError, setOllamaSaveError] = createSignal<string | null>(null);
  const [ollamaModels, setOllamaModels] = createSignal<OllamaModelInfo[]>([]);
  const [ollamaModelsLoading, setOllamaModelsLoading] = createSignal(false);
  const [ollamaModelsError, setOllamaModelsError] = createSignal<string | null>(null);
  const [showOllamaAdvanced, setShowOllamaAdvanced] = createSignal(false);

  /** Fetch the installed-model list from the user's Ollama server.
   *  Errors are non-fatal — the freeform input still works as a fallback. */
  async function refreshOllamaModels() {
    setOllamaModelsLoading(true);
    setOllamaModelsError(null);
    try {
      const base = ollamaBaseInput().trim() || settingsStore.settings().ollama_base_url;
      const apiKey = ollamaKeyInput().trim() || settingsStore.settings().ollama_api_key;
      const result = await unwrap(commands.listOllamaModels(base, apiKey || null));
      setOllamaModels(result);
    } catch (e) {
      const msg = errMsg(e);
      setOllamaModelsError(msg);
      setOllamaModels([]);
    } finally {
      setOllamaModelsLoading(false);
    }
  }

  async function handleTestOllama() {
    setOllamaTesting(true);
    setOllamaTest(null);
    try {
      const base = ollamaBaseInput().trim() || settingsStore.settings().ollama_base_url;
      const apiKey = ollamaKeyInput().trim() || settingsStore.settings().ollama_api_key;
      const result = await unwrap(commands.testOllamaConnection(base, apiKey || null));
      setOllamaTest(result);
      // A successful test is a great moment to refresh the model list —
      // we know Ollama is reachable right now.
      if (result.ok) void refreshOllamaModels();
    } catch (e) {
      const msg = errMsg(e);
      setOllamaTest({
        ok: false,
        latency_ms: 0,
        summary: "Test failed",
        error_message: msg,
      });
    } finally {
      setOllamaTesting(false);
    }
  }

  /** Save the Ollama config (base URL, model, optional key) in one
   *  shot. Together because they're meaningless apart — wrong host
   *  with the right model still doesn't work. */
  async function handleSaveOllama() {
    const base = ollamaBaseInput().trim() || "http://localhost:11434";
    const model = ollamaModelInput().trim();
    const key = ollamaKeyInput().trim();
    setOllamaSaveError(null);
    try {
      await settingsStore.save({
        ollama_base_url: base,
        ollama_model: model,
        ollama_api_key: key || null,
      });
      setOllamaSaved(true);
      setOllamaTest(null);
      setTimeout(() => setOllamaSaved(false), 2000);
    } catch (e) {
      const msg = errMsg(e);
      setOllamaSaveError(msg);
    }
  }

  onMount(() => {
    // Seed inputs from the current settings. We can read directly
    // because parent's onMount already awaited settingsStore.load()
    // before mounting any provider component (this child only renders
    // when llm_provider === "ollama", which can't be true pre-load).
    const s = settingsStore.settings();
    setOllamaBaseInput(s.ollama_base_url || "http://localhost:11434");
    setOllamaModelInput(s.ollama_model || "");
    if (s.ollama_api_key) setOllamaKeyInput(s.ollama_api_key);
    // Eager-fetch the installed-model list — this fires on initial
    // load AND whenever the user switches BACK to Ollama (because
    // the parent's Show clause re-mounts us). Same UX as before.
    void refreshOllamaModels();
  });

  return (
    <>
      <div>
        <label class="text-text-primary text-sm block mb-1">Ollama Server URL</label>
        <div class="flex gap-2">
          <input
            type="text"
            placeholder="http://localhost:11434"
            value={ollamaBaseInput()}
            onInput={(e) => setOllamaBaseInput(e.currentTarget.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter") void handleSaveOllama();
            }}
            class="flex-1 bg-bg-secondary border border-border rounded-lg px-3 py-2 text-sm text-text-primary placeholder-text-muted focus:outline-none focus:border-accent font-mono"
          />
          <button
            onClick={handleTestOllama}
            disabled={ollamaTesting()}
            class="px-3 py-2 border border-border bg-bg-secondary text-text-primary rounded-lg text-sm hover:bg-bg-hover disabled:opacity-50 disabled:cursor-wait transition-colors"
            title="Probe /api/version to verify Ollama is reachable"
          >
            {ollamaTesting() ? "Testing…" : "Test"}
          </button>
        </div>
        <div class="flex items-center justify-between mt-1.5">
          <a
            href="https://ollama.com/download"
            target="_blank"
            rel="noopener"
            class="text-xs text-accent hover:underline"
          >
            Install Ollama and run `ollama serve`
          </a>
        </div>

        <Show when={ollamaTest()}>
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
        <label class="text-text-primary text-sm block mb-1">
          <span>Ollama Model</span>
          <button
            onClick={() => void refreshOllamaModels()}
            disabled={ollamaModelsLoading()}
            class="ml-2 text-xs text-accent hover:underline disabled:opacity-50"
            title="Re-fetch the installed-model list from the Ollama server"
          >
            {ollamaModelsLoading() ? "Refreshing…" : "Refresh"}
          </button>
        </label>

        {/* Dropdown of installed models. Falls back to the freeform
            text input below when the list is empty OR a fetch error
            occurred — keeps the UI useful even when Ollama is down. */}
        <Show
          when={ollamaModels().length > 0}
          fallback={
            <div class="space-y-1.5">
              <input
                type="text"
                placeholder="llama3.2:3b"
                value={ollamaModelInput()}
                onInput={(e) => setOllamaModelInput(e.currentTarget.value)}
                onKeyDown={(e) => {
                  if (e.key === "Enter") void handleSaveOllama();
                }}
                class="w-full bg-bg-secondary border border-border rounded-lg px-3 py-2 text-sm text-text-primary placeholder-text-muted focus:outline-none focus:border-accent font-mono"
              />
              <Show
                when={ollamaModelsError()}
                fallback={
                  <div class="text-xs text-text-muted">
                    No installed models found. Pull one with{" "}
                    <span class="font-mono">ollama pull llama3.2:3b</span>{" "}
                    then click Refresh.
                  </div>
                }
              >
                {(err) => (
                  <div class="text-xs text-amber-400">
                    Couldn't fetch installed models: {err()}. You can still
                    type a model id manually.
                  </div>
                )}
              </Show>
            </div>
          }
        >
          <select
            class="w-full bg-bg-secondary border border-border rounded-lg px-3 py-2 text-sm text-text-primary focus:outline-none focus:border-accent font-mono"
            value={ollamaModelInput()}
            onChange={(e) => {
              setOllamaModelInput(e.currentTarget.value);
              void handleSaveOllama();
            }}
          >
            <option value="">Pick a model…</option>
            <For each={ollamaModels()}>
              {(m) => (
                <option value={m.name}>
                  {m.name} · {formatBytes(m.size_bytes)}
                </option>
              )}
            </For>
          </select>
        </Show>

        <div class="flex items-center gap-2 mt-1.5">
          <button
            onClick={handleSaveOllama}
            class="px-4 py-1.5 bg-accent text-bg-primary rounded-lg text-sm font-medium hover:opacity-90 transition-opacity"
          >
            Save
          </button>
          <Show when={ollamaSaved()}>
            <span class="text-xs text-green">Saved</span>
          </Show>
        </div>

        <Show when={ollamaSaveError()}>
          {(err) => (
            <div class="mt-1.5 text-xs text-red">
              Save failed: {err()}
            </div>
          )}
        </Show>

        <div class="text-xs text-text-muted mt-1">
          Active:{" "}
          <span class="font-mono text-text-secondary">
            {settingsStore.settings().ollama_model || "(none)"}
          </span>
        </div>
      </div>

      {/* Advanced — collapsed by default. Toggle is intentionally not
          persisted — if the field has a value, expanding it on every
          visit would be more noise than help; the saved value sticks
          in settings regardless. */}
      <div>
        <button
          onClick={() => setShowOllamaAdvanced(!showOllamaAdvanced())}
          class="text-xs text-text-secondary hover:text-text-primary transition-colors"
        >
          {showOllamaAdvanced() ? "▾" : "▸"} Advanced (auth proxy)
        </button>
        <Show when={showOllamaAdvanced()}>
          <div class="mt-2 space-y-1">
            <label class="text-text-muted text-xs block">
              Bearer token — only needed if you front Ollama with an auth proxy.
            </label>
            <div class="flex gap-2">
              <input
                type="password"
                placeholder="(none)"
                value={ollamaKeyInput()}
                onInput={(e) => setOllamaKeyInput(e.currentTarget.value)}
                onKeyDown={(e) => {
                  if (e.key === "Enter") void handleSaveOllama();
                }}
                class="flex-1 bg-bg-secondary border border-border rounded-lg px-3 py-2 text-sm text-text-primary placeholder-text-muted focus:outline-none focus:border-accent"
              />
              <button
                onClick={handleSaveOllama}
                class="px-3 py-2 border border-border bg-bg-secondary text-text-primary rounded-lg text-sm hover:bg-bg-hover transition-colors"
              >
                Save
              </button>
            </div>
          </div>
        </Show>
      </div>
    </>
  );
}
