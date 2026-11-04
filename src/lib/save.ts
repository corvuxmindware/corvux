/**
 * AUDIT-053: thin TS wrapper around the bundled `save_roost_note` IPC.
 *
 * Lives in its own file (not inline at the call site) so the contract
 * stays testable without rendering a component, and so the type shape
 * has a single source of truth across `tabs.ts` / `roost.ts` /
 * `effects.ts`.
 *
 * Replaces the prior fan-out: every markdown save used to fire
 * `write_file` then chain `index_note` → `get_backlinks` (with
 * `index_search` racing on the side). On mobile each one hopped the
 * WebView/JNI bridge. This wrapper hits one IPC and gets everything in
 * the response.
 *
 * Post-C7: the Rust shape is the canonical source. All types come from
 * the generated tauri-specta bindings (`./bindings`).
 */

import { commands, unwrap } from "./invoke";
import type { SaveOutcome } from "./bindings";

// Re-exported from the generated bindings so existing call sites at
// `tabs.ts` / `roost.ts` / `effects.ts` keep their import paths.
export type {
  GraphStatsResult as GraphStats,
  TaskRow,
  BacklinkResult as BacklinkEntry,
  Fts5Outcome,
  SaveOutcome,
} from "./bindings";

/**
 * Save a markdown note in the active roost. ONE IPC; returns the
 * post-save graph stats, task list, backlinks, and FTS5 status in a
 * single round-trip.
 *
 * Rejects non-markdown paths (use `commands.writeFile(...)` for
 * those). Rejects when no roost is open. Errors from the embed
 * pipeline don't surface here — that work is queued and drained by
 * the background drainer.
 *
 * `baseHash` (EDT-1) is the SHA-256 of the content the editor loaded. When
 * provided, the backend refuses the save with a `__save_conflict__` error
 * if the on-disk file has diverged from that base (a second tab, external
 * editor, or sync peer wrote in between). Pass `null` to skip the guard.
 */
export function saveRoostNote(
  path: string,
  content: string,
  baseHash: string | null = null,
): Promise<SaveOutcome> {
  return unwrap(commands.saveRoostNote(path, content, baseHash));
}

/** EDT-1: prefix the backend uses on last-write-wins conflict errors. */
export const SAVE_CONFLICT_PREFIX = "__save_conflict__";

