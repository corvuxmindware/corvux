/**
 * Browser-dev FSAccess + IDB persistence layer.
 *
 * In Tauri builds this module is unused — the real backend handles file
 * I/O. In browser dev (pnpm dev → localhost:1420) it provides:
 *   - A real-folder bridge so file ops touch the user's picked directory
 *     via the FileSystem Access API.
 *   - IndexedDB-backed handle persistence so a page reload silently
 *     re-binds the folder instead of re-prompting the user.
 *   - Path resolution + tree-walk helpers used by the mock IPC switch.
 *
 * Owns the active `mockRoostPath`, the bound FSAccess handle, and the
 * IDB persistence layer. The mock-IPC switch in `invoke.ts` reads these
 * via the live exports + accessor functions below.
 */

import type { FileEntry } from "./bindings";
import { log } from "./log";

export const MOCK_ROOST_PATH = "/mock-roost";

// Active mock roost path — updated when the user picks a folder in browser dev.
let _mockRoostPath = MOCK_ROOST_PATH;

// Real-FS handle when the user picked a folder via showDirectoryPicker. Routes
// open_roost/read/write through the actual folder so they see their own files.
let _browserRoostHandle: FileSystemDirectoryHandle | null = null;
let _browserRoostRoot: string | null = null;

/** Read accessor for the current mock roost path (live). */
export function getMockRoostPath(): string {
  return _mockRoostPath;
}

/** Read accessor for the bound FSAccess directory handle, if any. */
export function getBrowserRoostHandle(): FileSystemDirectoryHandle | null {
  return _browserRoostHandle;
}

/** Read accessor for the root path of the bound FSAccess roost, if any. */
export function getBrowserRoostRoot(): string | null {
  return _browserRoostRoot;
}

/** Set the active roost path for the in-memory mock (browser dev only). */
export function setMockRoostPath(path: string) {
  _mockRoostPath = path;
}

/** Bind a picked FileSystemDirectoryHandle so file ops touch the real folder.
 *  Also persists the handle to IndexedDB so a page reload can restore it
 *  silently (no re-pick) as long as the browser still grants permission. */
export function setBrowserRoost(handle: FileSystemDirectoryHandle, rootPath: string) {
  _browserRoostHandle = handle;
  _browserRoostRoot = rootPath;
  _mockRoostPath = rootPath;
  void idbSaveHandle(rootPath, handle).catch((e) =>
    log.warn("browser-roost", "IDB save failed", e),
  );
}

/** Whether a live FSAccess handle is currently bound to the given path.
 *  Browser dev mode: handles don't survive page reload, so the registry path
 *  may still exist while the in-memory handle is gone — callers use this to
 *  decide whether they need to re-pick the folder. */
export function isBrowserRoostBound(path: string): boolean {
  return _browserRoostHandle !== null && _browserRoostRoot === path;
}

// ── IndexedDB-backed handle persistence ─────────────────────────
// FileSystemDirectoryHandle is structured-cloneable and survives reload when
// stored in IDB. The browser may still gate access on `requestPermission()`,
// but if the permission is already "granted" we can rebind without prompting
// the user — turning every refresh from "re-pick the folder" into a no-op.

const IDB_NAME = "corvux-fsaccess";
const IDB_STORE = "roosts";
const IDB_KEY = "lastRoost";

function idbOpen(): Promise<IDBDatabase> {
  return new Promise((resolve, reject) => {
    const req = indexedDB.open(IDB_NAME, 1);
    req.onupgradeneeded = () => req.result.createObjectStore(IDB_STORE);
    req.onsuccess = () => resolve(req.result);
    req.onerror = () => reject(req.error);
  });
}

async function idbSaveHandle(
  path: string,
  handle: FileSystemDirectoryHandle,
): Promise<void> {
  const db = await idbOpen();
  await new Promise<void>((resolve, reject) => {
    const tx = db.transaction(IDB_STORE, "readwrite");
    tx.objectStore(IDB_STORE).put({ path, handle }, IDB_KEY);
    tx.oncomplete = () => resolve();
    tx.onerror = () => reject(tx.error);
  });
  db.close();
}

async function idbLoadHandle(): Promise<{
  path: string;
  handle: FileSystemDirectoryHandle;
} | null> {
  try {
    const db = await idbOpen();
    const result = await new Promise<{
      path: string;
      handle: FileSystemDirectoryHandle;
    } | null>((resolve, reject) => {
      const tx = db.transaction(IDB_STORE, "readonly");
      const req = tx.objectStore(IDB_STORE).get(IDB_KEY);
      req.onsuccess = () =>
        resolve(
          (req.result as { path: string; handle: FileSystemDirectoryHandle } | undefined) ??
            null,
        );
      req.onerror = () => reject(req.error);
    });
    db.close();
    return result;
  } catch {
    return null;
  }
}

