/**
 * One place for "a user gesture's mutating call failed — tell the user".
 *
 * Why this exists (SAF device matrix, 7–8 Nov 2026, rows A12/A15/A18): six
 * sidebar entry points and a handful of settings buttons caught an IPC
 * failure and only called `log.error`. On a phone, where nobody reads the
 * log, a refused delete (`…cannot move items to the trash, so nothing was
 * deleted`) or a refused create looked like "nothing happened". The files
 * were safe; the user was blind. `reportActionError` logs AND shows a toast,
 * so the two can't be done separately and forgotten.
 *
 * `tests/unit/silent-catch-guard.test.ts` freezes the class: a new
 * `catch { log.error(...) }` anywhere under `src/` fails the build unless it
 * is deliberately added to that file's baseline.
 */

import { toastStore } from "../components/Toast/Toast";
import { errMsg } from "./error";
import { log } from "./log";

export interface ActionErrorOpts {
  /** Stable machine code shown in the toast footer + bug reports. */
  code: string;
  /** Subsystem tag (same string you would give `log.error`). */
  module: string;
  /**
   * Append the error's own sentence to the visible message. Use it where the
   * backend's text is written for the user and is the actionable part
   * ("…Delete it with your file manager instead."). Leave it off where the
   * text is a raw provider/IPC string — it stays one tap away under "Copy
   * details" either way.
   */
  showReason?: boolean;
}

/**
 * Provider and OS error text can carry absolute paths (`/storage/emulated/0/…`,
 * `content://…`). They belong in logs and "Copy details", not in the line a
 * user reads and screenshots.
 */
export function scrubPaths(text: string): string {
  return text
    .replace(/content:\/\/\S+/g, "<path>")
    .replace(/\/storage\/\S+/g, "<path>")
    .replace(/\b[A-Za-z]:\\\S+/g, "<path>");
}

/**
 * Log a failed user-initiated action and show it. Never throws.
 *
 * @param message Short, user-facing, past-tense ("Failed to delete note").
 */
export function reportActionError(
  message: string,
  e: unknown,
  opts: ActionErrorOpts,
): void {
  log.error(opts.module, message, e);
  const visible = opts.showReason ? `${message}: ${scrubPaths(errMsg(e))}` : message;
  toastStore.showError(visible, e, { code: opts.code, module: opts.module });
}
