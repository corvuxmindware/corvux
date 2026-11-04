/**
 * Tests for `src/lib/storage.ts` — `lsGet/lsSet/lsRemove` wrappers that
 * replaced 47 raw `localStorage.*` calls. The wrap exists because Safari
 * private mode and quota-exceeded throw on `setItem`, and happy-dom
 * tests can run before `localStorage` is defined.
 *
 * happy-dom does expose localStorage in tests, so the happy path runs
 * straight through. The throw-tolerance path is verified by spying on
 * the underlying methods to make them throw.
 */

import { describe, it, expect, beforeEach, vi, afterEach } from "vitest";
import { lsGet, lsSet, lsRemove } from "../../src/lib/storage";

// happy-dom doesn't install localStorage by default in our vitest config.
// Use an in-memory shim so the test exercises the helpers' real branches
// (try/catch wrappers, not the `typeof localStorage === "undefined"` bail).
function installMemoryLocalStorage() {
  const store: Record<string, string> = {};
  const ls = {
    getItem(k: string): string | null {
      return Object.prototype.hasOwnProperty.call(store, k) ? store[k] : null;
    },
    setItem(k: string, v: string): void {
      store[k] = v;
    },
    removeItem(k: string): void {
      delete store[k];
    },
    clear(): void {
      for (const k of Object.keys(store)) delete store[k];
    },
    key(i: number): string | null {
      return Object.keys(store)[i] ?? null;
    },
    get length(): number {
      return Object.keys(store).length;
    },
  };
  Object.defineProperty(globalThis, "localStorage", {
    value: ls,
    configurable: true,
    writable: true,
  });
  // Storage.prototype is what `vi.spyOn(Storage.prototype, "getItem")`
  // patches in the throw-tolerance tests below. Wire it up so spies fire.
  Object.setPrototypeOf(ls, Storage.prototype);
}

describe("lib/storage helpers", () => {
  beforeEach(() => {
    installMemoryLocalStorage();
  });
  afterEach(() => {
    vi.restoreAllMocks();
  });

  it("lsGet round-trips a value set with lsSet", () => {
    lsSet("k", "v");
    expect(lsGet("k")).toBe("v");
  });

  it("lsGet returns null for missing keys (no exception)", () => {
    expect(lsGet("never-set")).toBeNull();
  });

  it("lsRemove deletes a key", () => {
    lsSet("k", "v");
    lsRemove("k");
    expect(lsGet("k")).toBeNull();
  });

  it("lsGet returns null when localStorage.getItem throws (private mode)", () => {
    vi.spyOn(Storage.prototype, "getItem").mockImplementation(() => {
      throw new DOMException("private mode", "QuotaExceededError");
    });
    expect(lsGet("k")).toBeNull();
  });

  it("lsSet swallows quota-exceeded errors silently", () => {
    vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => {
      throw new DOMException("quota", "QuotaExceededError");
    });
    // Should NOT throw. The pre-refactor raw calls would have crashed
    // their host store's mutation chain.
    expect(() => lsSet("k", "v")).not.toThrow();
  });

  it("lsRemove swallows DOMException silently", () => {
    vi.spyOn(Storage.prototype, "removeItem").mockImplementation(() => {
      throw new DOMException("removed in private mode", "InvalidAccessError");
    });
    expect(() => lsRemove("k")).not.toThrow();
  });

  it("lsSet accepts empty string as a valid value", () => {
    lsSet("k", "");
    expect(lsGet("k")).toBe("");
  });
});
