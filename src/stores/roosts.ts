import { createSignal, createMemo, createRoot } from "solid-js";
import { lsGet, lsSet, lsRemove } from "../lib/storage";
import { commands } from "../lib/invoke";
import { log } from "../lib/log";

// Roost registry — the list of known roosts the user has opened or added.
// Separate from `roostStore` (the *active* roost's files/content). Persisted
// to localStorage so the dropdown and Manage Roosts screen survive restarts.
// Removing a roost from the registry does NOT delete files on disk.

export type RoostType = "native" | "saf" | "ios-scoped";

export interface RoostEntry {
  id: string;
  path: string;
  displayName: string;
  type: RoostType;
  lastOpened: number;
  /// iOS only — base64 security-scoped bookmark needed to re-acquire
  /// access to a user-picked folder after process restart. Absent for
  /// app-private iOS roosts, Android SAF roosts, and all desktop roosts.
  bookmark?: string;
}

const STORAGE_KEY = "corvux:vaults";
const ACTIVE_KEY = "corvux:activeVaultId";

/**
 * The bookmark to use when opening `path`: an explicitly-passed one wins,
 * otherwise fall back to the bookmark stored in the registry for that path.
 *
 * Why this exists: iOS scoped roosts NEED their security-scoped bookmark
 * handed to `open_roost` so the backend can re-acquire access before any
 * `std::fs` touch — without it the OS denies the raw path (EPERM,
 * ROOST_OPEN_FAIL). Some call sites (RoostSwitcher, tree-refresh re-opens)
 * historically opened by path + name only, silently dropping the bookmark
 * and breaking iOS access. Resolving centrally means no caller can
 * reintroduce that bug. Pure function so it's unit-testable.
 */
export function resolveBookmark(
  explicit: string | undefined,
  entries: RoostEntry[],
  path: string,
): string | undefined {
  if (explicit) return explicit;
  return entries.find((v) => v.path === path)?.bookmark;
}

/**
 * Did `open_roost` fail because an iOS security-scoped bookmark couldn't be
 * resumed (stale after an app update / iCloud change / revoked grant)? The
 * backend returns the literal token `SCOPE_EXPIRED` in that case so the UI
 * can prompt a re-pick instead of surfacing a raw permission error. Keep
 * the token in sync with `SCOPE_EXPIRED` in `commands/roost.rs`.
 */
export function isScopeExpiredError(err: unknown): boolean {
  const msg =
    err instanceof Error ? err.message : typeof err === "string" ? err : String(err);
  return msg.includes("SCOPE_EXPIRED");
}

function isValidRoostEntry(v: unknown): v is RoostEntry {
  return (
    typeof v === "object" &&
    v !== null &&
    typeof (v as RoostEntry).id === "string" &&
    typeof (v as RoostEntry).path === "string" &&
    typeof (v as RoostEntry).displayName === "string" &&
    ((v as RoostEntry).type === "native" ||
      (v as RoostEntry).type === "saf" ||
      (v as RoostEntry).type === "ios-scoped") &&
    typeof (v as RoostEntry).lastOpened === "number"
  );
}

// Parse + validate a raw JSON registry blob (from localStorage or the on-disk
// manifest). Returns [] on any malformed input.
function parseRegistry(raw: string | null): RoostEntry[] {
  if (!raw) return [];
  try {
    const parsed = JSON.parse(raw) as unknown;
    return Array.isArray(parsed) ? parsed.filter(isValidRoostEntry) : [];
  } catch {
    return [];
  }
}

/**
 * NEW-M2: union the live registry with the manifest, keyed by path. The live
 * (localStorage-backed) entry wins on a path collision — it's the freshest —
 * and any manifest-only roosts are appended. Pure so it's unit-testable.
 *
 * Why this replaces the old "only hydrate when the live list is empty" gate:
 * `restoreLastRoost` can `registerOpened` a single roost while
 * `hydrateFromManifest`'s async manifest read is in flight. The old code then
 * saw a non-empty live list and dropped the manifest ENTIRELY — losing every
 * OTHER roost the manifest held (the localStorage-wipe recovery MR-B1 was meant
 * to provide). Merging by path recovers the rest without clobbering the live one.
 */
export function mergeRegistries(
  current: RoostEntry[],
  recovered: RoostEntry[],
): RoostEntry[] {
  const byPath = new Map<string, RoostEntry>();
  for (const e of current) byPath.set(e.path, e); // live wins
  for (const e of recovered) if (!byPath.has(e.path)) byPath.set(e.path, e);
  return Array.from(byPath.values());
}

function loadFromStorage(): RoostEntry[] {
  return parseRegistry(lsGet(STORAGE_KEY));
}

function generateId(): string {
  return `v_${Date.now().toString(36)}_${Math.random().toString(36).slice(2, 8)}`;
}

function inferType(path: string): RoostType {
  return path.startsWith("content://") ? "saf" : "native";
}

// MR-B1: best-effort durable mirror of the registry to
// `app_data_dir/roosts.json`. localStorage stays the live cache; this manifest
// is the recovery source if the webview store is ever cleared/corrupted. Never
// throws — a failed mirror just falls back to localStorage-only (prior
// behavior). Fire-and-forget so registry writes stay synchronous.
function mirrorToManifest(entries: RoostEntry[]) {
  void commands
    .writeRoostManifest(JSON.stringify(entries))
    .then((r) => {
      if (r.status === "error") {
        log.error("roosts", "roost manifest mirror failed", r.error);
      }
    })
    .catch((e) => log.error("roosts", "roost manifest mirror threw", e));
}

