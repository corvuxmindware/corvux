/**
 * CodeMirror 6 extension for inline #tag highlighting.
 *
 * - Highlights #tags with a colored background pill
 * - Skips headings (lines starting with #), code blocks, and inline code
 * - Cycles through Catppuccin Mocha tag colors
 */

import {
  Decoration,
  DecorationSet,
  EditorView,
  ViewPlugin,
  ViewUpdate,
} from "@codemirror/view";
import { RangeSetBuilder } from "@codemirror/state";

// Tag regex: # followed by word chars, hyphens, underscores, slashes
// Must be preceded by whitespace or start of line
const TAG_RE = /(?:^|(?<=\s))#([\w][\w\-_/]*)/g;

// Catppuccin Mocha tag colors (same palette as Properties panel)
const TAG_COLORS = [
  { bg: "#a6e3a120", text: "#a6e3a1" }, // green
  { bg: "#89b4fa20", text: "#89b4fa" }, // blue
  { bg: "#f5c2e720", text: "#f5c2e7" }, // pink
  { bg: "#f9e2af20", text: "#f9e2af" }, // yellow
  { bg: "#cba6f720", text: "#cba6f7" }, // purple
  { bg: "#94e2d520", text: "#94e2d5" }, // teal
  { bg: "#fab38720", text: "#fab387" }, // orange
];

// Simple hash to pick a consistent color per tag name
function tagColorIndex(tag: string): number {
  let hash = 0;
  for (let i = 0; i < tag.length; i++) {
    hash = (hash * 31 + tag.charCodeAt(i)) | 0;
  }
  return Math.abs(hash) % TAG_COLORS.length;
}

function buildDecorations(view: EditorView): DecorationSet {
  const builder = new RangeSetBuilder<Decoration>();
  const doc = view.state.doc;
  let inCodeBlock = false;

  for (let i = 1; i <= doc.lines; i++) {
    const line = doc.line(i);
    const text = line.text;
    const trimmed = text.trim();

    // Toggle fenced code blocks
    if (trimmed.startsWith("```") || trimmed.startsWith("~~~")) {
      inCodeBlock = !inCodeBlock;
      continue;
    }
    if (inCodeBlock) continue;

    // Skip heading lines (# Heading)
    if (/^#{1,6}\s/.test(trimmed)) continue;

    // Find inline code spans to skip
    const codeSpans: Array<[number, number]> = [];
    let codeMatch: RegExpExecArray | null;
    const codeRe = /`[^`]+`/g;
    while ((codeMatch = codeRe.exec(text)) !== null) {
      codeSpans.push([codeMatch.index, codeMatch.index + codeMatch[0].length]);
    }

    TAG_RE.lastIndex = 0;
    let match: RegExpExecArray | null;

    while ((match = TAG_RE.exec(text)) !== null) {
      const tagStart = match.index + (match[0].startsWith("#") ? 0 : match[0].indexOf("#"));
      const tagEnd = tagStart + 1 + match[1].length; // # + tag text

      // Skip if inside inline code
      const inCode = codeSpans.some(
        ([s, e]) => tagStart >= s && tagEnd <= e
      );
      if (inCode) continue;

      const colorIdx = tagColorIndex(match[1]);
      const color = TAG_COLORS[colorIdx];

      builder.add(
        line.from + tagStart,
        line.from + tagEnd,
        Decoration.mark({
          class: `cm-tag cm-tag-${colorIdx}`,
          attributes: {
            style: `color: ${color.text}; background: ${color.bg}; border-radius: 3px; padding: 0 2px;`,
          },
        })
      );
    }
  }

  return builder.finish();
}

const tagPlugin = ViewPlugin.fromClass(
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

const tagTheme = EditorView.baseTheme({
  ".cm-tag": {
    cursor: "pointer",
    fontWeight: "500",
    transition: "opacity 0.15s",
    "&:hover": {
      opacity: "0.8",
    },
  },
});

/**
 * Full tag highlight extension: decorations + theme.
 */
export function tagHighlight() {
  return [tagPlugin, tagTheme];
}
