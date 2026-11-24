// @vitest-environment happy-dom
/**
 * Settings → AI key UI under the write-only-key contract (finding F11).
 *
 * The webview never holds a saved provider key, so the UI must:
 *  - show only "Key saved" (no characters of the key);
 *  - start "Replace" with an EMPTY box;
 *  - never wipe a saved key as a side effect of Save on an empty box
 *    (Save is disabled; Remove is the explicit action and sends `""`);
 *  - test the saved key by sending an empty string (the backend resolves it);
 *  - revert an optimistic toggle / form when the user declines the native
 *    confirmation, with an info toast instead of an error.
 *
 * GeminiProvider stands in for the four cloud providers (same structure);
 * OllamaProvider has the base-URL confirmation + validation paths.
 */

import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import { render } from "solid-js/web";
import { mockInvoke } from "./setup";

const toast = vi.hoisted(() => ({ show: vi.fn(), showError: vi.fn() }));
vi.mock("../../src/components/Toast/Toast", () => ({ toastStore: toast }));

import { settingsStore } from "../../src/stores/settings";
import GeminiProvider from "../../src/components/Settings/AISection/GeminiProvider";
import OllamaProvider from "../../src/components/Settings/AISection/OllamaProvider";
import AgentAutonomyPanel from "../../src/components/Settings/AgentAutonomyPanel";

type Payload = Record<string, unknown>;

const DECLINED = "Not saved: the security confirmation was cancelled, so nothing was changed.";
const flush = () => new Promise((r) => setTimeout(r, 0));

let dispose: (() => void) | null = null;
let container: HTMLDivElement | null = null;

/** Fake backend following the F11 contract; records every save and probe. */
function fakeBackend(opts: { geminiSaved?: boolean; ollamaSaved?: boolean; baseUrl?: string } = {}) {
  const state = {
    gemini: !!opts.geminiSaved,
    ollama: !!opts.ollamaSaved,
    saves: [] as Payload[],
    geminiTests: [] as string[],
    ollamaTests: [] as Array<{ baseUrl: string; apiKey: string | null }>,
  };
  mockInvoke.set("get_settings", () => ({
    gemini_api_key: null,
    model: "gemini-2.5-flash",
    llm_backend: "auto",
    agent_profile: "default",
    llm_provider: "gemini",
    ollama_base_url: opts.baseUrl ?? "http://localhost:11434",
    ollama_model: "",
    ollama_api_key: null,
    agent_autonomy: {
      auto_approve_reads: true,
      auto_approve_writes: false,
      auto_approve_tools: [],
      agent_blocked_paths: [],
    },
  }));
  mockInvoke.set("get_provider_key_status", () => ({
    gemini: state.gemini,
    openrouter: false,
    ollama: state.ollama,
    openai: false,
    anthropic: false,
  }));
  mockInvoke.set("save_settings", (args: { settings: Payload }) => {
    state.saves.push(args.settings);
    const g = args.settings.gemini_api_key;
    if (typeof g === "string") state.gemini = g.trim().length > 0;
    const o = args.settings.ollama_api_key;
    if (typeof o === "string") state.ollama = o.trim().length > 0;
    return undefined;
  });
  mockInvoke.set("test_gemini_connection", (args: { apiKey: string }) => {
    state.geminiTests.push(args.apiKey);
    return { ok: true, latency_ms: 5, summary: "Key works", error_message: null };
  });
  mockInvoke.set("test_ollama_connection", (args: { baseUrl: string; apiKey: string | null }) => {
    state.ollamaTests.push({ baseUrl: args.baseUrl, apiKey: args.apiKey });
    return { ok: true, latency_ms: 5, summary: "Reachable", error_message: null };
  });
  mockInvoke.set("list_ollama_models", () => []);
  return state;
}

async function mount(ui: () => unknown) {
  container = document.createElement("div");
  document.body.appendChild(container);
  dispose = render(ui as () => never, container);
  await flush();
  return container;
}

function button(el: HTMLElement, label: string): HTMLButtonElement {
  const found = Array.from(el.querySelectorAll("button")).find(
    (b) => b.textContent?.trim() === label,
  );
  if (!found) throw new Error(`no "${label}" button in: ${el.textContent}`);
  return found as HTMLButtonElement;
}

function type(input: HTMLInputElement, value: string) {
  input.value = value;
  input.dispatchEvent(new Event("input", { bubbles: true }));
}

beforeEach(() => {
  toast.show.mockReset();
  toast.showError.mockReset();
  vi.spyOn(console, "log").mockImplementation(() => {});
  vi.spyOn(console, "error").mockImplementation(() => {});
  window.confirm = vi.fn(() => true) as unknown as typeof window.confirm;
});

afterEach(() => {
  dispose?.();
  dispose = null;
  container?.remove();
  container = null;
  vi.restoreAllMocks();
});

