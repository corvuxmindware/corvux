/**
 * Two-action notification dispatcher — `planDispatch` maps a plugin
 * action payload to an IPC + args pair. Pure function, no Tauri
 * runtime needed.
 *
 * Why pure-function tests instead of a full integration test: the
 * upstream `tauri-plugin-notification` plugin's `onAction` listener
 * has a known bug on Android (#2968) where button labels don't
 * render. Until that's verified on-device we can at least lock down
 * our half of the contract — given a payload shape, did we route it
 * to the right IPC with the right args?
 */

import { describe, it, expect } from "vitest";
import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { planDispatch } from "../../src/lib/notification-actions";

describe("notification-actions.planDispatch", () => {
  const fixedNow = new Date("2026-05-20T14:30:00");

  it("routes the Complete button to complete_task with the right args", () => {
    const plan = planDispatch(
      {
        actionId: "complete",
        extra: {
          note_path: "/v/Journal/2026-05-20.md",
          source_line: 5,
          text_anchor: "go to gym",
        },
      },
      fixedNow,
    );
    expect(plan.kind).toBe("complete");
    expect(plan.ipc).toBe("complete_task");
    expect(plan.args).toEqual({
      notePath: "/v/Journal/2026-05-20.md",
      sourceLine: 5,
      completed: true,
      contentAnchor: "go to gym",
    });
  });

  it("parses a STRING source_line (the iOS payload shape) into a number", () => {
    // Regression (2026-06-03): iOS decodes notification `extra` as
    // [String: String], so the Rust scheduler now sends source_line as a
    // string ("5"). The dispatcher must coerce it to a number, not skip.
    // Pre-fix, every task notification on iOS died with
    // DecodingError(typeMismatch) at extra.source_line.
    const plan = planDispatch(
      {
        actionId: "complete",
        extra: {
          note_path: "/v/Journal/2026-05-20.md",
          source_line: "5", // string, exactly as iOS delivers it
          text_anchor: "go to gym",
        },
      },
      fixedNow,
    );
    expect(plan.kind).toBe("complete");
    expect(plan.args?.sourceLine).toBe(5); // coerced string → number
  });

  it("routes the Snooze button to reschedule_task pushed 15 minutes ahead", () => {
    const plan = planDispatch(
      {
        actionId: "snooze-15",
        extra: {
          note_path: "/v/Journal/2026-05-20.md",
          source_line: 5,
          text_anchor: "go to gym",
        },
      },
      fixedNow,
    );
    expect(plan.kind).toBe("snooze");
    expect(plan.ipc).toBe("reschedule_task");
    expect(plan.args).toEqual({
      notePath: "/v/Journal/2026-05-20.md",
      textAnchor: "go to gym",
      newDate: "2026-05-20",
      newTime: "14:45",
    });
  });

  it("accepts extras spread at the top level (alternate payload shape)", () => {
    // Some plugin versions flatten the extras instead of nesting them
    // under `extra`. The dispatcher must tolerate both.
    const plan = planDispatch(
      {
        actionId: "complete",
        note_path: "/v/notes/x.md",
        source_line: 1,
        text_anchor: "task",
      },
      fixedNow,
    );
    expect(plan.kind).toBe("complete");
    expect(plan.args?.notePath).toBe("/v/notes/x.md");
  });

  it("skips when note_path is missing", () => {
    const plan = planDispatch(
      { actionId: "complete", extra: { source_line: 1 } },
      fixedNow,
    );
    expect(plan.kind).toBe("skip");
    expect(plan.reason).toContain("note_path");
  });

  it("skips when source_line is missing or wrong type", () => {
    const plan = planDispatch(
      {
        actionId: "complete",
        extra: { note_path: "/v/x.md", source_line: "not a number" },
      },
      fixedNow,
    );
    expect(plan.kind).toBe("skip");
    expect(plan.reason).toContain("source_line");
  });

  it("skips on unknown actionId — graceful for plugin-version drift", () => {
    const plan = planDispatch(
      {
        actionId: "future-button",
        extra: {
          note_path: "/v/x.md",
          source_line: 1,
          text_anchor: "t",
        },
      },
      fixedNow,
    );
    expect(plan.kind).toBe("skip");
    expect(plan.reason).toContain("future-button");
  });

  it("snooze handles midnight rollover by re-formatting from the absolute new instant", () => {
    const lateNight = new Date("2026-05-20T23:55:00");
    const plan = planDispatch(
      {
        actionId: "snooze-15",
        extra: {
          note_path: "/v/x.md",
          source_line: 1,
          text_anchor: "t",
        },
      },
      lateNight,
    );
    expect(plan.args?.newDate).toBe("2026-05-21");
    expect(plan.args?.newTime).toBe("00:10");
  });

  it("complete dispatch uses empty string when text_anchor is missing", () => {
    // Defensive — older notifications fired before extras were added
    // wouldn't have text_anchor. Don't reject; the backend can still
    // dispatch by source_line.
    const plan = planDispatch(
      {
        actionId: "complete",
        extra: { note_path: "/v/x.md", source_line: 1 },
      },
      fixedNow,
    );
    expect(plan.kind).toBe("complete");
    expect(plan.args?.contentAnchor).toBe("");
  });
});

/**
 * AUDIT-005 regression: every IPC name emitted by planDispatch must be
 * present in src-tauri/src/lib.rs's `invoke_handler!` list. Pre-fix,
 * "reschedule_task" was invoked but never registered, so every Snooze
 * tap was a silent no-op. This test reads lib.rs at runtime and grep-
 * checks the handler block — would have caught the original regression
 * the moment notification-actions.ts shipped.
 */
describe("AUDIT-005 — Tauri IPC registration", () => {
  // Q-01 followup: command registration moved out of lib.rs into the
  // `collect_commands![...]` macro inside specta_bridge.rs, which is now
  // the single source of truth (see lib.rs comment block on the bridge).
  // The grep here follows the move — the regression test still catches
  // the original 2026-02 bug (notification-actions.ts invoked a command
  // that was never registered) but reads the file that actually holds
  // the registry.
  const spectaBridge = readFileSync(
    resolve(__dirname, "../../src-tauri/src/specta_bridge.rs"),
    "utf-8",
  );

  it.each(["complete_task", "reschedule_task"])(
    "%s is registered in specta_bridge collect_commands!",
    (cmd) => {
      const pattern = new RegExp(`commands::tasks::${cmd}\\b`);
      expect(spectaBridge).toMatch(pattern);
    },
  );
});
