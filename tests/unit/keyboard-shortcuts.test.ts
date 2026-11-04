import { afterEach, describe, expect, it, vi } from "vitest";
import {
  registerKeyboardShortcuts,
  type ShortcutContext,
} from "../../src/lib/keyboard-shortcuts";

function key(init: KeyboardEventInit): KeyboardEvent {
  return new KeyboardEvent("keydown", { bubbles: true, cancelable: true, ...init });
}

/** Build a ShortcutContext whose UI hooks are all spies, with a mutable
 * `activePanel` so toggle behavior can be observed. */
function makeCtx(initialPanel = ""): {
  ctx: ShortcutContext;
  panel: () => string;
} {
  let panel = initialPanel;
  const ctx: ShortcutContext = {
    activePanel: () => panel,
    setActivePanel: vi.fn((p: string) => {
      panel = p;
    }),
    setShowCommandPalette: vi.fn(),
    setShowQuickSwitcher: vi.fn(),
    setShowChat: vi.fn(),
    showChat: () => false,
    setShowShortcuts: vi.fn(),
    showShortcuts: () => false,
    setShowPathFinder: vi.fn(),
    openDailyNote: vi.fn(),
  };
  return { ctx, panel: () => panel };
}

describe("Ctrl+T — Tasks panel toggle", () => {
  let dispose: (() => void) | null = null;
  afterEach(() => {
    dispose?.();
    dispose = null;
  });

  it("opens the Tasks panel when none is active", () => {
    const { ctx } = makeCtx("");
    dispose = registerKeyboardShortcuts(ctx);
    document.dispatchEvent(key({ key: "t", ctrlKey: true }));
    expect(ctx.setActivePanel).toHaveBeenCalledWith("tasks");
  });

  it("closes the Tasks panel when it is already active (toggle)", () => {
    const { ctx } = makeCtx("tasks");
    dispose = registerKeyboardShortcuts(ctx);
    document.dispatchEvent(key({ key: "t", ctrlKey: true }));
    expect(ctx.setActivePanel).toHaveBeenCalledWith("");
  });

  it("works with Cmd+T on macOS", () => {
    const { ctx } = makeCtx("");
    dispose = registerKeyboardShortcuts(ctx);
    document.dispatchEvent(key({ key: "t", metaKey: true }));
    expect(ctx.setActivePanel).toHaveBeenCalledWith("tasks");
  });

  it("does NOT toggle Tasks on Ctrl+Shift+T (that's the theme toggle)", () => {
    const { ctx } = makeCtx("");
    dispose = registerKeyboardShortcuts(ctx);
    // Shift produces an uppercase key; the Tasks branch matches lowercase
    // "t" with !shiftKey, so it must not fire here.
    document.dispatchEvent(key({ key: "T", ctrlKey: true, shiftKey: true }));
    expect(ctx.setActivePanel).not.toHaveBeenCalledWith("tasks");
  });

  it("ignores plain T with no modifier", () => {
    const { ctx } = makeCtx("");
    dispose = registerKeyboardShortcuts(ctx);
    document.dispatchEvent(key({ key: "t" }));
    expect(ctx.setActivePanel).not.toHaveBeenCalled();
  });
});
