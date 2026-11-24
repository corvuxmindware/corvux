/**
 * F10 frontend half: when the user DECLINES the backend's native "remove
 * roost" confirmation, the roost list must not lie.
 *
 * `persist()` updates the list immediately and mirrors it to the on-disk
 * manifest fire-and-forget. If the manifest write that drops a roost is
 * refused ("Roost removal was cancelled."), the manifest still lists the roost
 * — and the next launch's recovery would bring it back. So the list puts the
 * roost back at once and says why (info toast, not an error).
 */

import { describe, it, expect, beforeEach, vi } from "vitest";
import { mockInvoke } from "./setup";

const toast = vi.hoisted(() => ({ show: vi.fn(), showError: vi.fn() }));
vi.mock("../../src/components/Toast/Toast", () => ({ toastStore: toast }));

import { roostRegistry } from "../../src/stores/roosts";

const flush = () => new Promise((r) => setTimeout(r, 0));

function registerTwo() {
  const a = roostRegistry.registerOpened("/r/a", "Alpha");
  const b = roostRegistry.registerOpened("/r/b", "Beta");
  return { a, b };
}

describe("roost registry — declined removal", () => {
  beforeEach(() => {
    toast.show.mockReset();
    toast.showError.mockReset();
    // Start from an empty registry (singleton shared across tests in this file).
    for (const r of roostRegistry.roosts()) roostRegistry.remove(r.id);
    vi.spyOn(console, "error").mockImplementation(() => {});
    vi.spyOn(console, "log").mockImplementation(() => {});
  });

  it("puts the roost back and shows an info toast when the native dialog is declined", async () => {
    mockInvoke.set("write_roost_manifest", () => null);
    const { a } = registerTwo();
    await flush();

    mockInvoke.set("write_roost_manifest", () => {
      throw new Error("Roost removal was cancelled.");
    });
    roostRegistry.remove(a.id);
    // Optimistic: gone right away.
    expect(roostRegistry.roosts().map((r) => r.path)).toEqual(["/r/b"]);

    await flush();
    expect(roostRegistry.roosts().map((r) => r.path).sort()).toEqual(["/r/a", "/r/b"]);
    expect(localStorage.getItem("corvux:vaults")).toContain("/r/a");
    expect(toast.show).toHaveBeenCalledWith(expect.stringContaining("Alpha"), "info");
    expect(toast.showError).not.toHaveBeenCalled();
  });

  it("keeps the roost removed when the manifest write succeeds", async () => {
    mockInvoke.set("write_roost_manifest", () => null);
    const { a } = registerTwo();
    await flush();

    roostRegistry.remove(a.id);
    await flush();

    expect(roostRegistry.roosts().map((r) => r.path)).toEqual(["/r/b"]);
    expect(toast.show).not.toHaveBeenCalled();
  });

  it("does not resurrect a roost for an unrelated manifest failure", async () => {
    mockInvoke.set("write_roost_manifest", () => null);
    const { a } = registerTwo();
    await flush();

    mockInvoke.set("write_roost_manifest", () => {
      throw new Error("disk full");
    });
    roostRegistry.remove(a.id);
    await flush();

    expect(roostRegistry.roosts().map((r) => r.path)).toEqual(["/r/b"]);
    expect(toast.show).not.toHaveBeenCalled();
  });

  it("a declined write that dropped nothing restores nothing", async () => {
    mockInvoke.set("write_roost_manifest", () => null);
    registerTwo();
    await flush();

    mockInvoke.set("write_roost_manifest", () => {
      throw new Error("Roost removal was cancelled.");
    });
    // Rename drops no roost; a stray decline must not duplicate or toast.
    const first = roostRegistry.roosts()[0];
    roostRegistry.rename(first.id, "Renamed");
    await flush();

    expect(roostRegistry.roosts()).toHaveLength(2);
    expect(toast.show).not.toHaveBeenCalled();
  });
});