function createRoostRegistry() {
  const [roosts, setRoosts] = createSignal<RoostEntry[]>(loadFromStorage());
  const [activeId, setActiveId] = createSignal<string | null>(
    lsGet(ACTIVE_KEY),
  );

  const activeRoost = createMemo(() => {
    const id = activeId();
    if (!id) return null;
    return roosts().find((v) => v.id === id) ?? null;
  });

  function persist(next: RoostEntry[]) {
    setRoosts(next);
    lsSet(STORAGE_KEY, JSON.stringify(next));
    mirrorToManifest(next);
  }

  // MR-B1 / NEW-M2: reconcile the roost list with the on-disk manifest. Recovers
  // roosts the live localStorage registry is missing — the full list after a
  // cleared/corrupted store, OR the roosts that a `registerOpened` race dropped
  // by racing ahead of this async manifest read. We MERGE by path (live entries
  // win) instead of the old "bail if the live list is non-empty" gate, which
  // silently discarded every manifest roost the moment `restoreLastRoost`
  // registered one. localStorage stays the source of truth for entries it has;
  // this only fills gaps, and re-seeds localStorage + the manifest via persist().
  async function hydrateFromManifest() {
    try {
      const res = await commands.readRoostManifest();
      if (res.status !== "ok" || !res.data) return;
      const recovered = parseRegistry(res.data);
      if (recovered.length === 0) return;
      const current = roosts();
      const currentPaths = new Set(current.map((v) => v.path));
      const added = recovered.filter((v) => !currentPaths.has(v.path));
      if (added.length > 0) {
        persist(mergeRegistries(current, recovered));
        log.info("roosts", `merged ${added.length} roost(s) from manifest`);
      }
    } catch (e) {
      log.error("roosts", "roost manifest hydrate failed", e);
    }
  }

  function setActive(id: string | null) {
    setActiveId(id);
    if (id) lsSet(ACTIVE_KEY, id);
    else lsRemove(ACTIVE_KEY);
  }

  // Register a newly-opened roost. If the path already exists in the
  // registry, updates the displayName + lastOpened and sets it active.
  // Returns the entry so callers can read the (possibly new) id.
  //
  // `extras` carries fields that aren't always known at first open —
  // primarily `bookmark` for iOS scoped roosts and `type` overrides for
  // platform-specific picker results. When supplied, they overwrite the
  // existing entry's value.
  function registerOpened(
    path: string,
    displayName: string,
    extras?: { bookmark?: string; type?: RoostType },
  ): RoostEntry {
    const now = Date.now();
    const list = roosts();
    const existing = list.find((v) => v.path === path);
    if (existing) {
      const updated: RoostEntry = {
        ...existing,
        displayName: displayName || existing.displayName,
        lastOpened: now,
        bookmark: extras?.bookmark ?? existing.bookmark,
        type: extras?.type ?? existing.type,
      };
      persist(list.map((v) => (v.id === existing.id ? updated : v)));
      setActive(updated.id);
      return updated;
    }
    const entry: RoostEntry = {
      id: generateId(),
      path,
      displayName: displayName || deriveName(path),
      type: extras?.type ?? inferType(path),
      lastOpened: now,
      bookmark: extras?.bookmark,
    };
    persist([...list, entry]);
    setActive(entry.id);
    return entry;
  }

  // Update the bookmark for an existing roost entry (iOS only). Used
  // when the picker plugin re-mints a stale bookmark on resume; the
  // refreshed blob must be persisted or future cold starts will keep
  // hitting the stale one.
  function updateBookmark(path: string, bookmark: string) {
    const list = roosts();
    const next = list.map((v) =>
      v.path === path ? { ...v, bookmark } : v,
    );
    persist(next);
  }

  // The user-picked folder URL can shift (Files.app rename/move) and
  // the iOS bookmark resolves to the new path. Carry that through to
  // the registry so the dropdown shows the right entry going forward.
  function updatePath(oldPath: string, newPath: string) {
    if (oldPath === newPath) return;
    const list = roosts();
    const next = list.map((v) =>
      v.path === oldPath ? { ...v, path: newPath } : v,
    );
    persist(next);
  }

  function remove(id: string) {
    const next = roosts().filter((v) => v.id !== id);
    persist(next);
    if (activeId() === id) setActive(null);
  }

  function rename(id: string, newName: string) {
    const trimmed = newName.trim();
    if (!trimmed) return;
    persist(
      roosts().map((v) =>
        v.id === id ? { ...v, displayName: trimmed } : v,
      ),
    );
  }

  // Kick off manifest recovery once, at registry creation. No-op unless
  // localStorage was empty, so a normal launch never touches the backend.
  void hydrateFromManifest();

  return {
    roosts,
    activeId,
    activeRoost,
    registerOpened,
    updateBookmark,
    updatePath,
    setActive,
    remove,
    rename,
    hydrateFromManifest,
  };
}

// Derive a sensible display name for a path when the picker didn't give one.
// Mirrors the Kotlin SAF fallback: decode URI, strip volume prefix, leaf.
export function deriveName(path: string): string {
  if (!path) return "Roost";
  if (path.startsWith("content://")) {
    try {
      const decoded = decodeURIComponent(path);
      const afterTree = decoded.split("/tree/").pop() ?? decoded;
      const afterVolume = afterTree.includes(":")
        ? afterTree.substring(afterTree.indexOf(":") + 1)
        : afterTree;
      const leaf = afterVolume.replace(/\/+$/, "").split("/").pop() ?? "";
      if (leaf) return leaf;
    } catch {
      // fall through
    }
  }
  const parts = path.split(/[/\\]/).filter(Boolean);
  return parts[parts.length - 1] ?? path;
}

export const roostRegistry = createRoot(createRoostRegistry);
