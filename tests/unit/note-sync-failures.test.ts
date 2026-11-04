/**
 * H-6 — per-note sync-failure tracking in the sync store.
 *
 * The backend emits `note_sync_failed { path, reason }` when a peer's change
 * couldn't be applied to a note locally (previously swallowed with no user
 * signal). The store records it, keyed by roost-relative path, so the sidebar
 * can mark the note; a later successful `note_written` (or `note_deleted`) for
 * that path clears the marker.
 *
 * Like chat-listeners.test.ts, these bypass the global isTauri=false setup with
 * file-local mocks so the listener code path actually runs, and capture the
 * `sync://event` handler to drive synthetic payloads through it.
 */

import { describe, it, expect, beforeAll, vi } from "vitest";
import type { SyncEventPayload } from "../../src/lib/bindings";

// Capture each listen() callback by event name so the test can invoke the
// `sync://event` handler directly.
const handlers = new Map<string, (e: { payload: unknown }) => void>();

vi.mock("../../src/lib/invoke", async (importOriginal) => {
  const original = await importOriginal<typeof import("../../src/lib/invoke")>();
  return {
    ...original,
    isTauri: true,
    unwrap: async <T, E>(
      p: Promise<{ status: "ok"; data: T } | { status: "error"; error: E }>,
    ): Promise<T> => {
      const r = await p;
      if (r.status === "error") throw r.error;
      return r.data;
    },
    commands: new Proxy(
      {},
      {
        get: (_t, name: string) => async () => {
          // The reconnect sweep kicked off by start() reads syncStatus and
          // dereferences .running — hand it a valid, idle shape so the loop
          // short-circuits instead of throwing on `undefined.running`.
          if (name === "syncStatus") {
            return {
              status: "ok",
              data: { running: false, connected_peers: [], known_peers: [] },
            };
          }
          return { status: "ok", data: undefined };
        },
      },
    ),
  };
});

vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn(async (event: string, cb: (e: { payload: unknown }) => void) => {
    handlers.set(event, cb);
    return () => handlers.delete(event);
  }),
}));

vi.mock("../../src/lib/platform", () => ({
  isMobile: () => false,
  isAndroid: () => false,
  isIOS: () => false,
}));

// Keep the roost dependency inert — the store only reads roostPath here.
vi.mock("../../src/stores/roost", () => ({
  roostStore: { roostPath: () => "/roost", files: () => [] },
}));

// happy-dom doesn't install localStorage by default in this vitest config, and
// the store's start() → hydrateAddrsFromStorage() iterates it. A minimal
// in-memory stub is enough (we don't exercise the address cache here).
if (typeof globalThis.localStorage === "undefined") {
  const store = new Map<string, string>();
  Object.defineProperty(globalThis, "localStorage", {
    configurable: true,
    value: {
      get length() {
        return store.size;
      },
      key: (i: number) => Array.from(store.keys())[i] ?? null,
      getItem: (k: string) => store.get(k) ?? null,
      setItem: (k: string, v: string) => void store.set(k, String(v)),
      removeItem: (k: string) => void store.delete(k),
      clear: () => store.clear(),
    },
  });
}

let syncStatusStore: typeof import("../../src/stores/sync").syncStatusStore;

function emit(payload: SyncEventPayload) {
  const h = handlers.get("sync://event");
  if (!h) throw new Error("sync://event handler was never registered");
  h({ payload });
}

// The store is a module-level singleton and start() registers its listener
// exactly once, so import + start once for the whole file (clearing handlers
// between tests would strip the only registration). Each test uses distinct
// paths so there's no cross-test state bleed in noteSyncFailures.
beforeAll(async () => {
  ({ syncStatusStore } = await import("../../src/stores/sync"));
  await syncStatusStore.start();
});

describe("H-6 — note sync-failure tracking", () => {
  it("records a note_sync_failed event keyed by path", () => {
    emit({ kind: "note_sync_failed", path: "folder/a.md", reason: "decode error" });
    expect(syncStatusStore.noteSyncFailures["folder/a.md"]).toBeDefined();
    expect(syncStatusStore.noteSyncFailures["folder/a.md"].reason).toBe("decode error");
  });

  it("clears the failure when the same note later writes successfully", () => {
    emit({ kind: "note_sync_failed", path: "b.md", reason: "merge conflict" });
    expect(syncStatusStore.noteSyncFailures["b.md"]).toBeDefined();
    emit({ kind: "note_written", path: "b.md" });
    expect(syncStatusStore.noteSyncFailures["b.md"]).toBeUndefined();
  });

  it("clears the failure when the note is deleted", () => {
    emit({ kind: "note_sync_failed", path: "c.md", reason: "write error" });
    emit({ kind: "note_deleted", path: "c.md" });
    expect(syncStatusStore.noteSyncFailures["c.md"]).toBeUndefined();
  });

  it("keeps failures for other notes when one recovers", () => {
    emit({ kind: "note_sync_failed", path: "x.md", reason: "e1" });
    emit({ kind: "note_sync_failed", path: "y.md", reason: "e2" });
    emit({ kind: "note_written", path: "x.md" });
    expect(syncStatusStore.noteSyncFailures["x.md"]).toBeUndefined();
    expect(syncStatusStore.noteSyncFailures["y.md"]).toBeDefined();
  });
});