async function idbClearHandle(): Promise<void> {
  try {
    const db = await idbOpen();
    await new Promise<void>((resolve, reject) => {
      const tx = db.transaction(IDB_STORE, "readwrite");
      tx.objectStore(IDB_STORE).delete(IDB_KEY);
      tx.oncomplete = () => resolve();
      tx.onerror = () => reject(tx.error);
    });
    db.close();
  } catch {
    /* swallow */
  }
}

/** Try to silently rebind a previously-picked browser roost from IDB.
 *  Returns the bound path on success, or null if no handle was stored or the
 *  browser revoked permission. Caller falls back to onboarding on null.
 *
 *  Pass `isTauri = true` to no-op (real backend doesn't use FSAccess). */
export async function tryRestoreBrowserRoost(
  expectedPath?: string,
  isTauri = false,
): Promise<string | null> {
  if (isTauri) return null;
  const stored = await idbLoadHandle();
  if (!stored) return null;
  if (expectedPath && stored.path !== expectedPath) {
    // localStorage and IDB out of sync — clear stale entry, fall through.
    await idbClearHandle();
    return null;
  }
  try {
    type PermHandle = FileSystemDirectoryHandle & {
      queryPermission: (opts: { mode: string }) => Promise<string>;
    };
    const ph = stored.handle as PermHandle;
    const state = await ph.queryPermission({ mode: "readwrite" });
    if (state !== "granted") {
      // Permission needs a user gesture to re-prompt — caller should show
      // onboarding so the user can click "Pick Folder" to re-bind.
      return null;
    }
    setBrowserRoost(stored.handle, stored.path);
    return stored.path;
  } catch (e) {
    log.warn("browser-roost", "silent restore failed", e);
    return null;
  }
}

/** True when `p` is the root of the bound FSAccess roost or a path inside it. */
export function pathInBrowserRoost(p: string): boolean {
  return !!_browserRoostRoot && (p === _browserRoostRoot || p.startsWith(_browserRoostRoot + "/"));
}

/** Walk a roost-relative path down to its parent dir + final segment, so the
 *  caller can call getFileHandle/getDirectoryHandle on it. Returns null when
 *  no FSAccess handle is bound or the path is outside the roost root. */
export async function resolveBrowserPath(
  path: string,
  options: { create?: boolean } = {},
): Promise<{ parent: FileSystemDirectoryHandle; name: string } | null> {
  if (!_browserRoostHandle || !_browserRoostRoot) return null;
  if (!path.startsWith(_browserRoostRoot + "/")) return null;
  const relative = path.slice(_browserRoostRoot.length + 1);
  const parts = relative.split("/").filter((p) => p.length > 0);
  if (parts.length === 0) return null;
  const name = parts.pop()!;
  let dir: FileSystemDirectoryHandle = _browserRoostHandle;
  for (const part of parts) {
    dir = await dir.getDirectoryHandle(part, { create: options.create });
  }
  return { parent: dir, name };
}

/** Recursively read the bound FSAccess roost into a FileEntry tree.
 *  Returns null when no roost is bound. */
export async function walkBrowserRoost(): Promise<FileEntry[] | null> {
  if (!_browserRoostHandle || !_browserRoostRoot) return null;
  return walkBrowserRoostInternal(_browserRoostHandle, _browserRoostRoot);
}

async function walkBrowserRoostInternal(
  handle: FileSystemDirectoryHandle,
  basePath: string,
): Promise<FileEntry[]> {
  const entries: FileEntry[] = [];
  // FSAccess `entries()` is async-iterable but not yet in lib.dom for some TS configs.
  for await (const [name, child] of (
    handle as unknown as { entries(): AsyncIterable<[string, FileSystemHandle]> }
  ).entries()) {
    const path = `${basePath}/${name}`;
    if (child.kind === "directory") {
      entries.push({
        name,
        path,
        is_dir: true,
        children: await walkBrowserRoostInternal(child as FileSystemDirectoryHandle, path),
      });
    } else {
      entries.push({ name, path, is_dir: false, children: [] });
    }
  }
  entries.sort((a, b) => {
    if (a.is_dir !== b.is_dir) return a.is_dir ? -1 : 1;
    return a.name.localeCompare(b.name);
  });
  return entries;
}
