/**
 * Liquid morphing bubble used by RecorderSheet. Drives its border-radius
 * from four frequency bands of the analyser FFT (asymmetric warp) and
 * its scale from overall RMS. Idle = static circle; recording = morphing
 * blob with two ambient halo rings that mirror the same corner radii.
 *
 * Extracted from RecorderSheet (2026-05-26, A-13 follow-up) so the
 * createMemos that drive the visual are owned by this component's
 * lifetime instead of living inside an anonymous IIFE in the parent.
 * Same behaviour, conventional reactivity, easier to test.
 */
import { Show, createMemo } from "solid-js";
import type { AudioRecorder } from "../../lib/audio-recorder";

/**
 * Map the analyser's frequency bins to four corner perturbations.
 * Lows pull one corner, mids another, highs the rest — asymmetric warp.
 * Outputs are normalised 0..1 so the consumer can multiply by a fixed
 * deformation amplitude.
 */
function bandsToCorners(bins: Uint8Array): [number, number, number, number] {
  if (bins.length === 0) return [0, 0, 0, 0];
  const q = Math.floor(bins.length / 4);
  function avg(start: number, end: number): number {
    let s = 0;
    for (let i = start; i < end; i++) s += bins[i];
    return s / Math.max(1, end - start) / 255;
  }
  return [
    avg(0, q),                 // top-left   — low freq
    avg(q, 2 * q),             // top-right  — low-mid
    avg(2 * q, 3 * q),         // bottom-right — high-mid
    avg(3 * q, bins.length),   // bottom-left  — high
  ];
}

/** Overall RMS-ish level (0..1) — drives the bubble's scale. */
function overallLevel(bins: Uint8Array): number {
  if (bins.length === 0) return 0;
  let s = 0;
  for (let i = 0; i < bins.length; i++) s += bins[i];
  return s / bins.length / 255;
}

export interface RecorderBubbleProps {
  recorder: AudioRecorder;
  isRecording: () => boolean;
}

export default function RecorderBubble(props: RecorderBubbleProps) {
  // Per-band corner deformations. 0..1 per corner → 30..70% border-radius.
  // Base 50% is a perfect circle; we never go past ~67 to keep the blob
  // recognisable as a single rounded shape.
  const corners = createMemo(() => bandsToCorners(props.recorder.frequencyBins()));
  const level = createMemo(() => overallLevel(props.recorder.frequencyBins()));

  const r = (v: number) => 50 + (v - 0.5) * 35;
  const radii = () => {
    const [a, b, c, d] = corners();
    return `${r(a)}% ${r(b)}% ${r(c)}% ${r(d)}% / ${r(d)}% ${r(a)}% ${r(b)}% ${r(c)}%`;
  };

  // Idle (not recording) → static circle, base scale.
  // Recording → scale grows ~10..40% with amplitude.
  const scale = () => (props.isRecording() ? 1.0 + level() * 0.4 : 1.0);

  return (
    // Wrapper sized to match the maximum visible diameter of the halos
    // so CSS scale transforms don't visually overflow into the sibling
    // controls below. Transforms don't update layout — without explicit
    // headroom here the outer halo (peak ~1.3×) collides with the
    // duration / Discard row.
    <div class="relative w-52 h-52 flex items-center justify-center my-2">
      <Show when={props.isRecording()}>
        {/* Outer halo blob — bigger, slower, lower opacity. Mirrors the
            corner radii of the main blob so they morph in sync instead
            of fighting each other visually. Inset shrinks the halo so
            the wrapper has room for its max scale. */}
        <div
          class="absolute inset-6 bg-red-500/15"
          style={{
            "border-radius": radii(),
            transform: `scale(${1.1 + level() * 0.2})`,
            transition: "border-radius 220ms ease-out, transform 220ms ease-out",
          }}
        />
        <div
          class="absolute inset-8 bg-red-500/20"
          style={{
            "border-radius": radii(),
            transform: `scale(${1.05 + level() * 0.15})`,
            transition: "border-radius 180ms ease-out, transform 180ms ease-out",
          }}
        />
      </Show>

      {/* The main blob. Gradient gives it a "liquid mercury" feel —
          lighter top, darker bottom-right. */}
      <div
        class={`relative w-28 h-28 flex items-center justify-center transition-colors ${
          props.isRecording()
            ? "bg-gradient-to-br from-red-400 via-red-500 to-red-600 shadow-lg shadow-red-500/30"
            : "bg-bg-tertiary"
        }`}
        style={{
          "border-radius": props.isRecording() ? radii() : "50%",
          transform: `scale(${scale()})`,
          transition: "border-radius 140ms ease-out, transform 140ms ease-out, background 220ms",
        }}
      >
        <svg
          width="40"
          height="40"
          viewBox="0 0 24 24"
          fill="none"
          stroke="currentColor"
          stroke-width="1.8"
          stroke-linecap="round"
          stroke-linejoin="round"
          class={props.isRecording() ? "text-white" : "text-text-secondary"}
        >
          <rect x="9" y="2" width="6" height="12" rx="3" />
          <path d="M5 10v2a7 7 0 0 0 14 0v-2" />
          <line x1="12" y1="19" x2="12" y2="22" />
          <line x1="8" y1="22" x2="16" y2="22" />
        </svg>
      </div>
    </div>
  );
}
