/**
 * Tests for `src/lib/insert-attachment.ts`.
 *
 * `appendAttachmentMarkdown` is the pure spacing/link logic shared by the
 * image-attach and voice-memo flows — the bit most likely to silently drift,
 * so it's tested directly. `appendAttachmentToActiveNote` is thin store glue;
 * we mock `tabStore` to verify the active-note guard and the write path.
 */
import { describe, it, expect, vi, beforeEach } from "vitest";

const h = vi.hoisted(() => ({
  state: { path: "/roost/note.md" as string | null, content: "" },
  updateContent: vi.fn(),
}));

vi.mock("../../src/stores/tabs", () => ({
  tabStore: {
    activeTabPath: () => h.state.path,
    activeTab: () => (h.state.path ? { path: h.state.path, content: h.state.content } : undefined),
    updateContent: h.updateContent,
  },
}));

import {
  appendAttachmentMarkdown,
  appendAttachmentToActiveNote,
} from "../../src/lib/insert-attachment";

describe("appendAttachmentMarkdown", () => {
  it("embeds with no separator into empty content", () => {
    expect(appendAttachmentMarkdown("", "paste-1.png")).toBe(
      "![paste-1.png](.attachments/paste-1.png)\n",
    );
  });

  it("inserts a blank line when content has no trailing newline", () => {
    expect(appendAttachmentMarkdown("# Note", "p.png")).toBe(
      "# Note\n\n![p.png](.attachments/p.png)\n",
    );
  });

  it("does not add a blank line when content already ends in a newline", () => {
    expect(appendAttachmentMarkdown("line\n", "p.png")).toBe(
      "line\n![p.png](.attachments/p.png)\n",
    );
  });

  it("always terminates with a trailing newline", () => {
    expect(appendAttachmentMarkdown("a", "x.webp").endsWith("\n")).toBe(true);
  });
});

describe("appendAttachmentToActiveNote", () => {
  beforeEach(() => {
    h.updateContent.mockReset();
    h.state.path = "/roost/note.md";
    h.state.content = "hello";
  });

  it("appends to the active note and reports ok", () => {
    const result = appendAttachmentToActiveNote("pic.jpg");
    expect(result).toBe("ok");
    expect(h.updateContent).toHaveBeenCalledWith(
      "hello\n\n![pic.jpg](.attachments/pic.jpg)\n",
    );
  });

  it("returns no-tab and writes nothing when there is no active tab", () => {
    h.state.path = null;
    expect(appendAttachmentToActiveNote("pic.jpg")).toBe("no-tab");
    expect(h.updateContent).not.toHaveBeenCalled();
  });

  it("returns no-tab for non-file tabs (corvux:// scheme, e.g. graph view)", () => {
    h.state.path = "corvux://graph";
    expect(appendAttachmentToActiveNote("pic.jpg")).toBe("no-tab");
    expect(h.updateContent).not.toHaveBeenCalled();
  });
});
