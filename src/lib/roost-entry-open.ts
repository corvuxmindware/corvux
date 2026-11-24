/**
 * Is this registry entry the roost that is actually open right now?
 *
 * The registry's `activeId` is the roost last picked, and it stays set when
 * that roost then fails to open (startup re-open after a reboot, a provider
 * that cannot list its root, a revoked folder). Treating `activeId` alone as
 * "open" showed the failed roost as current — ticked in Switch Roost, marked
 * Active in Manage Roosts with its Open button hidden — and ignored taps on
 * it, so the user had no in-app retry (SAF device matrix row A1, 24 Nov
 * 2026). Every "is this the current roost" check in the roost UI goes through
 * here so the switcher and the manager cannot drift apart again.
 */
export function isRoostEntryOpen(
  entry: { id: string; path: string },
  activeId: string | null,
  openPath: string | null,
): boolean {
  return openPath !== null && entry.id === activeId && entry.path === openPath;
}
