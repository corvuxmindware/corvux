/**
 * Markdown formatting commands for the mobile editor toolbar.
 *
 * Each operation is split into:
 *   - a PURE `compute*` function taking a doc string + selection and
 *     returning CodeMirror change specs (testable without a DOM — same
 *     pattern as `table-render`'s `computeCellChanges`), and
 *   - a thin wrapper that dispatches the spec into a live EditorView
 *     and refocuses it.
 *
 * Every wrapper ends with `view.focus()`. On Android (`interactive-widget=
 * overlays-content`) tapping a toolbar button blurs the editor and the
 * WebView dismisses the on-screen keyboard on the next frame — which
 * defeats the whole feature. Refocusing on each tap keeps the keyboard up.
 */

import type { EditorView } from "@codemirror/view";
import type { ChangeSpec } from "@codemirror/state";

export interface CommandSpec {
  changes: ChangeSpec[];
  /** Selection anchor/head in the NEW doc, in absolute char offsets.
   *  Omit to let CodeMirror map the existing selection through the
   *  changes. */
  selection?: { anchor: number; head?: number };
}

/** Per-line start offset and text. Line 0 starts at 0; line N+1 starts
 *  immediately after line N's trailing "\n". */
interface LineSpan {
  start: number;
  text: string;
}

function lineSpans(source: string): LineSpan[] {
  const out: LineSpan[] = [];
  let start = 0;
  for (const text of source.split("\n")) {
    out.push({ start, text });
    start += text.length + 1; // +1 for the "\n"
  }
  return out;
}

/** 0-based index of the line containing `pos`. Clamped to the doc. */
function lineIndexAt(source: string, pos: number): number {
  const clamped = Math.max(0, Math.min(pos, source.length));
  let line = 0;
  for (let i = 0; i < clamped; i++) {
    if (source.charCodeAt(i) === 10) line++; // "\n"
  }
  return line;
}

/**
 * Wrap the selection with `open` and `close`. Empty selection → caret
 * lands between the two markers; non-empty → the original text stays
 * selected inside the markers.
 */
export function computeWrap(
  source: string,
  from: number,
  to: number,
  open: string,
  close: string = open,
): CommandSpec {
  const selected = source.slice(from, to);
  const empty = from === to;
  const insert = open + selected + close;
  return {
    changes: [{ from, to, insert }],
    selection: empty
      ? { anchor: from + open.length }
      : {
          anchor: from + open.length,
          head: from + open.length + selected.length,
        },
  };
}

/**
 * Cycle heading level on the line(s) touched by the selection. The
 * FIRST selected line's current level is the reference; the next level
 * is applied uniformly to every line.
 *
 * Cycle: plain → `#` → `##` → `###` → `####` → plain. An existing
 * `#####` or `######` is treated as level 4 for cycling purposes
 * (treated as "already past max"), so the next tap clears it.
 */
