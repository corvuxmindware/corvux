// Turns raw provider/backend error strings (e.g. the Gemini 429 JSON blob)
// into a friendly, user-facing message. The backend emits these verbatim
// over the `ai:error` event — they're fine for logs but hostile in the UI,
// so we classify here and the chat panel renders the friendly variant.
//
// NOTE: the rate-limit ceiling lives on the *user's own* provider key
// (BYO-key, Locked Decision #10). The "Corvux Pro" framing is a product
// decision (chosen 2026-05-27) — it only resolves the user's problem if a
// Pro tier proxies AI through Corvux-managed keys.

/** `notice` = not an error (e.g. the user declined the AI consent prompt);
 *  rendered neutral, not red. */
export type ChatErrorKind = "rate_limit" | "generic" | "notice";

/** Prefix the chat store puts on a declined-consent message so it is shown
 *  as a neutral notice. Set only by `stores/chat.ts`, never by the backend. */
export const AI_CONSENT_DECLINED_PREFIX = "AI_CONSENT_DECLINED:";

export interface ChatErrorInfo {
  kind: ChatErrorKind;
  /** Short headline for the error card. */
  title: string;
  /** One- or two-sentence explanation in plain language. */
  body: string;
  /** Optional call-to-action button (label + external URL). */
  cta?: { label: string; url: string };
  /** The original backend string, kept for logging / "show details". */
  raw: string;
}

/** Where the upgrade CTA points — the live pricing page on the canonical
 *  marketing domain. Swap for an in-app billing route once that exists. */
export const CORVUX_PRO_URL = "https://meetcorvux.com/pricing";

/** Detects provider rate-limit / quota-exhaustion errors. Provider-agnostic
 *  by design — works for any chat key the user configures:
 *    • HTTP path (all providers): the string carries the numeric status, so
 *      "429" alone catches Gemini, OpenRouter, Ollama, etc.
 *    • Mid-stream SSE path: only the provider's `message`/error-code arrives
 *      (no status), so we also match each provider's vocabulary —
 *      Gemini ("RESOURCE_EXHAUSTED"), OpenAI-compatible / OpenRouter / Claude
 *      ("rate_limit_exceeded", "rate_limit_error", "insufficient_quota"),
 *      and the generic "rate limit" / "too many requests" / "quota exceeded".
 */
export function isRateLimitError(raw: string): boolean {
  const s = raw.toLowerCase();
  return (
    s.includes("429") ||
    s.includes("too many requests") ||
    s.includes("resource_exhausted") ||
    s.includes("rate limit") ||
    s.includes("rate-limit") ||
    s.includes("rate_limit") ||
    s.includes("insufficient_quota") ||
    (s.includes("quota") && s.includes("exceed"))
  );
}

export interface ClassifyOptions {
  /** False on iOS: App Store guideline 3.1.1 forbids buttons or links that
   *  steer users to a purchase outside In-App Purchase, so the card drops
   *  the Pro pitch and CTA there. Defaults to true. */
  allowPurchaseCta?: boolean;
}

export function classifyChatError(raw: string, opts: ClassifyOptions = {}): ChatErrorInfo {
  const allowPurchaseCta = opts.allowPurchaseCta ?? true;
  if (raw.startsWith(AI_CONSENT_DECLINED_PREFIX)) {
    return {
      kind: "notice",
      title: "Not sent",
      body: raw.slice(AI_CONSENT_DECLINED_PREFIX.length),
      raw,
    };
  }
  // Backend consent gate (`commands/ai/consent.rs`). Normally the prompt in
  // `ai-consent.ts` runs first; this covers a provider switch in between.
  if (raw.startsWith("AI_CONSENT_REQUIRED:")) {
    return {
      kind: "generic",
      title: "Permission needed",
      body: "Corvux needs your permission before sending this to your AI provider. Send your message again to review the request.",
      raw,
    };
  }
  if (isRateLimitError(raw)) {
    if (!allowPurchaseCta) {
      return {
        kind: "rate_limit",
        title: "Rate limit reached",
        body: "You've hit your AI provider's rate limit. Wait a minute and try again, or check the usage limits on your provider account.",
        raw,
      };
    }
    return {
      kind: "rate_limit",
      title: "Rate limit reached",
      body: "You've hit your AI provider's rate limit. Upgrade to Corvux Pro for higher limits and uninterrupted AI.",
      cta: { label: "Upgrade to Corvux Pro", url: CORVUX_PRO_URL },
      raw,
    };
  }

  return {
    kind: "generic",
    title: "Something went wrong",
    body: raw,
    raw,
  };
}
