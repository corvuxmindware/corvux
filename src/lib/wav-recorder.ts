/**
 * Web-Audio WAV recorder. Used on iOS where MediaRecorder produces
 * fragmented MP4 (CMAF, brand `hlsf`) that `<audio>` elements can't decode
 * standalone — bytes are valid AAC but WebKit only plays fMP4 through MSE.
 * Verified against an actual on-device recording: `file` reports
 * `ISO Media, MP4 Base Media v5`, `afinfo` shows "File type ID: mp4f"
 * with "not optimized", and the Tauri WebView <audio> element raises
 * MEDIA_ERR_SRC_NOT_SUPPORTED on every load.
 *
 * Trade-off: WAV is uncompressed. At 16-bit / 24 kHz mono that's ~46 KB/s,
 * or ~2.7 MB/minute — well under the 25 MB attachment cap for voice memos
 * up to ~9 minutes. We downsample from the AudioContext's native rate
 * (typically 48 kHz on iOS) to 24 kHz on stop; that's twice voice bandwidth
 * and indistinguishable from native rate for speech, while keeping files
 * small enough that iCloud sync doesn't groan.
 *
 * Same external shape as the MediaRecorder version in audio-recorder.ts —
 * callers don't have to care which backend ran. The visualizer wiring is
 * shared (AnalyserNode tap on the source).
 */
import { createSignal } from "solid-js";
import { log } from "./log";
import type { AudioRecorder, RecorderState } from "./audio-recorder";

// FFT size for the live waveform analyser. Matches the MediaRecorder
// variant so the RecorderSheet UI gets the same bin count either way.
const FFT_SIZE = 64;

// Target output sample rate. 24 kHz is the standard for "wideband voice"
// (4x the telephone band) — small files, clean speech, room to spare.
const TARGET_SAMPLE_RATE = 24_000;

// ScriptProcessorNode buffer size. 4096 is the WebKit-friendly default;
// smaller buffers thrash the audio thread, larger ones add latency.
const SP_BUFFER_SIZE = 4096;

