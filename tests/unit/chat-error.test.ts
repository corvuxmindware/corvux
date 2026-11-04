/**
 * Tests for `src/lib/chat-error.ts` — classifies raw backend/provider
 * error strings into friendly chat-panel cards. Regression guard for the
 * 2026-05-27 fix that stopped surfacing the raw Gemini 429 JSON blob to
 * the user and replaced it with a "Rate limit reached → Upgrade to
 * Corvux Pro" card.
 */
import { describe, it, expect } from "vitest";
import {
  classifyChatError,
  isRateLimitError,
  CORVUX_PRO_URL,
} from "../../src/lib/chat-error";

// The exact blob the user saw surfaced in the chat panel.
const GEMINI_429 = `Gemini API error (429 Too Many Requests): { "error": { "code": 429, "message": "You exceeded your current quota, please check your plan and billing details.", "status": "RESOURCE_EXHAUSTED", "details": [ { "@type": "type.googleapis.com/google.rpc.QuotaFailure", "violations": [ { "quotaMetric": "generativelanguage.googleapis.com/generate_content_free_tier_requests", "quotaId": "GenerateRequestsPerMinutePerProjectPerModel-FreeTier" } ] }, { "@type": "type.googleapis.com/google.rpc.RetryInfo", "retryDelay": "59s" } ] } }`;

describe("isRateLimitError", () => {
  it("matches the Gemini 429 blob", () => {
    expect(isRateLimitError(GEMINI_429)).toBe(true);
  });

  it.each([
    // Gemini
    "Gemini API error (429): rate exceeded",
    "status: RESOURCE_EXHAUSTED",
    "You exceeded your current quota",
    // OpenRouter HTTP path (status code present)
    "OpenRouter API error (429): {...}",
    // OpenAI-compatible / OpenRouter / Claude mid-stream codes (no status)
    `{"error":{"code":"rate_limit_exceeded","message":"Rate limit reached"}}`,
    `{"type":"rate_limit_error","message":"Number of requests exceeded"}`,
    `{"error":{"code":"insufficient_quota"}}`,
    // Ollama / generic
    "Ollama API error (429): too many requests",
    "Error: Too Many Requests",
    "Provider rate-limit hit",
  ])("matches rate-limit phrasing: %s", (msg) => {
    expect(isRateLimitError(msg)).toBe(true);
  });

  it.each([
    "Gemini API error (400): invalid request",
    "Network error: connection refused",
    "Invalid API key (401)",
    "context length exceeded",
  ])("does not match unrelated errors: %s", (msg) => {
    expect(isRateLimitError(msg)).toBe(false);
  });
});

describe("classifyChatError", () => {
  it("classifies the Gemini 429 as rate_limit with an upgrade CTA", () => {
    const info = classifyChatError(GEMINI_429);
    expect(info.kind).toBe("rate_limit");
    expect(info.title).toBe("Rate limit reached");
    expect(info.body).toContain("Corvux Pro");
    expect(info.cta?.label).toBe("Upgrade to Corvux Pro");
    // Domain regression guard: must point at the canonical marketing
    // domain, not the dead corvux.app placeholder (fixed 2026-06-04).
    expect(info.cta?.url).toBe(CORVUX_PRO_URL);
    expect(info.cta?.url).toContain("meetcorvux.com");
    expect(info.cta?.url).not.toContain("corvux.app");
    expect(info.raw).toBe(GEMINI_429);
    // Crucially, the raw JSON blob must NOT leak into the user-facing body.
    expect(info.body).not.toContain("RESOURCE_EXHAUSTED");
    expect(info.body).not.toContain("googleapis.com");
  });

  it("passes through generic errors with the raw message as the body", () => {
    const info = classifyChatError("Network error: connection refused");
    expect(info.kind).toBe("generic");
    expect(info.body).toBe("Network error: connection refused");
    expect(info.cta).toBeUndefined();
  });
});
