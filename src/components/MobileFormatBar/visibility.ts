/**
 * Pure visibility rule for the mobile markdown format bar.
 *
 * Split out of `MobileFormatBar.tsx` so it can be unit-tested without
 * rendering — same split as `format-commands.ts`, whose `compute*` halves are
 * pure and its `view`-dispatching halves are not.
 *
 * History worth keeping: this predicate used to require `keyboardUp`. That
 * made the bar useless to the people who needed it most — a non-technical
 * user had to already be typing to discover the checkbox and bullet buttons,
 * and had to already know markdown to start typing. Dropping that condition
 * is the whole point of the parked state.
 */
export interface FormatBarVisibilityInput {
  /** Touch platform — phones and tablets. Desktop has its own chrome. */
  mobile: boolean;
  /** A note tab is active (not the graph tab, not an empty workspace). */
  fileTab: boolean;
  /** The active tab is locked in read mode. */
  readOnly: boolean;
  /** On-screen keyboard is open, so the bar docks to it. */
  keyboardUp: boolean;
  /** Chrome is auto-hidden because the user is scrolling. */
  chromeHidden: boolean;
}

export function shouldShowFormatBar(i: FormatBarVisibilityInput): boolean {
  if (!i.mobile || !i.fileTab || i.readOnly) return false;
  // While the keyboard is up the bar is docked to it and the user is
  // mid-edit — scroll-hide must not yank the buttons out from under their
  // thumb. Only the parked state follows the bottom bar's auto-hide.
  if (i.keyboardUp) return true;
  return !i.chromeHidden;
}
