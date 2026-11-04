/**
 * Tests for `src/lib/error.ts` — the `errMsg(unknown)` helper that
 * replaced 28 inline copies of `e instanceof Error ? e.message : String(e)`.
 *
 * The contract covers the three real-world cases that thrown values
 * land in across the codebase: native `Error` (most JS APIs), plain
 * strings (Tauri IPC rejection payload), and "other" (anything else).
 */

import { describe, it, expect } from "vitest";
import { errMsg } from "../../src/lib/error";

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
