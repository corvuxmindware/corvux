/**
 * CodeMirror 6 extension for [[wiki-link]] decoration.
 *
 * Off-line (cursor elsewhere) the link renders like Obsidian read-mode:
 *   [[Corvux/00 - Index|Index]]  →  Index
 *   [[folder/note]]              →  note
 *   [[plain]]                    →  plain
 * The brackets, folder prefix, and `|` separator are hidden.
 *
 * On-line (cursor on the link's line) the full `[[target|alias]]` is revealed
 * so you can edit the raw syntax. A plain click navigates while the link is in
 * its rendered state; Ctrl/Cmd+click still navigates while editing raw syntax.
 */

import {
  Decoration,
  DecorationSet,
  EditorView,
  ViewPlugin,
  ViewUpdate,
} from "@codemirror/view";
import { activeEditLines, setEditMode } from "./edit-mode";

// Regex to find [[links]] — non-greedy, no newlines inside
const WIKILINK_RE = /\[\[([^\]\n|]+?)(?:\|([^\]\n]+?))?\]\]/g;

const bracketDeco = Decoration.mark({ class: "cm-wikilink-bracket" });
const linkDeco = Decoration.mark({ class: "cm-wikilink-link" });
const hideDeco = Decoration.replace({});

function buildDecorations(view: EditorView): DecorationSet {
  const doc = view.state.doc;

  // Only reveal raw `[[...]]` syntax while the user is in edit mode
  // (double-clicked into a line). Outside edit mode this set is empty.
  const activeLines = activeEditLines(view.state);

  type R = { from: number; to: number; value: Decoration };
  const out: R[] = [];

  for (let i = 1; i <= doc.lines; i++) {
    const line = doc.line(i);
    const text = line.text;
    const isActive = activeLines.has(i);
    let match: RegExpExecArray | null;
    WIKILINK_RE.lastIndex = 0;

    while ((match = WIKILINK_RE.exec(text)) !== null) {
      const from = line.from + match.index;
      const to = from + match[0].length;
      const target = match[1];
      const alias = match[2];

      if (isActive) {
        // Editing view — show everything, just color it.
        out.push({ from, to: from + 2, value: bracketDeco });
        out.push({ from: from + 2, to: to - 2, value: linkDeco });
        out.push({ from: to - 2, to, value: bracketDeco });
        continue;
      }

      // Rendered view — compute the visible slice (display text only).
      let displayFrom: number;
      if (alias !== undefined) {
        // Has alias — show text after the pipe.
        const pipeOffset = match[0].indexOf("|");
        displayFrom = from + pipeOffset + 1;
      } else {
        // No alias — drop any `folder/` prefix, show the last segment.
        const targetStart = from + 2;
        const lastSlash = target.lastIndexOf("/");
        displayFrom = lastSlash >= 0 ? targetStart + lastSlash + 1 : targetStart;
      }
      const displayTo = to - 2;

      if (displayFrom > from) {
        out.push({ from, to: displayFrom, value: hideDeco });
      }
      if (displayTo > displayFrom) {
        out.push({ from: displayFrom, to: displayTo, value: linkDeco });
      }
      out.push({ from: displayTo, to, value: hideDeco });
    }
  }

  return Decoration.set(
    out.map((r) => r.value.range(r.from, r.to)),
    true,
  );
}

const wikilinkPlugin = ViewPlugin.fromClass(
  class {
    decorations: DecorationSet;

    constructor(view: EditorView) {
      this.decorations = buildDecorations(view);
    }

    update(update: ViewUpdate) {
      const modeToggle = update.transactions.some((tr) =>
        tr.effects.some((e) => e.is(setEditMode)),
      );
      if (
        update.docChanged ||
        update.viewportChanged ||
        update.selectionSet ||
        modeToggle
      ) {
        this.decorations = buildDecorations(update.view);
      }
    }
  },
  { decorations: (v) => v.decorations }
);

// Theme for wiki-link styles — uses CSS variables for theme switching
const wikilinkTheme = EditorView.baseTheme({
  ".cm-wikilink-bracket": {
    color: "var(--color-text-muted)",
    fontWeight: "normal",
  },
  ".cm-wikilink-link": {
    color: "var(--color-accent)",
    textDecoration: "underline",
    textDecorationColor: "color-mix(in srgb, var(--color-accent) 25%, transparent)",
    textUnderlineOffset: "2px",
    cursor: "pointer",
    "&:hover": {
      textDecorationColor: "var(--color-accent)",
    },
  },
});

/**
 * Extract the link target from a position in the document.
 * Returns null if the position is not inside a [[wiki-link]].
 */
export function getWikiLinkAtPos(
  doc: string,
  pos: number
): string | null {
  WIKILINK_RE.lastIndex = 0;
  let match: RegExpExecArray | null;
  while ((match = WIKILINK_RE.exec(doc)) !== null) {
    const start = match.index;
    const end = start + match[0].length;
    if (pos >= start && pos <= end) {
      return match[1].trim();
    }
  }
  return null;
}

/**
 * Create the wiki-link click handler extension.
 * `onNavigate` is called with the link target when a rendered wiki-link is
 * clicked, or when the user Ctrl/Cmd+clicks a wiki-link in raw edit mode.
 */
export function shouldNavigateWikiLinkClick(
  modifier: boolean,
  lineActive: boolean,
): boolean {
  return modifier || !lineActive;
}

function wikilinkClickHandler(onNavigate: (target: string) => void) {
  return EditorView.domEventHandlers({
    click(event: MouseEvent, view: EditorView) {
      const pos = view.posAtCoords({ x: event.clientX, y: event.clientY });
      if (pos === null) return false;

      const modifier = event.ctrlKey || event.metaKey;
      const lineActive = activeEditLines(view.state).has(
        view.state.doc.lineAt(pos).number,
      );
      if (!shouldNavigateWikiLinkClick(modifier, lineActive)) return false;

      const doc = view.state.doc.toString();
      const target = getWikiLinkAtPos(doc, pos);
      if (target) {
        event.preventDefault();
        onNavigate(target);
        return true;
      }
      return false;
    },
  });
}

/**
 * Full wiki-link extension: decorations + click handler + theme.
 */
export function wikilinks(onNavigate: (target: string) => void) {
  return [wikilinkPlugin, wikilinkClickHandler(onNavigate), wikilinkTheme];
}
