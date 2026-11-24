// @vitest-environment happy-dom
/**
 * Declining the native confirmation when saving / removing a provider key (or
 * the support address) is a CHOICE — review AK-5, 2026-11-12.
 *
 * Since R3 every key save raises a native OS dialog. The panels used to show
 * `Save failed: Not saved: the security confirmation was cancelled…` in red when
 * the user pressed Cancel, and first-run onboarding showed the same. This file
 * pins, for EVERY provider panel (the twins) and onboarding:
 *  - a decline is a calm info toast (cancel wording), never a red error;
 *  - nothing is cleared that the user typed;
 *  - a genuine failure still shows inline as an error;
 * and a source scan fails if a panel saves a gated setting without handling a
 * decline (so a sixth provider cannot forget it).
 */

import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import { render } from "solid-js/web";
import { readdirSync, readFileSync, statSync } from "node:fs";
import { join } from "node:path";
import { mockInvoke } from "./setup";

const toast = vi.hoisted(() => ({ show: vi.fn(), showError: vi.fn() }));
vi.mock("../../src/components/Toast/Toast", () => ({ toastStore: toast }));

// Onboarding pulls these in; the key step does not need them to be real.
vi.mock("../../src/stores/roost", () => ({
  roostStore: {
    openRoost: vi.fn(async () => {}),
    createNewNote: vi.fn(async () => null),
    roostPath: () => "/mock-roost",
  },
}));
vi.mock("../../src/stores/tabs", () => ({ tabStore: { openTab: vi.fn(async () => {}) } }));

import { settingsStore } from "../../src/stores/settings";
import GeminiProvider from "../../src/components/Settings/AISection/GeminiProvider";
import OpenRouterProvider from "../../src/components/Settings/AISection/OpenRouterProvider";
import OpenAiProvider from "../../src/components/Settings/AISection/OpenAiProvider";
import AnthropicProvider from "../../src/components/Settings/AISection/AnthropicProvider";
import OllamaProvider, {
  urlHasCredentials,
} from "../../src/components/Settings/AISection/OllamaProvider";
import {
  SAVE_CANCELLED_NOTICE,
  reportProviderSaveFailure,
} from "../../src/components/Settings/AISection/saveOutcome";
import Onboarding, {
  KEY_CANCELLED_NOTE,
  KEY_CONFIRM_HINT,
} from "../../src/components/Onboarding/Onboarding";

type Payload = Record<string, unknown>;
type Provider = "gemini" | "openrouter" | "openai" | "anthropic";

/** What the backend's native-dialog refusal looks like on the wire. */
const DECLINED = "Not saved: the security confirmation was cancelled, so nothing was changed.";
const flush = () => new Promise((r) => setTimeout(r, 0));

let dispose: (() => void) | null = null;
let container: HTMLDivElement | null = null;

