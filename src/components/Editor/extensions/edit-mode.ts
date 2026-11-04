/**
 * Edit-mode gate for live-preview-style extensions.
 *
 * By default the editor renders markdown with markers hidden — `##`, `**`,
 * `[[wiki-links]]`, tables, and task checkboxes all appear in their rendered
 * form. Single-clicking a line moves the cursor but does NOT reveal the
 * underlying source.
 *
 * To edit a line's raw markdown the user **double-clicks** it. That sets a
 * boolean flag in the editor state; extensions observe the flag and, while
 * it's on, reveal markers on whichever line holds the cursor. Pressing
 * Escape (or losing focus) turns the flag back off and everything re-renders.
 *
 * Other live-preview extensions should call `isEditMode(state)` and compute
 * their "active lines" set only when that returns true. The helper
 * `activeEditLines(state)` returns the set directly.
 */

import { keymap } from "@codemirror/view";
import {
  EditorState,
  StateEffect,
  StateField,
  type Extension,
} from "@codemirror/state";
import { EditorView } from "@codemirror/view";

export const setEditMode = StateEffect.define<boolean>();

const editModeField = StateField.define<boolean>({
  create() {
    return false;
  },
  update(value, tr) {
    for (const e of tr.effects) {
      if (e.is(setEditMode)) return e.value;
    }
    return value;
  },
});

/** True when the user has double-clicked into edit mode. */
export function isEditMode(state: EditorState): boolean {
  return state.field(editModeField, false) ?? false;
}

/**
 * Line numbers (1-based) that should show raw markdown markers. Empty when
 * edit mode is off — callers can plug this straight into existing
 * `activeLines` logic and the reveal disappears.
 */
export function activeEditLines(state: EditorState): Set<number> {
  const lines = new Set<number>();
  if (!isEditMode(state)) return lines;
  const doc = state.doc;
  for (const r of state.selection.ranges) {
    const fromLine = doc.lineAt(r.from).number;
    const toLine = doc.lineAt(r.to).number;
    for (let i = fromLine; i <= toLine; i++) lines.add(i);
  }
  return lines;
}

const events = EditorView.domEventHandlers({
  dblclick(_event, view) {
    if (!isEditMode(view.state)) {
      view.dispatch({ effects: setEditMode.of(true) });
    }
    return false;
  },
  blur(_event, view) {
    if (isEditMode(view.state)) {
      view.dispatch({ effects: setEditMode.of(false) });
    }
    return false;
  },
});

const escBinding = keymap.of([
  {
    key: "Escape",
    run(view) {
      if (!isEditMode(view.state)) return false;
      view.dispatch({ effects: setEditMode.of(false) });
      return true;
    },
  },
]);

export function editMode(): Extension {
  return [editModeField, events, escBinding];
}
