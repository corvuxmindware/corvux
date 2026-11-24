/**
 * Canonical "what string do I show the user" extractor.
 *
 * `catch (e)` gives us `unknown`. The historical pattern duplicated across
 * 20+ sites was `e instanceof Error ? e.message : String(e)`, with some
 * sites also handling the case where the thrown value is already a string
 * (`typeof e === "string" ? e : …`). This helper bundles both — extra
 * branches don't hurt the common case, and the string passthrough avoids
 * "Error: foo" wrapping when callers throw bare strings (Tauri IPC, for
 * instance, can reject with a string payload).
 */
export function errMsg(e: unknown): string {
  if (typeof e === "string") return e;
  if (e instanceof Error) return e.message;
  return String(e);
}

/** Phrases the backend uses when the USER said "no" in a native OS
 *  confirmation dialog it showed for a security-relevant change:
 *  - `save_settings`:        "Not saved: the security confirmation was cancelled, …"
 *  - `sync_pin_custom_queue_host`: "Not trusted: the confirmation was cancelled, …"
 *  - `write_roost_manifest` / `delete_roost_index`: "Roost removal was cancelled."
 *  - `open_roost` (R2):      the `ROOST_OPEN_DECLINED` token
 *  - `sync_connect` / `sync_connect_iroh` (R1): the `SYNC_PAIR_DECLINED` token
 *  Matched case-insensitively as substrings so a wrapper prefix
 *  ("Error: …") doesn't hide it. */
const SECURITY_CANCEL_MARKERS = [
  "confirmation was cancelled",
  "roost removal was cancelled",
  "roost_open_declined",
  "sync_pair_declined",
];

/** True when `e` is the user declining a native security confirmation —
 *  a choice, not a failure. Callers should revert any optimistic UI and
 *  show at most an info-level notice, never an error toast. */
export function isSecurityCancel(e: unknown): boolean {
  const msg = errMsg(e).toLowerCase();
  return SECURITY_CANCEL_MARKERS.some((m) => msg.includes(m));
}
