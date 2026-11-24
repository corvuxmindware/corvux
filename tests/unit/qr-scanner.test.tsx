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

  it("builds its preview <video> with document.createElement, not a JSX template clone", () => {
    // iOS WKWebView bug (verified on iPhone 16 Pro, 2026-11-06): a <video>
    // cloned from Solid's JSX <template> plays the camera stream (readyState 4,
    // frames readable via drawImage) but WebKit never composites its video
    // layer, so the preview is solid black. A createElement'd <video> renders.
    // Probe on-device: template-clone → black; createElement / importNode /
    // main-document cloneNode → live picture.
    mockGetUserMedia(() => new Promise<MediaStream>(() => {})); // stay "starting"
    const created: Element[] = [];
    const realCreate = document.createElement.bind(document);
    const spy = vi
      .spyOn(document, "createElement")
      .mockImplementation(((tag: string, opts?: ElementCreationOptions) => {
        const el = realCreate(tag, opts);
        created.push(el);
        return el;
      }) as typeof document.createElement);

    const container = document.createElement("div");
    document.body.appendChild(container);
    try {
      dispose = render(
        () => (
          <QrScanner onResult={() => {}} onError={() => {}} onCancel={() => {}} />
        ),
        container,
      );
    } finally {
      spy.mockRestore();
    }

    const video = container.querySelector("video");
    expect(video).not.toBeNull();
    expect(created).toContain(video);
    // Inline playback must be set on the element itself (iOS).
    expect(video?.hasAttribute("playsinline")).toBe(true);
    expect(video?.muted).toBe(true);
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
