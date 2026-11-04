/**
 * "Knowledge-base friendly" image optimization for pasted / captured images.
 *
 * Before an image is saved to `.attachments/`, we downscale it to a generous
 * max edge and re-encode to WebP at high quality. A 5 MB phone photo becomes a
 * few hundred KB, while a pasted screenshot of code/text stays sharp and
 * readable. Unlike WhatsApp — which crushes images for chat bandwidth — the
 * defaults here preserve legibility, because a second brain is for *recall*,
 * not chat. (Decision: 2026-06-17, "knowledge-base friendly" over aggressive.)
 *
 * Never touched: SVG (vector), GIF (animation), and images already small
 * enough not to bother. If optimizing doesn't actually shrink the file, or the
 * platform can't encode WebP, the ORIGINAL bytes are kept untouched — we never
 * silently switch format or degrade fidelity for no gain.
 *
 * Default ON; user-toggleable (Settings → Editor). The preference is
 * client-only (the resize happens in the WebView), so it lives in
 * localStorage, not the Rust `Settings` struct.
 */
import { createRoot, createSignal } from "solid-js";
import { lsGet, lsSet } from "./storage";
import { log } from "./log";

const LS_KEY = "corvux:image-optimize";

// Tuning knobs — "knowledge-base friendly".
export const MAX_EDGE_PX = 2048; // longest side; larger images downscale
export const WEBP_QUALITY = 0.85; // high enough to keep text crisp
export const MIN_OPTIMIZE_BYTES = 200 * 1024; // below this, leave it alone

// Formats we must NOT rasterize/re-encode.
const SKIP_MIME = new Set(["image/svg+xml", "image/gif"]);

const store = createRoot(() => {
  const seed = lsGet(LS_KEY);
  const [enabled, setSig] = createSignal(seed === null ? true : seed === "1");
  function setEnabled(v: boolean) {
    setSig(v);
    lsSet(LS_KEY, v ? "1" : "0");
  }
  return { enabled, setEnabled };
});

/** Reactive: is image optimization enabled? (default true) */
export const imageOptimizeEnabled = store.enabled;
export const setImageOptimizeEnabled = store.setEnabled;

/** Pure: should we even attempt to optimize this file? */
export function shouldSkipOptimization(file: {
  type: string;
  size: number;
}): boolean {
  if (!file.type.startsWith("image/")) return true;
  if (SKIP_MIME.has(file.type)) return true;
  if (file.size < MIN_OPTIMIZE_BYTES) return true;
  return false;
}

/** Pure: target dimensions preserving aspect ratio, capped at `maxEdge`. */
export function computeTargetDimensions(
  width: number,
  height: number,
  maxEdge: number = MAX_EDGE_PX,
): { width: number; height: number; resized: boolean } {
  const longest = Math.max(width, height);
  if (longest <= maxEdge || longest === 0) {
    return { width, height, resized: false };
  }
  const scale = maxEdge / longest;
  return {
    width: Math.max(1, Math.round(width * scale)),
    height: Math.max(1, Math.round(height * scale)),
    resized: true,
  };
}

export interface OptimizedImage {
  blob: Blob;
  ext: string;
  optimized: boolean;
}

function keepOriginal(file: File): OptimizedImage {
  return {
    blob: file,
    ext: (file.type.split("/")[1] || "png").toLowerCase(),
    optimized: false,
  };
}

/**
 * Downscale + re-encode to WebP when it helps. ALWAYS returns something
 * saveable — on any failure (decode error, no WebP encoder, no size win) it
 * falls back to the original bytes untouched. Never throws, so a paste can
 * never be broken by optimization.
 */
export async function optimizeImage(file: File): Promise<OptimizedImage> {
  if (shouldSkipOptimization(file)) return keepOriginal(file);
  try {
    const bitmap = await createImageBitmap(file);
    const { width, height } = computeTargetDimensions(
      bitmap.width,
      bitmap.height,
    );
    const canvas = document.createElement("canvas");
    canvas.width = width;
    canvas.height = height;
    const ctx = canvas.getContext("2d");
    if (!ctx) {
      bitmap.close?.();
      return keepOriginal(file);
    }
    ctx.drawImage(bitmap, 0, 0, width, height);
    bitmap.close?.();
    const blob = await new Promise<Blob | null>((resolve) =>
      canvas.toBlob((b) => resolve(b), "image/webp", WEBP_QUALITY),
    );
    // No blob, or the platform ignored WebP and gave us PNG (older WebKit):
    // keep the original to preserve fidelity rather than silently switching.
    if (!blob || blob.type !== "image/webp") return keepOriginal(file);
    // Only adopt the optimized version if it's actually smaller.
    if (blob.size >= file.size) return keepOriginal(file);
    return { blob, ext: "webp", optimized: true };
  } catch (e) {
    log.error("image-optimize", "optimize failed; keeping original", e);
    return keepOriginal(file);
  }
}
