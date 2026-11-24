/**
 * What the user reads when the roost they had open last fails to re-open at
 * app start.
 *
 * Why this exists (SAF device matrix row A1, re-tested 24 Nov 2026): the
 * startup re-open runs `openRoost(..., { silent: true })` so that it can
 * decide for itself whether the stored path is dead. Its catch then only
 * cleared dead paths and said nothing, so a roost that failed to open (a
 * provider that could not list its root, a revoked folder, a lock) left the
 * user on an ordinary-looking empty welcome screen with no error.
 */

import { errMsg } from "./error";
import { scrubPaths } from "./action-error";

export function startupRoostErrorMessage(
  name: string | null | undefined,
  e: unknown,
  removedFromList: boolean,
): string {
  const label = name && name.trim() ? `"${name.trim()}"` : "your last roost";
  const reason = scrubPaths(errMsg(e)).trim();
  const head = reason
    ? `Could not open ${label}: ${reason.replace(/[.\s]+$/, "")}.`
    : `Could not open ${label}.`;
  return removedFromList
    ? `${head} It was removed from your roost list. No files were deleted.`
    : `${head} To try again, open the sidebar, tap Switch Roost and pick it.`;
}
