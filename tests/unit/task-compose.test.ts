/**
 * Tests for `src/components/Tasks/taskCompose.ts` — the quick-add 📅/⏰
 * markdown contract (ROOST-CONVENTIONS.md) that both the desktop inline
 * inputs and the new mobile date/time sheet feed into. Locking it here means
 * the picker UX can't silently break the format the indexer parses.
 */

import { describe, it, expect } from "vitest";
import {
  composeTaskBody,
  todayLocalIso,
} from "../../src/components/Tasks/taskCompose";

const TODAY = "2026-06-15";

describe("composeTaskBody", () => {
  it("returns empty for empty / whitespace-only text", () => {
    expect(composeTaskBody({ text: "", date: "", time: "" }, TODAY)).toBe("");
    expect(composeTaskBody({ text: "   ", date: "2026-06-16", time: "10:00" }, TODAY)).toBe("");
  });

  it("text only → bare task", () => {
    expect(composeTaskBody({ text: "Reply to Ravi", date: "", time: "" }, TODAY)).toBe(
      "Reply to Ravi",
    );
  });

  it("trims surrounding whitespace", () => {
    expect(composeTaskBody({ text: "  Stand-up  ", date: "", time: "" }, TODAY)).toBe(
      "Stand-up",
    );
  });

  it("date picker → stamps 📅", () => {
    expect(composeTaskBody({ text: "Stand-up", date: "2026-06-16", time: "" }, TODAY)).toBe(
      "Stand-up 📅 2026-06-16",
    );
  });

  it("date + time → stamps 📅 then ⏰", () => {
    expect(
      composeTaskBody({ text: "Stand-up", date: "2026-06-16", time: "10:00" }, TODAY),
    ).toBe("Stand-up 📅 2026-06-16 ⏰ 10:00");
  });

  it("time without date → defaults the date to today (⏰ needs a paired 📅)", () => {
    expect(composeTaskBody({ text: "Call mom", date: "", time: "18:30" }, TODAY)).toBe(
      `Call mom 📅 ${TODAY} ⏰ 18:30`,
    );
  });

  it("inline 📅 the user typed wins — no double date stamp", () => {
    expect(
      composeTaskBody({ text: "Pay rent 📅 2026-01-01", date: "2026-06-16", time: "" }, TODAY),
    ).toBe("Pay rent 📅 2026-01-01");
  });

  it("inline ⏰ the user typed wins — no double time stamp", () => {
    expect(
      composeTaskBody({ text: "Meeting 📅 2026-06-16 ⏰ 09:00", date: "2026-06-16", time: "15:00" }, TODAY),
    ).toBe("Meeting 📅 2026-06-16 ⏰ 09:00");
  });
});

describe("todayLocalIso", () => {
  it("formats local Y-M-D with zero padding (no UTC shift)", () => {
    // Local June 9 2026 — month/day must zero-pad.
    expect(todayLocalIso(new Date(2026, 5, 9))).toBe("2026-06-09");
    expect(todayLocalIso(new Date(2026, 11, 31))).toBe("2026-12-31");
  });
});