describe("GeminiProvider — saved key", () => {
  it("shows only 'Key saved', with an empty password box hidden until Replace", async () => {
    fakeBackend({ geminiSaved: true });
    await settingsStore.load();
    const el = await mount(() => <GeminiProvider />);

    expect(el.textContent).toContain("Key saved");
    expect(el.textContent).not.toContain("•••");
    expect(el.textContent).not.toContain("ends in");
    expect(el.querySelector('input[type="password"]')).toBeNull();
  });

  it("Replace starts with an EMPTY box and a replace-the-saved-one hint; Save stays off until typed", async () => {
    const be = fakeBackend({ geminiSaved: true });
    await settingsStore.load();
    const el = await mount(() => <GeminiProvider />);

    button(el, "Replace").click();
    await flush();

    const input = el.querySelector('input[type="password"]') as HTMLInputElement;
    expect(input.value).toBe("");
    expect(input.placeholder).toBe("Paste a new key to replace the saved one");
    expect(button(el, "Save").disabled).toBe(true);

    type(input, "AIza-new-key");
    await flush();
    expect(button(el, "Save").disabled).toBe(false);

    button(el, "Save").click();
    await flush();
    expect(be.saves.at(-1)?.gemini_api_key).toBe("AIza-new-key");
    // The typed key is not kept in the box or the store afterwards.
    expect(settingsStore.settings().gemini_api_key).toBeNull();
    expect(el.textContent).toContain("Key saved");
    expect(el.querySelector('input[type="password"]')).toBeNull();
  });

  it("Enter on an empty box saves nothing (never wipes the saved key)", async () => {
    const be = fakeBackend({ geminiSaved: true });
    await settingsStore.load();
    const el = await mount(() => <GeminiProvider />);
    button(el, "Replace").click();
    await flush();

    const input = el.querySelector('input[type="password"]') as HTMLInputElement;
    input.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true }));
    await flush();

    expect(be.saves).toHaveLength(0);
    expect(be.gemini).toBe(true);
  });

  it("Cancel leaves edit mode without touching the saved key", async () => {
    const be = fakeBackend({ geminiSaved: true });
    await settingsStore.load();
    const el = await mount(() => <GeminiProvider />);
    button(el, "Replace").click();
    await flush();

    button(el, "Cancel").click();
    await flush();

    expect(el.textContent).toContain("Key saved");
    expect(be.saves).toHaveLength(0);
  });

  it("Remove saves an empty string (null would now mean 'unchanged')", async () => {
    const be = fakeBackend({ geminiSaved: true });
    await settingsStore.load();
    const el = await mount(() => <GeminiProvider />);

    button(el, "Remove").click();
    await flush();

    expect(be.saves.at(-1)?.gemini_api_key).toBe("");
    expect(settingsStore.keyStatus().gemini).toBe(false);
    // Back to the empty input state.
    expect(el.querySelector('input[type="password"]')).not.toBeNull();
  });

  it("Remove does nothing if the user declines the confirmation", async () => {
    const be = fakeBackend({ geminiSaved: true });
    await settingsStore.load();
    window.confirm = vi.fn(() => false) as unknown as typeof window.confirm;
    const el = await mount(() => <GeminiProvider />);

    button(el, "Remove").click();
    await flush();

    expect(be.saves).toHaveLength(0);
    expect(el.textContent).toContain("Key saved");
  });

  it("Test with nothing typed sends an empty string — the backend uses the saved key", async () => {
    const be = fakeBackend({ geminiSaved: true });
    await settingsStore.load();
    const el = await mount(() => <GeminiProvider />);

    button(el, "Test").click();
    await flush();

    expect(be.geminiTests).toEqual([""]);
    expect(el.textContent).toContain("Key works");
  });
});

describe("GeminiProvider — no saved key", () => {
  it("Save and Test are off until a key is typed; Test then sends the typed key", async () => {
    const be = fakeBackend({ geminiSaved: false });
    await settingsStore.load();
    const el = await mount(() => <GeminiProvider />);

    const input = el.querySelector('input[type="password"]') as HTMLInputElement;
    expect(input.placeholder).toBe("AIza...");
    expect(button(el, "Save").disabled).toBe(true);
    expect(button(el, "Test").disabled).toBe(true);

    type(input, "AIza-typed");
    await flush();
    expect(button(el, "Save").disabled).toBe(false);
    expect(button(el, "Test").disabled).toBe(false);

    button(el, "Test").click();
    await flush();
    expect(be.geminiTests).toEqual(["AIza-typed"]);
  });
});

