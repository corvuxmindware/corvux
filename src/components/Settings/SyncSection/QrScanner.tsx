import { createSignal, onCleanup, onMount, Show } from "solid-js";
import jsQR from "jsqr";
import { log } from "../../../lib/log";

/**
 * Camera-based QR scanner for the pairing flow. Opens a back-facing camera via
 * `getUserMedia`, scans each frame with `jsQR`, and calls `onResult` once with
 * the decoded string (the pairing ticket), then tears the camera down.
 *
 * ## Lens selection (the Samsung ultra-wide problem)
 *
 * `facingMode: "environment"` alone picks an arbitrary back camera — on
 * multi-lens phones (e.g. Samsung) that's frequently the **ultra-wide**, which
 * can't focus on a close QR and shrinks it in a wide field of view. We:
 *   1. open with `facingMode` to get permission (labels are only exposed after
 *      a grant),
 *   2. `enumerateDevices()` and pick the **main** back lens — by label when the
 *      OS gives semantic ones, else the lowest `camera2 N` index (the main lens
 *      on Samsung),
 *   3. enable continuous autofocus where supported,
 *   4. expose a **Switch camera** button, because Samsung's WebView labels are
 *      non-semantic ("camera2 0, facing back") and no heuristic is perfect — the
 *      manual override is the reliable fallback.
 *
 * ## Cross-platform
 * - **Android WebView**: `getUserMedia({video})` triggers wry's
 *   `onPermissionRequest` → OS `CAMERA` grant (same path as the mic recorder).
 * - **iOS WKWebView / macOS**: needs `NSCameraUsageDescription`; `playsinline`
 *   keeps the preview from going fullscreen.
 *
 * The PIN is intentionally NOT in the QR — it stays manual as the out-of-band
 * MitM check. Scanning only fills the ticket.
 */

// `focusMode` isn't in the standard DOM lib types yet; narrow extensions keep
// us off `any` (project rule) while reading the experimental capability.
type FocusCapabilities = MediaTrackCapabilities & { focusMode?: string[] };
type FocusConstraint = MediaTrackConstraintSet & { focusMode?: string };

