import { commands, unwrap } from "./invoke";
import { log } from "./log";
import { imageOptimizeEnabled, optimizeImage } from "./image-optimize";
import { lsGet } from "./storage";
import { toastStore } from "../components/Toast/Toast";

// NEW-DB1: an attachment that clears the 25 MiB save cap but exceeds the sync
// blob limit saves locally yet silently never syncs. Warn at attach time.
//
// Keep in lock-step with the Rust constants:
//   SYNC_BLOB_MAX_BYTES  — src-tauri/src/sync/protocol/mod.rs (8 MiB): the sync
//     engine refuses to transfer attachment blobs larger than this.
//   ATTACHMENT_MAX_BYTES — src-tauri/src/commands/attachments.rs (25 MiB): the
//     hard save cap; above it save_attachment rejects outright (its own error
//     toast fires), so it is NOT the "saved but won't sync" case.
const SYNC_BLOB_MAX_BYTES = 8 * 1024 * 1024;
const ATTACHMENT_MAX_BYTES = 25 * 1024 * 1024;

/**
 * Pure: the "saved locally, too large to sync" toast message for an attachment
 * of `sizeBytes`, or `null` when no warning is warranted. Fires only when sync
 * is enabled AND the size is in the gap between the sync blob limit (exclusive)
 * and the save cap (inclusive). At-or-below 8 MiB syncs fine; above 25 MiB the
 * save itself is rejected (different error path). Exported for unit tests.
 */
export function attachmentSyncWarning(
  sizeBytes: number,
  syncEnabled: boolean,
): string | null {
  if (!syncEnabled) return null;
  if (sizeBytes > SYNC_BLOB_MAX_BYTES && sizeBytes <= ATTACHMENT_MAX_BYTES) {
    return "Attachment saved locally — too large to sync to your other devices";
  }
  return null;
}

/** Local hint for "the user has turned sync on" (mirrors the check in
 *  stores/sync.ts). The backend prefs.json is the real source of truth, but
 *  this fast localStorage flag is enough to decide whether a "won't sync"
 *  warning is meaningful to show. */
function isSyncEnabled(): boolean {
  return lsGet("corvux:syncEnabled") === "1";
}

/** Show the "saved locally, won't sync" toast if `sizeBytes` falls in the
 *  no-sync gap and sync is enabled. No-op otherwise. */
function maybeWarnAttachmentSync(sizeBytes: number): void {
  const msg = attachmentSyncWarning(sizeBytes, isSyncEnabled());
  if (msg) toastStore.show(msg, "warning", 6000);
}

/**
 * Base64-encode bytes in 32 KB chunks. The naive
 * `String.fromCharCode(...bytes)` blows the call stack on multi-MB buffers,
 * and a per-byte `binary += ...` loop is O(n) string churn that janks the UI
 * on large pastes. Shared by the image and audio save paths.
 */
function bytesToBase64(bytes: Uint8Array): string {
  let binary = "";
  const CHUNK = 0x8000;
  for (let i = 0; i < bytes.length; i += CHUNK) {
    binary += String.fromCharCode(
      ...bytes.subarray(i, Math.min(i + CHUNK, bytes.length)),
    );
  }
  return btoa(binary);
}

/**
 * Resolve a roost-relative image path to a data URL so CodeMirror can embed
 * it inline. Using a data URL (not the Tauri asset protocol) keeps this
 * working in `pnpm dev` browser mode and WSL Tauri-dev uniformly.
 *
 * Native builds can swap to `convertFileSrc` for perf when the roost is on
 * a real filesystem. SAF roosts have no equivalent, so data URL is the
 * portable choice for now.
 */
export async function loadImage(
  roostPath: string | null,
  relPath: string,
): Promise<string | null> {
  if (!roostPath) return null;
  try {
    const b64 = await unwrap(commands.readAttachment(roostPath, relPath));
    const ext = (relPath.split(".").pop() || "png").toLowerCase();
    const mime =
      ext === "svg"
        ? "image/svg+xml"
        : ext === "jpg" || ext === "jpeg"
          ? "image/jpeg"
          : `image/${ext}`;
    return `data:${mime};base64,${b64}`;
  } catch (e) {
    log.error("attachments", `failed to load image: ${relPath}`, e);
    return null;
  }
}

// Extension → audio MIME for data-URL playback. webm/opus and m4a/aac are the
// two formats produced by our recorder; the rest cover user-imported files.
const AUDIO_MIME: Record<string, string> = {
  webm: "audio/webm",
  opus: "audio/ogg",
  ogg: "audio/ogg",
  m4a: "audio/mp4",
  mp4: "audio/mp4",
  aac: "audio/aac",
  mp3: "audio/mpeg",
  wav: "audio/wav",
};

/**
 * Audio resource handed to the inline player: a `blob:` URL for the
 * `<audio>` element (range-capable playback) PLUS the decoded byte
 * buffer for waveform peak extraction. We carry the buffer here so the
 * player can `decodeAudioData(buffer)` directly instead of re-`fetch`ing
 * the blob URL — `fetch(blob:)` is unreliable in iOS WKWebView (the
 * waveform silently never rendered on device, 2026-05-27), and re-
 * fetching would also double the base64 IPC transfer.
 */
