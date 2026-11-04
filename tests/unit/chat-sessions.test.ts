/**
 * Multi-session chat store — covers the session lifecycle without
 * touching the UI. These are the cases that would silently break when
 * a future refactor touches the persistence layout.
 */

import { describe, it, expect, beforeEach } from "vitest";
import { mockFs, mockInvoke } from "./setup";
import { chatStore } from "../../src/stores/chat";

const ROOST = "/test/roost";

describe("chat sessions", () => {
  beforeEach(async () => {
    // Wipe anything the store carried over from a prior test. The store
    // itself is a singleton via createRoot, so we re-load with an empty
    // mock FS to reset its in-memory signals.
    await chatStore.loadSessions(ROOST);
    // Loading on an empty FS auto-creates a fresh "New chat" session;
    // some tests want to start truly empty.
  });

  it("auto-creates a session when loading an empty roost", async () => {
    expect(chatStore.sessions().length).toBeGreaterThan(0);
    expect(chatStore.activeSessionId()).not.toBeNull();
    expect(chatStore.messages()).toEqual([]);
  });

  it("newSession creates a blank session and makes it active", async () => {
    // Rename the auto-created session first — otherwise newSession() hits
    // the reuse-empty short-circuit and returns the existing id.
    const existing = chatStore.activeSessionId();
    if (existing) await chatStore.renameSession(existing, "Existing");
    const before = chatStore.sessions().length;
    const id = await chatStore.newSession();
    expect(id).toBeTruthy();
    expect(id).not.toBe(existing);
    expect(chatStore.sessions().length).toBe(before + 1);
    expect(chatStore.activeSessionId()).toBe(id);
    const created = chatStore.sessions().find((s) => s.id === id);
    expect(created?.title).toBe("New chat");
    expect(created?.message_count).toBe(0);
  });

  it("renameSession updates title and persists to index", async () => {
    const id = await chatStore.newSession();
    await chatStore.renameSession(id, "Rust async refactor");
    const session = chatStore.sessions().find((s) => s.id === id);
    expect(session?.title).toBe("Rust async refactor");
    // Persisted: index file should contain the new title
    const index = mockFs.get(`${ROOST}/.corvux/chat-sessions/index.json`);
    expect(index).toContain("Rust async refactor");
  });

  it("renameSession with empty input is a no-op", async () => {
    const id = await chatStore.newSession();
    await chatStore.renameSession(id, "Real Title");
    await chatStore.renameSession(id, "   ");
    const session = chatStore.sessions().find((s) => s.id === id);
    expect(session?.title).toBe("Real Title");
  });

  it("togglePin flips the pinned flag and persists", async () => {
    const id = await chatStore.newSession();
    expect(chatStore.sessions().find((s) => s.id === id)?.pinned).toBeFalsy();
    await chatStore.togglePin(id);
    expect(chatStore.sessions().find((s) => s.id === id)?.pinned).toBe(true);
    await chatStore.togglePin(id);
    expect(chatStore.sessions().find((s) => s.id === id)?.pinned).toBe(false);
  });

  it("deleteSession removes the session + its messages file", async () => {
    const id = await chatStore.newSession();
    const sessionPath = `${ROOST}/.corvux/chat-sessions/${id}.json`;
    expect(mockFs.has(sessionPath)).toBe(true);
    await chatStore.deleteSession(id);
    expect(chatStore.sessions().find((s) => s.id === id)).toBeUndefined();
    expect(mockFs.has(sessionPath)).toBe(false);
  });

  it("deleteSession on the active session activates another (or creates one)", async () => {
    const id = await chatStore.newSession();
    expect(chatStore.activeSessionId()).toBe(id);
    await chatStore.deleteSession(id);
    // Either picks up an existing one or auto-creates a fresh session.
    expect(chatStore.activeSessionId()).not.toBeNull();
    expect(chatStore.activeSessionId()).not.toBe(id);
  });

  it("switchSession loads the target's messages and persists active id", async () => {
    const a = await chatStore.newSession();
    const b = await chatStore.newSession();
    expect(chatStore.activeSessionId()).toBe(b);
    await chatStore.switchSession(a);
    expect(chatStore.activeSessionId()).toBe(a);
    // Persisted active-session marker
    const marker = mockFs.get(`${ROOST}/.corvux/active-session.json`);
    expect(marker).toContain(a);
  });

  it("legacy chat-history.json gets migrated into a 'Migrated chat' session", async () => {
    // Seed the legacy file BEFORE loadSessions runs.
    mockFs.clear();
    const legacyMessages = [
      { role: "user", content: "Old question" },
      { role: "model", content: "Old answer" },
    ];
    mockFs.set(
      `${ROOST}/.corvux/chat-history.json`,
      JSON.stringify(legacyMessages)
    );
    await chatStore.loadSessions(ROOST);
    const migrated = chatStore.sessions().find((s) => s.title === "Migrated chat");
    expect(migrated).toBeTruthy();
    expect(migrated?.message_count).toBe(2);
  });

  it("a delayed old-roost load cannot overwrite or persist into the new roost", async () => {
    const a = "/test/a";
    const b = "/test/b";
    const aSessions = [{
      id: "a-session",
      title: "A session",
      created_at: 1,
      last_active_at: 1,
      message_count: 0,
    }];
    const bSessions = [{
      id: "b-session",
      title: "B session",
      created_at: 2,
      last_active_at: 2,
      message_count: 0,
    }];
    const aIndex = `${a}/.corvux/chat-sessions/index.json`;
    const bIndex = `${b}/.corvux/chat-sessions/index.json`;
    mockFs.set(aIndex, JSON.stringify(aSessions));
    mockFs.set(bIndex, JSON.stringify(bSessions));

    let releaseA!: () => void;
    let markAStarted!: () => void;
    const aStarted = new Promise<void>((resolve) => { markAStarted = resolve; });
    const aGate = new Promise<void>((resolve) => { releaseA = resolve; });
    mockInvoke.set("read_file", async ({ path }) => {
      if (path === aIndex) {
        markAStarted();
        await aGate;
      }
      const value = mockFs.get(String(path));
      if (value === undefined) throw new Error(`mock fs: not found ${path}`);
      return value;
    });

    chatStore.resetForRoostSwitch();
    const loadA = chatStore.loadSessions(a);
    await aStarted;
    await chatStore.loadSessions(b);
    releaseA();
    await loadA;

    expect(chatStore.sessions().map((s) => s.id)).toEqual(["b-session"]);
    expect(JSON.parse(mockFs.get(bIndex) ?? "[]")).toEqual(bSessions);
  });
});
