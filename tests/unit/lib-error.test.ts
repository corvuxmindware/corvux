/**
 * Tests for `src/lib/error.ts` — the `errMsg(unknown)` helper that
 * replaced 28 inline copies of `e instanceof Error ? e.message : String(e)`.
 *
 * The contract covers the three real-world cases that thrown values
 * land in across the codebase: native `Error` (most JS APIs), plain
 * strings (Tauri IPC rejection payload), and "other" (anything else).
 */

import { describe, it, expect } from "vitest";
import { errMsg, isSecurityCancel } from "../../src/lib/error";

describe("errMsg", () => {
  it("returns the message of a native Error", () => {
    expect(errMsg(new Error("kaboom"))).toBe("kaboom");
  });

  it("preserves a bare string verbatim (Tauri IPC rejection shape)", () => {
    // Tauri's Result<_, String> rejection lands as a plain string. The
    // pre-refactor branch in DiagnosticsSection/AISection specifically
    // checked for this to avoid the "Error: foo" wrapping that String()
    // would produce.
    expect(errMsg("user cancelled")).toBe("user cancelled");
  });

  it("stringifies non-Error non-string values", () => {
    expect(errMsg(42)).toBe("42");
    expect(errMsg(null)).toBe("null");
    expect(errMsg(undefined)).toBe("undefined");
  });

  it("handles subclasses of Error (TypeError, custom)", () => {
    class CustomError extends Error {
      constructor() {
        super("custom-msg");
        this.name = "CustomError";
      }
    }
    expect(errMsg(new TypeError("type-msg"))).toBe("type-msg");
    expect(errMsg(new CustomError())).toBe("custom-msg");
  });

  it("does not double-wrap when message is already exception-like", () => {
    // Pre-refactor, sites that received a bare string "Error: foo"
    // would still pass it through String() unchanged. New behavior keeps
    // the same — string passthrough means no extra wrapping.
    expect(errMsg("Error: original")).toBe("Error: original");
  });

  it("handles thrown objects with toString", () => {
    expect(errMsg({ toString: () => "obj-str" })).toBe("obj-str");
  });
});

describe("isSecurityCancel", () => {
  // The exact sentences the backend uses when the user says "no" in a native
  // OS confirmation dialog.
  const DECLINES = [
    "Not saved: the security confirmation was cancelled, so nothing was changed.",
    "Not trusted: the confirmation was cancelled, so the host was not pinned.",
    "Roost removal was cancelled.",
  ];

  it("recognises every backend decline message", () => {
    for (const msg of DECLINES) expect(isSecurityCancel(msg), msg).toBe(true);
  });

  it("recognises a decline wrapped in an Error or an 'Error: ' prefix", () => {
    expect(isSecurityCancel(new Error(DECLINES[0]))).toBe(true);
    expect(isSecurityCancel(`Error: ${DECLINES[2]}`)).toBe(true);
  });

  it("is case-insensitive", () => {
    expect(isSecurityCancel("NOT SAVED: THE SECURITY CONFIRMATION WAS CANCELLED")).toBe(true);
  });

  it("does not hide real failures", () => {
    expect(
      isSecurityCancel("The Ollama server address must use https:// for a non-local host"),
    ).toBe(false);
    expect(isSecurityCancel("Lock error: poisoned")).toBe(false);
    expect(isSecurityCancel("Couldn't delete local index: access denied")).toBe(false);
    expect(isSecurityCancel("Add roost cancelled")).toBe(false);
  });

  it("is false for non-error values", () => {
    expect(isSecurityCancel(null)).toBe(false);
    expect(isSecurityCancel(undefined)).toBe(false);
    expect(isSecurityCancel(42)).toBe(false);
  });
});
