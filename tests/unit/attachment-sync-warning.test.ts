/**
 * NEW-DB1: tests for `attachmentSyncWarning` in `src/lib/attachments.ts`.
 *
 * The load-bearing bit is the boundary logic: an attachment between the sync
 * blob limit (8 MiB) and the save cap (25 MiB) saves locally but never syncs,
 * so the user must be warned — but only when sync is actually enabled, and NOT
 * for files that sync fine (<= 8 MiB) or that the save itself rejects (> 25 MiB).
 *
 * Pure-function test: the IPC/toast/localStorage glue is mocked so importing the
 * module doesn't need a Tauri harness.
 */
import { describe, it, expect, vi } from "vitest";

vi.mock("../../src/lib/invoke", () => ({
  commands: { saveAttachment: vi.fn(), readAttachment: vi.fn() },
  unwrap: <T>(p: Promise<T>): Promise<T> => p,
}));
vi.mock("../../src/lib/log", () => ({
  log: { error: vi.fn(), warn: vi.fn(), info: vi.fn() },
}));

import { attachmentSyncWarning } from "../../src/lib/attachments";

const MIB = 1024 * 1024;
const SYNC_LIMIT = 8 * MIB;
const SAVE_CAP = 25 * MIB;

describe("attachmentSyncWarning", () => {
  it("never warns when sync is disabled, regardless of size", () => {
    expect(attachmentSyncWarning(SYNC_LIMIT + 1, false)).toBeNull();
    expect(attachmentSyncWarning(20 * MIB, false)).toBeNull();
  });

  it("does not warn at or below the sync blob limit (these sync fine)", () => {
    expect(attachmentSyncWarning(SYNC_LIMIT - 1, true)).toBeNull();
    expect(attachmentSyncWarning(SYNC_LIMIT, true)).toBeNull(); // exactly 8 MiB
    expect(attachmentSyncWarning(1, true)).toBeNull();
    expect(attachmentSyncWarning(0, true)).toBeNull();
  });

  it("warns in the gap: above the sync limit, up to and including the save cap", () => {
    expect(attachmentSyncWarning(SYNC_LIMIT + 1, true)).toMatch(/saved locally/i);
    expect(attachmentSyncWarning(12 * MIB, true)).toMatch(/other devices/i);
    expect(attachmentSyncWarning(SAVE_CAP, true)).toMatch(/saved locally/i); // exactly 25 MiB
  });

  it("does not warn above the save cap (the save itself is rejected — different path)", () => {
    expect(attachmentSyncWarning(SAVE_CAP + 1, true)).toBeNull();
    expect(attachmentSyncWarning(100 * MIB, true)).toBeNull();
  });
});
