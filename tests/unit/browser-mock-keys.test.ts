/**
 * Browser-dev mock (`src/lib/invoke.ts`) mirrors the backend's provider-key
 * contract (finding F11), so `pnpm dev` exercises the same UI paths as the
 * real app:
 *  - `get_settings` never returns a key;
 *  - `save_settings`: null = unchanged, ""/whitespace = remove, value = set;
 *  - `get_provider_key_status` reports booleans from what was saved.
 *
 * The suite-wide setup replaces `lib/invoke` with a stub, so this test loads
 * the REAL module via `vi.importActual`.
 */

import { describe, it, expect, beforeEach, vi } from "vitest";

type Mock = <T>(cmd: string, args?: Record<string, unknown>) => Promise<T>;
type Payload = Record<string, unknown>;
type Status = {
  gemini: boolean;
  openrouter: boolean;
  ollama: boolean;
  openai: boolean;
  anthropic: boolean;
};

const KEYS = [
  "gemini_api_key",
  "openrouter_api_key",
  "ollama_api_key",
  "openai_api_key",
  "anthropic_api_key",
] as const;

async function realMock(): Promise<Mock> {
  const actual = await vi.importActual<{ mockInvoke: Mock }>("../../src/lib/invoke");
  return actual.mockInvoke;
}

function allNull(): Payload {
  return Object.fromEntries(KEYS.map((k) => [k, null]));
}

describe("browser mock — provider key contract", () => {
  let invoke: Mock;

  beforeEach(async () => {
    invoke = await realMock();
    localStorage.clear();
    vi.spyOn(console, "log").mockImplementation(() => {});
  });

  it("fresh state: no keys saved, get_settings returns null keys", async () => {
    const s = await invoke<Payload>("get_settings");
    for (const k of KEYS) expect(s[k], k).toBeNull();
    expect(await invoke<Status>("get_provider_key_status")).toEqual({
      gemini: false,
      openrouter: false,
      ollama: false,
      openai: false,
      anthropic: false,
    });
  });

  it("a saved key shows in the status but is never handed back or stored in the settings blob", async () => {
    const base = await invoke<Payload>("get_settings");
    await invoke("save_settings", {
      settings: { ...base, ...allNull(), gemini_api_key: "AIza-mock-secret" },
    });

    expect((await invoke<Status>("get_provider_key_status")).gemini).toBe(true);
    const back = await invoke<Payload>("get_settings");
    expect(back.gemini_api_key).toBeNull();
    expect(localStorage.getItem("corvux-settings")).not.toContain("AIza-mock-secret");
  });

  it("null leaves a saved key unchanged (the redacted round-trip)", async () => {
    const base = await invoke<Payload>("get_settings");
    await invoke("save_settings", { settings: { ...base, ...allNull(), openai_api_key: "sk-1" } });
    // A later save that does not touch keys:
    await invoke("save_settings", { settings: { ...base, ...allNull(), model: "x" } });

    expect((await invoke<Status>("get_provider_key_status")).openai).toBe(true);
  });

  it('"" and whitespace remove only that provider\'s key', async () => {
    const base = await invoke<Payload>("get_settings");
    await invoke("save_settings", {
      settings: { ...base, ...allNull(), gemini_api_key: "AIza-1", anthropic_api_key: "sk-ant-1" },
    });

    await invoke("save_settings", { settings: { ...base, ...allNull(), gemini_api_key: "" } });
    let st = await invoke<Status>("get_provider_key_status");
    expect(st.gemini).toBe(false);
    expect(st.anthropic).toBe(true);

    await invoke("save_settings", { settings: { ...base, ...allNull(), anthropic_api_key: "   " } });
    st = await invoke<Status>("get_provider_key_status");
    expect(st.anthropic).toBe(false);
  });

  it("a legacy key left inside the settings blob is never returned", async () => {
    localStorage.setItem(
      "corvux-settings",
      JSON.stringify({ model: "m", gemini_api_key: "AIza-legacy" }),
    );
    const s = await invoke<Payload>("get_settings");
    expect(s.gemini_api_key).toBeNull();
    expect(s.model).toBe("m");
  });
});
