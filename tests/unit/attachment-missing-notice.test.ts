import { describe, it, expect } from "vitest";
import {
  attachmentMissingNotice,
  markAttachmentTooLarge,
} from "../../src/components/Editor/extensions/attachment-missing-notice";

describe("attachmentMissingNotice", () => {
  it("sync OFF → honest 'Missing' warning with the path", () => {
    const n = attachmentMissingNotice({
      syncOn: false,
      path: ".attachments/paste-123.png",
      kind: "image",
    });
    expect(n.variant).toBe("missing");
    expect(n.text).toContain("⚠ Missing");
    expect(n.text).toContain(".attachments/paste-123.png");
  });

  it("sync ON → 'not synced yet', NOT the alarming Missing and NOT the stale 'this version' lie", () => {
    const n = attachmentMissingNotice({
      syncOn: true,
      path: ".attachments/paste-123.png",
      kind: "image",
    });
    expect(n.variant).toBe("unsynced");
    expect(n.text).toContain("Not synced yet");
    expect(n.text).not.toContain("Missing");
    // Attachments DO sync now — must not claim otherwise.
    expect(n.text.toLowerCase()).not.toContain("don't sync");
    expect(n.text.toLowerCase()).not.toContain("this version");
  });

  it("sync ON, audio → same reassuring copy", () => {
    const n = attachmentMissingNotice({
      syncOn: true,
      path: ".attachments/recording-9.webm",
      kind: "audio",
    });
    expect(n.variant).toBe("unsynced");
    expect(n.text).toContain("Not synced yet");
    expect(n.text).not.toContain("Missing");
  });

  it("over-cap path → honest 'too large', regardless of kind", () => {
    markAttachmentTooLarge(".attachments/huge.gif");
    const n = attachmentMissingNotice({
      syncOn: true,
      path: ".attachments/huge.gif",
      kind: "image",
    });
    expect(n.variant).toBe("unsynced");
    expect(n.text.toLowerCase()).toContain("too large");
    expect(n.text).not.toContain("Not synced yet");
  });

  it("sync OFF, audio → still surfaces the missing path honestly", () => {
    const n = attachmentMissingNotice({
      syncOn: false,
      path: ".attachments/recording-9.webm",
      kind: "audio",
    });
    expect(n.variant).toBe("missing");
    expect(n.text).toContain(".attachments/recording-9.webm");
  });
});