export default function QrScanner(props: {
  onResult: (ticket: string) => void;
  onError: (message: string) => void;
  onCancel: () => void;
}) {
  let video!: HTMLVideoElement;
  let canvas!: HTMLCanvasElement;
  const [status, setStatus] = createSignal<"starting" | "scanning">("starting");
  const [cameras, setCameras] = createSignal<MediaDeviceInfo[]>([]);
  const [camIndex, setCamIndex] = createSignal(0);

  let stream: MediaStream | null = null;
  let raf: number | null = null;
  let done = false;

  function stopStream() {
    if (stream) {
      for (const track of stream.getTracks()) track.stop();
      stream = null;
    }
  }
  function cleanup() {
    done = true;
    if (raf !== null) {
      cancelAnimationFrame(raf);
      raf = null;
    }
    stopStream();
  }
  onCleanup(cleanup);

  /** (Re)open a camera by deviceId, or the default back camera if omitted. */
  async function openCamera(deviceId?: string): Promise<void> {
    stopStream();
    // Request a high capture resolution: the pairing ticket is ~287 chars, so
    // the QR is dense (many small modules). At default webcam resolution jsQR
    // can't resolve the modules; 1280×720+ gives it enough pixels to decode.
    const resolution = {
      width: { ideal: 1280 },
      height: { ideal: 720 },
    };
    const videoConstraints: MediaTrackConstraints = deviceId
      ? { deviceId: { exact: deviceId }, ...resolution }
      : { facingMode: { ideal: "environment" }, ...resolution };
    stream = await navigator.mediaDevices.getUserMedia({
      video: videoConstraints,
      audio: false,
    });
    video.muted = true;
    video.setAttribute("playsinline", "true");
    video.srcObject = stream;
    try {
      await video.play();
    } catch {
      /* some webviews resolve play() late; the rAF loop still reads frames */
    }
    // Best-effort continuous autofocus — helps a close QR snap into focus,
    // especially after switching away from a fixed-focus ultra-wide lens.
    const track = stream.getVideoTracks()[0];
    try {
      if (track && typeof track.getCapabilities === "function") {
        const caps = track.getCapabilities() as FocusCapabilities;
        if (caps.focusMode?.includes("continuous")) {
          await track.applyConstraints({
            advanced: [{ focusMode: "continuous" } as FocusConstraint],
          });
        }
      }
    } catch {
      /* focus tuning is best-effort; scanning works without it */
    }
  }

  /** Back-facing video inputs (fallback: all video inputs if labels are bare). */
  function backCameras(list: MediaDeviceInfo[]): MediaDeviceInfo[] {
    const vids = list.filter((d) => d.kind === "videoinput");
    const back = vids.filter((d) => /back|rear|environment/i.test(d.label));
    return back.length > 0 ? back : vids;
  }

  /** Best-guess index of the *main* back lens within `back`. */
  function preferredIndex(back: MediaDeviceInfo[]): number {
    const isAux = (l: string) =>
      /ultra|wide.?angle|tele|zoom|depth|mono|infrared|\bir\b/i.test(l);
    // If the OS gives semantic labels AND flags some as aux, pick a non-aux one.
    if (back.some((d) => isAux(d.label))) {
      const main = back.findIndex((d) => d.label && !isAux(d.label));
      if (main >= 0) return main;
    }
    // Samsung WebView: labels are "camera2 N, facing back" — the main lens is
    // typically the lowest N. Fall back to that ordering.
    let bestIdx = 0;
    let bestN = Number.POSITIVE_INFINITY;
    back.forEach((d, i) => {
      const m = d.label.match(/camera2?\s+(\d+)/i);
      const n = m ? Number.parseInt(m[1], 10) : i;
      if (n < bestN) {
        bestN = n;
        bestIdx = i;
      }
    });
    return bestIdx;
  }

  function startLoop() {
    const ctx = canvas.getContext("2d", { willReadFrequently: true });
    const tick = () => {
      if (done) return;
      if (ctx && video.readyState >= video.HAVE_CURRENT_DATA) {
        const w = video.videoWidth;
        const h = video.videoHeight;
        if (w > 0 && h > 0) {
          canvas.width = w;
          canvas.height = h;
          ctx.drawImage(video, 0, 0, w, h);
          const frame = ctx.getImageData(0, 0, w, h);
          const code = jsQR(frame.data, w, h, { inversionAttempts: "dontInvert" });
          if (code && code.data) {
            cleanup();
            props.onResult(code.data.trim());
            return;
          }
        }
      }
      raf = requestAnimationFrame(tick);
    };
    raf = requestAnimationFrame(tick);
  }

  onMount(async () => {
    if (!navigator.mediaDevices?.getUserMedia) {
      props.onError("Camera isn't available on this device — paste the code instead.");
      return;
    }
    // 1) Permission + initial stream (labels become available after the grant).
    try {
      await openCamera();
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      log.warn("qr-scanner", "getUserMedia rejected", e);
      props.onError(`Camera access failed: ${msg}`);
      return;
    }
    // 2) Re-select the main back lens if the default grabbed an aux one.
    try {
      const back = backCameras(await navigator.mediaDevices.enumerateDevices());
      setCameras(back);
      if (back.length > 1) {
        const idx = preferredIndex(back);
        setCamIndex(idx);
        const want = back[idx]?.deviceId;
        const current = stream?.getVideoTracks()[0]?.getSettings().deviceId;
        if (want && want !== current) await openCamera(want);
      }
    } catch (e) {
      log.warn("qr-scanner", "camera enumerate/select failed", e);
      /* keep the default stream — still scannable */
    }
    if (done) return; // cancelled mid-setup
    setStatus("scanning");
    startLoop();
  });

  async function switchCamera() {
    const list = cameras();
    if (list.length < 2) return;
    const next = (camIndex() + 1) % list.length;
    setCamIndex(next);
    try {
      await openCamera(list[next]?.deviceId);
    } catch (e) {
      log.warn("qr-scanner", "switch camera failed", e);
    }
  }

  return (
    <div class="space-y-2">
      <div
        class="relative bg-black rounded overflow-hidden mx-auto"
        style={{ "aspect-ratio": "1 / 1", "max-width": "240px" }}
      >
        <video ref={video} class="w-full h-full object-cover" />
        <canvas ref={canvas} class="hidden" />
        {/* Aiming frame */}
        <div class="absolute inset-5 border-2 border-accent/70 rounded pointer-events-none" />
        <Show when={cameras().length > 1}>
          <button
            class="absolute bottom-2 right-2 bg-black/60 hover:bg-black/80 text-white text-[11px] rounded px-2 py-1"
            onClick={switchCamera}
          >
            Switch camera
          </button>
        </Show>
      </div>
      <div class="text-text-muted text-[11px] text-center">
        {status() === "starting"
          ? "Starting camera…"
          : "Point at the QR code on the other device"}
      </div>
      <button
        class="w-full text-text-muted text-xs hover:text-text-secondary"
        onClick={() => {
          cleanup();
          props.onCancel();
        }}
      >
        Cancel scan
      </button>
    </div>
  );
}
