/**
 * Obsidian-style "Live Preview" for markdown.
 *
 * The editor stays a text editor — you're still typing into CodeMirror, cursor
 * positions still land on real characters. What this extension adds is a layer
 * of *decorations* that make the source look like rendered prose:
 *
 *   - Headings (`# Title`, `## Sub`) get scaled font-size via a line class.
 *   - `**bold**`, `*italic*`, `~~strike~~`, `` `code` `` pick up weight/style.
 *   - Marker characters (`#`, `**`, `*`, `~~`, `` ` ``, `>`) disappear when the
 *     cursor is on a different line. Click into a heading and the `#` snaps
 *     back so you can edit — move away and it hides again.
 *   - List dashes (`- `) are swapped for a bullet glyph off-line.
 *   - Blockquotes and fenced code blocks get line-level classes for styling.
 *
 * Cursor-line markers stay visible on purpose: editing markdown only works if
 * you can still see what you're editing. A caret-aware reveal is the standard
 * compromise (Obsidian does the same thing).
 *
 * Implementation: a ViewPlugin that walks `syntaxTree()` over the current
 * viewport on doc/selection/viewport changes. Line decorations here are safe
 * from a ViewPlugin (they don't affect layout the way block decorations do).
 */

import { syntaxTree } from "@codemirror/language";
import {
  Decoration,
  DecorationSet,
  EditorView,
  ViewPlugin,
  ViewUpdate,
  WidgetType,
} from "@codemirror/view";
import { activeEditLines, setEditMode } from "./edit-mode";

class BulletWidget extends WidgetType {
  eq(): boolean {
    return true;
  }
  toDOM(): HTMLElement {
    const span = document.createElement("span");
    span.className = "cm-md-bullet";
    span.textContent = "\u2022"; // •
    return span;
  }
  ignoreEvent(): boolean {
    return false;
  }
}

const bulletDeco = Decoration.replace({ widget: new BulletWidget() });
const hideDeco = Decoration.replace({});

const headingLine = [1, 2, 3, 4, 5, 6].map((n) =>
  Decoration.line({ class: `cm-md-h${n}` }),
);
const blankLine = Decoration.line({ class: "cm-md-blank" });
const blockquoteLine = Decoration.line({ class: "cm-md-blockquote" });
const codeBlockLine = Decoration.line({ class: "cm-md-codeblock" });
const hrLine = Decoration.line({ class: "cm-md-hr" });

const strongMark = Decoration.mark({ class: "cm-md-strong" });
const emphasisMark = Decoration.mark({ class: "cm-md-emphasis" });
const strikeMark = Decoration.mark({ class: "cm-md-strike" });
const inlineCodeMark = Decoration.mark({ class: "cm-md-inline-code" });
const taskDoneMark = Decoration.mark({ class: "cm-md-task-done" });
const taskDoneLine = Decoration.line({ class: "cm-md-task-done-line" });

/**
 * GFM task checkbox rendered in place of `[x]` / `[ ]`. Click toggles the
 * underlying `x`/space character, which round-trips to disk unchanged as
 * plain markdown — no hidden state.
 *
 * Works in read-only ("reading") mode too, matching Obsidian: ticking a task
 * is a deliberate single-character action, not free-text editing. The toggle
 * is a programmatic `dispatch`, which `EditorState.readOnly` does not block
 * (readOnly only suppresses user keyboard input via `editable.of(false)`),
 * so the change still applies and autosave (not read-only-gated) persists it.
 */
class TaskCheckboxWidget extends WidgetType {
  constructor(
    private checked: boolean,
    private markerFrom: number,
  ) {
    super();
  }
  eq(other: TaskCheckboxWidget): boolean {
    return (
      other.checked === this.checked && other.markerFrom === this.markerFrom
    );
  }
  toDOM(view: EditorView): HTMLElement {
    const box = document.createElement("input");
    box.type = "checkbox";
    box.checked = this.checked;
    box.className = "cm-md-task-checkbox";
    box.setAttribute(
      "aria-label",
      this.checked ? "Mark task incomplete" : "Mark task complete",
    );
    box.addEventListener("mousedown", (e) => e.stopPropagation());
    box.addEventListener("click", (e) => {
      e.preventDefault();
      e.stopPropagation();
      // Marker is 3 chars: `[`, state, `]`. Flip the middle char.
      const stateFrom = this.markerFrom + 1;
      const cur = view.state.doc.sliceString(stateFrom, stateFrom + 1);
      const next = /x/i.test(cur) ? " " : "x";
      view.dispatch({
        changes: { from: stateFrom, to: stateFrom + 1, insert: next },
      });
    });
    return box;
  }
  ignoreEvent(event: Event): boolean {
    return event.type !== "mousedown" && event.type !== "click";
  }
}

