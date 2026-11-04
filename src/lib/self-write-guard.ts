/**
 * Self-write echo guard.
 *
 * The roost has a filesystem watcher (`index_watcher.rs`) that emits a
 * `note_written` / `note_deleted` sync event for EVERY change on disk — it
 * can't tell the app's own save apart from an outside edit. When the in-app
 * editor autosaves, that echo comes straight back and the `effects.ts`
 * handler would reload the open tab (a full CodeMirror doc-replace, which
 * drops the soft keyboard on mobile) or — when the atomic `write tmp → rename`
 * is misread as a removal — close the note outright.
 *
 * This tracks the notes the app just saved, keyed by the tab path that the
 * save and the event handler both use, so the handler can ignore the echo of
 * our own write. Genuine external/agent/peer writes are NOT recorded, so they
 * still reload the editor as intended.
 *
 * The window only needs to outlast the watcher debounce + IPC round-trip;
 * a real external edit landing inside it with the same path is vanishingly
 * rare and, if skipped, harmless (the buffer is already the saved content).
 */

const TTL_MS = 4000;

// path → epoch ms of the most recent in-app save.
const recent = new Map<string, number>();

function prune(now: number): void {
  const cutoff = now - TTL_MS;
  for (const [path, ts] of recent) {
    if (ts < cutoff) recent.delete(path);
  }
}

/** Record that the app just saved `path` (the tab path used at save time). */
export function recordSelfWrite(path: string): void {
  const now = Date.now();
  recent.set(path, now);
  prune(now);
}

/** True if `path` was saved by the app within the TTL window. */
export function wasRecentSelfWrite(path: string): boolean {
  const ts = recent.get(path);
  if (ts === undefined) return false;
  if (Date.now() - ts > TTL_MS) {
    recent.delete(path);
    return false;
  }
  return true;
}

/** Test-only: drop all tracked writes so cases don't bleed into each other. */
export function __resetSelfWriteGuard(): void {
  recent.clear();
}
