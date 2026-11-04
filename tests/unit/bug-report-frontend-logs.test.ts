/**
 * Tests for B3: the bug-report payload includes the frontend log ring
 * (from lib/log) at the bottom of the markdown blob.
 *
 * Verifies the wire-up between log.drain() and buildPayload — the
 * concrete contract that closes the loop the user actually cares
 * about ("when I send a bug report, my recent errors are in it").
 */

import { describe, it, expect, beforeEach, vi, afterEach } from "vitest";
import { log } from "../../src/lib/log";
import { mockInvoke } from "./setup";

describe("bugReport B3 — frontend logs in payload", () => {
  let clipboard = "";

  beforeEach(() => {
    log.reset();
    clipboard = "";
    // Provide a navigator.clipboard.writeText spy that captures the
    // payload. happy-dom doesn't ship a real clipboard.
    Object.defineProperty(navigator, "clipboard", {
      value: {
        writeText: vi.fn(async (text: string) => {
          clipboard = text;
        }),
      },
      configurable: true,
    });
    // Mock the backend diagnostic IPC so the submit path runs end-to-end.
    mockInvoke.set("get_diagnostic_report", () => ({
      text: "## Backend diagnostic report\n_stub_",
      app_version: "0.1.1",
      os: "macos-test",
      arch: "aarch64",
      log_count: 0,
    }));
    vi.spyOn(window, "open").mockImplementation(() => null);
  });

  afterEach(() => {
    vi.restoreAllMocks();
  });

  it("includes frontend log entries in the clipboard payload after submit", async () => {
    // Generate some events the way real call sites would.
    log.warn("roost", "save failed", new Error("disk full"));
    log.error("chat", "embed timeout");
    log.info("startup", "loaded settings");

    // Dynamic import so the store module sees the post-spy navigator.
    const { bugReportStore } = await import("../../src/stores/bugReport");
    bugReportStore.show({ kind: "manual" });
    const result = await bugReportStore.submit("Click happened then nothing.");

    expect(result.copied).toBe(true);
    expect(clipboard).toContain("# Bug report");
    expect(clipboard).toContain("Click happened then nothing.");
    expect(clipboard).toContain("## Frontend Logs (last 3 entries)");
    expect(clipboard).toContain("roost: save failed");
    expect(clipboard).toContain("Error: disk full");
    expect(clipboard).toContain("chat: embed timeout");
    expect(clipboard).toContain("startup: loaded settings");
  });

  it("omits the Frontend Logs section when the ring is empty", async () => {
    // No log.* calls — ring is empty.
    const { bugReportStore } = await import("../../src/stores/bugReport");
    bugReportStore.show({ kind: "manual" });
    await bugReportStore.submit("Empty ring case.");

    expect(clipboard).toContain("# Bug report");
    expect(clipboard).not.toContain("## Frontend Logs");
  });

  it("formats levels right-padded so columns line up in the markdown blob", async () => {
    log.warn("roost", "warn-msg");
    log.error("chat", "err-msg");

    const { bugReportStore } = await import("../../src/stores/bugReport");
    bugReportStore.show({ kind: "manual" });
    await bugReportStore.submit("formatting check");

    // Level field padded to 5 chars: WARN  / ERROR
    expect(clipboard).toMatch(/WARN  roost: warn-msg/);
    expect(clipboard).toMatch(/ERROR chat: err-msg/);
  });

  it("redacts paths and secrets from frontend logs and crash excerpts", async () => {
    const secret = "sk-ant-abcdefghijklmnopqrstuvwxyz123456";
    log.error("files", `failed at /Users/alice/private-roost/secret.md`, secret);

    const { bugReportStore } = await import("../../src/stores/bugReport");
    bugReportStore.show({
      kind: "previous-crash",
      crashExcerpt: `panic reading C:\\Users\\alice\\vault\\note.md with ${secret}`,
    });
    await bugReportStore.submit("redaction check");

    expect(clipboard).not.toContain("/Users/alice");
    expect(clipboard).not.toContain("C:\\Users\\alice");
    expect(clipboard).not.toContain(secret);
    expect(clipboard).toContain("<redacted:path>");
    expect(clipboard).toContain("<redacted:secret>");
  });
});
