// @vitest-environment happy-dom
/**
 * Tests for `QrScanner` — the camera QR-scan component used in pairing.
 *
 * The happy path (real camera frames → jsQR decode) can't run in happy-dom
 * (no media stream, no canvas pixel readback), so it's verified on-device.
 * What we CAN and must lock here is the graceful-failure invariant: when the
 * camera is unavailable or denied, the scanner reports `onError` and never
 * `onResult` — so the panel falls back to paste instead of hanging or
 * throwing. That's the behavior that keeps a denied camera from breaking
 * pairing.
 */
import { describe, it, expect, vi, afterEach } from "vitest";
import { render } from "solid-js/web";
import QrScanner from "../../src/components/Settings/SyncSection/QrScanner";

let dispose: (() => void) | null = null;

afterEach(() => {
  dispose?.();
  dispose = null;
});

function mockGetUserMedia(impl: () => Promise<MediaStream>) {
  Object.defineProperty(navigator, "mediaDevices", {
    value: { getUserMedia: vi.fn(impl) },
    configurable: true,
  });
}

describe("QrScanner", () => {
  it("calls onError (not onResult) when camera access is denied", async () => {
    mockGetUserMedia(() =>
      Promise.reject(new DOMException("Permission denied", "NotAllowedError")),
    );
    const onResult = vi.fn();
    const onError = vi.fn();
    const onCancel = vi.fn();

    const container = document.createElement("div");
    document.body.appendChild(container);
    dispose = render(
      () => (
        <QrScanner onResult={onResult} onError={onError} onCancel={onCancel} />
      ),
      container,
    );

    await vi.waitFor(() => expect(onError).toHaveBeenCalledTimes(1));
    expect(onError.mock.calls[0][0]).toMatch(/camera access failed/i);
    expect(onResult).not.toHaveBeenCalled();
  });

  it("reports an error when the device has no camera API at all", async () => {
    // Some webviews expose no mediaDevices (e.g. insecure context). The
    // scanner must degrade to an error, not throw an unhandled exception.
    Object.defineProperty(navigator, "mediaDevices", {
      value: undefined,
      configurable: true,
    });
    const onResult = vi.fn();
    const onError = vi.fn();

    const container = document.createElement("div");
    document.body.appendChild(container);
    dispose = render(
      () => (
        <QrScanner onResult={onResult} onError={onError} onCancel={() => {}} />
      ),
      container,
    );

    await vi.waitFor(() => expect(onError).toHaveBeenCalledTimes(1));
    expect(onResult).not.toHaveBeenCalled();
  });
});