function buildLivePreview(view: EditorView): DecorationSet {
  const doc = view.state.doc;

  // Reveal raw markers only while the user is in edit mode (double-clicked
  // into a line). Outside edit mode this set is empty — everything renders.
  const activeLines = activeEditLines(view.state);
  const lineActive = (pos: number) => activeLines.has(doc.lineAt(pos).number);

  type R = { from: number; to: number; value: Decoration };
  const out: R[] = [];

  // Markdown blank lines separate blocks, but in live preview a full body-row
  // blank makes headings and lists feel looser than rendered markdown.
  for (const range of view.visibleRanges) {
    const start = doc.lineAt(range.from).number;
    const end = doc.lineAt(range.to).number;
    for (let i = start; i <= end; i++) {
      const line = doc.line(i);
      if (line.length === 0 && !activeLines.has(i)) {
        out.push({ from: line.from, to: line.from, value: blankLine });
      }
    }
  }

  // Pre-pass: collect GFM task markers so the main pass knows which list
  // items are tasks. Tree walk is pre-order so ListMark fires *before*
  // TaskMarker in the same item — we need the task info in hand first.
  type TaskInfo = { checked: boolean; markerFrom: number; markerTo: number };
  const tasksByLine = new Map<number, TaskInfo>();
  for (const range of view.visibleRanges) {
    syntaxTree(view.state).iterate({
      from: range.from,
      to: range.to,
      enter: (node) => {
        if (node.type.name !== "TaskMarker") return;
        const lineNum = doc.lineAt(node.from).number;
        const text = doc.sliceString(node.from, node.to);
        tasksByLine.set(lineNum, {
          checked: /x/i.test(text),
          markerFrom: node.from,
          markerTo: node.to,
        });
      },
    });
  }

  // Restrict traversal to the viewport — large docs get thousands of nodes.
  for (const range of view.visibleRanges) {
    syntaxTree(view.state).iterate({
      from: range.from,
      to: range.to,
      enter: (node) => {
        const name = node.type.name;

        // ATX headings: scale the whole line.
        const hm = /^ATXHeading([1-6])$/.exec(name);
        if (hm) {
          const lvl = parseInt(hm[1], 10);
          const line = doc.lineAt(node.from);
          out.push({
            from: line.from,
            to: line.from,
            value: headingLine[lvl - 1],
          });
          return;
        }

        if (name === "StrongEmphasis") {
          out.push({ from: node.from, to: node.to, value: strongMark });
          return;
        }
        if (name === "Emphasis") {
          out.push({ from: node.from, to: node.to, value: emphasisMark });
          return;
        }
        if (name === "Strikethrough") {
          out.push({ from: node.from, to: node.to, value: strikeMark });
          return;
        }
        if (name === "InlineCode") {
          out.push({ from: node.from, to: node.to, value: inlineCodeMark });
          return;
        }

        // Marker characters — hide when cursor isn't on this line.
        if (
          name === "HeaderMark" ||
          name === "EmphasisMark" ||
          name === "StrikethroughMark" ||
          name === "QuoteMark"
        ) {
          if (!lineActive(node.from)) {
            // HeaderMark and QuoteMark own the `#` / `>` but not the space
            // after them — chew through one trailing space so the rendered
            // line doesn't start with stray indentation.
            let end = node.to;
            if (name === "HeaderMark" || name === "QuoteMark") {
              if (doc.sliceString(end, end + 1) === " ") end += 1;
            }
            out.push({ from: node.from, to: end, value: hideDeco });
          }
          return;
        }

        // CodeMark covers both inline backticks and fenced ``` rows. Hide
        // only the short inline variants (1-2 chars) off-line; fence rows
        // stay visible because they delimit the whole code block.
        if (name === "CodeMark") {
          const len = node.to - node.from;
          if (len <= 2 && !lineActive(node.from)) {
            out.push({ from: node.from, to: node.to, value: hideDeco });
          }
          return;
        }

        if (name === "Blockquote") {
          const s = doc.lineAt(node.from).number;
          const e = doc.lineAt(node.to).number;
          for (let i = s; i <= e; i++) {
            const ln = doc.line(i);
            out.push({ from: ln.from, to: ln.from, value: blockquoteLine });
          }
          return;
        }

        if (name === "FencedCode") {
          const s = doc.lineAt(node.from).number;
          const e = doc.lineAt(node.to).number;
          // Extract the language tag from the opening fence: ```rust → "rust".
          // Tilde fences (~~~) are supported the same way.
          const openingText = doc.line(s).text;
          const langMatch = openingText.match(/^\s*[`~]{3,}\s*([A-Za-z0-9_+\-#.]*)/);
          const lang = (langMatch?.[1] ?? "").toLowerCase();

          for (let i = s; i <= e; i++) {
            const ln = doc.line(i);
            const isFenceLine = i === s || i === e;
            if (isFenceLine) {
              // Hide the ```lang row (and the closing ``` row) off-line so
              // the block reads like rendered code. Active-line shows the
              // raw fence so the user can edit the language tag etc.
              if (!activeLines.has(i) && ln.length > 0) {
                out.push({ from: ln.from, to: ln.to, value: hideDeco });
              }
              continue;
            }
            // First content line of a fenced block carries the language
            // label, rendered as a CSS ::before pinned to the top-right
            // corner of the line. Regular codeblock rows just inherit the
            // muted-background styling.
            if (i === s + 1 && lang) {
              out.push({
                from: ln.from,
                to: ln.from,
                value: Decoration.line({
                  class: "cm-md-codeblock cm-md-codeblock-start",
                  attributes: { "data-lang": lang },
                }),
              });
            } else {
              out.push({ from: ln.from, to: ln.from, value: codeBlockLine });
            }
          }
          return;
        }

        if (name === "HorizontalRule") {
          const line = doc.lineAt(node.from);
          out.push({ from: line.from, to: line.from, value: hrLine });
          // Hide the `---` text off-line; the border draws itself via CSS.
          if (!lineActive(node.from)) {
            out.push({ from: node.from, to: node.to, value: hideDeco });
          }
          return;
        }

        if (name === "ListMark") {
          const lineNum = doc.lineAt(node.from).number;
          const task = tasksByLine.get(lineNum);
          if (task) {
            // On task lines, the checkbox replaces the bullet entirely. Hide
            // `-` / `*` / `+` plus any spaces between it and the marker so
            // the checkbox sits where the bullet used to be.
            if (!lineActive(node.from)) {
              out.push({
                from: node.from,
                to: task.markerFrom,
                value: hideDeco,
              });
            }
            return;
          }
          const text = doc.sliceString(node.from, node.to);
          if (
            (text === "-" || text === "*" || text === "+") &&
            !lineActive(node.from)
          ) {
            out.push({ from: node.from, to: node.to, value: bulletDeco });
          }
          return;
        }

        if (name === "TaskMarker") {
          const task = tasksByLine.get(doc.lineAt(node.from).number);
          if (!task) return;
          // Always swap the `[x]` / `[ ]` text for a real checkbox — including
          // when the line is active, because the marker is ugly to look at
          // even while editing and the widget stays clickable.
          out.push({
            from: node.from,
            to: node.to,
            value: Decoration.replace({
              widget: new TaskCheckboxWidget(task.checked, node.from),
            }),
          });
          // Strike + mute the rest of the line for completed tasks. Line
          // decoration handles indent/list-marker area; mark decoration
          // covers the visible text after the checkbox.
          if (task.checked) {
            const line = doc.lineAt(node.from);
            out.push({ from: line.from, to: line.from, value: taskDoneLine });
            let contentStart = node.to;
            if (doc.sliceString(contentStart, contentStart + 1) === " ") {
              contentStart += 1;
            }
            if (contentStart < line.to) {
              out.push({
                from: contentStart,
                to: line.to,
                value: taskDoneMark,
              });
            }
          }
          return;
        }
      },
    });
  }

  return Decoration.set(
    out.map((r) => r.value.range(r.from, r.to)),
    true,
  );
}

const livePreviewPlugin = ViewPlugin.fromClass(
  class {
    decorations: DecorationSet;
    constructor(view: EditorView) {
      this.decorations = buildLivePreview(view);
    }
    update(u: ViewUpdate) {
      const modeToggle = u.transactions.some((tr) =>
        tr.effects.some((e) => e.is(setEditMode)),
      );
      if (
        u.docChanged ||
        u.selectionSet ||
        u.viewportChanged ||
        modeToggle
      ) {
        this.decorations = buildLivePreview(u.view);
      }
    }
  },
  { decorations: (v) => v.decorations },
);

const livePreviewTheme = EditorView.baseTheme({
  ".cm-md-bullet": {
    color: "var(--color-accent)",
    fontWeight: "600",
    marginRight: "2px",
  },
  ".cm-md-task-checkbox": {
    appearance: "none",
    width: "14px",
    height: "14px",
    margin: "0 6px 0 0",
    padding: "0",
    verticalAlign: "-2px",
    border: "1.5px solid var(--color-text-muted)",
    borderRadius: "3px",
    background: "transparent",
    cursor: "pointer",
    position: "relative",
    transition: "border-color 0.15s, background 0.15s",
  },
  ".cm-md-task-checkbox:hover": {
    borderColor: "var(--color-accent)",
  },
  ".cm-md-task-checkbox:checked": {
    borderColor: "var(--color-accent)",
    background: "var(--color-accent)",
  },
  ".cm-md-task-checkbox:checked::after": {
    content: '""',
    position: "absolute",
    left: "3px",
    top: "0px",
    width: "4px",
    height: "8px",
    border: "solid var(--color-bg-primary)",
    borderWidth: "0 2px 2px 0",
    transform: "rotate(45deg)",
  },
  ".cm-md-task-done": {
    textDecoration: "line-through",
    color: "var(--color-text-muted)",
  },
  ".cm-md-task-done-line": {
    opacity: "0.75",
  },
});

export function livePreview() {
  return [livePreviewPlugin, livePreviewTheme];
}
