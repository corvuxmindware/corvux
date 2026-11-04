/**
 * Regression: GFM task checkboxes must toggle in read-only ("reading") mode,
 * not only in edit mode.
 *
 * The bug: the live-preview checkbox widget's click handler bailed early with
 * `if (view.state.readOnly) return;`, so pinning a tab read-only made every
 * inline checkbox inert. Ticking a task is a deliberate one-character action
 * (Obsidian parity) — reading mode should still allow it; only free-text
 * typing is blocked (via `EditorView.editable.of(false)`). The toggle is a
 * programmatic `dispatch`, which `EditorState.readOnly` does NOT block, so
 * removing the guard makes the change apply in both modes.
 *
 * This mounts the real `livePreview()` extension over a GFM markdown document
 * and clicks the rendered checkbox DOM node, asserting the underlying
 * `[ ]`/`[x]` marker flips.
 */
import { describe, expect, it } from "vitest";
import { EditorState } from "@codemirror/state";
import { EditorView } from "@codemirror/view";
import { forceParsing } from "@codemirror/language";
import { markdown, markdownLanguage } from "@codemirror/lang-markdown";
import { livePreview } from "../../src/components/Editor/extensions/live-preview";
import { editMode } from "../../src/components/Editor/extensions/edit-mode";

function mount(doc: string, readOnly: boolean) {
  const host = document.createElement("div");
  document.body.appendChild(host);
  const view = new EditorView({
    state: EditorState.create({
      doc,
      extensions: [
        markdown({ base: markdownLanguage }),
        editMode(),
        livePreview(),
        EditorState.readOnly.of(readOnly),
        EditorView.editable.of(!readOnly),
      ],
    }),
    parent: host,
  });
  // Force the GFM parse to completion so `TaskMarker` nodes exist, then a
  // selection-only transaction (allowed under readOnly) so the live-preview
  // ViewPlugin rebuilds its decorations against the finished syntax tree.
  forceParsing(view, view.state.doc.length, 10_000);
  view.dispatch({ selection: { anchor: 0 } });
  return { view, host };
}

const flush = async () => {
  await Promise.resolve();
  await Promise.resolve();
};

function checkbox(host: HTMLElement): HTMLInputElement | null {
  return host.querySelector<HTMLInputElement>(".cm-md-task-checkbox");
}

describe("read-only task checkbox toggle", () => {
  it("ticks an unchecked task in read-only mode", async () => {
    const { view, host } = mount("- [ ] write tests\n", true);
    await flush();

    const box = checkbox(host);
    expect(box, "checkbox widget should render in read-only mode").not.toBeNull();

    box!.click();
    expect(view.state.doc.toString()).toBe("- [x] write tests\n");

    view.destroy();
    host.remove();
  });

  it("un-ticks a checked task in read-only mode", async () => {
    const { view, host } = mount("- [x] ship it\n", true);
    await flush();

    checkbox(host)!.click();
    expect(view.state.doc.toString()).toBe("- [ ] ship it\n");

    view.destroy();
    host.remove();
  });

  it("still toggles when not read-only (the unlocked path is unbroken)", async () => {
    const { view, host } = mount("- [ ] edit path\n", false);
    await flush();

    checkbox(host)!.click();
    expect(view.state.doc.toString()).toBe("- [x] edit path\n");

    view.destroy();
    host.remove();
  });
});
