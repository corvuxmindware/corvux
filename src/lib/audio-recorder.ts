/**
 * MediaRecorder wrapper for the voice-memo feature.
 *
 * Lifecycle: idle → requesting → recording → stopping → idle.
 * `start()` resolves once the mic stream is granted; `stop()` resolves with
 * the recorded Blob (or null on cancel/error). Callers wire this to a UI
 * that shows recording state + elapsed time.
 *
 * Format negotiation: opus-in-webm preferred (Android Chrome WebView, smaller
 * files), m4a/aac fallback (iOS WKWebView), raw webm as last resort. The Rust
 * allowlist accepts all of these; mismatched MIME on save just collapses to a
 * sensible extension via `saveAudio`.
 *
 * Locked Decision #11 compliance: user must explicitly start AND stop. No
 * timer auto-stop, no background re-arming, no continuous capture. State
 * is observable from UI so the user always sees when the mic is hot.
 */
import { createSignal } from "solid-js";
import { log } from "./log";
import { createWavRecorder } from "./wav-recorder";

export type RecorderState =
  | "idle"
  | "requesting"
  | "recording"
  | "stopping"
  | "error";

// Preference order is platform-conditional. Apple WebKit (iOS WKWebView and
// desktop Safari) will sometimes report `isTypeSupported("audio/webm")` as
// true while the matching `<audio>` element refuses to decode webm — so the
// recording succeeds but playback shows "Error" on the same device. Force
// mp4/AAC there so capture and playback agree.
//
// Everywhere else we prefer opus-in-webm: ~2-4x smaller than AAC for speech
// and the default for Android Chrome WebView.
const APPLE_PRIORITY: readonly string[] = [
  "audio/mp4;codecs=mp4a.40.2",
  "audio/mp4",
  "audio/aac",
  "audio/webm;codecs=opus",
  "audio/webm",
];

const DEFAULT_PRIORITY: readonly string[] = [
  "audio/webm;codecs=opus",
  "audio/webm",
  "audio/mp4;codecs=mp4a.40.2",
  "audio/mp4",
  "audio/ogg;codecs=opus",
];

export function isAppleWebKit(): boolean {
  if (typeof navigator === "undefined") return false;
  const ua = navigator.userAgent ?? "";
  if (/iPhone|iPad|iPod/.test(ua)) return true;
  // iPadOS 13+ reports as Mac in UA; touch capability disambiguates from real Mac.
  if (/Macintosh/.test(ua) && typeof document !== "undefined" && "ontouchend" in document) {
    return true;
  }
  return false;
}

export function pickMimeType(): string | undefined {
  if (typeof MediaRecorder === "undefined") return undefined;
  const candidates = isAppleWebKit() ? APPLE_PRIORITY : DEFAULT_PRIORITY;
  for (const mime of candidates) {
    if (MediaRecorder.isTypeSupported(mime)) return mime;
  }
  return undefined;
}

export interface AudioRecorder {
  state: () => RecorderState;
  durationMs: () => number;
  error: () => string | null;
  /** Frequency-domain amplitudes (0-255) sampled at ~60Hz while recording.
   *  Empty array while idle. Reactive — UI binds for a live waveform. */
  frequencyBins: () => Uint8Array;
  start: () => Promise<void>;
  stop: () => Promise<Blob | null>;
  cancel: () => void;
}

// Bin count — power-of-two FFT size / 2. 32 bars looks dense without
// turning the visualizer into uninterpretable noise on a phone-width
// screen. UI can subsample further if it wants fewer bars.
const FFT_SIZE = 64;

