/**
 * Tests for `src/lib/ai-consent.ts` — the explicit per-provider permission
 * asked before chat content goes to a third-party AI (App Store guideline
 * 5.1.2(i)). The Rust gate in `commands/ai/consent.rs` enforces it; this is
 * the prompt in front of it.
 */
import { describe, expect, it, vi } from "vitest";
import {
  AiConsentDeclined,
  consentPromptText,
  ensureAiConsent,
} from "../../src/lib/ai-consent";
import { AI_CONSENT_DECLINED_PREFIX, classifyChatError } from "../../src/lib/chat-error";

const GEMINI = {
  id: "gemini",
  provider_name: "Google Gemini",
  host: "generativelanguage.googleapis.com",
  detail: "",
};

function deps(over: Partial<Parameters<typeof ensureAiConsent>[0]> = {}) {
  return {
    check: vi.fn(async () => GEMINI as typeof GEMINI | null),
    grant: vi.fn(async (_id: string) => {}),
    confirm: vi.fn(async (_msg: string) => true),
    ...over,
  };
}

describe("consentPromptText", () => {
  it("names the provider, its host, and everything chat_send can include", () => {
    const text = consentPromptText(GEMINI);
    expect(text).toContain("Google Gemini");
    expect(text).toContain("generativelanguage.googleapis.com");
    // Must match what the backend actually sends (system prompt + tools):
    // messages, open note, note titles + tags, AGENTS.md, memory, and any
    // note / task / reminder the agent's tools read — scope stated plainly.
    for (const what of [
      "messages",
      "the note you have open",
      "note titles and tags",
      "AGENTS.md",
      "memory",
      "search and read any note",
      "tasks and reminders",
    ]) {
      expect(text).toContain(what);
    }
    expect(text).toContain("Settings → Privacy");
    // Not every provider uses an API key (e.g. a key-less Ollama server).
    expect(text).not.toContain("API key");
  });

  it("includes the provider-specific detail when there is one", () => {
    const text = consentPromptText({
      ...GEMINI,
      id: "openrouter",
      provider_name: "OpenRouter",
      host: "openrouter.ai",
      detail: "OpenRouter passes it on to the company that makes the model you picked (anthropic).",
    });
    expect(text).toContain("(anthropic)");
  });
});

describe("ensureAiConsent", () => {
  it("does nothing when no consent is pending", async () => {
    const d = deps({ check: vi.fn(async () => null) });
    await expect(ensureAiConsent(d)).resolves.toBe("not-needed");
    expect(d.confirm).not.toHaveBeenCalled();
    expect(d.grant).not.toHaveBeenCalled();
  });

  it("asks, then records the grant for that provider id", async () => {
    const d = deps();
    await expect(ensureAiConsent(d)).resolves.toBe("granted");
    expect(d.confirm).toHaveBeenCalledWith(consentPromptText(GEMINI));
    expect(d.grant).toHaveBeenCalledWith("gemini");
  });

  it("throws AiConsentDeclined and records nothing when the user declines", async () => {
    const d = deps({ confirm: vi.fn(async () => false) });
    const err = await ensureAiConsent(d).catch((e: unknown) => e);
    expect(err).toBeInstanceOf(AiConsentDeclined);
    expect((err as Error).message).toContain("Google Gemini");
    expect((err as Error).message).toContain("Nothing left your device");
    expect(d.grant).not.toHaveBeenCalled();
  });
});

describe("classifyChatError — declined consent", () => {
  it("is a neutral notice, not an error", () => {
    const info = classifyChatError(`${AI_CONSENT_DECLINED_PREFIX}Nothing left your device. X`);
    expect(info.kind).toBe("notice");
    expect(info.title).toBe("Not sent");
    expect(info.body).toBe("Nothing left your device. X");
    expect(info.cta).toBeUndefined();
  });
});

describe("classifyChatError — backend consent gate", () => {
  it("turns the gate's machine string into a readable message", () => {
    const info = classifyChatError("AI_CONSENT_REQUIRED:openrouter");
    expect(info.title).toBe("Permission needed");
    expect(info.body).not.toContain("AI_CONSENT_REQUIRED");
    expect(info.body).toContain("permission");
    expect(info.cta).toBeUndefined();
  });
});
