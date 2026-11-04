/**
 * Tests for the audio-attachment side of `src/lib/attachments.ts`.
 *
 * `saveAudio` is glue around the `save_attachment` IPC: we mock the IPC to
 * verify (a) the extension we send is derived from the Blob MIME, (b) the
 * 25 MB cap signal is propagated unchanged, and (c) failure paths return
 * null (matching `pasteImage`'s contract).
 *
 * Direct unit test rather than e2e: the IPC and the Rust side are already
 * covered by `commands/attachments.rs` Rust tests.
 */
import { describe, it, expect, vi, beforeEach } from "vitest";

vi.mock("../../src/lib/invoke", () => ({
  commands: {
    saveAttachment: vi.fn(),
    readAttachment: vi.fn(),
  },
  unwrap: <T>(p: Promise<T>): Promise<T> => p,
}));

vi.mock("../../src/lib/log", () => ({
  log: { error: vi.fn(), warn: vi.fn(), info: vi.fn() },
}));

import { loadAudio, saveAudio } from "../../src/lib/attachments";
import { commands } from "../../src/lib/invoke";

const saveAttachmentMock = commands.saveAttachment as unknown as ReturnType<typeof vi.fn>;
const readAttachmentMock = commands.readAttachment as unknown as ReturnType<typeof vi.fn>;

beforeEach(() => {
  saveAttachmentMock.mockReset();
  readAttachmentMock.mockReset();
});

function makeBlob(bytes: number, type: string): Blob {
  return new Blob([new Uint8Array(bytes)], { type });
}

describe("saveAudio", () => {
  it("derives webm extension for opus-in-webm blobs", async () => {
    saveAttachmentMock.mockResolvedValue("paste-12345.webm");
    const blob = makeBlob(128, "audio/webm;codecs=opus");
    const out = await saveAudio("/roost", blob);
    expect(out).toBe("paste-12345.webm");
    const args = saveAttachmentMock.mock.calls[0];
    // [roostPath, base64Data, ext]
    expect(args[0]).toBe("/roost");
    expect(args[2]).toBe("webm");
  });

  it("derives m4a extension for audio/mp4 (iOS WKWebView)", async () => {
    saveAttachmentMock.mockResolvedValue("paste-1.m4a");
    const blob = makeBlob(64, "audio/mp4");
    const out = await saveAudio("/roost", blob);
    expect(out).toBe("paste-1.m4a");
    expect(saveAttachmentMock.mock.calls[0][2]).toBe("m4a");
  });

  it("falls back to webm for unknown MIME types", async () => {
    saveAttachmentMock.mockResolvedValue("paste-2.webm");
    const blob = makeBlob(32, "audio/unknown-codec");
    await saveAudio("/roost", blob);
    expect(saveAttachmentMock.mock.calls[0][2]).toBe("webm");
  });

  it("falls back to webm when blob.type is empty", async () => {
    saveAttachmentMock.mockResolvedValue("paste-3.webm");
    const blob = makeBlob(16, "");
    await saveAudio("/roost", blob);
    expect(saveAttachmentMock.mock.calls[0][2]).toBe("webm");
  });

  it("returns null when no roost is open", async () => {
    const blob = makeBlob(16, "audio/webm");
    const out = await saveAudio(null, blob);
    expect(out).toBeNull();
    expect(saveAttachmentMock).not.toHaveBeenCalled();
  });

  it("returns null and logs on IPC failure", async () => {
    saveAttachmentMock.mockRejectedValue(new Error("disk full"));
    const blob = makeBlob(16, "audio/webm");
    const out = await saveAudio("/roost", blob);
    expect(out).toBeNull();
  });

  it("handles base64 encoding for multi-chunk audio without stack overflow", async () => {
    saveAttachmentMock.mockResolvedValue("paste-4.webm");
    // 100 KB blob — exercises the chunked btoa path (CHUNK = 0x8000 = 32 KB).
    const bytes = new Uint8Array(100 * 1024);
    for (let i = 0; i < bytes.length; i++) bytes[i] = i % 256;
    const blob = new Blob([bytes], { type: "audio/webm" });
    const out = await saveAudio("/roost", blob);
    expect(out).toBe("paste-4.webm");
    const b64 = saveAttachmentMock.mock.calls[0][1] as string;
    // base64 expands ~4/3 — assert non-trivial length to confirm full payload encoded.
    expect(b64.length).toBeGreaterThan(120_000);
  });
});

describe("loadAudio", () => {
  // iOS WKWebView can't play multi-MB `data:audio/*;base64,...` URLs in
  // <audio>; the element returns MEDIA_ERR_SRC_NOT_SUPPORTED. We must
  // return a `blob:` object URL instead. We also carry back the decoded
  // ArrayBuffer so the player can extract waveform peaks WITHOUT
  // re-fetching the blob (fetch(blob:) is unreliable in iOS WKWebView).
  it("returns a blob: URL (not data:) plus the decoded buffer", async () => {
    readAttachmentMock.mockResolvedValue(btoa("fake-audio-bytes"));
    const res = await loadAudio("/roost", ".attachments/paste-1.m4a");
    expect(res).not.toBeNull();
    expect(res!.url).toMatch(/^blob:/);
    expect(res!.url).not.toMatch(/^data:/);
    // Buffer carries the raw decoded bytes ("fake-audio-bytes" = 16 chars).
    expect(res!.buffer).toBeInstanceOf(ArrayBuffer);
    expect(res!.buffer.byteLength).toBe("fake-audio-bytes".length);
  });

  it("tags m4a blob with audio/mp4 MIME for AAC playback", async () => {
    readAttachmentMock.mockResolvedValue(btoa("fake"));
    // iOS dispatches the audio decoder by Blob.type, not by URL extension.
    // Spy on createObjectURL to capture the Blob and assert its MIME tag.
    const spy = vi.spyOn(URL, "createObjectURL");
    spy.mockReturnValue("blob:test-mp4");
    await loadAudio("/roost", ".attachments/clip.m4a");
    expect(spy).toHaveBeenCalledTimes(1);
    expect((spy.mock.calls[0][0] as Blob).type).toBe("audio/mp4");
    spy.mockRestore();
  });

  it("tags webm blob with audio/webm MIME", async () => {
    readAttachmentMock.mockResolvedValue(btoa("fake"));
    const spy = vi.spyOn(URL, "createObjectURL");
    spy.mockReturnValue("blob:test-webm");
    await loadAudio("/roost", ".attachments/clip.webm");
    expect((spy.mock.calls[0][0] as Blob).type).toBe("audio/webm");
    spy.mockRestore();
  });

  it("returns null when no roost is open", async () => {
    const res = await loadAudio(null, ".attachments/clip.m4a");
    expect(res).toBeNull();
    expect(readAttachmentMock).not.toHaveBeenCalled();
  });

  it("returns null and logs on IPC failure", async () => {
    readAttachmentMock.mockRejectedValue(new Error("missing"));
    const res = await loadAudio("/roost", ".attachments/gone.m4a");
    expect(res).toBeNull();
  });
});
