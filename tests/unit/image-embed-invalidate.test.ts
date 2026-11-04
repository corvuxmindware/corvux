/**
 * Phase 4 — re-decoration on synced-blob arrival.
 *
 * When an attachment blob arrives via sync, `invalidateImageInEditors(rel)`
 * must make the embed for that path re-run its loader (so a "not synced yet"
 * placeholder flips to the image) WITHOUT a doc change. Covers:
 *  - the loader is called for the embed's path on mount
 *  - invalidating that path re-invokes the loader (cache evicted + widget
 *    rebuilt via the generation bump)
 *  - invalidating an UNRELATED path does not re-invoke the loader
 */
import { describe, expect, it, vi } from "vitest";
import { EditorState } from "@codemirror/state";
import { EditorView } from "@codemirror/view";
import {
  imageEmbed,
  invalidateImageInEditors,
  type ImageLoader,
} from "../../src/components/Editor/extensions/image-embed";

function mountView(doc: string, loader: ImageLoader) {
  const host = document.createElement("div");
  document.body.appendChild(host);
  const view = new EditorView({
    state: EditorState.create({ doc, extensions: imageEmbed(loader) }),
    parent: host,
  });
  return { view, host };
}

const flush = async () => {
  await Promise.resolve();
  await Promise.resolve();
};

describe("image-embed invalidation", () => {
  it("calls the loader with the embed's path on mount", async () => {
    const loader = vi.fn<ImageLoader>(async () => null);
    const { view, host } = mountView(
      "![pic](.attachments/paste-1.webp)\n",
      loader,
    );
    await flush();
    expect(loader).toHaveBeenCalledWith(".attachments/paste-1.webp");
    view.destroy();
    host.remove();
  });

  it("re-invokes the loader when the path is invalidated (blob arrived)", async () => {
    const loader = vi.fn<ImageLoader>(async () => null);
    const { view, host } = mountView(
      "![pic](.attachments/paste-2.webp)\n",
      loader,
    );
    await flush();
    const callsBefore = loader.mock.calls.length;

    invalidateImageInEditors(".attachments/paste-2.webp");
    await flush();

    expect(loader.mock.calls.length).toBeGreaterThan(callsBefore);
    expect(loader).toHaveBeenLastCalledWith(".attachments/paste-2.webp");
    view.destroy();
    host.remove();
  });

  it("ignores invalidation for an unrelated path", async () => {
    const loader = vi.fn<ImageLoader>(async () => null);
    const { view, host } = mountView(
      "![pic](.attachments/paste-3.webp)\n",
      loader,
    );
    await flush();
    const callsBefore = loader.mock.calls.length;

    invalidateImageInEditors(".attachments/some-other-image.webp");
    await flush();

    expect(loader.mock.calls.length).toBe(callsBefore);
    view.destroy();
    host.remove();
  });
});
