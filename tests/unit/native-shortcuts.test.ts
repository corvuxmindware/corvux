import { afterEach, describe, expect, it } from "vitest";
import {
  installReloadShortcutGuard,
  isReloadShortcut,
} from "../../src/lib/native-shortcuts";

function key(init: KeyboardEventInit): KeyboardEvent {
  return new KeyboardEvent("keydown", { bubbles: true, cancelable: true, ...init });
}

describe("isReloadShortcut", () => {
  it("matches F5 and its hard-reload variants", () => {
    expect(isReloadShortcut(key({ key: "F5" }))).toBe(true);
    expect(isReloadShortcut(key({ key: "F5", ctrlKey: true }))).toBe(true);
    expect(isReloadShortcut(key({ key: "F5", shiftKey: true }))).toBe(true);
  });

  it("matches Ctrl+R and Cmd+R", () => {
    expect(isReloadShortcut(key({ key: "r", ctrlKey: true }))).toBe(true);
    expect(isReloadShortcut(key({ key: "r", metaKey: true }))).toBe(true);
  });

  it("matches Ctrl+Shift+R (hard reload)", () => {
    expect(isReloadShortcut(key({ key: "R", ctrlKey: true, shiftKey: true }))).toBe(true);
  });

  it("ignores plain R with no modifier", () => {
    expect(isReloadShortcut(key({ key: "r" }))).toBe(false);
  });

  it("ignores unrelated shortcuts", () => {
    expect(isReloadShortcut(key({ key: "s", ctrlKey: true }))).toBe(false);
    expect(isReloadShortcut(key({ key: "p", ctrlKey: true }))).toBe(false);
  });
});

describe("installReloadShortcutGuard", () => {
  let dispose: (() => void) | null = null;
  afterEach(() => {
    dispose?.();
    dispose = null;
  });

  it("prevents the default for reload shortcuts", () => {
    dispose = installReloadShortcutGuard(document);
    const ev = key({ key: "r", ctrlKey: true });
    document.dispatchEvent(ev);
    expect(ev.defaultPrevented).toBe(true);
  });

  it("leaves non-reload shortcuts alone", () => {
    dispose = installReloadShortcutGuard(document);
    const ev = key({ key: "s", ctrlKey: true });
    document.dispatchEvent(ev);
    expect(ev.defaultPrevented).toBe(false);
  });

  it("does not stop propagation, so app handlers still run", () => {
    dispose = installReloadShortcutGuard(document);
    let appHandlerRan = false;
    const appHandler = () => {
      appHandlerRan = true;
    };
    document.addEventListener("keydown", appHandler);
    document.dispatchEvent(key({ key: "R", ctrlKey: true, shiftKey: true }));
    document.removeEventListener("keydown", appHandler);
    expect(appHandlerRan).toBe(true);
  });

  it("stops guarding after the disposer runs", () => {
    const off = installReloadShortcutGuard(document);
    off();
    const ev = key({ key: "r", ctrlKey: true });
    document.dispatchEvent(ev);
    expect(ev.defaultPrevented).toBe(false);
  });
});