export function createAudioRecorder(): AudioRecorder {
  // On Apple WebKit, MediaRecorder produces fragmented MP4 (CMAF brand
  // `hlsf`) that `<audio>` elements can't decode standalone — verified
  // against an on-device recording. Route to the Web-Audio WAV path
  // there so capture and playback both work; everywhere else stays on
  // MediaRecorder for compression. See wav-recorder.ts top comment.
  if (isAppleWebKit()) {
    // Lazy import to avoid loading the WAV path's deps on non-iOS.
    // Dynamic import would change the function to async; instead we
    // accept the small static-cost overhead and import at the top.
    return createWavRecorder();
  }

  const [state, setState] = createSignal<RecorderState>("idle");
  const [durationMs, setDurationMs] = createSignal(0);
  const [error, setError] = createSignal<string | null>(null);
  const [frequencyBins, setFrequencyBins] = createSignal<Uint8Array>(
    new Uint8Array(0),
    // Bypass referential-equality short-circuit so the UI re-renders every
    // time we hand back a fresh array snapshot.
    { equals: false },
  );

  let mediaRecorder: MediaRecorder | null = null;
  let stream: MediaStream | null = null;
  let chunks: Blob[] = [];
  let startedAt = 0;
  let tickHandle: number | null = null;
  // Resolver wired up in start(); fulfilled by the `stop` event handler so
  // callers `await recorder.stop()` and get the finalized Blob.
  let stopResolver: ((blob: Blob | null) => void) | null = null;

  // Web Audio plumbing for the live waveform. Built lazily on start() because
  // creating an AudioContext before user gesture warns in some browsers.
  let audioCtx: AudioContext | null = null;
  let analyser: AnalyserNode | null = null;
  let visualizerFrame: number | null = null;

  function cleanupStream() {
    if (visualizerFrame !== null) {
      cancelAnimationFrame(visualizerFrame);
      visualizerFrame = null;
    }
    if (analyser) {
      try { analyser.disconnect(); } catch { /* ignore */ }
      analyser = null;
    }
    if (audioCtx) {
      audioCtx.close().catch(() => { /* ignore */ });
      audioCtx = null;
    }
    if (stream) {
      for (const track of stream.getTracks()) track.stop();
      stream = null;
    }
    mediaRecorder = null;
    chunks = [];
    if (tickHandle !== null) {
      clearInterval(tickHandle);
      tickHandle = null;
    }
    setFrequencyBins(new Uint8Array(0));
  }

  async function start(): Promise<void> {
    if (state() !== "idle") return;
    setError(null);
    setDurationMs(0);
    setState("requesting");

    try {
      stream = await navigator.mediaDevices.getUserMedia({
        audio: {
          echoCancellation: true,
          noiseSuppression: true,
          autoGainControl: true,
        },
      });
    } catch (e) {
      cleanupStream();
      const msg = e instanceof Error ? e.message : String(e);
      setError(`Microphone access denied: ${msg}`);
      setState("error");
      log.warn("recorder", "getUserMedia rejected", e);
      // Surface to caller — let UI revert button state.
      throw e;
    }

    const mimeType = pickMimeType();
    try {
      mediaRecorder = new MediaRecorder(
        stream,
        mimeType ? { mimeType } : undefined,
      );
    } catch (e) {
      cleanupStream();
      const msg = e instanceof Error ? e.message : String(e);
      setError(`MediaRecorder unsupported: ${msg}`);
      setState("error");
      throw e;
    }

    mediaRecorder.ondataavailable = (ev) => {
      if (ev.data && ev.data.size > 0) chunks.push(ev.data);
    };

    mediaRecorder.onstop = () => {
      const type = mediaRecorder?.mimeType || mimeType || "audio/webm";
      const blob = chunks.length > 0 ? new Blob(chunks, { type }) : null;
      cleanupStream();
      setState("idle");
      const resolver = stopResolver;
      stopResolver = null;
      resolver?.(blob);
    };

    mediaRecorder.onerror = (ev) => {
      const err = (ev as unknown as { error?: Error }).error;
      log.error("recorder", "MediaRecorder error", err);
      setError(err?.message ?? "Recording error");
      setState("error");
      const resolver = stopResolver;
      stopResolver = null;
      resolver?.(null);
      cleanupStream();
    };

    // Spin up the visualizer pipeline. We tap a clone of the captured stream
    // into an AnalyserNode and poll it every animation frame. The analyser
    // doesn't write back into the recording path, so this is purely a
    // read-side feature with no audio-quality impact.
    try {
      type AudioCtxCtor = typeof AudioContext;
      const Ctor: AudioCtxCtor | undefined =
        typeof AudioContext !== "undefined"
          ? AudioContext
          : (window as unknown as { webkitAudioContext?: AudioCtxCtor }).webkitAudioContext;
      if (Ctor) {
        audioCtx = new Ctor();
        const source = audioCtx.createMediaStreamSource(stream);
        analyser = audioCtx.createAnalyser();
        analyser.fftSize = FFT_SIZE;
        analyser.smoothingTimeConstant = 0.6;
        source.connect(analyser);
        const bins = new Uint8Array(analyser.frequencyBinCount);
        const tick = () => {
          if (!analyser) return;
          analyser.getByteFrequencyData(bins);
          // Hand the UI its own copy — Solid signals shouldn't share a mutable
          // buffer the recorder keeps overwriting.
          setFrequencyBins(new Uint8Array(bins));
          visualizerFrame = requestAnimationFrame(tick);
        };
        visualizerFrame = requestAnimationFrame(tick);
      }
    } catch (e) {
      // Visualizer is best-effort — recording must still work even if the
      // AudioContext setup explodes on a quirky WebView.
      log.warn("recorder", "visualizer setup failed", e);
    }

    // Emit a dataavailable event every second so a mid-recording crash still
    // leaves us with partial audio rather than losing the whole capture.
    mediaRecorder.start(1000);
    startedAt = performance.now();
    tickHandle = window.setInterval(() => {
      setDurationMs(Math.floor(performance.now() - startedAt));
    }, 100);
    setState("recording");
  }

  function stop(): Promise<Blob | null> {
    if (state() !== "recording") return Promise.resolve(null);
    setState("stopping");
    return new Promise<Blob | null>((resolve) => {
      stopResolver = resolve;
      try {
        mediaRecorder?.stop();
      } catch (e) {
        log.error("recorder", "stop() threw", e);
        cleanupStream();
        setState("idle");
        resolve(null);
      }
    });
  }

  function cancel(): void {
    if (state() === "idle") return;
    // Drop the buffer before stop() flushes it — onstop will see empty chunks
    // and resolve null. Same path keeps mic-track cleanup atomic.
    chunks = [];
    try {
      mediaRecorder?.stop();
    } catch {
      cleanupStream();
      setState("idle");
    }
  }

  return { state, durationMs, error, frequencyBins, start, stop, cancel };
}

/** "0:23" / "12:05" — clamps to mm:ss; recordings >1h get hh:mm:ss. */
export function formatDuration(ms: number): string {
  const totalSec = Math.floor(ms / 1000);
  const h = Math.floor(totalSec / 3600);
  const m = Math.floor((totalSec % 3600) / 60);
  const s = totalSec % 60;
  const pad = (n: number) => String(n).padStart(2, "0");
  if (h > 0) return `${h}:${pad(m)}:${pad(s)}`;
  return `${m}:${pad(s)}`;
}
