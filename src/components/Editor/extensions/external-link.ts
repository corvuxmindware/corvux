/**
 * CodeMirror 6 extension for external links — the counterpart to `wikilink.ts`.
 *
 * `wikilink.ts` handles internal `[[note]]` navigation. This handles everything
 * that points *out* of the roost:
 *
 *   - Inline markdown links   `[text](https://example.com)`
 *   - Autolinks               `<https://example.com>`
 *   - Bare / GFM autolinks    `https://example.com`
 *   - `mailto:` links
 *
 * Rendering (Obsidian live-preview style, cursor off the line):
 *   `[Corvux](https://meetcorvux.com)`  →  Corvux
 * The `[`, and the whole `](url)` tail, are hidden; only the display text shows,
 * styled as a link. Move the cursor onto the line (double-click to edit) and the
 * raw `[text](url)` snaps back so you can edit it.
 *
 * Clicking:
 *   - Single click on a rendered link (line not in edit mode) opens it.
 *   - Ctrl/Cmd+click opens it from anywhere, even while editing the raw source
 *     (mirrors the wiki-link modifier-click convention).
 * Opening goes through the Tauri opener plugin → system default browser. In
 * browser-dev mode (`!isTauri`) it falls back to `window.open`.
 */

import { syntaxTree } from "@codemirror/language";
import { EditorState } from "@codemirror/state";
import {
  Decoration,
  DecorationSet,
  EditorView,
  ViewPlugin,
  ViewUpdate,
} from "@codemirror/view";
import { activeEditLines, setEditMode } from "./edit-mode";
import { isTauri } from "../../../lib/invoke";
import { log } from "../../../lib/log";

// Bare-URL detector for the click-handler fallback. Stops at whitespace and the
// brackets/parens that delimit markdown link syntax so it doesn't swallow a
// trailing `)` from `(see https://x)`. Trailing sentence punctuation is trimmed
// separately in `cleanUrl`.
const BARE_URL_RE = /(?:https?:\/\/|mailto:)[^\s<>()[\]]+/g;

/** True for the schemes we are willing to hand to the system browser. */
export function isExternalUrl(url: string): boolean {
  return /^(https?:\/\/|mailto:)/i.test(url.trim());
}

/**
 * Strip wrapping angle brackets and trailing sentence punctuation that a bare
 * URL match commonly over-captures (`https://x.com.` → `https://x.com`).
 */
export function cleanUrl(raw: string): string {
  let url = raw.trim().replace(/^<+/, "").replace(/>+$/, "");
  // Peel trailing punctuation that is almost never part of the URL itself.
  url = url.replace(/[.,;:!?]+$/, "");
  return url;
}

/**
 * Open an external URL in the system default browser. Uses the Tauri opener
 * plugin when running inside the app; falls back to `window.open` in the
 * browser-only dev server so links still work during frontend-only work.
 */
export async function openExternalUrl(url: string): Promise<void> {
  const target = cleanUrl(url);
  if (!isExternalUrl(target)) return;
  try {
    if (isTauri) {
      const { openUrl } = await import("@tauri-apps/plugin-opener");
      await openUrl(target);
    } else {
      window.open(target, "_blank", "noopener,noreferrer");
    }
  } catch (err) {
    log.error("editor", "failed to open external url", err);
    // Last-ditch fallback — better a stray tab than a dead click.
    try {
      window.open(target, "_blank", "noopener,noreferrer");
    } catch {
      /* nothing more we can do */
    }
  }
}

const hideDeco = Decoration.replace({});
const linkDeco = Decoration.mark({ class: "cm-external-link" });