export interface AudioResource {
  url: string;
  buffer: ArrayBuffer;
}

/**
 * Resolve a roost-relative audio path to a playable blob URL + its
 * decoded bytes. We return `blob:` not `data:` because iOS WKWebView
 * refuses to play multi-MB base64 data URLs in <audio> — the element
 * raises MEDIA_ERR_SRC_NOT_SUPPORTED even when the bytes decode fine
 * elsewhere. Blob URLs work because WebKit treats them as range-capable
 * sources, matching what HTMLMediaElement actually wants.
 *
 * Note: object URLs leak unless revoked. The editor extension caches them
 * for the editor's lifetime + revokes on LRU eviction (see audio-embed).
 */
export async function loadAudio(
  roostPath: string | null,
  relPath: string,
): Promise<AudioResource | null> {
  if (!roostPath) return null;
  try {
    const b64 = await unwrap(commands.readAttachment(roostPath, relPath));
    const ext = (relPath.split(".").pop() || "webm").toLowerCase().replace(/[?#].*$/, "");
    const mime = AUDIO_MIME[ext] ?? "audio/webm";
    // Decode base64 -> bytes once. The Blob copies the bytes for the
    // playback URL; we hand the original ArrayBuffer back for decode so
    // the player never has to re-fetch the blob (iOS WKWebView blocks /
    // mishandles fetch(blob:), which left the waveform empty on device).
    const binary = atob(b64);
    const bytes = new Uint8Array(binary.length);
    for (let i = 0; i < binary.length; i++) bytes[i] = binary.charCodeAt(i);
    const blob = new Blob([bytes], { type: mime });
    return { url: URL.createObjectURL(blob), buffer: bytes.buffer };
  } catch (e) {
    log.error("attachments", `failed to load audio: ${relPath}`, e);
    return null;
  }
}

/**
 * Handle a pasted / dropped image: encode the File as base64 and hand it to
 * the Rust `save_attachment` command, which writes to `.attachments/` under
 * the active roost and returns the stored filename the editor can reference.
 */
export async function pasteImage(
  roostPath: string | null,
  file: File,
): Promise<string | null> {
  if (!roostPath) return null;
  try {
    // Downscale + re-encode (WebP) before saving, unless the user turned it
    // off. optimizeImage never throws and falls back to the original bytes,
    // so a paste can't be broken by optimization.
    const opt = imageOptimizeEnabled() ? await optimizeImage(file) : null;
    const blob: Blob = opt?.blob ?? file;
    const ext = opt?.ext ?? (file.type.split("/")[1] || "png");
    const buffer = await blob.arrayBuffer();
    const data = bytesToBase64(new Uint8Array(buffer));
    const filename = await unwrap(commands.saveAttachment(roostPath, data, ext));
    // NEW-DB1: the save succeeded but the blob may be too big to sync — warn on
    // the actual stored size (post-optimization), not the original file size.
    maybeWarnAttachmentSync(buffer.byteLength);
    return filename;
  } catch (e) {
    log.error("attachments", "failed to save pasted image", e);
    return null;
  }
}

// MediaRecorder MIME → file extension. The Rust allowlist accepts these
// verbatim; anything not in this map collapses to "webm" which the WebView
// always supports (Android Chrome default).
const AUDIO_EXT: Record<string, string> = {
  "audio/webm": "webm",
  "audio/webm;codecs=opus": "webm",
  "audio/ogg": "ogg",
  "audio/ogg;codecs=opus": "ogg",
  "audio/mp4": "m4a",
  "audio/mp4;codecs=mp4a.40.2": "m4a",
  "audio/aac": "aac",
  "audio/mpeg": "mp3",
  "audio/wav": "wav",
  "audio/x-wav": "wav",
};

/**
 * Persist a recorded audio Blob (from MediaRecorder) to `.attachments/`
 * via the same `save_attachment` IPC path used for images. Returns the
 * roost-relative filename for `![audio](.attachments/...)` insertion.
 *
 * The 25 MB cap enforced by the Rust side applies here too; a 30-min opus
 * recording at 64 kbps is ~14 MB, so it fits with margin.
 */
export async function saveAudio(
  roostPath: string | null,
  blob: Blob,
): Promise<string | null> {
  if (!roostPath) return null;
  try {
    const buffer = await blob.arrayBuffer();
    const data = bytesToBase64(new Uint8Array(buffer));
    // blob.type can include codec params (e.g. "audio/webm;codecs=opus").
    // Look up the full string first, then fall back to the bare MIME.
    const mime = blob.type || "audio/webm";
    const ext =
      AUDIO_EXT[mime] ?? AUDIO_EXT[mime.split(";")[0]] ?? "webm";
    const filename = await unwrap(commands.saveAttachment(roostPath, data, ext));
    // NEW-DB1: warn if this recording is too large to sync (a 30-min opus clip
    // is ~14 MB — over the 8 MiB sync blob limit but under the 25 MiB save cap).
    maybeWarnAttachmentSync(buffer.byteLength);
    return filename;
  } catch (e) {
    log.error("attachments", "failed to save audio recording", e);
    return null;
  }
}
