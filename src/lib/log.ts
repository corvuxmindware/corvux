/**
 * Centralized frontend logger.
 *
 * Replaces ~108 scattered `console.warn`/`console.error` calls with a
 * scope-prefixed API that ALSO records entries in an in-memory ring
 * buffer. The ring is drained by `bugReport.ts` so frontend logs
 * finally make it into bug reports — pre-refactor they were emitted to
 * the dev console and lost the moment the user closed the page.
 *
 * Mirrors `src-tauri/src/diagnostics.rs` shape:
 *   - 500-entry cap
 *   - `{ ts, level, scope, message, details? }` per entry
 *   - drain() returns a snapshot (caller-owned, won't mutate underfoot)
 *
 * Scope convention: lowercase short identifier matching the module
 * area, e.g. "roost", "chat", "sync", "embed", "ai", "graph",
 * "settings". Future log viewer can filter on scope.
 */

export type LogLevel = "debug" | "info" | "warn" | "error";

export interface LogEntry {
  ts: number;
  level: LogLevel;
  scope: string;
  message: string;
  details?: unknown;
}

const RING_MAX = 500;
const ring: LogEntry[] = [];

function record(entry: LogEntry): void {
  ring.push(entry);
  // FIFO eviction — keep only the most recent RING_MAX. Long sessions
  // (hours of edits) don't accumulate unbounded memory.
  if (ring.length > RING_MAX) ring.shift();
}

function emit(level: LogLevel, scope: string, message: string, details?: unknown): void {
  record({ ts: Date.now(), level, scope, message, details });
  // Forward to the native console too — keeps live debugging in
  // Safari Web Inspector / Chrome devtools intact, and ensures any
  // pre-existing log filters / breakpoints still fire.
  const tag = `[corvux:${scope}]`;
  const fn =
    level === "error" ? console.error
    : level === "warn" ? console.warn
    : level === "debug" ? console.debug
    : console.log;
  if (details !== undefined) fn(tag, message, details);
  else fn(tag, message);
}

export const log = {
  /** Verbose dev-time tracing. Stripped from production builds. */
  debug(scope: string, message: string, details?: unknown): void {
    if (!import.meta.env.DEV) return;
    emit("debug", scope, message, details);
  },
  info(scope: string, message: string, details?: unknown): void {
    emit("info", scope, message, details);
  },
  warn(scope: string, message: string, details?: unknown): void {
    emit("warn", scope, message, details);
  },
  error(scope: string, message: string, details?: unknown): void {
    emit("error", scope, message, details);
  },

  /** Snapshot of the ring for bug reports. Returns a fresh array each
   *  call so callers can't mutate the live ring by accident. */
  drain(): readonly LogEntry[] {
    return ring.slice();
  },

  /** Wipe the ring. Used by tests; production callers shouldn't need
   *  this — the FIFO cap handles size. */
  reset(): void {
    ring.length = 0;
  },
};
