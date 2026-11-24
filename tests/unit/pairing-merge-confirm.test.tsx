// @vitest-environment happy-dom
/**
 * Pairing merge confirmation (SYN-I11 follow-up, 2026-11-07).
 *
 * The sync handshake only checks that both devices' roost FOLDER NAMES match;
 * the roost id is not checked on a first pair. So pairing a phone "Notes" roost
 * with a different laptop "Notes" roost union-merges both, permanently. These
 * tests lock:
 *  1. the pure rules — the folder name the UI shows is the one the Rust
 *     handshake compares, and a merge prompt is needed only when this device
 *     already has notes;
 *  2. the panel — a device WITH notes must confirm before we dial, Back
 *     never dials, and an empty roost (a plain join) dials straight away.
 */
import { describe, it, expect, afterEach, beforeEach, vi } from "vitest";
import { render } from "solid-js/web";

const api = vi.hoisted(() => ({
  syncConnectIroh: vi.fn(),
  syncListenIroh: vi.fn(),
}));
const roost = vi.hoisted(() => ({
  path: "/Users/me/Notes" as string | null,
  notes: 0,
}));

vi.mock("../../src/lib/invoke", () => ({
  commands: api,
  unwrap: async <T,>(
    p: Promise<{ status: "ok"; data: T } | { status: "error"; error: unknown }>,
  ) => {
    const r = await p;
    if (r.status === "error") throw r.error;
    return r.data;
  },
}));
vi.mock("../../src/stores/roost", () => ({
  roostStore: {
    roostPath: () => roost.path,
    noteCount: () => roost.notes,
  },
}));
vi.mock("../../src/stores/sync", () => ({
  syncStatusStore: { onEvent: () => () => {} },
}));
vi.mock("../../src/components/Settings/SyncSection/QrScanner", () => ({
  default: () => null,
}));

import IrohPairingPanel from "../../src/components/Settings/SyncSection/IrohPairingPanel";
import {
  mergeConfirmationFor,
  roostFolderName,
} from "../../src/components/Settings/SyncSection/pairingRoost";

describe("roostFolderName (must match Rust roost_folder_name)", () => {
  it("takes the last path segment on desktop paths", () => {
    expect(roostFolderName("/Users/me/Notes")).toBe("Notes");
    expect(roostFolderName("/Users/me/Notes/")).toBe("Notes");
    expect(roostFolderName("C:\\Users\\me\\Brain-Dump")).toBe("Brain-Dump");
  });
  it("URL-decodes SAF content:// URIs before taking the last segment", () => {
    expect(
      roostFolderName(
        "content://com.android.externalstorage.documents/tree/primary%3ADocuments%2FNotes",
      ),
    ).toBe("Notes");
  });
  it("is empty when no roost is open", () => {
    expect(roostFolderName(null)).toBe("");
    expect(roostFolderName(undefined)).toBe("");
  });
});

describe("mergeConfirmationFor", () => {
  it("asks only when this device already has notes", () => {
    expect(mergeConfirmationFor("/a/Notes", 0)).toBeNull();
    expect(mergeConfirmationFor("/a/Notes", 312)).toEqual({
      roost: "Notes",
      notes: 312,
    });
  });
});

let dispose: (() => void) | null = null;
let container: HTMLDivElement | null = null;

beforeEach(() => {
  api.syncConnectIroh.mockReset();
  api.syncConnectIroh.mockResolvedValue({ status: "ok", data: "device-id" });
  api.syncListenIroh.mockReset();
  roost.path = "/Users/me/Notes";
  roost.notes = 0;
});

afterEach(() => {
  dispose?.();
  dispose = null;
  container?.remove();
  container = null;
});

const flush = () => new Promise((r) => setTimeout(r, 0));

function buttonByText(el: HTMLElement, text: string): HTMLButtonElement {
  const btn = Array.from(el.querySelectorAll("button")).find((b) =>
    (b.textContent ?? "").includes(text),
  );
  if (!btn) throw new Error(`no button "${text}" in: ${el.textContent}`);
  return btn as HTMLButtonElement;
}

function type(input: HTMLInputElement | HTMLTextAreaElement, value: string) {
  input.value = value;
  input.dispatchEvent(new Event("input", { bubbles: true }));
}

/** Mount, go to "Enter pairing code", fill the form, press Connect. */
async function enterCodeAndConnect(): Promise<HTMLDivElement> {
  container = document.createElement("div");
  document.body.appendChild(container);
  dispose = render(() => <IrohPairingPanel />, container);
  buttonByText(container, "Enter pairing code").click();
  await flush();
  type(container.querySelector("textarea")!, "corvux1:abc");
  type(container.querySelector("input")!, "123456");
  await flush();
  buttonByText(container, "Connect").click();
  await flush();
  return container;
}

describe("IrohPairingPanel merge confirmation", () => {
  it("asks before pairing when this roost already has notes", async () => {
    roost.notes = 312;
    const el = await enterCodeAndConnect();

    const confirm = el.querySelector('[data-testid="merge-confirm"]');
    expect(confirm, "merge confirmation shown").toBeTruthy();
    expect(confirm!.textContent).toContain('"Notes"');
    expect(confirm!.textContent).toContain("312 notes");
    expect(confirm!.textContent).toContain("can't be undone");
    expect(api.syncConnectIroh, "must not dial before the user agrees").not
      .toHaveBeenCalled();

    buttonByText(el, "Merge and pair").click();
    await flush();
    expect(api.syncConnectIroh).toHaveBeenCalledTimes(1);
    expect(api.syncConnectIroh).toHaveBeenCalledWith("corvux1:abc", "123456");
    expect(el.textContent).toContain("Paired with");
  });

  it("Back returns to the form without dialing and keeps what was typed", async () => {
    roost.notes = 5;
    const el = await enterCodeAndConnect();
    buttonByText(el, "Back").click();
    await flush();

    expect(el.querySelector('[data-testid="merge-confirm"]')).toBeNull();
    expect(api.syncConnectIroh).not.toHaveBeenCalled();
    expect(el.querySelector("textarea")!.value).toBe("corvux1:abc");
    expect(el.querySelector("input")!.value).toBe("123456");
  });

  it("an empty roost joins straight away (no prompt)", async () => {
    roost.notes = 0;
    const el = await enterCodeAndConnect();

    expect(el.querySelector('[data-testid="merge-confirm"]')).toBeNull();
    expect(api.syncConnectIroh).toHaveBeenCalledTimes(1);
  });

  it("shows a pairing rejection as the backend's plain sentence", async () => {
    roost.notes = 0;
    api.syncConnectIroh.mockResolvedValue({
      status: "error",
      error:
        'Roost names don\'t match — the other device has "Work" open, this device has "Notes". Open a roost with the same folder name on both devices.',
    });
    const el = await enterCodeAndConnect();
    await flush();
    expect(el.textContent).toContain("Roost names don't match");
    expect(el.textContent).not.toContain("RoostMismatch");
  });
});