export function computeHeadingCycle(
  source: string,
  from: number,
  to: number,
  maxLevel = 4,
): CommandSpec {
  const lines = lineSpans(source);
  const startLine = lineIndexAt(source, from);
  const endLine = lineIndexAt(source, to);

  const first = lines[startLine];
  const m = first ? first.text.match(/^(#{1,6})\s/) : null;
  const currentLevel = m ? m[1].length : 0;
  const nextLevel = currentLevel >= maxLevel ? 0 : currentLevel + 1;

  const changes: ChangeSpec[] = [];
  for (let n = startLine; n <= endLine; n++) {
    const line = lines[n];
    if (!line) continue;
    const existing = line.text.match(/^(#{1,6})\s+/);
    const stripLen = existing ? existing[0].length : 0;
    const insert = nextLevel === 0 ? "" : "#".repeat(nextLevel) + " ";
    if (stripLen !== 0 || insert !== "") {
      changes.push({ from: line.start, to: line.start + stripLen, insert });
    }
  }
  return { changes };
}

/**
 * Toggle a per-line prefix (`- `, `> `). When EVERY selected line
 * already starts with the prefix, it's stripped from all; otherwise
 * it's added to every line that lacks it. Matches Obsidian's toggle
 * semantics for quote and bullet on multi-line selections.
 */
export function computeLinePrefixToggle(
  source: string,
  from: number,
  to: number,
  prefix: string,
): CommandSpec {
  const lines = lineSpans(source);
  const startLine = lineIndexAt(source, from);
  const endLine = lineIndexAt(source, to);

  const selected = lines.slice(startLine, endLine + 1).filter(Boolean);
  const allHave =
    selected.length > 0 && selected.every((l) => l.text.startsWith(prefix));

  const changes: ChangeSpec[] = [];
  for (let n = startLine; n <= endLine; n++) {
    const line = lines[n];
    if (!line) continue;
    if (allHave) {
      changes.push({ from: line.start, to: line.start + prefix.length, insert: "" });
    } else if (!line.text.startsWith(prefix)) {
      changes.push({ from: line.start, to: line.start, insert: prefix });
    }
  }
  return { changes };
}

/**
 * Toggle checkbox state on each selected line, independently:
 *   - plain line           → `- [ ] ` (unchecked)
 *   - bare `- `/`* `/`+ `  → same marker, with `[ ] ` after the space
 *   - unchecked `[ ]`      → `[x]` (checked)
 *   - checked `[x]`/`[X]`  → `[ ]` (unchecked)
 */
export function computeCheckboxToggle(
  source: string,
  from: number,
  to: number,
): CommandSpec {
  const lines = lineSpans(source);
  const startLine = lineIndexAt(source, from);
  const endLine = lineIndexAt(source, to);

  const changes: ChangeSpec[] = [];
  for (let n = startLine; n <= endLine; n++) {
    const line = lines[n];
    if (!line) continue;
    const text = line.text;

    // Existing checkbox — flip the mark char. The trailing `\s*`
    // (not `\s+`) is deliberate: a line that is *exactly* `- [ ]` with
    // no trailing content/space is still a checkbox and should toggle,
    // not get re-promoted to `- [ ] [ ]` via the bare-list-item branch.
    const cb = text.match(/^([-*+])\s+\[([ xX])\]\s*/);
    if (cb) {
      const isChecked = cb[2] !== " ";
      const markPos = line.start + cb[0].indexOf("[") + 1;
      changes.push({
        from: markPos,
        to: markPos + 1,
        insert: isChecked ? " " : "x",
      });
      continue;
    }
    // Bare list item — promote to checkbox by inserting `[ ] ` after the marker.
    const list = text.match(/^([-*+])\s+/);
    if (list) {
      const insertAt = line.start + list[0].length;
      changes.push({ from: insertAt, to: insertAt, insert: "[ ] " });
      continue;
    }
    // Plain line — add a full unchecked checkbox marker.
    changes.push({ from: line.start, to: line.start, insert: "- [ ] " });
  }
  return { changes };
}

/**
 * Insert `text` at the cursor, replacing any selection. When the
 * selection is empty and `caretOffset` is provided, the caret lands
 * that many chars into the inserted text — used by `[[|]]`-style
 * snippets so the user can keep typing the note name.
 */
export function computeInsert(
  _source: string,
  from: number,
  to: number,
  text: string,
  caretOffset?: number,
): CommandSpec {
  const empty = from === to;
  return {
    changes: [{ from, to, insert: text }],
    selection:
      empty && caretOffset !== undefined
        ? { anchor: from + caretOffset }
        : undefined,
  };
}

// ── View wrappers ─────────────────────────────────────────────────
// Translate EditorView → inputs, run the pure compute, dispatch, and
// refocus so the on-screen keyboard stays open.

function applySpec(view: EditorView, spec: CommandSpec): void {
  view.dispatch({
    changes: spec.changes,
    selection: spec.selection,
  });
  view.focus();
}

function mainRange(view: EditorView): { from: number; to: number } {
  const r = view.state.selection.main;
  return { from: r.from, to: r.to };
}

export function wrapSelection(
  view: EditorView,
  open: string,
  close: string = open,
): void {
  const { from, to } = mainRange(view);
  applySpec(view, computeWrap(view.state.doc.toString(), from, to, open, close));
}

export function cycleHeading(view: EditorView, maxLevel = 4): void {
  const { from, to } = mainRange(view);
  applySpec(view, computeHeadingCycle(view.state.doc.toString(), from, to, maxLevel));
}

export function toggleLinePrefix(view: EditorView, prefix: string): void {
  const { from, to } = mainRange(view);
  applySpec(view, computeLinePrefixToggle(view.state.doc.toString(), from, to, prefix));
}

export function toggleCheckbox(view: EditorView): void {
  const { from, to } = mainRange(view);
  applySpec(view, computeCheckboxToggle(view.state.doc.toString(), from, to));
}

export function insertAtCursor(
  view: EditorView,
  text: string,
  caretOffset?: number,
): void {
  const { from, to } = mainRange(view);
  applySpec(view, computeInsert(view.state.doc.toString(), from, to, text, caretOffset));
}
