/**
 * Regression: an edit made inside the 500ms auto-save debounce must reach
 * disk even if the user closes the tab / switches roost / closes all tabs
 * before the timer fires.
 *
 * The bug (reported for read-mode table-cell edits, but general to any edit):
 * `closeTab` / `clearAutoSaveTimers` / `closeAllTabs` cancelled the pending
 * debounced save WITHOUT flushing it first, so a quick edit-then-close silently
 * dropped the change. These tests fail against the pre-fix code — the tab is
 * removed and its timer cleared, so `save_roost_note` is never called — and
 * pass once the close paths flush through `saveTab`.
 *
 * Component/editor rendering is out of vitest scope (see save-roost-note.test.ts),
 * so we drive the tab store directly and assert the IPC that persists to disk.
 */

import { describe, it, expect, afterEach, vi } from "vitest";
import { mockInvoke, mockFs } from "./setup";
import { tabStore } from "../../src/stores/tabs";

// Record every save_roost_note the store fires so we can assert the flush ran.
type SaveCall = { path: string; content: string };
function trackSaves(): SaveCall[] {
  const calls: SaveCall[] = [];
  mockInvoke.set("save_roost_note", (args) => {
    calls.push({ path: String(args.path), content: String(args.content) });
    // Minimal well-formed SaveOutcome — the store only reads it opaquely.
    return { stats: { notes: 1, links: 0, tags: 0 }, tasks: [], backlinks: [], fts5: { kind: "indexed" } };
  });
  return calls;
}

// Open a note (seeds the mock fs so read_file succeeds), then make it the
// active tab and apply an edit — which sets isModified and schedules the
// 500ms auto-save timer.
async function openAndEdit(path: string, original: string, edited: string) {
  mockFs.set(path, original);
  await tabStore.openTab(path);
  tabStore.updateContent(edited); // edits the active tab (just-opened)
}

afterEach(async () => {
  // Reset the shared singleton. closeAllTabs flushes, but mockInvoke is
  // cleared before the next test, so any cleanup save is harmless.
  await tabStore.closeAllTabs();
});

describe("close paths flush pending edits (auto-save debounce)", () => {
  it("closeTab saves a modified tab instead of dropping the debounced edit", async () => {
    const saves = trackSaves();
    await openAndEdit("/v/notes/a.md", "# A\n", "# A\n\nedited in read mode\n");

    tabStore.closeTab("/v/notes/a.md");

    // The save fires from the flush, not the 500ms timer (which closeTab
    // cleared). Wait for the async saveTab chain to settle.
    await vi.waitFor(() => expect(saves.length).toBe(1));
    expect(saves[0]).toEqual({
      path: "/v/notes/a.md",
      content: "# A\n\nedited in read mode\n",
    });
    // Tab is gone from the list after close.
    expect(tabStore.tabs().find((t) => t.path === "/v/notes/a.md")).toBeUndefined();
  });

  it("closeTab does NOT save an unmodified tab", async () => {
    const saves = trackSaves();
    mockFs.set("/v/notes/clean.md", "# Clean\n");
    await tabStore.openTab("/v/notes/clean.md");

    tabStore.closeTab("/v/notes/clean.md");

    // Give any stray async a tick; there should be nothing to save.
    await Promise.resolve();
    expect(saves).toEqual([]);
  });

  it("clearAutoSaveTimers flushes every modified tab before clearing (roost switch)", async () => {
    const saves = trackSaves();
    await openAndEdit("/v/notes/one.md", "1", "1 edited");
    await openAndEdit("/v/notes/two.md", "2", "2 edited");

    await tabStore.clearAutoSaveTimers();

    expect(saves.map((s) => s.path).sort()).toEqual([
      "/v/notes/one.md",
      "/v/notes/two.md",
    ]);
    expect(saves.find((s) => s.path === "/v/notes/two.md")?.content).toBe("2 edited");
  });

  it("closeAllTabs flushes modified tabs, then empties the list", async () => {
    const saves = trackSaves();
    await openAndEdit("/v/notes/x.md", "x", "x edited");
    await openAndEdit("/v/notes/y.md", "y", "y edited");

    await tabStore.closeAllTabs();

    expect(saves.map((s) => s.path).sort()).toEqual([
      "/v/notes/x.md",
      "/v/notes/y.md",
    ]);
    expect(tabStore.tabs()).toEqual([]);
  });

  it("keeps a dirty tab open when its close-time save fails", async () => {
    mockInvoke.set("save_roost_note", () => {
      throw new Error("disk full");
    });
    await openAndEdit("/v/notes/keep.md", "old", "irreplaceable edit");

    const closed = await tabStore.closeTab("/v/notes/keep.md");

    expect(closed).toBe(false);
    expect(tabStore.tabs().find((t) => t.path === "/v/notes/keep.md")?.content)
      .toBe("irreplaceable edit");
    expect(tabStore.tabs().find((t) => t.path === "/v/notes/keep.md")?.isModified)
      .toBe(true);
  });

  it("remote deletion discards a dirty buffer without recreating the note", async () => {
    const saves = trackSaves();
    await openAndEdit("/v/notes/deleted.md", "old", "dirty");

    await tabStore.onFileDeleted("/v/notes/deleted.md");
    await new Promise((resolve) => setTimeout(resolve, 550));

    expect(saves).toEqual([]);
    expect(tabStore.tabs().find((t) => t.path === "/v/notes/deleted.md")).toBeUndefined();
  });

  it("serializes overlapping saves and leaves the newest edit dirty until committed", async () => {
    const calls: SaveCall[] = [];
    let releaseFirst!: () => void;
    const firstGate = new Promise<void>((resolve) => { releaseFirst = resolve; });
    mockInvoke.set("save_roost_note", async (args) => {
      calls.push({ path: String(args.path), content: String(args.content) });
      if (calls.length === 1) await firstGate;
      return { stats: { notes: 1, links: 0, tags: 0 }, tasks: [], backlinks: [], fts5: { kind: "indexed" } };
    });
    await openAndEdit("/v/notes/serial.md", "base", "first");

    const first = tabStore.saveTab("/v/notes/serial.md");
    await vi.waitFor(() => expect(calls).toHaveLength(1));
    tabStore.updateContent("second");
    const second = tabStore.saveTab("/v/notes/serial.md");
    expect(calls).toHaveLength(1);

    releaseFirst();
    await Promise.all([first, second]);

    expect(calls.map((call) => call.content)).toEqual(["first", "second"]);
    const tab = tabStore.tabs().find((t) => t.path === "/v/notes/serial.md");
    expect(tab?.originalContent).toBe("second");
    expect(tab?.isModified).toBe(false);
  });
});
