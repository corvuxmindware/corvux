/**
 * CodeMirror 6 extension that hides a YAML frontmatter block (`---\n...\n---`)
 * from the editor view. The Properties panel already renders these fields — showing
 * them a second time as raw YAML at the top of the note is noise.
 *
 * Behaviour:
 *  - The block is replaced with a tiny "Properties" pill widget so the user knows
 *    the metadata is still there, just collapsed.
 *  - The range is marked atomic — cursor movement skips across it automatically
 *    (no "stuck cursor" on opening the note, no arrow-key dead zones).
 *  - Clicking the pill places the cursor just past the closing `---`, so typing
 *    resumes in the note body.
 *  - If there is no frontmatter, the extension is a no-op.
 *
 * NOTE: Block decorations *must* come from a StateField, not a ViewPlugin.
 * CodeMirror throws "Block decorations may not be specified via plugins"
 * otherwise, because block decorations affect layout before the viewport
 * is computed.
 */

import {
  Decoration,
  DecorationSet,
  EditorView,
  WidgetType,
} from "@codemirror/view";
import {
  EditorSelection,
  RangeSet,
  StateField,
  type Text,
  type Transaction,
} from "@codemirror/state";

interface FrontmatterRange {
  from: number;
  to: number;
  propertyCount: number;
}

function findFrontmatter(doc: Text): FrontmatterRange | null {
  if (doc.lines < 2) return null;
  const firstLine = doc.line(1);
  if (firstLine.text.trim() !== "---") return null;

  for (let i = 2; i <= doc.lines; i++) {
    const line = doc.line(i);
    if (line.text.trim() === "---") {
      // Count top-level YAML keys as a property count for the pill label
      let props = 0;
      for (let j = 2; j < i; j++) {
        const inner = doc.line(j).text;
        if (/^[^\s#-][^:]*:/.test(inner)) props++;
      }
      return { from: firstLine.from, to: line.to, propertyCount: props };
    }
  }
  return null;
}

class PropertiesPillWidget extends WidgetType {
  constructor(private count: number) {
    super();
  }

  eq(other: PropertiesPillWidget): boolean {
    return other.count === this.count;
  }

  toDOM(): HTMLElement {
    // The widget DOM exists only to satisfy CM's block-replace contract —
    // its job is to hide the YAML range. Visually it's empty: the
    // Properties disclosure rendered above the editor already shows
    // the field count + acts as the affordance for editing them, so a
    // duplicate pill here is just noise (see screenshot 2026-05-22).
    // _count is preserved as a no-op param so the WidgetType eq() check
    // still discriminates between different frontmatter states; without
    // that, CM would skip re-decoration after edits.
    void this.count;
    const outer = document.createElement("div");
    outer.className = "cm-frontmatter-pill-outer";
    return outer;
  }

  ignoreEvent(): boolean {
    return false;
  }
}

function buildDecorations(doc: Text): DecorationSet {
  const range = findFrontmatter(doc);
  if (!range) return Decoration.none;

  return Decoration.set([
    Decoration.replace({
      widget: new PropertiesPillWidget(range.propertyCount),
      block: true,
    }).range(range.from, range.to),
  ]);
}

const frontmatterField = StateField.define<DecorationSet>({
  create(state) {
    return buildDecorations(state.doc);
  },
  update(deco: DecorationSet, tr: Transaction) {
    if (!tr.docChanged) return deco;
    return buildDecorations(tr.state.doc);
  },
  provide: (f) => [
    EditorView.decorations.from(f),
    // Marking the replaced range atomic makes cursor movement skip over it
    // cleanly — no invisible caret sitting inside the hidden block.
    EditorView.atomicRanges.of((view) => view.state.field(f, false) ?? RangeSet.empty),
  ],
});

// Number of document lines occupied by the frontmatter block (including both
// `---` fences). Zero when no frontmatter is present. Used by the gutter so
// line numbers appear to start at 1 for the visible body.
export const frontmatterLineCount = StateField.define<number>({
  create(state) {
    const range = findFrontmatter(state.doc);
    return range ? state.doc.lineAt(range.to).number : 0;
  },
  update(value, tr) {
    if (!tr.docChanged) return value;
    const range = findFrontmatter(tr.state.doc);
    return range ? tr.state.doc.lineAt(range.to).number : 0;
  },
});

const frontmatterClickHandler = EditorView.domEventHandlers({
  mousedown(event, view) {
    const target = event.target as HTMLElement;
    const pill = target.closest(".cm-frontmatter-pill");
    if (!pill) return false;
    const range = findFrontmatter(view.state.doc);
    if (!range) return false;
    event.preventDefault();
    view.dispatch({
      selection: EditorSelection.cursor(range.to),
      scrollIntoView: true,
    });
    view.focus();
    return true;
  },
});

const frontmatterTheme = EditorView.baseTheme({
  // Zero-footprint hidden block. The YAML range is replaced by an
  // invisible div so the editor body slides up flush against the
  // Properties disclosure rendered in EditorArea above. No padding,
  // no styling — the disclosure header already does the
  // "Properties · N fields" job.
  ".cm-frontmatter-pill-outer": {
    display: "none",
  },
});

/** Hide YAML frontmatter behind a collapsible "Properties" pill. */
export function frontmatterHide() {
  return [
    frontmatterField,
    frontmatterLineCount,
    frontmatterClickHandler,
    frontmatterTheme,
  ];
}
