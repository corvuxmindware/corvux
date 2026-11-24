// @vitest-environment happy-dom
/**
 * Tests for `AllowedAiProviders` — the Settings → Privacy list of AI
 * providers the user allowed to receive chats (App Store guideline 5.1.2(i)),
 * with a Withdraw button each.
 *
 * Locks: no false "none yet" while loading; a failed load degrades to a
 * message instead of crashing the Privacy panel (a Solid resource rethrows
 * its error on read); Withdraw revokes and refreshes the list.
 */
import { describe, it, expect, afterEach, vi, beforeEach } from "vitest";
import { render } from "solid-js/web";

const api = vi.hoisted(() => ({
  aiConsentList: vi.fn(),
  aiConsentRevoke: vi.fn(),
}));

vi.mock("../../src/lib/invoke", () => ({
  commands: api,
  unwrap: async <T,>(p: Promise<{ status: "ok"; data: T } | { status: "error"; error: unknown }>) => {
    const r = await p;
    if (r.status === "error") throw r.error;
    return r.data;
  },
}));
vi.mock("../../src/components/Toast/Toast", () => ({
  toastStore: { show: vi.fn(), showError: vi.fn() },
}));

import AllowedAiProviders, {
  consentLabel,
} from "../../src/components/Settings/AllowedAiProviders";

let dispose: (() => void) | null = null;
let container: HTMLDivElement | null = null;

beforeEach(() => {
  api.aiConsentList.mockReset();
  api.aiConsentRevoke.mockReset();
});

afterEach(() => {
  dispose?.();
  dispose = null;
  container?.remove();
  container = null;
});

function mount() {
  container = document.createElement("div");
  document.body.appendChild(container);
  dispose = render(() => <AllowedAiProviders />, container);
  return container;
}

const flush = () => new Promise((r) => setTimeout(r, 0));

describe("AllowedAiProviders", () => {
  it("shows nothing while loading, then 'none yet' for an empty list", async () => {
    let resolve!: (v: unknown) => void;
    api.aiConsentList.mockReturnValue(new Promise((r) => (resolve = r)));
    const el = mount();
    await flush();
    expect(el.textContent).not.toContain("none yet");
    resolve({ status: "ok", data: [] });
    await flush();
    expect(el.textContent).toContain("none yet");
  });

  it("lists granted providers and withdraws one", async () => {
    api.aiConsentList
      .mockResolvedValueOnce({ status: "ok", data: ["gemini", "ollama:ollama.com"] })
      .mockResolvedValueOnce({ status: "ok", data: ["ollama:ollama.com"] });
    api.aiConsentRevoke.mockResolvedValue({ status: "ok", data: null });
    const el = mount();
    await flush();
    expect(el.textContent).toContain("Google Gemini");
    expect(el.textContent).toContain("Ollama Cloud (ollama.com)");

    const buttons = [...el.querySelectorAll("button")];
    expect(buttons).toHaveLength(2);
    buttons[0].click();
    await flush();
    await flush();
    expect(api.aiConsentRevoke).toHaveBeenCalledWith("gemini");
    expect(el.textContent).not.toContain("Google Gemini");
    expect(el.textContent).toContain("Ollama Cloud (ollama.com)");
  });

  it("degrades to a message when the list can't load (no crash)", async () => {
    api.aiConsentList.mockResolvedValue({ status: "error", error: "disk error" });
    const el = mount();
    await flush();
    expect(el.textContent).toContain("Couldn't load");
    expect(el.textContent).not.toContain("none yet");
  });
});

describe("consentLabel", () => {
  it("labels every consent id shape the backend produces", () => {
    expect(consentLabel("anthropic")).toBe("Anthropic (Claude)");
    expect(consentLabel("ollama:localhost")).toBe("Ollama at localhost");
    expect(consentLabel("ollama:ollama.com")).toBe("Ollama Cloud (ollama.com)");
  });
});
