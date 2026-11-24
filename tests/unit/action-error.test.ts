/**
 * Tests for `src/lib/action-error.ts` — `reportActionError` / `scrubPaths`.
 *
 * Contract: a failed user gesture is logged AND shown. The visible line is
 * short by default; `showReason` appends the backend's own sentence (for the
 * curated "…cannot move items to the trash" style messages) with provider
 * paths scrubbed. The raw error always stays in the toast details.
 */

import { beforeEach, describe, expect, it } from "vitest";
import { reportActionError, scrubPaths } from "../../src/lib/action-error";
import { toastStore } from "../../src/components/Toast/Toast";
import { log } from "../../src/lib/log";

const NO_TRASH =
  "This storage location cannot move items to the trash, so nothing was deleted. Delete it with your file manager instead.";

function lastToast() {
  const all = toastStore.toasts();
  return all[all.length - 1];
}

beforeEach(() => {
  for (const t of toastStore.toasts()) toastStore.dismiss(t.id);
  log.reset();
});

describe("reportActionError", () => {
  it("shows an error toast with code, module and the raw error in details", () => {
    reportActionError("Failed to delete note", new Error("boom"), {
      code: "NOTE_DELETE_FAIL",
      module: "sidebar",
    });
    const t = lastToast();
    expect(t.type).toBe("error");
    expect(t.message).toBe("Failed to delete note");
    expect(t.details?.code).toBe("NOTE_DELETE_FAIL");
    expect(t.details?.module).toBe("sidebar");
    expect(t.details?.error).toBe("boom");
  });

  it("also logs the failure, so the log and the toast cannot drift apart", () => {
    reportActionError("Failed to duplicate", "disk full", { code: "X", module: "sidebar" });
    const entry = log.drain().find((e) => e.scope === "sidebar");
    expect(entry?.level).toBe("error");
    expect(entry?.message).toBe("Failed to duplicate");
  });

  it("keeps the visible line short unless showReason is set", () => {
    reportActionError("Failed to delete note", NO_TRASH, { code: "X", module: "sidebar" });
    expect(lastToast().message).toBe("Failed to delete note");
  });

  it("with showReason, puts the backend's actionable sentence in the visible line", () => {
    reportActionError("Failed to delete note", NO_TRASH, {
      code: "NOTE_DELETE_FAIL",
      module: "sidebar",
      showReason: true,
    });
    const msg = lastToast().message;
    expect(msg).toContain("cannot move items to the trash, so nothing was deleted");
    expect(msg.startsWith("Failed to delete note: ")).toBe(true);
  });

  it("scrubs provider paths from the visible line but keeps them in details", () => {
    const raw =
      "Already exists /storage/emulated/0/Documents/corvux-saf-lab/a.md content://com.android.externalstorage.documents/tree/primary%3ADocuments";
    reportActionError("Failed to create note", raw, {
      code: "X",
      module: "sidebar",
      showReason: true,
    });
    const t = lastToast();
    expect(t.message).not.toContain("/storage/");
    expect(t.message).not.toContain("content://");
    expect(t.message).toContain("<path>");
    // The copyable details keep the raw text for bug reports.
    expect(t.details?.error).toContain("/storage/emulated/0");
  });

  it("never throws, whatever was thrown", () => {
    for (const v of [undefined, null, 42, {}, "s", new Error("e")]) {
      expect(() =>
        reportActionError("Failed", v, { code: "X", module: "m", showReason: true }),
      ).not.toThrow();
    }
  });
});

describe("scrubPaths", () => {
  it("replaces Android storage paths, content URIs and Windows paths", () => {
    expect(scrubPaths("x /storage/emulated/0/a/b.md y")).toBe("x <path> y");
    expect(scrubPaths("x content://a.b/tree/c y")).toBe("x <path> y");
    expect(scrubPaths("x C:\\Users\\me\\n.md y")).toBe("x <path> y");
  });

  it("leaves ordinary text and relative note paths alone", () => {
    expect(scrubPaths("Nothing was moved.")).toBe("Nothing was moved.");
    expect(scrubPaths("Proj/sub: Directory listing incomplete")).toBe(
      "Proj/sub: Directory listing incomplete",
    );
  });
});
