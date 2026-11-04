/**
 * Inline audio player used in note embeds. Replaces the native
 * `<audio controls>` UI, which renders poorly against the Corvux dark
 * theme on every platform (macOS shows light-themed transport controls,
 * iOS the OS player, Android Chrome the green-progress chrome).
 *
 * Behaviour:
 *  - On mount: fetch the source, decode it once into ~100 peak buckets,
 *    cache them while the player is alive. Failures (decoder rejects,
 *    fetch errors) fall back to "no waveform" with the play head still
 *    working off the underlying HTMLAudioElement's metadata.
 *  - The HTMLAudioElement does the actual playback — we don't try to
 *    reinvent media decoding. It's hidden; we draw our own scrubber.
 *  - Reversed palette per user preference (2026-05-26): the upcoming
 *    (unplayed) portion is accented, the played portion fades to muted.
 *    Reads like "this is what's still ahead" instead of the conventional
 *    "look at what you've consumed".
 *  - Click the waveform to seek; the underlying <audio> handles the
 *    actual time-jump and fires timeupdate so our playhead follows.
 */
import { For, Show, createSignal, createMemo, onMount, onCleanup } from "solid-js";
import { log } from "../../lib/log";
import { parseWavPeaks } from "../../lib/wav-recorder";

export interface AudioPlayerProps {
  /** Resolved playback URL (typically `blob:` from `loadAudio`). */
  url: string;
  /** Decoded audio bytes for waveform peak extraction. Passed in (not
   *  re-fetched from `url`) because `fetch(blob:)` is unreliable in iOS
   *  WKWebView — re-fetching left the waveform empty on device. */
  buffer: ArrayBuffer;
  /** Optional display label below the player. Generated filenames are hidden. */
  label?: string | null;
  /** Optional delete callback. When supplied the X button mounts at the
   *  top-right of the player; the handler is responsible for both the
   *  IPC delete and the markdown removal (audio-embed.ts wires this up). */
  onDelete?: () => void;
}

// Raw extraction resolution — high enough to look detailed on a wide
// desktop track, downsampled per-render to whatever fits the measured
// width. Decoupling extraction from display fixes the sub-pixel-bar bug
// on narrow mobile tracks (96 fixed bars + 2px gaps = ~0.6px bars on a
// ~250px iPhone track → invisible).
const RAW_PEAKS = 240;
// Each rendered bar targets ~3px (≈2px bar + 1px gap). Clamp so very
// narrow tracks still show a sensible minimum and very wide ones don't
// exceed the raw resolution.
const BAR_PX = 3;
const MIN_BARS = 24;

/** Downsample a peak array to `target` buckets (max of each slice). */
function downsamplePeaks(peaks: number[], target: number): number[] {
  if (peaks.length <= target) return peaks;
  const out = new Array(target).fill(0);
  const step = peaks.length / target;
  for (let i = 0; i < target; i++) {
    let max = 0;
    const start = Math.floor(i * step);
    const end = Math.min(peaks.length, Math.floor((i + 1) * step));
    for (let j = start; j < end; j++) if (peaks[j] > max) max = peaks[j];
    out[i] = max;
  }
  return out;
}

/** Sample peaks from a decoded AudioBuffer into N buckets (mono first channel). */
function extractPeaks(buffer: AudioBuffer, buckets: number): number[] {
  const channel = buffer.getChannelData(0);
  const samplesPerBucket = Math.max(1, Math.floor(channel.length / buckets));
  const peaks = new Array(buckets).fill(0);
  for (let i = 0; i < buckets; i++) {
    let max = 0;
    const start = i * samplesPerBucket;
    const end = Math.min(channel.length, start + samplesPerBucket);
    for (let j = start; j < end; j++) {
      const v = Math.abs(channel[j]);
      if (v > max) max = v;
    }
    peaks[i] = max;
  }
  // Normalize so the loudest bucket fills the bar height. Without this,
  // a quiet recording shows up as a flat line.
  const peak = Math.max(...peaks, 0.001);
  return peaks.map((p) => p / peak);
}

function formatTime(seconds: number): string {
  if (!isFinite(seconds) || seconds < 0) return "0:00";
  const m = Math.floor(seconds / 60);
  const s = Math.floor(seconds % 60);
  return `${m}:${String(s).padStart(2, "0")}`;
}

