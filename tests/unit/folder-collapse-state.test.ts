import { afterEach, beforeEach, describe, expect, it } from "vitest";
import {
  collapseStorageKey,
  loadCollapsedPaths,
  saveCollapsedPaths,
} from "../../src/lib/folder-collapse-state";

const ROOST = "/home/user/MyRoost";

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
  Object.setPrototypeOf(ls, Storage.prototype);
}

beforeEach(() => {
  installMemoryLocalStorage();
  localStorage.clear();
});
afterEach(() => localStorage.clear());

describe("collapse-state persistence", () => {
  it("returns an empty set for an unseen roost (all expanded by default)", () => {
    expect(loadCollapsedPaths(ROOST).size).toBe(0);
  });

  it("round-trips a saved set", () => {
    const set = new Set(["/v/A", "/v/A/B", "/v/C"]);
    saveCollapsedPaths(ROOST, set);
    expect(loadCollapsedPaths(ROOST)).toEqual(set);
  });

  it("keys per roost — one roost's state never leaks into another", () => {
    saveCollapsedPaths("/roost/one", new Set(["/roost/one/Folder"]));
    saveCollapsedPaths("/roost/two", new Set());
    expect(loadCollapsedPaths("/roost/one").size).toBe(1);
    expect(loadCollapsedPaths("/roost/two").size).toBe(0);
    expect(collapseStorageKey("/roost/one")).not.toBe(
      collapseStorageKey("/roost/two"),
    );
  });

  it("persisting an empty set clears the remembered collapse", () => {
    saveCollapsedPaths(ROOST, new Set(["/v/A"]));
    saveCollapsedPaths(ROOST, new Set());
    expect(loadCollapsedPaths(ROOST).size).toBe(0);
  });

  it("is a no-op when there is no open roost", () => {
    saveCollapsedPaths(null, new Set(["/v/A"]));
    expect(localStorage.length).toBe(0);
  });

  it("degrades to empty on corrupt stored JSON", () => {
    localStorage.setItem(collapseStorageKey(ROOST), "{not valid json");
    expect(loadCollapsedPaths(ROOST).size).toBe(0);
  });

  it("ignores non-string entries in a stored array", () => {
    localStorage.setItem(
      collapseStorageKey(ROOST),
      JSON.stringify(["/v/A", 42, null, "/v/B"]),
    );
    expect(loadCollapsedPaths(ROOST)).toEqual(new Set(["/v/A", "/v/B"]));
  });
});
