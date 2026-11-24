/**
 * Settings store — provider API keys are WRITE-ONLY from the webview
 * (sharp-edges finding F11).
 *
 * Contract under test:
 *  - `get_settings` never returns a key; "is one saved?" comes only from
 *    `get_provider_key_status`, so `hasApiKey()` is driven by that status.
 *  - `save_settings` key fields: `null` = leave unchanged, `""` = remove,
 *    non-empty = set. So clearing a key must send `""`, and a save that does
 *    not touch keys must send `null` for all five.
 *  - After a successful save the store holds NO key strings (a key the user
 *    just typed must not linger in the long-lived store).
 *  - A declined native confirmation (`Not saved: the security confirmation
 *    was cancelled…`) is the user's choice: the save rejects (so the caller
 *    can revert its UI), the stored value is untouched, and it is not logged
 *    as an error.
 */

import { describe, it, expect, beforeEach, vi } from "vitest";
import { mockInvoke } from "./setup";
import { settingsStore } from "../../src/stores/settings";
import { isSecurityCancel } from "../../src/lib/error";
import { log } from "../../src/lib/log";

type Provider = "gemini" | "openrouter" | "ollama" | "openai" | "anthropic";
type Status = Record<Provider, boolean>;
type Payload = Record<string, unknown>;

const ALL: Provider[] = ["gemini", "openrouter", "ollama", "openai", "anthropic"];
const KEY_FIELDS = ALL.map((p) => `${p}_api_key`);
const NONE: Status = {
  gemini: false,
  openrouter: false,
  ollama: false,
  openai: false,
  anthropic: false,
};

const DECLINED = "Not saved: the security confirmation was cancelled, so nothing was changed.";

/** A stand-in backend that follows the F11 contract. */
function fakeBackend(opts: {
  saved?: Partial<Status>;
  provider?: string;
  ollamaModel?: string;
  /** What `get_settings` returns for the gemini key (a misbehaving backend). */
  leakedGeminiKey?: string | null;
} = {}) {
  const state = {
    status: { ...NONE, ...opts.saved } as Status,
    payloads: [] as Payload[],
  };
  mockInvoke.set("get_settings", () => ({
    gemini_api_key: opts.leakedGeminiKey ?? null,
    model: "gemini-2.5-flash",
    llm_backend: "auto",
    agent_profile: "default",
    llm_provider: opts.provider ?? "gemini",
    ollama_model: opts.ollamaModel ?? "",
    agent_autonomy: {
      auto_approve_reads: true,
      auto_approve_writes: false,
      auto_approve_tools: [],
      agent_blocked_paths: [],
    },
  }));
  mockInvoke.set("get_provider_key_status", () => ({ ...state.status }));
  mockInvoke.set("save_settings", (args: { settings: Payload }) => {
    state.payloads.push(args.settings);
    for (const p of ALL) {
      const v = args.settings[`${p}_api_key`];
      if (typeof v === "string") state.status[p] = v.trim().length > 0; // "" removes
    }
    return undefined;
  });
  return state;
}

describe("hasApiKey() is driven by keyStatus", () => {
  beforeEach(() => {
    vi.spyOn(console, "log").mockImplementation(() => {});
  });

  it.each(["gemini", "openrouter", "openai", "anthropic"] as const)(
    "%s: ready only when ITS key is saved",
    async (provider) => {
      fakeBackend({ saved: { [provider]: true }, provider });
      await settingsStore.load();
      expect(settingsStore.keyStatus()[provider]).toBe(true);
      expect(settingsStore.hasApiKey()).toBe(true);

      // Every OTHER provider has a key, the active one does not.
      const others = Object.fromEntries(ALL.filter((p) => p !== provider).map((p) => [p, true]));
      fakeBackend({ saved: others, provider });
      await settingsStore.load();
      expect(settingsStore.hasApiKey()).toBe(false);
    },
  );

  it("ollama means 'a model is selected', whatever keyStatus says", async () => {
    fakeBackend({ saved: { ollama: false }, provider: "ollama", ollamaModel: "llama3.2:3b" });
    await settingsStore.load();
    expect(settingsStore.hasApiKey()).toBe(true);

    fakeBackend({ saved: { ollama: true }, provider: "ollama", ollamaModel: "" });
    await settingsStore.load();
    expect(settingsStore.hasApiKey()).toBe(false);
  });

  it("never trusts or keeps a key string from get_settings", async () => {
    // A backend that (wrongly) sends a key must not make the app look ready,
    // and the string must not be held in the store.
    fakeBackend({ saved: {}, provider: "gemini", leakedGeminiKey: "AIza-leaked" });
    await settingsStore.load();
    expect(settingsStore.settings().gemini_api_key).toBeNull();
    expect(settingsStore.hasApiKey()).toBe(false);
  });
});

