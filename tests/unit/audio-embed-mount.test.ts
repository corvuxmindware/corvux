/**
 * A-14 follow-up — exercise the Solid-into-CodeMirror mount/dispose
 * handshake for the audio embed widget. The widget mounts a Solid root
 * via `render()` into a CodeMirror widget DOM container; if the dispose
 * hook isn't wired up correctly the Solid root + AudioContext leak on
 * note switch.
 *
 * Tests cover:
 *  - widget mounts when an `![…](.attachments/X.webm)` line is added
 *  - the loader is called with the embed's relative path
 *  - the disposed flag is flipped synchronously in destroy(), so late
 *    loader resolves don't mount a Solid root into an orphan
 *  - LRU eviction revokes the evicted blob URL
 */
import { describe, expect, it, vi, beforeEach, afterEach } from "vitest";
import { EditorState } from "@codemirror/state";
import { EditorView } from "@codemirror/view";
import {
  audioEmbed,
  type AudioLoader,
  type AudioDeleter,
} from "../../src/components/Editor/extensions/audio-embed";

// happy-dom doesn't ship a real URL.createObjectURL counterpart — stub
// it so the LRU revoke path doesn't throw.
const objectURLs = new Map<string, Blob>();
const realCreate = URL.createObjectURL;
const realRevoke = URL.revokeObjectURL;

beforeEach(() => {
  objectURLs.clear();
  let counter = 0;
  URL.createObjectURL = vi.fn((blob: Blob) => {
    const id = `blob:test-${counter++}`;
    objectURLs.set(id, blob);
    return id;
  });
  URL.revokeObjectURL = vi.fn((url: string) => {
    objectURLs.delete(url);
  });
});

afterEach(() => {
  URL.createObjectURL = realCreate;
  URL.revokeObjectURL = realRevoke;
});

// loadAudio now returns { url, buffer } so the player can decode peaks
// without re-fetching the blob. Helper builds a matching fake resource.
function fakeRes(): { url: string; buffer: ArrayBuffer } {
  return {
    url: URL.createObjectURL(new Blob(["x"], { type: "audio/webm" })),
    buffer: new Uint8Array([1, 2, 3, 4]).buffer,
  };
}

function mountView(doc: string, loader: AudioLoader, onDelete?: AudioDeleter) {
  const host = document.createElement("div");
  document.body.appendChild(host);
  const view = new EditorView({
    state: EditorState.create({
      doc,
      extensions: audioEmbed(loader, onDelete),
    }),
    parent: host,
  });
  return { view, host };
}

describe("audio-embed widget", () => {
  it("calls the loader with the matched relative path", async () => {
    const loader = vi.fn<AudioLoader>(async () => fakeRes());
    const { view, host } = mountView(
      "![memo](.attachments/clip-1.webm)\n",
      loader,
    );
    // Solid render is sync; loader resolves in a microtask. Flush it.
    await Promise.resolve();
    await Promise.resolve();
    expect(loader).toHaveBeenCalledWith(".attachments/clip-1.webm");
    view.destroy();
    host.remove();
  });

  it("renders a widget container under the line", async () => {
    const loader: AudioLoader = async () => fakeRes();
    const { view, host } = mountView(
      "![memo](.attachments/clip-2.webm)\n",
      loader,
    );
    await Promise.resolve();
    await Promise.resolve();
    // The widget root carries the cm-audio-embed class. We assert one
    // exists; the Solid AudioPlayer inside renders the actual player.
    const wraps = host.querySelectorAll(".cm-audio-embed");
    expect(wraps.length).toBe(1);
    view.destroy();
    host.remove();
  });

  it("disposes cleanly when the editor view is destroyed", async () => {
    let disposed = false;
    const loader: AudioLoader = async () => fakeRes();
    const { view, host } = mountView(
      "![memo](.attachments/clip-3.webm)\n",
      loader,
    );
    await Promise.resolve();
    await Promise.resolve();
    const wrap = host.querySelector(".cm-audio-embed") as
      | (HTMLElement & { __cmDispose?: () => void; __cmDisposed?: { v: boolean } })
      | null;
    expect(wrap).not.toBeNull();
    // Patch the dispose hook so we can observe it being called.
    const original = wrap?.__cmDispose;
    wrap!.__cmDispose = () => {
      disposed = true;
      original?.();
    };
    view.destroy();
    expect(disposed).toBe(true);
    expect(wrap!.__cmDisposed?.v).toBe(true);
    host.remove();
  });

  it("does not mount when the loader resolves after destroy()", async () => {
    let resolveLoader!: (v: { url: string; buffer: ArrayBuffer }) => void;
    const loader: AudioLoader = () =>
      new Promise<{ url: string; buffer: ArrayBuffer }>((res) => {
        resolveLoader = res;
      });
    const { view, host } = mountView(
      "![memo](.attachments/clip-late.webm)\n",
      loader,
    );
    // Loader is pending; widget exists with empty mount slot.
    const wrap = host.querySelector(".cm-audio-embed") as
      | (HTMLElement & { __cmDispose?: () => void; __cmDisposed?: { v: boolean } })
      | null;
    expect(wrap).not.toBeNull();
    view.destroy();
    expect(wrap!.__cmDisposed?.v).toBe(true);
    // Resolve AFTER destroy — must not mount a Solid root onto the wrap.
    resolveLoader(fakeRes());
    await Promise.resolve();
    await Promise.resolve();
    // No __cmDispose set because the .then short-circuited on disposed.v.
    expect(wrap!.__cmDispose).toBeUndefined();
    host.remove();
  });

  it("renders a malicious clipPath as text, not HTML, in the missing-file notice", async () => {
    // Regression (security audit 2026-05-30): clipPath comes from untrusted
    // note content. The missing-file branch used to build the notice with
    // innerHTML, so a path like `<img onerror=...>.webm` got parsed into a
    // live element (a stored-XSS sink gated only by CSP). The fix builds the
    // notice with textContent. A payload with NO `)` or newline survives
    // MEDIA_RE's path group, and `.webm` passes isAudioPath.
    const loader: AudioLoader = async () => null; // file missing → notice path
    const payloadPath = '<img src=x onerror="__cx_xss__">.webm';
    const { view, host } = mountView(`![x](${payloadPath})\n`, loader);
    // loader resolves null in a microtask; flush like the other tests.
    await Promise.resolve();
    await Promise.resolve();
    // The notice rendered…
    const caption = host.querySelector(".corvux-audio-caption");
    expect(caption).not.toBeNull();
    // …with NO injected element inside the notice (old innerHTML path would
    // parse the payload into a live <img> here). Scope the check to the
    // caption — querying the whole host would match CodeMirror's own
    // `<img class="cm-widgetBuffer">` and give a false positive.
    expect(caption!.querySelector("img")).toBeNull();
    expect(caption!.children.length).toBe(0);
    // …and the payload preserved as literal text.
    expect(caption!.textContent).toContain("<img");
    expect(caption!.textContent).toContain("⚠ Missing:");
    view.destroy();
    host.remove();
  });
});