export function createWavRecorder(): AudioRecorder {
  const [state, setState] = createSignal<RecorderState>("idle");
  const [durationMs, setDurationMs] = createSignal(0);
  const [error, setError] = createSignal<string | null>(null);
  const [frequencyBins, setFrequencyBins] = createSignal<Uint8Array>(
    new Uint8Array(0),
    { equals: false },
  );

  let stream: MediaStream | null = null;
  let audioCtx: AudioContext | null = null;
  let source: MediaStreamAudioSourceNode | null = null;
  let processor: ScriptProcessorNode | null = null;
  let muteGain: GainNode | null = null;
  let analyser: AnalyserNode | null = null;
  let visualizerFrame: number | null = null;
  let tickHandle: number | null = null;
  let startedAt = 0;

  // Accumulated PCM at the AudioContext's native rate. We downsample once
  // on stop rather than in the audio callback — the callback runs on the
  // audio thread and should stay cheap.
  let nativeChunks: Float32Array[] = [];
  let nativeSampleCount = 0;
  let nativeRate = 0;

  let stopResolver: ((blob: Blob | null) => void) | null = null;

  function cleanup() {
    if (visualizerFrame !== null) {
      cancelAnimationFrame(visualizerFrame);
      visualizerFrame = null;
    }
    if (tickHandle !== null) {
      clearInterval(tickHandle);
      tickHandle = null;
    }
    if (processor) {
      try { processor.disconnect(); } catch { /* ignore */ }
      processor.onaudioprocess = null;
      processor = null;
    }
    if (muteGain) {
      try { muteGain.disconnect(); } catch { /* ignore */ }
      muteGain = null;
    }
    if (analyser) {
      try { analyser.disconnect(); } catch { /* ignore */ }
      analyser = null;
    }
    if (source) {
      try { source.disconnect(); } catch { /* ignore */ }
      source = null;
    }
    if (audioCtx) {
      audioCtx.close().catch(() => { /* ignore */ });
      audioCtx = null;
    }
    if (stream) {
      for (const track of stream.getTracks()) track.stop();
      stream = null;
    }
    nativeChunks = [];
    nativeSampleCount = 0;
    nativeRate = 0;
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
      cleanup();
      const msg = e instanceof Error ? e.message : String(e);
      setError(`Microphone access denied: ${msg}`);
      setState("error");
      log.warn("wav-recorder", "getUserMedia rejected", e);
      throw e;
    }

    try {
      type AudioCtxCtor = typeof AudioContext;
      const Ctor: AudioCtxCtor | undefined =
        typeof AudioContext !== "undefined"
          ? AudioContext
          : (window as unknown as { webkitAudioContext?: AudioCtxCtor }).webkitAudioContext;
      if (!Ctor) {
        throw new Error("AudioContext unavailable");
      }
      audioCtx = new Ctor();
      nativeRate = audioCtx.sampleRate;
      source = audioCtx.createMediaStreamSource(stream);

      // ScriptProcessorNode is deprecated in favour of AudioWorklet, but
      // still works on every iOS Safari we'll target and avoids shipping
      // a separate worklet module file. The callback runs on the audio
      // thread; keep it allocation-light.
      processor = audioCtx.createScriptProcessor(SP_BUFFER_SIZE, 1, 1);
      processor.onaudioprocess = (ev) => {
        const input = ev.inputBuffer.getChannelData(0);
        // Copy — the inputBuffer is reused across callbacks.
        nativeChunks.push(new Float32Array(input));
        nativeSampleCount += input.length;
      };

      analyser = audioCtx.createAnalyser();
      analyser.fftSize = FFT_SIZE;
      analyser.smoothingTimeConstant = 0.6;

      // source → analyser (waveform) AND source → processor → muteGain →
      // destination. Connecting the processor to destination is what keeps
      // the audio thread pumping on some WebKit versions — without it,
      // onaudioprocess never fires. We don't write to the processor's
      // output buffer, so today it arrives at the destination as zeroes.
      // A-12 hardening (2026-05-26): route through a zero-gain GainNode
      // so that if WebKit ever changes the default "unwritten output
      // arrives zeroed" behaviour, users still don't suddenly hear their
      // own mic monitored to the speakers with the feedback loop that
      // implies. Defensive; no current incident.
      muteGain = audioCtx.createGain();
      muteGain.gain.value = 0;
      source.connect(analyser);
      source.connect(processor);
      processor.connect(muteGain);
      muteGain.connect(audioCtx.destination);

      const bins = new Uint8Array(analyser.frequencyBinCount);
      const tick = () => {
        if (!analyser) return;
        analyser.getByteFrequencyData(bins);
        setFrequencyBins(new Uint8Array(bins));
        visualizerFrame = requestAnimationFrame(tick);
      };
      visualizerFrame = requestAnimationFrame(tick);
    } catch (e) {
      cleanup();
      const msg = e instanceof Error ? e.message : String(e);
      setError(`Audio capture setup failed: ${msg}`);
      setState("error");
      throw e;
    }

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
      // Disconnect the audio source FIRST so onaudioprocess stops firing
      // (no new chunks land mid-encode), then give the audio thread one
      // last callback cycle (~85ms for SP_BUFFER_SIZE=4096 at 48kHz) to
      // drain. queueMicrotask alone races the audio thread on WebKit and
      // truncated the last buffer of every recording on iOS.
      try { source?.disconnect(); } catch { /* ignore */ }
      try { processor?.disconnect(); } catch { /* ignore */ }
      setTimeout(() => {
        const localRate = nativeRate;
        const localChunks = nativeChunks;
        const localCount = nativeSampleCount;
        cleanup();
        setState("idle");
        const resolver = stopResolver;
        stopResolver = null;
        if (localCount === 0) {
          resolver?.(null);
          return;
        }
        const blob = encodeWav(localChunks, localCount, localRate, TARGET_SAMPLE_RATE);
        resolver?.(blob);
      }, 120);
    });
  }

  function cancel(): void {
    if (state() === "idle") return;
    nativeChunks = [];
    nativeSampleCount = 0;
    cleanup();
    setState("idle");
    const resolver = stopResolver;
    stopResolver = null;
    resolver?.(null);
  }

  const recorder: AudioRecorder = {
    state,
    durationMs,
    error,
    frequencyBins,
    start,
    stop,
    cancel,
  };
  return recorder;
}

/**
 * Encode buffered Float32 samples (native rate) to a 16-bit PCM mono WAV
 * Blob at `targetRate`. Downsample is a simple linear decimation — fine
 * for voice; we're not chasing audiophile fidelity here.
 *
 * Exported so the unit tests can hit the encoder directly without
 * standing up a full WebAudio graph.
 */