describe("save() key semantics", () => {
  beforeEach(() => {
    vi.spyOn(console, "log").mockImplementation(() => {});
  });

  it("clearing a key sends an empty string (null now means 'unchanged')", async () => {
    const be = fakeBackend({ saved: { gemini: true }, provider: "gemini" });
    await settingsStore.load();
    expect(settingsStore.hasApiKey()).toBe(true);

    await settingsStore.save({ gemini_api_key: "" });

    expect(be.payloads.at(-1)?.gemini_api_key).toBe("");
    // keyStatus is re-read from the backend after the save.
    expect(settingsStore.keyStatus().gemini).toBe(false);
    expect(settingsStore.hasApiKey()).toBe(false);
  });

  it("a save that does not touch keys sends null for all five (leave unchanged)", async () => {
    const be = fakeBackend({ saved: { gemini: true, openai: true } });
    await settingsStore.load();

    await settingsStore.save({ model: "gemini-3.1-flash" });

    const sent = be.payloads.at(-1) as Payload;
    expect(sent.model).toBe("gemini-3.1-flash");
    for (const f of KEY_FIELDS) expect(sent[f], f).toBeNull();
    // ...and the saved keys are still reported as saved.
    expect(settingsStore.keyStatus().gemini).toBe(true);
    expect(settingsStore.keyStatus().openai).toBe(true);
  });

  it("a typed key goes out once, then the store holds null for all five", async () => {
    const be = fakeBackend();
    await settingsStore.load();

    await settingsStore.save({
      gemini_api_key: "AIza-secret-1",
      openrouter_api_key: "sk-or-v1-secret-2",
      ollama_api_key: "tok-secret-3",
      openai_api_key: "sk-secret-4",
      anthropic_api_key: "sk-ant-secret-5",
    });

    // The backend received the real values...
    const sent = be.payloads.at(-1) as Payload;
    expect(sent.gemini_api_key).toBe("AIza-secret-1");
    expect(sent.anthropic_api_key).toBe("sk-ant-secret-5");

    // ...the webview store keeps none of them.
    const kept = settingsStore.settings();
    for (const f of KEY_FIELDS) expect(kept[f as keyof typeof kept], f).toBeNull();
    expect(JSON.stringify(kept)).not.toContain("secret");

    // keyStatus tells the UI they are saved.
    expect(settingsStore.keyStatus()).toEqual({
      gemini: true,
      openrouter: true,
      ollama: true,
      openai: true,
      anthropic: true,
    });

    // A later, unrelated save must NOT resend (or wipe) them.
    await settingsStore.save({ model: "m2" });
    const next = be.payloads.at(-1) as Payload;
    for (const f of KEY_FIELDS) expect(next[f], f).toBeNull();
    expect(settingsStore.keyStatus().gemini).toBe(true);
  });

  it("a failed key-status refresh does not fail the save", async () => {
    fakeBackend();
    await settingsStore.load();
    vi.spyOn(log, "warn").mockImplementation(() => {});
    mockInvoke.set("get_provider_key_status", () => {
      throw new Error("status unavailable");
    });

    await expect(settingsStore.save({ model: "still-saved" })).resolves.toBeUndefined();
    expect(settingsStore.settings().model).toBe("still-saved");
  });
});

describe("a declined native confirmation", () => {
  beforeEach(() => {
    vi.spyOn(console, "log").mockImplementation(() => {});
  });

  function autonomyWith(auto_approve_writes: boolean) {
    return {
      agent_autonomy: {
        ...settingsStore.settings().agent_autonomy,
        auto_approve_writes,
      },
    };
  }

  it("rejects the save, leaves stored values alone, and is not logged as an error", async () => {
    fakeBackend({ saved: { gemini: true } });
    await settingsStore.load();
    const errorLog = vi.spyOn(log, "error").mockImplementation(() => {});
    const infoLog = vi.spyOn(log, "info").mockImplementation(() => {});
    const before = settingsStore.settings();
    const statusBefore = settingsStore.keyStatus();

    mockInvoke.set("save_settings", () => {
      throw new Error(DECLINED);
    });
    let caught: unknown;
    try {
      await settingsStore.save(autonomyWith(true));
    } catch (e) {
      caught = e;
    }

    expect(caught).toBeDefined();
    // The envelope stringifies a thrown Error ("Error: Not saved: …"), and the
    // helper still recognises it.
    expect(isSecurityCancel(caught)).toBe(true);
    // The stored value did not change, so a toggle bound to it can revert.
    expect(settingsStore.settings()).toBe(before);
    expect(settingsStore.settings().agent_autonomy.auto_approve_writes).toBe(false);
    expect(settingsStore.keyStatus()).toEqual(statusBefore);
    // A choice is not a fault.
    expect(errorLog).not.toHaveBeenCalled();
    expect(infoLog).toHaveBeenCalled();
  });

  it("a genuine failure (e.g. bad Ollama address) is still logged as an error", async () => {
    fakeBackend();
    await settingsStore.load();
    const errorLog = vi.spyOn(log, "error").mockImplementation(() => {});
    mockInvoke.set("save_settings", () => {
      throw new Error("The Ollama server address must use https:// for a non-local host");
    });

    let caught: unknown;
    try {
      await settingsStore.save({ ollama_base_url: "http://example.com" });
    } catch (e) {
      caught = e;
    }

    expect(isSecurityCancel(caught)).toBe(false);
    expect(errorLog).toHaveBeenCalledTimes(1);
  });

  it("does not poison the save queue: the next save still goes through", async () => {
    fakeBackend();
    await settingsStore.load();
    vi.spyOn(log, "info").mockImplementation(() => {});
    mockInvoke.set("save_settings", () => {
      throw new Error(DECLINED);
    });
    await expect(settingsStore.save(autonomyWith(true))).rejects.toBeDefined();

    const be = fakeBackend();
    await settingsStore.save({ model: "after-cancel" });
    expect(be.payloads.at(-1)?.model).toBe("after-cancel");
    expect(settingsStore.settings().model).toBe("after-cancel");
  });
});