describe("OllamaProvider — confirmation and validation", () => {
  it("a declined address change snaps the form back and shows an info toast, not an error", async () => {
    const be = fakeBackend({ baseUrl: "http://localhost:11434" });
    await settingsStore.load();
    const el = await mount(() => <OllamaProvider />);

    const urlInput = el.querySelector('input[placeholder="http://localhost:11434"]') as HTMLInputElement;
    type(urlInput, "https://ollama.example.com");
    await flush();

    mockInvoke.set("save_settings", (args: { settings: Payload }) => {
      be.saves.push(args.settings);
      throw new Error(DECLINED);
    });
    button(el, "Save").click();
    await flush();

    expect(be.saves.at(-1)?.ollama_base_url).toBe("https://ollama.example.com");
    // Form shows what is actually stored.
    expect(urlInput.value).toBe("http://localhost:11434");
    expect(settingsStore.settings().ollama_base_url).toBe("http://localhost:11434");
    // Calm notice, no red error, no inline "Save failed".
    expect(toast.show).toHaveBeenCalledWith(expect.stringContaining("Cancelled"), "info");
    expect(toast.showError).not.toHaveBeenCalled();
    expect(el.textContent).not.toContain("Save failed");
  });

  it("an invalid address is shown inline as an error", async () => {
    fakeBackend();
    await settingsStore.load();
    const el = await mount(() => <OllamaProvider />);

    const urlInput = el.querySelector('input[placeholder="http://localhost:11434"]') as HTMLInputElement;
    type(urlInput, "http://example.com");
    await flush();

    mockInvoke.set("save_settings", () => {
      throw new Error("The Ollama server address must use https:// for a non-local host");
    });
    button(el, "Save").click();
    await flush();

    expect(el.textContent).toContain("Save failed");
    expect(el.textContent).toContain("Ollama server address");
    expect(toast.show).not.toHaveBeenCalledWith(expect.anything(), "info");
  });

  it("Test with no token typed sends null (backend attaches the saved token only for the saved server)", async () => {
    const be = fakeBackend({ ollamaSaved: true });
    await settingsStore.load();
    const el = await mount(() => <OllamaProvider />);

    button(el, "Test").click();
    await flush();

    expect(be.ollamaTests.at(-1)).toEqual({ baseUrl: "http://localhost:11434", apiKey: null });
  });

  it("saving without a typed token leaves the saved token alone (null), Remove sends an empty string", async () => {
    const be = fakeBackend({ ollamaSaved: true });
    await settingsStore.load();
    const el = await mount(() => <OllamaProvider />);

    button(el, "Save").click();
    await flush();
    expect(be.saves.at(-1)?.ollama_api_key).toBeNull();
    expect(be.ollama).toBe(true);

    button(el, "▸ Advanced (auth proxy)").click();
    await flush();
    expect(el.textContent).toContain("Token saved");
    const advanced = el.querySelector('input[type="password"]') as HTMLInputElement;
    expect(advanced.value).toBe("");
    expect(advanced.placeholder).toBe("Paste a new token to replace the saved one");

    button(el, "Remove").click();
    await flush();
    expect(be.saves.at(-1)?.ollama_api_key).toBe("");
    expect(be.ollama).toBe(false);
  });
});

describe("AgentAutonomyPanel — optimistic toggle", () => {
  const writesToggle = (el: HTMLElement) =>
    el.querySelector('input[type="checkbox"]') as HTMLInputElement;

  it("reverts the toggle and shows an info toast when the confirmation is declined", async () => {
    const be = fakeBackend();
    await settingsStore.load();
    const el = await mount(() => <AgentAutonomyPanel />);
    const toggle = writesToggle(el);
    expect(toggle.checked).toBe(false);

    mockInvoke.set("save_settings", (args: { settings: Payload }) => {
      be.saves.push(args.settings);
      throw new Error(DECLINED);
    });
    toggle.click(); // user flips it on
    await flush();

    expect(be.saves).toHaveLength(1);
    expect(toggle.checked).toBe(false);
    expect(settingsStore.settings().agent_autonomy.auto_approve_writes).toBe(false);
    expect(toast.show).toHaveBeenCalledWith(expect.stringContaining("Cancelled"), "info");
    expect(toast.showError).not.toHaveBeenCalled();
  });

  it("keeps the toggle on when the save goes through", async () => {
    fakeBackend();
    await settingsStore.load();
    const el = await mount(() => <AgentAutonomyPanel />);
    const toggle = writesToggle(el);

    toggle.click();
    await flush();

    expect(toggle.checked).toBe(true);
    expect(settingsStore.settings().agent_autonomy.auto_approve_writes).toBe(true);
    expect(toast.show).not.toHaveBeenCalled();
    expect(toast.showError).not.toHaveBeenCalled();
  });

  it("reverts and reports a real error when the save fails for another reason", async () => {
    fakeBackend();
    await settingsStore.load();
    const el = await mount(() => <AgentAutonomyPanel />);
    const toggle = writesToggle(el);

    mockInvoke.set("save_settings", () => {
      throw new Error("Lock error: poisoned");
    });
    toggle.click();
    await flush();

    expect(toggle.checked).toBe(false);
    expect(toast.showError).toHaveBeenCalledTimes(1);
    expect(toast.show).not.toHaveBeenCalledWith(expect.anything(), "info");
  });

  it("keeps a typed blocked-path pattern when the save is declined (un-block is the gated one)", async () => {
    const be = fakeBackend();
    await settingsStore.load();
    const el = await mount(() => <AgentAutonomyPanel />);
    const pattern = el.querySelector('input[type="text"]') as HTMLInputElement;
    type(pattern, "Journal/");
    await flush();

    mockInvoke.set("save_settings", (args: { settings: Payload }) => {
      be.saves.push(args.settings);
      throw new Error(DECLINED);
    });
    button(el, "Add").click();
    await flush();

    expect(pattern.value).toBe("Journal/");
    expect(settingsStore.settings().agent_autonomy.agent_blocked_paths).toEqual([]);
  });
});
