/**
 * `newNoteDestDir` / `relFolderLabel` — where a new note is created and how
 * that destination is labelled in the UI. Rule: the folder of the note
 * currently open, falling back to the roost root when nothing (or a
 * non-file tab like Graph) is open. Pure functions, no Tauri/DOM.
 */

import { describe, it, expect } from "vitest";
import { newNoteDestDir, relFolderLabel } from "../../src/lib/new-note-dest";

const ROOT = "/roost";

describe("newNoteDestDir", () => {
  it("uses the open note's folder", () => {
    expect(newNoteDestDir("/roost/Projects/Active/foo.md", ROOT)).toBe(
      "/roost/Projects/Active",
    );
  });

  it("returns root for a note directly under the roost root", () => {
    expect(newNoteDestDir("/roost/inbox.md", ROOT)).toBe("/roost");
  });

  it("falls back to root when no note is open", () => {
    expect(newNoteDestDir(null, ROOT)).toBe("/roost");
    expect(newNoteDestDir(undefined, ROOT)).toBe("/roost");
  });

  it("falls back to root for a sentinel tab (e.g. the Graph view)", () => {
    expect(newNoteDestDir("corvux://graph", ROOT)).toBe("/roost");
  });

  it("falls back to root for a path outside the roost", () => {
    expect(newNoteDestDir("/elsewhere/note.md", ROOT)).toBe("/roost");
  });

  it("tolerates a trailing slash on the roost root", () => {
    expect(newNoteDestDir("/roost/Journal/x.md", "/roost/")).toBe(
      "/roost/Journal",
    );
  });

  it("handles Windows-style backslash paths", () => {
    expect(newNoteDestDir("C:\\roost\\A\\b.md", "C:\\roost")).toBe(
      "C:\\roost\\A",
    );
  });
});

describe("relFolderLabel", () => {
  it("labels the roost root as 'Roost root'", () => {
    expect(relFolderLabel("/roost", ROOT)).toBe("Roost root");
    expect(relFolderLabel("/roost/", "/roost/")).toBe("Roost root");
  });

  it("shows the in-roost path for a subfolder", () => {
    expect(relFolderLabel("/roost/Projects/Active", ROOT)).toBe(
      "Projects/Active",
    );
  });

  it("normalises Windows separators for display", () => {
    expect(relFolderLabel("C:\\roost\\A\\sub", "C:\\roost")).toBe("A/sub");
  });
});