function backend(opts: { saved?: Partial<Record<Provider | "ollama", boolean>>; baseUrl?: string } = {}) {
  const status = {
    gemini: false,
    openrouter: false,
    ollama: false,
    openai: false,
    anthropic: false,
    ...opts.saved,
  };
  const state = { saves: [] as Payload[], fail: null as null | string };
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
  mockInvoke.set("get_provider_key_status", () => ({ ...status }));
  mockInvoke.set("save_settings", (args: { settings: Payload }) => {
    state.saves.push(args.settings);
    if (state.fail !== null) throw new Error(state.fail);
    return undefined;
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

const PANELS: Array<{ name: Provider; Panel: () => unknown; savedChip: string }> = [
  { name: "gemini", Panel: () => <GeminiProvider />, savedChip: "Key saved" },
  { name: "openrouter", Panel: () => <OpenRouterProvider />, savedChip: "Key saved" },
  { name: "openai", Panel: () => <OpenAiProvider />, savedChip: "Key saved" },
  { name: "anthropic", Panel: () => <AnthropicProvider />, savedChip: "Key saved" },
];

describe.each(PANELS)("$name panel — declining the native confirmation", ({ name, Panel, savedChip }) => {
  it("saving a key: a decline is a calm info toast, the typed key stays, no red error", async () => {
    const be = backend();
    await settingsStore.load();
    const el = await mount(Panel);

    const input = el.querySelector('input[type="password"]') as HTMLInputElement;
    type(input, "typed-key-123");
    await flush();
    be.fail = DECLINED;
    button(el, "Save").click();
    await flush();

    expect(be.saves.at(-1)?.[`${name}_api_key`]).toBe("typed-key-123");
    expect(toast.show).toHaveBeenCalledWith(SAVE_CANCELLED_NOTICE, "info");
    expect(toast.showError).not.toHaveBeenCalled();
    expect(el.textContent).not.toContain("Save failed");
    expect(el.textContent).not.toContain("cancelled");
    // The user's typing is kept so they can simply press Save again.
    expect(input.value).toBe("typed-key-123");
    expect(settingsStore.keyStatus()[name]).toBe(false);
  });

  it("saving a key: a genuine failure is still shown inline as an error", async () => {
    const be = backend();
    await settingsStore.load();
    const el = await mount(Panel);

    const input = el.querySelector('input[type="password"]') as HTMLInputElement;
    type(input, "typed-key-123");
    await flush();
    be.fail = "keyring set: locked";
    button(el, "Save").click();
    await flush();

    expect(el.textContent).toContain("Save failed");
    expect(el.textContent).toContain("keyring set: locked");
    expect(toast.show).not.toHaveBeenCalledWith(expect.anything(), "info");
  });

  it("removing a saved key: a decline leaves the key saved and says so calmly", async () => {
    const be = backend({ saved: { [name]: true } });
    await settingsStore.load();
    const el = await mount(Panel);
    expect(el.textContent).toContain(savedChip);

    be.fail = DECLINED;
    button(el, "Remove").click();
    await flush();

    expect(be.saves.at(-1)?.[`${name}_api_key`]).toBe("");
    expect(toast.show).toHaveBeenCalledWith(SAVE_CANCELLED_NOTICE, "info");
    expect(toast.showError).not.toHaveBeenCalled();
    expect(el.textContent).not.toContain("Save failed");
    expect(el.textContent).toContain(savedChip);
    expect(settingsStore.keyStatus()[name]).toBe(true);
  });

  it("removing a saved key: a genuine failure is still shown inline", async () => {
    const be = backend({ saved: { [name]: true } });
    await settingsStore.load();
    const el = await mount(Panel);

    be.fail = "keyring delete: access denied";
    button(el, "Remove").click();
    await flush();

    expect(el.textContent).toContain("Save failed");
    expect(el.textContent).toContain("keyring delete: access denied");
  });
});

describe("OllamaProvider — token removal and probes", () => {
  it("removing the saved token: a decline is calm, a real error is inline", async () => {
    const be = backend({ saved: { ollama: true } });
    await settingsStore.load();
    const el = await mount(() => <OllamaProvider />);
    button(el, "▸ Advanced (auth proxy)").click();
    await flush();

    be.fail = DECLINED;
    button(el, "Remove").click();
    await flush();
    expect(toast.show).toHaveBeenCalledWith(SAVE_CANCELLED_NOTICE, "info");
    expect(el.textContent).not.toContain("Save failed");
    expect(el.textContent).toContain("Token saved");

    be.fail = "keyring delete: access denied";
    button(el, "Remove").click();
    await flush();
    expect(el.textContent).toContain("Save failed");
  });

  it("declining the 'send a test request?' dialog is not a red failed test", async () => {
    backend();
    await settingsStore.load();
    const el = await mount(() => <OllamaProvider />);
    mockInvoke.set("test_ollama_connection", () => {
      throw new Error("The test request confirmation was cancelled — nothing was sent.");
    });

    button(el, "Test").click();
    await flush();

    expect(toast.show).toHaveBeenCalledWith(
      expect.stringContaining("Cancelled"),
      "info",
    );
    expect(el.textContent).not.toContain("Test failed");
    expect(el.querySelector('[role="status"]')).toBeNull();
  });

  it("declining the dialog during a model refresh shows no 'couldn't fetch' warning", async () => {
    backend();
    await settingsStore.load();
    mockInvoke.set("list_ollama_models", () => {
      throw new Error("The test request confirmation was cancelled — nothing was sent.");
    });
    const el = await mount(() => <OllamaProvider />);
    toast.show.mockReset();

    button(el, "Refresh").click();
    await flush();

    expect(toast.show).toHaveBeenCalledWith(expect.stringContaining("Cancelled"), "info");
    expect(el.textContent).not.toContain("Couldn't fetch");
  });

  it("a real probe failure is still reported", async () => {
    backend();
    await settingsStore.load();
    const el = await mount(() => <OllamaProvider />);
    mockInvoke.set("test_ollama_connection", () => {
      throw new Error("connection refused");
    });

    button(el, "Test").click();
    await flush();

    expect(el.textContent).toContain("Test failed");
    expect(el.textContent).toContain("connection refused");
  });

  it("a legacy saved address with credentials shows a visible warning", async () => {
    backend({ baseUrl: "http://alice:s3cret@proxy.lan:11434" });
    await settingsStore.load();
    const el = await mount(() => <OllamaProvider />);
    const note = el.querySelector('[role="note"]');
    expect(note?.textContent).toContain("username and password");
    // A clean address shows none.
    dispose?.();
    container?.remove();
    backend({ baseUrl: "http://proxy.lan:11434" });
    await settingsStore.load();
    const el2 = await mount(() => <OllamaProvider />);
    expect(el2.querySelector('[role="note"]')).toBeNull();
  });

  it("urlHasCredentials only reports a real user:password@", () => {
    expect(urlHasCredentials("http://a:b@h.lan:1")).toBe(true);
    expect(urlHasCredentials("http://a@h.lan")).toBe(true);
    expect(urlHasCredentials("http://h.lan:11434")).toBe(false);
    expect(urlHasCredentials("https://h.example/p@th")).toBe(false);
    expect(urlHasCredentials("not a url")).toBe(false);
    expect(urlHasCredentials("")).toBe(false);
  });
});

describe("reportProviderSaveFailure", () => {
  it("returns true and clears the inline error for a decline, false and sets it otherwise", () => {
    const set = vi.fn();
    expect(reportProviderSaveFailure(new Error(DECLINED), set)).toBe(true);
    expect(set).toHaveBeenLastCalledWith(null);
    expect(toast.show).toHaveBeenCalledWith(SAVE_CANCELLED_NOTICE, "info");

    toast.show.mockReset();
    expect(reportProviderSaveFailure(new Error("Lock error: poisoned"), set)).toBe(false);
    expect(set).toHaveBeenLastCalledWith("Lock error: poisoned");
    expect(toast.show).not.toHaveBeenCalled();
  });
});

// ── Onboarding (first run) ──────────────────────────────────────────────────

describe("Onboarding — saving the first key", () => {
  /** Walk to the AI-setup step: Continue (theme) → Open Folder (roost). */
  async function toKeyStep() {
    const el = await mount(() => <Onboarding onComplete={() => {}} />);
    button(el, "Continue").click();
    await flush();
    const open = Array.from(el.querySelectorAll("button")).find((b) =>
      b.textContent?.includes("Open Folder"),
    ) as HTMLButtonElement;
    open.click();
    await flush();
    expect(el.textContent).toContain("AI setup");
    return el;
  }

  it("tells the user a confirmation is coming BEFORE they save, so the dialog is expected", async () => {
    backend();
    await settingsStore.load();
    const el = await toKeyStep();
    expect(el.textContent).toContain(KEY_CONFIRM_HINT);
  });

  it("a decline is a neutral note, not 'Save failed', and the user stays on the step", async () => {
    const be = backend();
    await settingsStore.load();
    const el = await toKeyStep();
    const input = el.querySelector('input[type="password"]') as HTMLInputElement;
    type(input, "AIza-first-run-key");
    await flush();

    be.fail = DECLINED;
    button(el, "Continue").click();
    await flush();

    expect(be.saves.at(-1)?.gemini_api_key).toBe("AIza-first-run-key");
    expect(el.textContent).toContain(KEY_CANCELLED_NOTE);
    expect(el.textContent).not.toContain("Save failed");
    expect(el.textContent).toContain("AI setup");
    // The typed key is kept; the note is not red.
    expect(input.value).toBe("AIza-first-run-key");
    const note = Array.from(el.querySelectorAll("div")).find((d) =>
      d.textContent?.trim() === KEY_CANCELLED_NOTE,
    );
    expect(note?.className).not.toContain("text-red");

    // Trying again after allowing it goes through and advances.
    be.fail = null;
    button(el, "Continue").click();
    await flush();
    expect(el.textContent).not.toContain(KEY_CANCELLED_NOTE);
    expect(el.textContent).not.toContain("AI setup");
  });

  it("a genuine failure is still a red 'Save failed'", async () => {
    const be = backend();
    await settingsStore.load();
    const el = await toKeyStep();
    type(el.querySelector('input[type="password"]') as HTMLInputElement, "AIza-x");
    await flush();

    be.fail = "keyring set: locked";
    button(el, "Continue").click();
    await flush();

    expect(el.textContent).toContain("Save failed");
    expect(el.textContent).toContain("keyring set: locked");
    expect(el.textContent).not.toContain(KEY_CANCELLED_NOTE);
  });
});

// ── Class guard ─────────────────────────────────────────────────────────────

/**
 * Settings the backend gates behind a native confirmation. A component that
 * saves one of them MUST handle the user pressing Cancel (isSecurityCancel or
 * reportProviderSaveFailure) — otherwise a choice renders as a red error.
 */
const GATED_FIELDS = [
  "_api_key",
  "support_endpoint",
  "ollama_base_url",
  "auto_approve_writes",
  "auto_approve_reads",
  "auto_approve_tools",
  "agent_blocked_paths",
];

function listTsx(dir: string, out: string[] = []): string[] {
  for (const name of readdirSync(dir)) {
    const p = join(dir, name);
    if (statSync(p).isDirectory()) listTsx(p, out);
    else if (p.endsWith(".tsx")) out.push(p);
  }
  return out;
}

describe("class guard — every component that saves a gated setting handles a decline", () => {
  it("no panel calls settingsStore.save with a gated field without isSecurityCancel / reportProviderSaveFailure", () => {
    const root = join(process.cwd(), "src", "components");
    const offenders: string[] = [];
    let checked = 0;
    for (const file of listTsx(root)) {
      const text = readFileSync(file, "utf8");
      if (!text.includes("settingsStore.save(")) continue;
      // Only look at what is passed to save(): the text up to the closing `);`
      // of each call, so an unrelated field elsewhere in the file is ignored.
      const calls = text.split("settingsStore.save(").slice(1).map((c) => c.split(");")[0]);
      if (!calls.some((c) => GATED_FIELDS.some((f) => c.includes(f)))) continue;
      checked++;
      if (!/isSecurityCancel|reportProviderSaveFailure/.test(text)) {
        offenders.push(file.slice(root.length + 1));
      }
    }
    // The five provider panels, Onboarding and SettingsPanel (the support
    // address). AgentAutonomyPanel saves a variable, so the scan cannot see its
    // field; it is covered by its own tests in provider-key-ui.test.tsx.
    expect(checked).toBeGreaterThanOrEqual(7);
    expect(offenders, `these components save a gated setting but do not handle a declined confirmation: ${offenders.join(", ")}`).toEqual([]);
  });
});
