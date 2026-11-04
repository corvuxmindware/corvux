/**
 * Self-write echo guard.
 *
 * Regression for the mobile bug where typing → autosave → the filesystem
 * watcher echoed the app's own write back as a `note_written` / `note_deleted`
 * sync event, which reloaded the editor (dropping the soft keyboard) and
 * sometimes closed the note. The guard records the app's own saves so the
 * `effects.ts` handler can ignore their echoes; genuine external writes are
 * never recorded, so they still reload the editor.
 */

import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import {
  recordSelfWrite,
  wasRecentSelfWrite,
  __resetSelfWriteGuard,
} from "../../src/lib/self-write-guard";

describe("self-write guard", () => {
  beforeEach(() => {
    __resetSelfWriteGuard();
    vi.useFakeTimers();
    vi.setSystemTime(0);
  });
  afterEach(() => {
    vi.useRealTimers();
  });

  it("flags a just-saved path as a self-write", () => {
    recordSelfWrite("Daily/2026-06-14.md");
    expect(wasRecentSelfWrite("Daily/2026-06-14.md")).toBe(true);
  });

  it("does NOT flag a path the app never saved (genuine external edit)", () => {
    recordSelfWrite("Daily/2026-06-14.md");
    expect(wasRecentSelfWrite("Projects/other.md")).toBe(false);
  });

  it("expires the record after the TTL window", () => {
    recordSelfWrite("note.md");
    expect(wasRecentSelfWrite("note.md")).toBe(true);
    vi.advanceTimersByTime(3999);
    expect(wasRecentSelfWrite("note.md")).toBe(true); // still inside the window
    vi.advanceTimersByTime(2); // now past 4000ms
    expect(wasRecentSelfWrite("note.md")).toBe(false);
  });

  it("re-recording the same path refreshes its window", () => {
    recordSelfWrite("note.md");
    vi.advanceTimersByTime(3000);
    recordSelfWrite("note.md"); // second save resets the clock
    vi.advanceTimersByTime(3000); // 6000ms since first, 3000ms since second
    expect(wasRecentSelfWrite("note.md")).toBe(true);
  });

  it("tracks multiple notes independently", () => {
    recordSelfWrite("a.md");
    vi.advanceTimersByTime(2000);
    recordSelfWrite("b.md");
    vi.advanceTimersByTime(2500); // a.md is 4500ms old (expired), b.md 2500ms
    expect(wasRecentSelfWrite("a.md")).toBe(false);
    expect(wasRecentSelfWrite("b.md")).toBe(true);
  });
});
