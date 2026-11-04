import { describe, expect, it } from "vitest";
import { hasTimedTask } from "../../src/stores/tasks";

/**
 * Regression test for the Android HIGH-2 audit fix (2026-06-08).
 *
 * The backend scheduler arms an OS alarm for every *timed* task it
 * indexes. The frontend must request POST_NOTIFICATIONS whenever a
 * loaded task set contains a timed task — otherwise reminders typed
 * directly into a note (not via quick-add) schedule alarms the OS
 * silently drops on Android 13+. `hasTimedTask` is the predicate that
 * decides whether to prompt; it must fire on a clock time and ONLY on
 * an open (incomplete) task.
 */
describe("hasTimedTask", () => {
  const row = (dueAt: string, completed = false) => ({ dueAt, completed });

  it("is false for an empty list", () => {
    expect(hasTimedTask([])).toBe(false);
  });

  it("is false when no task carries a clock time", () => {
    expect(
      hasTimedTask([row(""), row("2026-06-10"), row("2026-12-31")]),
    ).toBe(false);
  });

  it("is true when an open task has a HH:MM time", () => {
    expect(hasTimedTask([row("2026-06-10 09:00")])).toBe(true);
  });

  it("detects a timed task among date-only and undated ones", () => {
    expect(
      hasTimedTask([row(""), row("2026-06-10"), row("2026-06-10 23:30")]),
    ).toBe(true);
  });

  it("ignores completed timed tasks (their alarms are already cancelled)", () => {
    expect(hasTimedTask([row("2026-06-10 09:00", true)])).toBe(false);
  });

  it("returns true if any OPEN timed task exists alongside completed ones", () => {
    expect(
      hasTimedTask([row("2026-06-10 09:00", true), row("2026-06-11 08:15")]),
    ).toBe(true);
  });

  it("does not treat a bare date as timed", () => {
    expect(hasTimedTask([row("2026-06-10")])).toBe(false);
  });
});