export default function AudioPlayer(props: AudioPlayerProps) {
  // Raw high-res peaks; downsampled to `barCount` for display.
  const [peaks, setPeaks] = createSignal<number[]>([]);
  // How many bars actually fit the rendered track width. Seeded with a
  // safe default; corrected once the track is measured (onMount + on
  // resize). This is the fix for the invisible-waveform-on-mobile bug.
  const [barCount, setBarCount] = createSignal(MIN_BARS);
  const [playing, setPlaying] = createSignal(false);
  const [currentTime, setCurrentTime] = createSignal(0);
  const [duration, setDuration] = createSignal(0);
  const [errored, setErrored] = createSignal(false);
  let audio: HTMLAudioElement | undefined;
  let trackEl: HTMLDivElement | undefined;
  const logLabel = () => props.label || "audio clip";

  // Bars to actually render: raw peaks downsampled to what fits.
  const displayPeaks = createMemo(() => downsamplePeaks(peaks(), barCount()));

  function measureBars() {
    if (!trackEl) return;
    const w = trackEl.clientWidth;
    if (w <= 0) return;
    const fit = Math.max(MIN_BARS, Math.min(RAW_PEAKS, Math.floor(w / BAR_PX)));
    setBarCount(fit);
  }

  // Mount-race guard: an audio decode can take longer than the editor
  // takes to switch notes / re-mount. Without this, the resolve path
  // sets signals on an already-disposed owner (Solid warns) and the
  // AudioContext kept running until GC.
  let disposed = false;

  onMount(async () => {
    // Measure the track NOW (it's laid out by mount) so barCount is
    // right on the first paint, then keep it in sync on resize.
    measureBars();
    try {
      // Fast path: PCM WAV (our iOS recorder output). Parse samples by
      // hand — no AudioContext. iOS WKWebView's decodeAudioData silently
      // fails on these, so the manual parse is the reliable route for the
      // format we control.
      const wav = parseWavPeaks(props.buffer, RAW_PEAKS);
      if (wav) {
        if (disposed) return;
        setPeaks(wav.peaks);
        if (wav.duration > 0) setDuration(wav.duration);
        return;
      }

      // Compressed formats (webm/opus on Android+desktop, m4a) — we can't
      // parse the container by hand, so lean on Web Audio.
      type AudioCtxCtor = typeof AudioContext;
      const Ctor: AudioCtxCtor | undefined =
        typeof AudioContext !== "undefined"
          ? AudioContext
          : (window as unknown as { webkitAudioContext?: AudioCtxCtor }).webkitAudioContext;
      if (!Ctor) return;
      const ctx = new Ctor();
      // decodeAudioData detaches the ArrayBuffer it's given, so pass a
      // copy — the original also backs the playback Blob in some paths.
      const decoded = await ctx.decodeAudioData(props.buffer.slice(0));
      if (disposed) {
        ctx.close().catch(() => { /* ignore */ });
        return;
      }
      setPeaks(extractPeaks(decoded, RAW_PEAKS));
      // Decoder duration beats the <audio> element's (some containers
      // report Infinity until played at least once).
      if (decoded.duration && isFinite(decoded.duration)) {
        setDuration(decoded.duration);
      }
      ctx.close().catch(() => { /* ignore */ });
    } catch (e) {
      // Peak extraction is a UI nicety — log but don't block playback.
      log.warn("audio-player", `peak extraction failed for ${logLabel()}`, e);
    }
  });

  // Keep barCount in sync with the track width — orientation changes,
  // window resizes, sidebar toggles all change how many bars fit.
  let ro: ResizeObserver | undefined;
  onMount(() => {
    if (typeof ResizeObserver !== "undefined" && trackEl) {
      ro = new ResizeObserver(() => measureBars());
      ro.observe(trackEl);
    }
  });

  onCleanup(() => {
    disposed = true;
    ro?.disconnect();
    if (audio && !audio.paused) {
      try { audio.pause(); } catch { /* ignore */ }
    }
  });

  function togglePlay() {
    if (!audio) return;
    if (audio.paused) {
      audio.play().catch((e) => {
        log.warn("audio-player", `play() rejected for ${logLabel()}`, e);
        setErrored(true);
      });
    } else {
      audio.pause();
    }
  }

  function seekFromEvent(e: MouseEvent) {
    if (!audio || !trackEl) return;
    const dur = duration();
    if (!isFinite(dur) || dur <= 0) return;
    const rect = trackEl.getBoundingClientRect();
    const ratio = Math.min(1, Math.max(0, (e.clientX - rect.left) / rect.width));
    audio.currentTime = ratio * dur;
    setCurrentTime(audio.currentTime);
  }

  const progress = () => {
    const d = duration();
    if (!isFinite(d) || d <= 0) return 0;
    return Math.min(1, currentTime() / d);
  };

  return (
    <div class="corvux-audio">
      <Show when={props.onDelete}>
        <button
          type="button"
          class="corvux-audio-delete"
          aria-label="Delete recording"
          title="Delete recording"
          onClick={(e) => {
            e.stopPropagation();
            e.preventDefault();
            props.onDelete?.();
          }}
        >
          <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round">
            <line x1="6" y1="6" x2="18" y2="18" />
            <line x1="18" y1="6" x2="6" y2="18" />
          </svg>
        </button>
      </Show>

      <div class="corvux-audio-row">
        <button
          type="button"
          class="corvux-audio-play"
          aria-label={playing() ? "Pause" : "Play"}
          onClick={togglePlay}
          disabled={errored()}
        >
          <Show
            when={playing()}
            fallback={
              <svg width="16" height="16" viewBox="0 0 24 24" fill="currentColor">
                <path d="M8 5v14l11-7z" />
              </svg>
            }
          >
            <svg width="16" height="16" viewBox="0 0 24 24" fill="currentColor">
              <rect x="6" y="5" width="4" height="14" rx="1" />
              <rect x="14" y="5" width="4" height="14" rx="1" />
            </svg>
          </Show>
        </button>

        <div
          ref={trackEl}
          class="corvux-audio-track"
          onClick={seekFromEvent}
          role="slider"
          aria-label="Audio scrubber"
          aria-valuemin={0}
          aria-valuemax={duration() || 0}
          aria-valuenow={currentTime()}
        >
          <Show
            when={peaks().length > 0}
            fallback={
              // Skeleton bars while peaks decode (or if decode failed).
              // Same visual shape, just static — keeps the player a
              // consistent height regardless of decode state.
              <div class="corvux-audio-skeleton">
                <For each={Array.from({ length: 48 })}>
                  {() => <div class="corvux-audio-skeleton-bar" />}
                </For>
              </div>
            }
          >
            <div class="corvux-audio-bars">
              <For each={displayPeaks()}>
                {(peak, i) => {
                  const ratio = (i() + 0.5) / displayPeaks().length;
                  const isUpcoming = () => ratio > progress();
                  return (
                    <div
                      class="corvux-audio-bar"
                      classList={{
                        "corvux-audio-bar-upcoming": isUpcoming(),
                        "corvux-audio-bar-played": !isUpcoming(),
                      }}
                      // Floor at 4% so silence is still visible as a
                      // thin line, ceiling at 100% of track height.
                      style={{ height: `${Math.max(4, peak * 100)}%` }}
                    />
                  );
                }}
              </For>
            </div>
          </Show>
        </div>

        <span class="corvux-audio-time" aria-live="off">
          {formatTime(currentTime())} / {formatTime(duration())}
        </span>
      </div>

      <Show when={props.label}>
        <div class="corvux-audio-caption">{props.label}</div>
      </Show>

      {/* Hidden <audio> drives actual playback + seeking. Native controls
          off — we draw our own. Keeping it as an HTMLAudioElement (not
          MediaSource) means we get range requests + scrubbing for free. */}
      <audio
        ref={audio}
        src={props.url}
        preload="metadata"
        style={{ display: "none" }}
        onPlay={() => setPlaying(true)}
        onPause={() => setPlaying(false)}
        onEnded={() => setPlaying(false)}
        onTimeUpdate={(e) => setCurrentTime(e.currentTarget.currentTime)}
        onLoadedMetadata={(e) => {
          const d = e.currentTarget.duration;
          if (isFinite(d) && d > 0 && duration() === 0) setDuration(d);
        }}
        onError={() => {
          setErrored(true);
          log.error("audio-player", `<audio> error for ${logLabel()}`);
        }}
      />
    </div>
  );
}
