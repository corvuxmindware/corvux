/**
 * CodeMirror 6 extension for Obsidian-style callout blocks.
 *
 * Styles blockquote lines that start with `> [!type]` pattern:
 *   > [!note] Optional title
 *   > Content here
 *
 * Supported types: note, tip, info, warning, danger, bug, example, quote, success, question, abstract, todo
 */

import {
  Decoration,
  DecorationSet,
  EditorView,
  ViewPlugin,
  ViewUpdate,
} from "@codemirror/view";
import { RangeSetBuilder } from "@codemirror/state";

interface CalloutStyle {
  color: string;
  bg: string;
  icon: string; // unicode character
}

const CALLOUT_STYLES: Record<string, CalloutStyle> = {
  note:     { color: "#89b4fa", bg: "#89b4fa10", icon: "\u270f\ufe0f" },   // pencil
  tip:      { color: "#a6e3a1", bg: "#a6e3a110", icon: "\ud83d\udca1" },   // lightbulb
  info:     { color: "#89dceb", bg: "#89dceb10", icon: "\u2139\ufe0f" },    // info
  warning:  { color: "#f9e2af", bg: "#f9e2af10", icon: "\u26a0\ufe0f" },   // warning
  danger:   { color: "#f38ba8", bg: "#f38ba810", icon: "\u26d4" },          // no entry
  bug:      { color: "#f38ba8", bg: "#f38ba810", icon: "\ud83d\udc1b" },    // bug
  example:  { color: "#cba6f7", bg: "#cba6f710", icon: "\ud83d\udcce" },    // paperclip
  quote:    { color: "#9399b2", bg: "#9399b210", icon: "\u275d" },          // quote mark
  success:  { color: "#a6e3a1", bg: "#a6e3a110", icon: "\u2705" },          // check
  question: { color: "#f9e2af", bg: "#f9e2af10", icon: "\u2753" },          // question
  abstract: { color: "#89dceb", bg: "#89dceb10", icon: "\ud83d\udcdd" },    // memo
  todo:     { color: "#89b4fa", bg: "#89b4fa10", icon: "\u2610" },          // ballot box
  caution:  { color: "#f38ba8", bg: "#f38ba810", icon: "\u26a0\ufe0f" },   // alias for warning
  important:{ color: "#cba6f7", bg: "#cba6f710", icon: "\u2757" },          // exclamation
};

// Matches `> [!type]` at start of line, with optional title
const CALLOUT_HEADER_RE = /^>\s*\[!(\w+)\]\s*(.*)/i;
// Matches continuation lines of a callout block
const CALLOUT_CONT_RE = /^>\s?(.*)/;

function buildDecorations(view: EditorView): DecorationSet {
  const builder = new RangeSetBuilder<Decoration>();
  const doc = view.state.doc;
  let inCallout = false;
  let calloutStyle: CalloutStyle | null = null;

  for (let i = 1; i <= doc.lines; i++) {
    const line = doc.line(i);
    const text = line.text;

    const headerMatch = text.match(CALLOUT_HEADER_RE);
    if (headerMatch) {
      const type = headerMatch[1].toLowerCase();
      calloutStyle = CALLOUT_STYLES[type] || CALLOUT_STYLES.note;
      inCallout = true;

      // Decorate the header line. Line decorations MUST be zero-length
      // at the line start (from === to) — the previous `line.from →
      // line.to` range made CM treat the whole line as a replaced range
      // and the callout text rendered as an empty colored bar (content
      // invisible in reading view). Same contract as live-preview's
      // heading/blockquote line decos.
      builder.add(
        line.from,
        line.from,
        Decoration.line({
          class: "cm-callout-header",
          attributes: {
            style: `background: ${calloutStyle.bg}; border-left: 3px solid ${calloutStyle.color}; padding-left: 8px; color: ${calloutStyle.color}; font-weight: 600;`,
            // Rendered by the theme's ::before — CALLOUT_STYLES defined
            // these icons since day one but nothing ever displayed them.
            "data-callout-icon": calloutStyle.icon,
          },
        })
      );
      continue;
    }

    if (inCallout && calloutStyle) {
      const contMatch = text.match(CALLOUT_CONT_RE);
      if (contMatch) {
        // Zero-length at line start — see header comment above.
        builder.add(
          line.from,
          line.from,
          Decoration.line({
            class: "cm-callout-body",
            attributes: {
              style: `background: ${calloutStyle.bg}; border-left: 3px solid ${calloutStyle.color}; padding-left: 8px;`,
            },
          })
        );
      } else {
        // Line doesn't start with >, callout block ends
        inCallout = false;
        calloutStyle = null;
      }
    }
  }

  return builder.finish();
}

const calloutPlugin = ViewPlugin.fromClass(
  class {
    decorations: DecorationSet;

    constructor(view: EditorView) {
      this.decorations = buildDecorations(view);
    }

    update(update: ViewUpdate) {
      if (update.docChanged || update.viewportChanged) {
        this.decorations = buildDecorations(update.view);
      }
    }
  },
  { decorations: (v) => v.decorations }
);

const calloutTheme = EditorView.baseTheme({
  ".cm-callout-header": {
    borderRadius: "3px 3px 0 0",
  },
  ".cm-callout-header::before": {
    content: "attr(data-callout-icon)",
    marginRight: "6px",
  },
  ".cm-callout-body": {
    borderRadius: "0",
    "&:last-child": {
      borderRadius: "0 0 3px 3px",
    },
  },
});

export function calloutHighlight() {
  return [calloutPlugin, calloutTheme];
}
