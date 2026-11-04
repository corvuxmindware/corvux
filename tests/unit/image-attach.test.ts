/**
 * Tests for `insertImageFromFile` in `src/lib/image-attach.ts` — the
 * save+link+toast orchestration behind the mobile "Add image" button.
 *
 * The DOM file-picker (`pickImageFile`) and the roost guard
 * (`pickAndInsertImage`) are not unit-tested: the picker needs a real OS
 * dialog and the guard is a one-line `roostPath` check. We test the path
 * that actually moves bytes and links them, with `pasteImage`, the active-
 * note append, and the toast surface all mocked.
 */
import { describe, it, expect, vi, beforeEach } from "vitest";

const h = vi.hoisted(() => ({
  pasteImage: vi.fn(),
  appendAttachmentToActiveNote: vi.fn(),
  show: vi.fn(),
}));

vi.mock("../../src/lib/attachments", () => ({ pasteImage: h.pasteImage }));
vi.mock("../../src/lib/insert-attachment", () => ({
  appendAttachmentToActiveNote: h.appendAttachmentToActiveNote,
}));
vi.mock("../../src/components/Toast/Toast", () => ({ toastStore: { show: h.show } }));
vi.mock("../../src/stores/roost", () => ({ roostStore: { roostPath: () => "/roost" } }));

import { insertImageFromFile } from "../../src/lib/image-attach";

function makeFile(): File {
  return new File([new Uint8Array(8)], "photo.jpg", { type: "image/jpeg" });
}

function toastTypes(): string[] {
  return h.show.mock.calls.map((c) => c[1]);
}

beforeEach(() => {
  h.pasteImage.mockReset();
  h.appendAttachmentToActiveNote.mockReset();
  h.show.mockReset();
});

describe("insertImageFromFile", () => {
  it("saves the file and links it into the active note on success", async () => {
    h.pasteImage.mockResolvedValue("paste-9.jpg");
    h.appendAttachmentToActiveNote.mockReturnValue("ok");

    await insertImageFromFile("/roost", makeFile());

    expect(h.pasteImage).toHaveBeenCalledWith("/roost", expect.any(File));
    expect(h.appendAttachmentToActiveNote).toHaveBeenCalledWith("paste-9.jpg");
    // A success toast naming the saved file is shown.
    const success = h.show.mock.calls.find((c) => c[1] === "success");
    expect(success?.[0]).toContain("paste-9.jpg");
  });

  it("shows an error and does not link when saving fails", async () => {
    h.pasteImage.mockResolvedValue(null);

    await insertImageFromFile("/roost", makeFile());

    expect(h.appendAttachmentToActiveNote).not.toHaveBeenCalled();
    expect(toastTypes()).toContain("error");
  });

  it("warns (info) when the image saved but there is no note to link from", async () => {
    h.pasteImage.mockResolvedValue("paste-10.jpg");
    h.appendAttachmentToActiveNote.mockReturnValue("no-tab");

    await insertImageFromFile("/roost", makeFile());

    const info = h.show.mock.calls.find(
      (c) => c[1] === "info" && String(c[0]).includes("paste-10.jpg"),
    );
    expect(info).toBeTruthy();
    // No success toast on the no-tab path.
    expect(toastTypes()).not.toContain("success");
  });
});
