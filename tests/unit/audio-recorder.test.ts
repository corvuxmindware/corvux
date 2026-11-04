/**
 * Tests for `src/lib/audio-recorder.ts`. Focus is on the pure pieces —
 * `formatDuration` and the MIME-pick heuristic — because the recorder
 * itself is largely orchestration over MediaRecorder + getUserMedia and
 * that surface area is more usefully validated on a real device.
 */
import { afterEach, beforeEach, describe, it, expect, vi } from "vitest";
import { formatDuration, isAppleWebKit, pickMimeType } from "../../src/lib/audio-recorder";

describe("formatDuration", () => {
  it("formats sub-minute durations as m:ss", () => {
    expect(formatDuration(0)).toBe("0:00");
    expect(formatDuration(1_000)).toBe("0:01");
    expect(formatDuration(59_999)).toBe("0:59");
  });

  it("formats minute-range durations as m:ss", () => {
    expect(formatDuration(60_000)).toBe("1:00");
    expect(formatDuration(125_000)).toBe("2:05");
    expect(formatDuration(59 * 60_000 + 59_000)).toBe("59:59");
  });

  it("switches to h:mm:ss past one hour", () => {
    expect(formatDuration(60 * 60_000)).toBe("1:00:00");
    expect(formatDuration(60 * 60_000 + 5 * 60_000 + 7_000)).toBe("1:05:07");
    expect(formatDuration(2 * 60 * 60_000 + 30 * 60_000)).toBe("2:30:00");
  });

  it("truncates fractional seconds", () => {
    expect(formatDuration(999)).toBe("0:00");
    expect(formatDuration(1_999)).toBe("0:01");
    expect(formatDuration(61_500)).toBe("1:01");
  });
});

describe("isAppleWebKit", () => {
  const originalUA = Object.getOwnPropertyDescriptor(globalThis.navigator, "userAgent");

  function setUA(ua: string) {
    Object.defineProperty(globalThis.navigator, "userAgent", {
      configurable: true,
      get: () => ua,
    });
  }

  afterEach(() => {
    if (originalUA) {
      Object.defineProperty(globalThis.navigator, "userAgent", originalUA);
    }
  });

  it("detects iPhone UA", () => {
    setUA("Mozilla/5.0 (iPhone; CPU iPhone OS 18_0 like Mac OS X) AppleWebKit/605.1.15");
    expect(isAppleWebKit()).toBe(true);
  });

  it("detects iPad UA", () => {
    setUA("Mozilla/5.0 (iPad; CPU OS 18_0 like Mac OS X) AppleWebKit/605.1.15");
    expect(isAppleWebKit()).toBe(true);
  });

  it("is false on Android Chrome", () => {
    setUA("Mozilla/5.0 (Linux; Android 14) AppleWebKit/537.36 Chrome/120 Mobile Safari/537.36");
    expect(isAppleWebKit()).toBe(false);
  });

  it("is false on desktop Chrome", () => {
    setUA("Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 Chrome/120");
    expect(isAppleWebKit()).toBe(false);
  });
});

describe("pickMimeType", () => {
  const realMR = globalThis.MediaRecorder;
  const originalUA = Object.getOwnPropertyDescriptor(globalThis.navigator, "userAgent");

  function setUA(ua: string) {
    Object.defineProperty(globalThis.navigator, "userAgent", {
      configurable: true,
      get: () => ua,
    });
  }

  function stubMediaRecorder(supported: string[]) {
    // Only `isTypeSupported` is exercised by pickMimeType; cast covers the
    // rest of the static API surface we don't touch here.
    (globalThis as unknown as { MediaRecorder: unknown }).MediaRecorder = {
      isTypeSupported: vi.fn((mime: string) => supported.includes(mime)),
    };
  }

  beforeEach(() => {
    stubMediaRecorder([]);
  });

  afterEach(() => {
    if (realMR) {
      (globalThis as unknown as { MediaRecorder: unknown }).MediaRecorder = realMR;
    } else {
      delete (globalThis as unknown as { MediaRecorder?: unknown }).MediaRecorder;
    }
    if (originalUA) {
      Object.defineProperty(globalThis.navigator, "userAgent", originalUA);
    }
  });

  it("returns undefined when no candidate is supported", () => {
    setUA("Mozilla/5.0 (X11; Linux x86_64)");
    stubMediaRecorder([]);
    expect(pickMimeType()).toBeUndefined();
  });

  it("prefers mp4 on iOS even when webm is reported supported", () => {
    setUA("Mozilla/5.0 (iPhone; CPU iPhone OS 18_0 like Mac OS X) AppleWebKit/605.1.15");
    stubMediaRecorder(["audio/webm;codecs=opus", "audio/webm", "audio/mp4;codecs=mp4a.40.2", "audio/mp4"]);
    // This is the bug the change is meant to fix — iOS's <audio> element
    // can't decode the webm the MediaRecorder produces, so we must not
    // pick webm even if isTypeSupported says yes.
    expect(pickMimeType()).toBe("audio/mp4;codecs=mp4a.40.2");
  });

  it("falls back to webm on iOS if mp4 is genuinely unsupported", () => {
    setUA("Mozilla/5.0 (iPhone; CPU iPhone OS 18_0 like Mac OS X) AppleWebKit/605.1.15");
    stubMediaRecorder(["audio/webm;codecs=opus", "audio/webm"]);
    expect(pickMimeType()).toBe("audio/webm;codecs=opus");
  });

  it("prefers opus-webm on Android Chrome", () => {
    setUA("Mozilla/5.0 (Linux; Android 14) AppleWebKit/537.36 Chrome/120 Mobile Safari/537.36");
    stubMediaRecorder(["audio/webm;codecs=opus", "audio/webm", "audio/mp4"]);
    expect(pickMimeType()).toBe("audio/webm;codecs=opus");
  });
});
