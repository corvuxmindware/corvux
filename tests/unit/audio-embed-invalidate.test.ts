/**
 * Item 2 — audio embeds re-render when a synced blob arrives.
 *
 * Mirrors the image-embed invalidation: a small (<cap) audio clip that
 * syncs after the note should flip "not synced yet" to the player without a
 * doc change. Covers loader-called-on-mount, re-invoke on matching
 * invalidation, and no-op for an unrelated path.
 */
import { describe, expect, it, vi, beforeEach, afterEach } from "vitest";
import { EditorState } from "@codemirror/state";
import { EditorView } from "@codemirror/view";
import {
  audioEmbed,
  invalidateAudioInEditors,
  type AudioLoader,
} from "../../src/components/Editor/extensions/audio-embed";

const realCreate = URL.createObjectURL;
const realRevoke = URL.revokeObjectURL;
beforeEach(() => {
  let n = 0;
  URL.createObjectURL = vi.fn(() => `blob:test-${n++}`);
  URL.revokeObjectURL = vi.fn();
});
afterEach(() => {
  URL.createObjectURL = realCreate;
  URL.revokeObjectURL = realRevoke;
});

function mountView(doc: string, loader: AudioLoader) {
  const host = document.createElement("div");
  document.body.appendChild(host);
  const view = new EditorView({
    state: EditorState.create({ doc, extensions: audioEmbed(loader) }),
    parent: host,
  });
  return { view, host };
}

const flush = async () => {
  await Promise.resolve();
  await Promise.resolve();
};

describe("audio-embed invalidation", () => {
  it("calls the loader for the clip path on mount", async () => {
    const loader = vi.fn<AudioLoader>(async () => null);
    const { view, host } = mountView(
      "![memo](.attachments/clip-1.webm)\n",
      loader,
    );
    await flush();
    expect(loader).toHaveBeenCalledWith(".attachments/clip-1.webm");
    view.destroy();
    host.remove();
  });

  it("re-invokes the loader when the path is invalidated", async () => {
    const loader = vi.fn<AudioLoader>(async () => null);
    const { view, host } = mountView(
      "![memo](.attachments/clip-2.webm)\n",
      loader,
    );
    await flush();
    const before = loader.mock.calls.length;

    invalidateAudioInEditors(".attachments/clip-2.webm");
    await flush();

    expect(loader.mock.calls.length).toBeGreaterThan(before);
    expect(loader).toHaveBeenLastCalledWith(".attachments/clip-2.webm");
    view.destroy();
    host.remove();
  });

  it("ignores invalidation for an unrelated path", async () => {
    const loader = vi.fn<AudioLoader>(async () => null);
    const { view, host } = mountView(
      "![memo](.attachments/clip-3.webm)\n",
      loader,
    );
    await flush();
    const before = loader.mock.calls.length;

    invalidateAudioInEditors(".attachments/other.webm");
    await flush();

    expect(loader.mock.calls.length).toBe(before);
    view.destroy();
    host.remove();
  });
});