function buildDecorations(view: EditorView): DecorationSet {
  const doc = view.state.doc;
  const activeLines = activeEditLines(view.state);
  const isActive = (pos: number) => activeLines.has(doc.lineAt(pos).number);

  type R = { from: number; to: number; value: Decoration };
  const out: R[] = [];

  for (const range of view.visibleRanges) {
    syntaxTree(view.state).iterate({
      from: range.from,
      to: range.to,
      enter: (node) => {
        const name = node.type.name;

        // Inline markdown link: `[text](url)`.
        if (name === "Link") {
          const linkNode = node.node;
          const url = linkNode.getChild("URL");
          // Reference-style (`[text][ref]`) and shorthand (`[text]`) links have
          // no URL child — leave them untouched. `[[wikilinks]]` also land here
          // (parsed as a bracketed span) and are handled by wikilink.ts.
          if (!url) return;
          const urlStr = doc.sliceString(url.from, url.to);
          if (!isExternalUrl(urlStr)) return; // relative / in-doc anchor — skip

          const marks = linkNode.getChildren("LinkMark"); // [ , ] , ( , )
          const openBracket = marks[0];
          const closeBracket = marks[1];
          if (!openBracket || !closeBracket) return;

          const displayFrom = openBracket.to; // just after `[`
          const displayTo = closeBracket.from; // the `]`

          if (isActive(linkNode.from)) {
            // Editing — reveal the raw syntax, just color the display text.
            if (displayTo > displayFrom) {
              out.push({ from: displayFrom, to: displayTo, value: linkDeco });
            }
            return;
          }

          // Rendered — hide `[` and the `](url)` tail, show only the text.
          if (displayFrom > linkNode.from) {
            out.push({ from: linkNode.from, to: displayFrom, value: hideDeco });
          }
          if (displayTo > displayFrom) {
            out.push({ from: displayFrom, to: displayTo, value: linkDeco });
          }
          if (linkNode.to > displayTo) {
            out.push({ from: displayTo, to: linkNode.to, value: hideDeco });
          }
          return;
        }

        // Autolinks (`<https://x>`) and GFM bare URLs both surface as `URL`
        // nodes. Skip the ones that are the target of a `Link`/`Image` (already
        // handled / hidden above); style the free-standing ones in place.
        if (name === "URL" || name === "Autolink") {
          const parent = node.node.parent?.name;
          if (parent === "Link" || parent === "Image") return;
          const text = doc.sliceString(node.from, node.to);
          if (!isExternalUrl(cleanUrl(text))) return;
          out.push({ from: node.from, to: node.to, value: linkDeco });
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

/**
 * Resolve the external URL at a document position, or null if there isn't one.
 * Walks the syntax tree first, then falls back to a bare-URL regex scan of the
 * clicked line (covers cases the parser hasn't tokenised as a URL node).
 */
export function getExternalUrlAtPos(
  state: EditorState,
  pos: number,
): string | null {
  const tree = syntaxTree(state);
  type Node = ReturnType<typeof tree.resolveInner>;
  for (let n: Node | null = tree.resolveInner(pos, -1); n; n = n.parent) {
    if (n.name === "URL" || n.name === "Autolink") {
      const url = cleanUrl(state.doc.sliceString(n.from, n.to));
      if (isExternalUrl(url)) return url;
    }
    if (n.name === "Link") {
      const urlNode = n.getChild("URL");
      if (urlNode) {
        const url = cleanUrl(state.doc.sliceString(urlNode.from, urlNode.to));
        if (isExternalUrl(url)) return url;
      }
    }
  }

  // Fallback: scan the clicked line for a bare URL spanning `pos`.
  const line = state.doc.lineAt(pos);
  BARE_URL_RE.lastIndex = 0;
  let m: RegExpExecArray | null;
  while ((m = BARE_URL_RE.exec(line.text)) !== null) {
    const from = line.from + m.index;
    const to = from + m[0].length;
    if (pos >= from && pos <= to) {
      const url = cleanUrl(m[0]);
      if (isExternalUrl(url)) return url;
    }
  }
  return null;
}

const externalLinkPlugin = ViewPlugin.fromClass(
  class {
    decorations: DecorationSet;
    constructor(view: EditorView) {
      this.decorations = buildDecorations(view);
    }
    update(u: ViewUpdate) {
      const modeToggle = u.transactions.some((tr) =>
        tr.effects.some((e) => e.is(setEditMode)),
      );
      if (u.docChanged || u.selectionSet || u.viewportChanged || modeToggle) {
        this.decorations = buildDecorations(u.view);
      }
    }
  },
  { decorations: (v) => v.decorations },
);

function externalLinkClickHandler() {
  return EditorView.domEventHandlers({
    click(event: MouseEvent, view: EditorView) {
      const pos = view.posAtCoords({ x: event.clientX, y: event.clientY });
      if (pos === null) return false;

      const modifier = event.ctrlKey || event.metaKey;
      // Without a modifier, only open when the link is in its rendered state
      // (line not being edited) — otherwise a plain click should just place
      // the caret so the user can edit the raw URL.
      const lineActive = activeEditLines(view.state).has(
        view.state.doc.lineAt(pos).number,
      );
      if (!modifier && lineActive) return false;

      const url = getExternalUrlAtPos(view.state, pos);
      if (!url) return false;

      event.preventDefault();
      void openExternalUrl(url);
      return true;
    },
  });
}

const externalLinkTheme = EditorView.baseTheme({
  ".cm-external-link": {
    color: "var(--color-accent)",
    textDecoration: "underline",
    textDecorationColor:
      "color-mix(in srgb, var(--color-accent) 25%, transparent)",
    textUnderlineOffset: "2px",
    cursor: "pointer",
    "&:hover": {
      textDecorationColor: "var(--color-accent)",
    },
  },
});

/**
 * Full external-link extension: decorations + click-to-open + theme.
 */
export function externalLinks() {
  return [externalLinkPlugin, externalLinkClickHandler(), externalLinkTheme];
}
