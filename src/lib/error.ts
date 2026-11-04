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
