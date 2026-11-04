/**
 * MSIX / Microsoft Store updater gate.
 *
 * A Store-packaged install must never self-update from the website
 * channel: Store policy says the Store owns updates, and a website NSIS
 * update landing beside the MSIX recreates the dual-install "two
 * windows" incident (2026-07-09 — two app identities, single-instance
 * can't dedupe). The backend exposes `is_store_packaged`; the updater
 * store must gate every entry point on it:
 *
 *   - background check → silent no-op ("up-to-date", no banner)
 *   - manual check     → honest message pointing at the Store
 *   - installAndRelaunch → hard no-op
 *   - classic (non-packaged) installs → unchanged website-check path
 *
 * The tauri-updater plugin mock records calls so we can prove the gate
 * short-circuits BEFORE any network-facing plugin code runs.
 */

import { describe, it, expect, beforeEach, vi } from "vitest";

// Mutable knobs the hoisted module mocks read at call time.
const knobs = vi.hoisted(() => ({
  packaged: false,
  pluginCheckCalls: 0,
}));

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
        get: (_t, prop) => {
          if (prop === "isStorePackaged") {
            return async () => ({ status: "ok", data: knobs.packaged });
          }
          return async () => ({ status: "ok", data: undefined });
        },
      },
    ),
  };
});

vi.mock("../../src/lib/platform", () => ({
  isMobile: () => false,
}));

// The plugin module is dynamically imported by the store; count check()
// calls and report "no update" so the classic path terminates cleanly.
vi.mock("@tauri-apps/plugin-updater", () => ({
  check: async () => {
    knobs.pluginCheckCalls += 1;
    return null;
  },
}));

async function freshUpdaterStore() {
  vi.resetModules();
  const mod = await import("../../src/stores/updater");
  return mod.updaterStore;
}

beforeEach(() => {
  knobs.packaged = false;
  knobs.pluginCheckCalls = 0;
  localStorage.clear();
});

describe("updater store MSIX gate", () => {
  it("packaged: manual check explains the Store and never touches the plugin", async () => {
    knobs.packaged = true;
    const store = await freshUpdaterStore();
    const status = await store.check({ manual: true });
    expect(status.kind).toBe("error");
    if (status.kind === "error") {
      expect(status.message).toContain("Microsoft Store");
    }
    expect(knobs.pluginCheckCalls).toBe(0);
  });

  it("packaged: background check is a silent up-to-date, no banner state", async () => {
    knobs.packaged = true;
    const store = await freshUpdaterStore();
    const status = await store.check();
    expect(status.kind).toBe("up-to-date");
    expect(store.available()).toBeNull();
    expect(knobs.pluginCheckCalls).toBe(0);
  });

  it("packaged: installAndRelaunch is a hard no-op", async () => {
    knobs.packaged = true;
    const store = await freshUpdaterStore();
    await store.installAndRelaunch();
    expect(knobs.pluginCheckCalls).toBe(0);
  });

  it("classic install: check still runs the website-channel plugin path", async () => {
    knobs.packaged = false;
    const store = await freshUpdaterStore();
    const status = await store.check({ manual: true });
    expect(status.kind).toBe("up-to-date"); // plugin mock reports no update
    expect(knobs.pluginCheckCalls).toBe(1);
  });
});
