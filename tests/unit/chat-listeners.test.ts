/**
 * Regression test for AUDIT-008 — chat listener leak.
 *
 * AUDIT-008: setupListeners() only released 3 of 6 listeners before
 * re-binding, so HMR or any second-call path doubled tool-result handling.
 *
 * These tests bypass the global setup.ts mock (which forces isTauri=false)
 * with file-local mocks that set isTauri=true and track listen()/unlisten().
 */

import { describe, it, expect, beforeEach, vi } from "vitest";

// Per-test ledger of every listen() call's event name + a tag for the
// returned unlisten so we can assert which got freed.
const listenLog: Array<{ event: string; tag: string }> = [];
const unlistenLog: string[] = [];

// File-local override of the invoke module → isTauri true so the listener
// code paths actually run.
//
// Q-01: stub the call-shape exports (`unwrap`, `commands`, `mockInvoke`)
// the store imports — these tests assert listen()/unlisten() bookkeeping,
// not IPC behaviour. `events` is left to the real `bindings.events`
// surface so `events.aiChunk.listen(cb)` reaches the mocked
// `@tauri-apps/api/event` listen function below (which is the actual
// thing the tests count).
vi.mock("../../src/lib/invoke", async (importOriginal) => {
  const original = await importOriginal<typeof import("../../src/lib/invoke")>();
  const noopCmd = async () => ({ status: "ok", data: undefined });
  return {
    ...original,
    isTauri: true,
    appInvoke: async () => undefined,
    mockInvoke: async () => undefined,
    unwrap: async <T, E>(
      p: Promise<{ status: "ok"; data: T } | { status: "error"; error: E }>,
    ): Promise<T> => {
      const r = await p;
      if (r.status === "error") throw r.error;
      return r.data;
    },
    commands: new Proxy({}, { get: () => noopCmd }),
    // `events` comes from the spread of `original` — bindings.events
    // routes listen() through the mocked @tauri-apps/api/event below.
  };
});

// Mock the Tauri event API. Every listen() call returns a fresh tagged
// unlisten function so we can verify which ones the cleanup path freed.
vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn(async (event: string) => {
    const tag = `${event}#${listenLog.length}`;
    listenLog.push({ event, tag });
    return () => {
      unlistenLog.push(tag);
    };
  }),
}));

// Sidestep modules the chat store imports that try to talk to Tauri
// platform helpers — keep the test surface narrow.
vi.mock("../../src/lib/platform", () => ({
  isMobile: () => false,
  isAndroid: () => false,
  isIOS: () => false,
}));

// Defer the chatStore import until after the mocks are in place.
let chatStore: typeof import("../../src/stores/chat").chatStore;
beforeEach(async () => {
  listenLog.length = 0;
  unlistenLog.length = 0;
  ({ chatStore } = await import("../../src/stores/chat"));
});

describe("AUDIT-008 — setupListeners re-bind", () => {
  it("releases every prior chat listener (all 6) before re-binding", async () => {
    await chatStore.setupListeners();
    const firstRoundTags = listenLog.map((l) => l.tag);
    expect(firstRoundTags.length).toBe(6); // chunk, done, error, tool-result, tool-pending, tool-decided

    await chatStore.setupListeners();
    // After the second setupListeners, every tag from round 1 must have
    // been released. Previously only chunk/done/error were freed and the
    // three tool listeners leaked.
    for (const tag of firstRoundTags) {
      expect(unlistenLog).toContain(tag);
    }
  });
});
