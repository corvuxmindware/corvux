/**
 * What a provider panel does when `settingsStore.save` rejects.
 *
 * Saving or removing a provider API key asks the user to confirm in a native OS
 * dialog (backend `settings_guard`, R3 / AK-8). Pressing Cancel there is the
 * user's CHOICE, not a failure: it must read as a calm "nothing changed", never
 * as a red "Save failed: Not saved: the security confirmation was cancelled…"
 * (review AK-5). Every panel that saves a key goes through this one function so
 * a fifth or sixth provider cannot forget it — `tests/unit/provider-key-ui.test.tsx`
 * scans the panels and fails if one calls `settingsStore.save` without it.
 */

import { errMsg, isSecurityCancel } from "../../../lib/error";
import { toastStore } from "../../Toast/Toast";

/** The one neutral notice shown when the user declines the confirmation. */
export const SAVE_CANCELLED_NOTICE = "Cancelled. Nothing was changed.";

/**
 * Report a rejected save/remove. `setError` is the panel's inline-error signal.
 *
 * - A declined native confirmation clears the inline error and shows an info
 *   toast; returns `true` so the caller can revert any inputs it changed.
 * - Anything else is a real failure: its message goes inline; returns `false`.
 */
export function reportProviderSaveFailure(
  e: unknown,
  setError: (message: string | null) => void,
): boolean {
  if (isSecurityCancel(e)) {
    setError(null);
    toastStore.show(SAVE_CANCELLED_NOTICE, "info");
    return true;
  }
  setError(errMsg(e));
  return false;
}
