/**
 * Tests for the WAV encoder used on iOS where MediaRecorder produces
 * fragmented MP4 that <audio> can't play. We can't exercise the live
 * AudioContext graph in vitest, so we hit `encodeWav` directly — the
 * piece whose correctness determines whether the resulting blob is a
 * valid RIFF/WAV file the iOS <audio> element will accept.
 */
import { describe, expect, it } from "vitest";
import { encodeWav, parseWavPeaks } from "../../src/lib/wav-recorder";

async function readBlob(blob: Blob): Promise<DataView> {
  const buf = await blob.arrayBuffer();
  return new DataView(buf);
}

function readStr(view: DataView, offset: number, length: number): string {
  let s = "";
  for (let i = 0; i < length; i++) s += String.fromCharCode(view.getUint8(offset + i));
  return s;
}

describe("encodeWav", () => {
  it("produces a valid 44-byte RIFF/WAVE header", async () => {
    const samples = new Float32Array(2400); // 0.1s at 24kHz target
    const blob = encodeWav([samples], samples.length, 24_000, 24_000);
    expect(blob.type).toBe("audio/wav");
    const view = await readBlob(blob);
    expect(readStr(view, 0, 4)).toBe("RIFF");
    expect(readStr(view, 8, 4)).toBe("WAVE");
    expect(readStr(view, 12, 4)).toBe("fmt ");
    expect(view.getUint32(16, true)).toBe(16);     // PCM fmt chunk size
    expect(view.getUint16(20, true)).toBe(1);      // PCM format
    expect(view.getUint16(22, true)).toBe(1);      // mono
    expect(view.getUint32(24, true)).toBe(24_000); // sample rate
    expect(view.getUint16(34, true)).toBe(16);     // 16-bit
    expect(readStr(view, 36, 4)).toBe("data");
  });

  it("declares data length matching emitted samples", async () => {
    // 1 sec @ 24kHz target with 1:1 ratio (sourceRate == targetRate)
    const samples = new Float32Array(24_000);
    const blob = encodeWav([samples], samples.length, 24_000, 24_000);
    const view = await readBlob(blob);
    // data chunk size = samples * 2 bytes (16-bit mono)
    expect(view.getUint32(40, true)).toBe(48_000);
    // RIFF size = 36 + data
    expect(view.getUint32(4, true)).toBe(36 + 48_000);
    // Total bytes = header (44) + data (48000)
    expect(blob.size).toBe(44 + 48_000);
  });

  it("downsamples 48kHz source to 24kHz target", async () => {
    // 1 sec at 48kHz source → expect ~0.5 sec at 24kHz target.
    const src = new Float32Array(48_000);
    const blob = encodeWav([src], src.length, 48_000, 24_000);
    // Output samples = floor(48000 / 2) = 24000; bytes = 48000.
    expect(blob.size).toBe(44 + 48_000);
  });

  it("clips out-of-range floats to int16 full-scale", async () => {
    // Float input outside [-1, 1] must clip, not wrap.
    const src = new Float32Array([2.0, -2.0, 0.5, -0.5]);
    const blob = encodeWav([src], src.length, 24_000, 24_000);
    const view = await readBlob(blob);
    expect(view.getInt16(44, true)).toBe(32767);  // +2.0 -> +full
    expect(view.getInt16(46, true)).toBe(-32768); // -2.0 -> -full
    // 0.5 * 32767 = 16383.5 -> 16384 with Math.round
    expect(view.getInt16(48, true)).toBe(16384);
    // -0.5 * 32768 = -16384
    expect(view.getInt16(50, true)).toBe(-16384);
  });

  it("concatenates multiple chunks in order", async () => {
    const a = new Float32Array([1.0, 0.5]);
    const b = new Float32Array([-0.5, -1.0]);
    const blob = encodeWav([a, b], 4, 24_000, 24_000);
    const view = await readBlob(blob);
    expect(view.getInt16(44, true)).toBe(32767);
    expect(view.getInt16(46, true)).toBe(16384);
    expect(view.getInt16(48, true)).toBe(-16384);
    expect(view.getInt16(50, true)).toBe(-32768);
  });

  it("handles empty input as a zero-data WAV header", async () => {
    const blob = encodeWav([], 0, 24_000, 24_000);
    expect(blob.size).toBe(44);
    const view = await readBlob(blob);
    expect(view.getUint32(40, true)).toBe(0);
  });
});

describe("parseWavPeaks (round-trips encodeWav)", () => {
  it("reads peaks + duration from an encoded WAV without Web Audio", async () => {
    // 1 second @ 24kHz: a ramp from -1..1 so peaks are non-trivial.
    const samples = new Float32Array(24_000);
    for (let i = 0; i < samples.length; i++) {
      samples[i] = Math.sin((i / samples.length) * Math.PI * 2);
    }
    const blob = encodeWav([samples], samples.length, 24_000, 24_000);
    const buffer = await blob.arrayBuffer();
    const result = parseWavPeaks(buffer, 32);
    expect(result).not.toBeNull();
    expect(result!.peaks.length).toBe(32);
    // Normalised: at least one bucket hits ~1.0 (the sine's crest).
    expect(Math.max(...result!.peaks)).toBeGreaterThan(0.9);
    // All peaks within [0, 1].
    for (const p of result!.peaks) {
      expect(p).toBeGreaterThanOrEqual(0);
      expect(p).toBeLessThanOrEqual(1);
    }
    // 24000 samples @ 24kHz ≈ 1 second.
    expect(result!.duration).toBeCloseTo(1, 1);
  });

  it("returns null for non-WAV input (so caller falls back to decode)", () => {
    // A fake webm header — not RIFF/WAVE.
    const fake = new Uint8Array([0x1a, 0x45, 0xdf, 0xa3, 0, 0, 0, 0]);
    expect(parseWavPeaks(fake.buffer, 32)).toBeNull();
  });

  it("returns null for too-short buffers", () => {
    expect(parseWavPeaks(new Uint8Array(10).buffer, 32)).toBeNull();
  });
});