export function encodeWav(
  chunks: Float32Array[],
  totalSamples: number,
  sourceRate: number,
  targetRate: number,
): Blob {
  // Flatten chunks into one Float32 buffer at native rate.
  const flat = new Float32Array(totalSamples);
  let offset = 0;
  for (const c of chunks) {
    flat.set(c, offset);
    offset += c.length;
  }

  // Linear-interp downsample. ratio < 1 means upsample, > 1 downsample.
  const ratio = sourceRate / targetRate;
  const outLen = Math.floor(totalSamples / ratio);
  const out = new Int16Array(outLen);
  for (let i = 0; i < outLen; i++) {
    const srcIdx = i * ratio;
    const srcFloor = Math.floor(srcIdx);
    const frac = srcIdx - srcFloor;
    const a = flat[srcFloor] ?? 0;
    const b = flat[srcFloor + 1] ?? a;
    const sample = a + (b - a) * frac;
    // Clip to [-1, 1] then convert. Negative full-scale is -32768,
    // positive full-scale is 32767 — asymmetric on purpose.
    const clipped = Math.max(-1, Math.min(1, sample));
    out[i] = clipped < 0 ? Math.round(clipped * 32768) : Math.round(clipped * 32767);
  }

  // WAV header: 44 bytes for PCM mono.
  const header = new ArrayBuffer(44);
  const view = new DataView(header);
  const dataBytes = out.length * 2;
  const fileSize = 36 + dataBytes;
  const writeStr = (off: number, s: string) => {
    for (let i = 0; i < s.length; i++) view.setUint8(off + i, s.charCodeAt(i));
  };
  writeStr(0, "RIFF");
  view.setUint32(4, fileSize, true);
  writeStr(8, "WAVE");
  writeStr(12, "fmt ");
  view.setUint32(16, 16, true);        // PCM fmt chunk size
  view.setUint16(20, 1, true);         // PCM format = 1
  view.setUint16(22, 1, true);         // channels = mono
  view.setUint32(24, targetRate, true);
  view.setUint32(28, targetRate * 2, true); // byte rate (rate * blockAlign)
  view.setUint16(32, 2, true);         // blockAlign = channels * bitsPerSample/8
  view.setUint16(34, 16, true);        // bitsPerSample
  writeStr(36, "data");
  view.setUint32(40, dataBytes, true);

  return new Blob([header, out.buffer], { type: "audio/wav" });
}

/**
 * Extract waveform peaks + duration from a PCM WAV ArrayBuffer WITHOUT
 * Web Audio. iOS WKWebView's `decodeAudioData` is unreliable for our
 * recorder output (the inline player's waveform stayed empty on device
 * even though desktop decoded the same code path fine, 2026-05-27).
 * Since we control the WAV layout (see `encodeWav` above), we can read
 * the samples directly and skip the AudioContext entirely.
 *
 * Returns `null` for non-WAV / non-PCM16 input so the caller can fall
 * back to `decodeAudioData` (needed for compressed webm/opus/m4a which
 * we can't parse by hand).
 *
 * `buckets` mirrors the player's bar count.
 */
export function parseWavPeaks(
  buffer: ArrayBuffer,
  buckets: number,
): { peaks: number[]; duration: number } | null {
  if (buffer.byteLength < 44) return null;
  const view = new DataView(buffer);
  const tag = (off: number) =>
    String.fromCharCode(
      view.getUint8(off),
      view.getUint8(off + 1),
      view.getUint8(off + 2),
      view.getUint8(off + 3),
    );
  if (tag(0) !== "RIFF" || tag(8) !== "WAVE") return null;

  // Walk the chunk list — fmt and data aren't guaranteed adjacent.
  let offset = 12;
  let sampleRate = 0;
  let channels = 1;
  let bitsPerSample = 16;
  let dataOffset = -1;
  let dataLen = 0;
  while (offset + 8 <= buffer.byteLength) {
    const id = tag(offset);
    const size = view.getUint32(offset + 4, true);
    const body = offset + 8;
    if (id === "fmt ") {
      channels = view.getUint16(body + 2, true) || 1;
      sampleRate = view.getUint32(body + 4, true);
      bitsPerSample = view.getUint16(body + 14, true);
    } else if (id === "data") {
      dataOffset = body;
      dataLen = Math.min(size, buffer.byteLength - body);
      break;
    }
    // Chunks are word-aligned — pad odd sizes by 1.
    offset = body + size + (size & 1);
  }
  if (dataOffset < 0 || bitsPerSample !== 16) return null;

  const bytesPerFrame = channels * 2;
  const frameCount = Math.floor(dataLen / bytesPerFrame);
  if (frameCount <= 0) return null;
  const duration = sampleRate > 0 ? frameCount / sampleRate : 0;

  const peaks = new Array(buckets).fill(0);
  const framesPerBucket = Math.max(1, Math.floor(frameCount / buckets));
  for (let b = 0; b < buckets; b++) {
    let max = 0;
    const start = b * framesPerBucket;
    const end = Math.min(frameCount, start + framesPerBucket);
    for (let f = start; f < end; f++) {
      // First channel only — voice memos are mono, and for stereo the
      // left channel is a fine proxy for the visual envelope.
      const sampleOff = dataOffset + f * bytesPerFrame;
      if (sampleOff + 2 > buffer.byteLength) break;
      const s = view.getInt16(sampleOff, true);
      const abs = Math.abs(s) / 32768;
      if (abs > max) max = abs;
    }
    peaks[b] = max;
  }
  const peak = Math.max(...peaks, 0.001);
  return { peaks: peaks.map((p) => p / peak), duration };
}
