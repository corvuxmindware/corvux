/**
 * Safe localStorage helpers — wrap raw `localStorage.*` calls in a try/catch
 * so private-mode browsers, full-quota disks, and test environments
 * (happy-dom before `setupGlobals`) don't throw at unexpected sites.
 *
 * Replaces 40+ raw `localStorage.getItem/setItem/removeItem` calls
 * scattered across stores. The original duplicated pattern was
 *
 *     localStorage.getItem(KEY)
 *
 * which throws in Safari private mode and on quota-exceeded writes. The
 * helpers below degrade silently to `null` / no-op, matching what every
 * caller actually wanted but few caught.
 */

export function lsGet(key: string): string | null {
  if (typeof localStorage === "undefined") return null;
  try {
    return localStorage.getItem(key);
  } catch {
    return null;
  }
}

export function lsSet(key: string, value: string): void {
  if (typeof localStorage === "undefined") return;
  try {
    localStorage.setItem(key, value);
  } catch {
    /* private mode / quota — silently drop */
  }
}

export function lsRemove(key: string): void {
  if (typeof localStorage === "undefined") return;
  try {
    localStorage.removeItem(key);
  } catch {
    /* private mode — silently drop */
  }
}

/**
 * All localStorage keys starting with `prefix`. Guards the same way as
 * `lsGet`/`lsSet`/`lsRemove` above — callers that need to enumerate keys
 * (address-cache hydration, stale-entry sweeps) used to do their own raw
 * `localStorage.length` / `localStorage.key(i)` loop, which throws in the
 * same environments this file exists to protect against (happy-dom before
 * `setupGlobals`, private-mode browsers). Route every such loop through here
 * instead of re-introducing the unguarded pattern at a new call site.
 */
export function lsKeysWithPrefix(prefix: string): string[] {
  if (typeof localStorage === "undefined") return [];
  try {
    const keys: string[] = [];
    for (let i = 0; i < localStorage.length; i++) {
      const key = localStorage.key(i);
      if (key && key.startsWith(prefix)) keys.push(key);
    }
    return keys;
  } catch {
    return [];
  }
}
