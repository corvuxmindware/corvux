/**
 * The sync panel must NEVER show users raw backend failure strings — those
 * leak internal library names ("iroh", "dial") and read like crashes, which
 * cost user trust. `humanizeReason` maps every backend reason to calm, plain
 * language and a severity that decides tone (amber = self-healing/retrying,
 * red = user must re-pair).
 *
 * These tests lock that contract: known backend reasons map to the right
 * text + severity, no output ever contains internal jargon, and unknown
 * reasons collapse to a calm generic instead of falling through to the raw
 * string.
 */

import { describe, it, expect } from "vitest";
import { humanizeReason } from "../../src/components/Settings/SyncSection/PeerList";

describe("humanizeReason", () => {
  it("maps benign timeout/unreachable states to a calm amber retry message", () => {
    for (const raw of [
      "iroh dial timed out",
      "iroh dial: connection timeout",
      "iroh endpoint unavailable",
      "transport: io: connection lost",
      "peer liveness timeout",
    ]) {
      const info = humanizeReason(raw);
      expect(info.severity).toBe("warn");
      expect(info.text.toLowerCase()).toContain("retr"); // "retrying"
    }
  });

  it("maps auth/identity failures to a red re-pair message", () => {
    for (const raw of [
      "fingerprint mismatch",
      "peer's roost id changed since first pair",
      "wrong PIN",
      "handshake rejected",
    ]) {
      const info = humanizeReason(raw);
      expect(info.severity).toBe("error");
      expect(info.text.toLowerCase()).toContain("re-pair");
    }
  });

  it("collapses unknown reasons to a calm generic, never the raw string", () => {
    const info = humanizeReason("some brand new internal error 0x1234");
    expect(info.severity).toBe("warn");
    expect(info.text).not.toContain("0x1234");
  });

  it("never leaks internal jargon to the user", () => {
    for (const raw of [
      "iroh dial timed out",
      "iroh endpoint unavailable",
      "quinn: reset by peer",
      "fingerprint mismatch",
    ]) {
      const { text } = humanizeReason(raw);
      const lower = text.toLowerCase();
      expect(lower).not.toContain("iroh");
      expect(lower).not.toContain("dial");
      expect(lower).not.toContain("quinn");
      expect(lower).not.toContain("endpoint");
    }
  });
});
