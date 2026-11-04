/**
 * EDT-1: content fingerprint for the save-time last-write-wins check.
 *
 * Computes the SHA-256 hex of note content using the WebCrypto API
 * (`crypto.subtle`), which is built into every Tauri webview — no bundled
 * dependency. The Rust save path computes the same digest over the bytes on
 * disk (`content_sha256_hex` in `commands/files.rs`); a mismatch means the
 * file changed under us since the editor loaded it, so the save is refused
 * with a `__save_conflict__` error instead of silently clobbering.
 *
 * Returns `null` if hashing is unavailable or fails for any reason. Callers
 * pass `null` straight through to the backend, which then skips the conflict
 * check entirely — so EDT-1 degrades to the prior behavior and can never
 * block a legitimate save.
 */
export async function sha256Hex(content: string): Promise<string | null> {
  try {
    const data = new TextEncoder().encode(content);
    const buf = await crypto.subtle.digest("SHA-256", data);
    return Array.from(new Uint8Array(buf))
      .map((b) => b.toString(16).padStart(2, "0"))
      .join("");
  } catch {
    // No secure context / no subtle crypto / unexpected failure: degrade to
    // no conflict guard rather than break saving.
    return null;
  }
}
