/**
 * Render `\`\`\`mermaid` fenced blocks as SVG diagrams, Obsidian-style.
 *
 * Behaviour mirrors table-render.ts:
 *  - Off-selection: the whole fenced block (opening fence → closing fence)
 *    is replaced by a block widget containing the rendered SVG. Source is
 *    hidden.
 *  - Double-click → edit mode → cursor inside the block: the replace
 *    decoration is skipped for that block, revealing raw markdown for
 *    editing. Escape / blur turns edit mode off and the diagram renders
 *    again. Same caret-aware reveal pattern the rest of the live-preview
 *    suite uses.
 *
 * Mermaid is dynamic-imported on first render so notes without diagrams
 * never pay the ~800 KB bundle cost. SVGs are cached by source hash.
 *
 * Block decorations must live in a StateField (not a ViewPlugin); same
 * constraint as table-render, image-embed, frontmatter-hide.
 */

import {
  Decoration,
  DecorationSet,
  EditorView,
  WidgetType,
} from "@codemirror/view";
import {
  EditorState,
  Range,
  StateField,
  type Transaction,
} from "@codemirror/state";
import { isEditMode, setEditMode } from "./edit-mode";
import { errMsg } from "../../../lib/error";
import { attachPinchZoom } from "./mermaid-zoom-dom";

// AUDIT-031 (revised 2026-05-23): mermaid emits SVG that we inject via
// innerHTML, so the XSS surface is real and must stay closed. Our v1 fix
// ran the output through a second DOMPurify pass — which broke labels
// completely under mermaid v11, because v11 puts every node label inside
// `<foreignObject>` containing XHTML and a string-based second pass loses
// the namespace boundary the first pass relied on, so the inner
// `<div>`/`<span>`/`<p>` get stripped (cure53/DOMPurify#1002,
// mermaid-js/mermaid#5904).
//
// The cleaner fix: trust mermaid's own sanitization. Mermaid 11.15.0
// (since PR #6079) internally calls `DOMPurify.sanitize(svgCode, {
//   ADD_TAGS: DOMPURIFY_TAGS, ADD_ATTR: DOMPURIFY_ATTR,
//   HTML_INTEGRATION_POINTS: { foreignobject: true }
// })` before returning, with `securityLevel: "strict"` further encoding
// HTML tags in user-supplied text. That covers our threat model — script
// tags, event handlers, javascript:/data: URLs, srcdoc/srcset — without
// the namespace-roundtrip issue.
//
// We add belt-and-suspenders regex strips as a backstop against a
// future mermaid regression that bypasses its own sanitize. (We do NOT
// re-add DOMPurify here; that's what broke labels under foreignObject.)
//
// S-05: extend the original single-line `<script>` strip to also cover
// inline event handlers (`onload=...`, `onclick=...` etc) and
// dangerous URL schemes (`javascript:`, `data:`) on href/xlink:href
// attributes. These shapes are uncommon in legitimate mermaid output
// but represent the realistic XSS regressions if the upstream
// sanitizer drifts.
export function sanitizeSvg(svg: string): string {
  let out = svg.replace(/<script\b[^>]*>[\s\S]*?<\/script\s*>/gi, "");
  // Strip inline event handlers — match `on<word>` attributes followed
  // by `=` and either quoted or unquoted values. Keep the rest of the
  // tag intact (this is a regex strip; a real parser would be safer
  // but DOMPurify on the SVG breaks foreignObject labels).
  out = out.replace(
    /\s+on[a-z]+\s*=\s*(?:"[^"]*"|'[^']*'|[^\s>]+)/gi,
    "",
  );
  // Neutralize javascript:/data: URLs on href / xlink:href attributes.
  // SVG <a xlink:href="javascript:..."> is the classic SVG-XSS sink.
  out = out.replace(
    /\s+(xlink:href|href)\s*=\s*("|')\s*(javascript|data)\s*:[^"']*\2/gi,
    " $1=\"#blocked\"",
  );
  return out;
}

// Lazy-loaded mermaid instance — shared across all widgets.
let mermaidPromise: Promise<typeof import("mermaid").default> | null = null;
/**
 * Kick off the mermaid dynamic-import. Exported so Editor can pre-warm
 * in parallel with CodeMirror's parse when the initial doc contains a
 * mermaid fence — two waits overlap instead of stacking.
 */
export function preloadMermaid() {
  return getMermaid();
}
function getMermaid() {
  if (!mermaidPromise) {
    mermaidPromise = import("mermaid").then((m) => {
      const mermaid = m.default;
      mermaid.initialize({
        startOnLoad: false,
        theme: "dark",
        securityLevel: "strict",
        fontFamily: "Inter, -apple-system, sans-serif",
        // Render node/edge labels as native SVG <text> (+ <tspan> per
        // <br/>) instead of HTML inside <foreignObject>. iOS WebKit /
        // WKWebView does NOT scale foreignObject content with the SVG
        // viewBox — when we shrink a wide diagram to `max-width:100%` the
        // HTML labels keep their intrinsic px size and render as a giant
        // overlay on top of the scaled-down diagram (seen on the
        // text-dense _architecture-flowchart on iPhone). SVG <text> scales
        // with the viewBox, so labels stay glued to their boxes. Trade-off:
        // no HTML rich-text in labels, but our diagrams only use plain text
        // + <br/> line breaks, which SVG labels handle via <tspan>.
        //
        // NB: must be the TOP-LEVEL `htmlLabels` key — the nested
        // `flowchart: { htmlLabels: false }` form is silently ignored in
        // mermaid 11.x (verified: nested → still <foreignObject>; top-level
        // → native <text>/<tspan>, <br/> splits into 11 tspans).
        htmlLabels: false,
        // v11+ default behaviour on a syntax error is to RESOLVE with an
        // SVG containing mermaid's built-in bomb icon and the text
        // "Syntax error in text — mermaid version 11.14.0". The widget's
        // try/catch never fires because no exception is thrown, so the
        // bomb SVG ends up in wrap.innerHTML. suppressErrorRendering
        // forces mermaid to throw instead, falling into our catch block
        // which renders the inline `⚠ Mermaid parse error — …` text.
        suppressErrorRendering: true,
      });
      return mermaid;
    });
  }
  return mermaidPromise;
}

// SVG cache keyed by source hash — survives scrolls, tab switches, and
// unrelated doc changes. Re-render only fires when a block's source
// actually changes. Capped at 256 entries (FIFO eviction) so long sessions
// with many unique diagrams don't accumulate unbounded memory.
const SVG_CACHE_MAX = 256;
const svgCache = new Map<string, string>();
const svgCacheKeys: string[] = [];

function setSvgCache(key: string, value: string): void {
  if (svgCache.has(key)) {
    svgCache.set(key, value);
    return;
  }
  svgCache.set(key, value);
  svgCacheKeys.push(key);
  if (svgCacheKeys.length > SVG_CACHE_MAX) {
    const evict = svgCacheKeys.shift()!;
    svgCache.delete(evict);
  }
}

function hashCode(s: string): string {
  let h = 0;
  for (let i = 0; i < s.length; i++) h = (h * 31 + s.charCodeAt(i)) | 0;
  return `m${(h >>> 0).toString(36)}`;
}

interface MermaidBlock {
  /** Offset of the first char of the opening ```mermaid line. */
  from: number;
  /** Offset just past the closing ``` line. */
  to: number;
  /** The diagram source (lines between the fences, joined by `\n`). */
  code: string;
}

const FENCE_OPEN_RE = /^\s*```\s*mermaid\b/i;
const FENCE_CLOSE_RE = /^\s*```\s*$/;

function findMermaidBlocks(state: EditorState): MermaidBlock[] {
  const doc = state.doc;
  const blocks: MermaidBlock[] = [];
  let inBlock = false;
  let blockFrom = -1;
  let codeLines: string[] = [];

  for (let i = 1; i <= doc.lines; i++) {
    const line = doc.line(i);
    const text = line.text;
    if (!inBlock) {
      if (FENCE_OPEN_RE.test(text)) {
        inBlock = true;
        blockFrom = line.from;
        codeLines = [];
      }
      continue;
    }
    if (FENCE_CLOSE_RE.test(text)) {
      blocks.push({ from: blockFrom, to: line.to, code: codeLines.join("\n") });
      inBlock = false;
      continue;
    }
    codeLines.push(text);
  }
  return blocks;
}

class MermaidWidget extends WidgetType {
  constructor(private readonly code: string) {
    super();
  }

  eq(other: MermaidWidget): boolean {
    return other.code === this.code;
  }

  toDOM(view: EditorView): HTMLElement {
    // Outer container provides vertical breathing room via padding —
    // CM's height map measures padding but not margin. Putting margin on
    // the styled `.cm-mermaid-embed` (as we used to) drifted clicks below
    // every diagram. Padding on this outer keeps the spacing without
    // extending the bordered/backgrounded box of the embed itself.
    const outer = document.createElement("div");
    outer.className = "cm-mermaid-outer";

    // wrap = the clipping viewport (overflow hidden, bordered box). pan = the
    // inner element the zoom transform is applied to; the SVG lives inside it.
    const wrap = document.createElement("div");
    wrap.className = "cm-mermaid-embed";
    wrap.setAttribute("role", "img");
    wrap.setAttribute("aria-label", "mermaid diagram — double-click to edit, pinch to zoom");
    outer.appendChild(wrap);

    const pan = document.createElement("div");
    pan.className = "cm-mermaid-pan";
    wrap.appendChild(pan);

    // The diagram renders asynchronously and inflates the widget's height
    // after CodeMirror has already cached its line-position table. Without
    // re-measuring, every click below this widget lands on the wrong line
    // (off by however many px the SVG grew). Observe the wrap and ask
    // CodeMirror to re-measure on every size change — cheap, and the
    // observer is torn down in destroy() below. Same fix applies to
    // image-embed for the same reason. (The zoom transform is visual-only,
    // so it never changes wrap's layout height — only the async SVG does.)
    const ro = new ResizeObserver(() => view.requestMeasure());
    ro.observe(wrap);
    (outer as { __cmRO?: ResizeObserver }).__cmRO = ro;

    // In-place pinch/pan. Attached once; the handler reads live box size so it
    // stays correct as the SVG fills in. Cleanup torn down in destroy().
    (outer as { __cmZoom?: () => void }).__cmZoom = attachPinchZoom(wrap, pan);

    const hash = hashCode(this.code);
    const cached = svgCache.get(hash);
    if (cached) {
      // Self-heal: any cache entry written before we set
      // suppressErrorRendering: true could contain mermaid's bomb SVG.
      // Detect those by their identifying text and force a fresh render
      // so the catch block can replace the bomb with our inline error.
      if (cached.includes("Syntax error in text") || cached.includes("aria-roledescription=\"error\"")) {
        svgCache.delete(hash);
      } else {
        pan.innerHTML = sanitizeSvg(cached);
        return outer;
      }
    }

    const loading = document.createElement("div");
    loading.className = "cm-mermaid-loading";
    loading.textContent = "Rendering diagram…";
    pan.appendChild(loading);

    getMermaid()
      .then(async (mermaid) => {
        try {
          const renderId = `${hash}-${Math.random().toString(36).slice(2, 7)}`;
          const { svg } = await mermaid.render(renderId, this.code);
          setSvgCache(hash, svg);
          pan.innerHTML = sanitizeSvg(svg);
        } catch (e) {
          wrap.classList.add("cm-mermaid-error");
          const err = document.createElement("div");
          err.className = "cm-mermaid-error-msg";
          const msg = errMsg(e);
          err.textContent = `⚠ Mermaid parse error — ${msg.split("\n")[0]}`;
          pan.innerHTML = "";
          pan.appendChild(err);
        }
      })
      .catch((e) => {
        wrap.classList.add("cm-mermaid-error");
        pan.textContent = `⚠ Mermaid failed to load: ${e}`;
      });

    return outer;
  }

  destroy(dom: HTMLElement): void {
    (dom as { __cmRO?: ResizeObserver }).__cmRO?.disconnect();
    (dom as { __cmZoom?: () => void }).__cmZoom?.();
  }

  ignoreEvent(): boolean {
    // Let double-click bubble through so edit-mode can toggle on.
    return false;
  }
}

function buildDecorations(state: EditorState): DecorationSet {
  const sel = state.selection.main;
  const editing = isEditMode(state);
  const blocks = findMermaidBlocks(state);
  const out: Range<Decoration>[] = [];

  for (const b of blocks) {
    // Reveal raw source only while user is in edit mode AND the cursor
    // overlaps this specific block. All other blocks in the doc stay
    // rendered — matches table-render's per-node gating.
    if (editing && sel.from <= b.to && sel.to >= b.from) continue;
    out.push(
      Decoration.replace({
        widget: new MermaidWidget(b.code),
        block: true,
      }).range(b.from, b.to),
    );
  }

  return Decoration.set(out, true);
}

const mermaidTheme = EditorView.baseTheme({
  // Outer holds vertical breathing room as padding (counted by CM).
  // The styled embed inside has no margin — see toDOM comment.
  ".cm-mermaid-outer": {
    paddingTop: "8px",
    paddingBottom: "8px",
  },
  ".cm-mermaid-embed": {
    // Clipping viewport for the zoom transform. Padding is 0 so the pan
    // layer's top-left coincides with the box origin (the pinch focal math
    // measures from getBoundingClientRect) — internal breathing room is on
    // the loading/error children instead.
    position: "relative",
    padding: "0",
    borderRadius: "8px",
    background: "var(--color-bg-secondary)",
    border: "1px solid var(--color-border)",
    overflow: "hidden",
    textAlign: "center",
    cursor: "pointer",
  },
  // The transform target. transform-origin top-left keeps the (tx,ty) values
  // in plain box pixels (see mermaid-zoom.ts).
  //
  // will-change is deliberately NOT set statically here. A permanent
  // `will-change: transform` promotes the layer and PINS its raster scale at
  // 1× (the fit-size at first render), so zooming in up-scales a 1× bitmap →
  // blurry diagram even though the source is vector SVG. mermaid-zoom-dom
  // toggles will-change ON only during an active gesture (smooth 60fps pinch)
  // and drops it ~200ms after the zoom settles, which lets Chromium
  // re-rasterize the SVG vectors at the zoomed scale → stays crisp.
  ".cm-mermaid-pan": {
    width: "100%",
    transformOrigin: "0 0",
    padding: "12px",
    boxSizing: "border-box",
  },
  ".cm-mermaid-embed svg": {
    maxWidth: "100%",
    height: "auto",
    display: "inline-block",
  },
  // "Fit" pill — appears only while zoomed (toggled inline by the gesture
  // handler). Sits top-right inside the viewport, above the diagram.
  ".cm-mermaid-fit": {
    position: "absolute",
    top: "6px",
    right: "6px",
    zIndex: "2",
    padding: "2px 8px",
    fontSize: "11px",
    lineHeight: "1.4",
    color: "var(--color-text-secondary)",
    background: "color-mix(in srgb, var(--color-bg-primary) 85%, transparent)",
    border: "1px solid var(--color-border)",
    borderRadius: "6px",
    cursor: "pointer",
    backdropFilter: "blur(4px)",
  },
  ".cm-mermaid-fit:hover": {
    color: "var(--color-text-primary)",
    background: "var(--color-bg-primary)",
  },
  // Safety net: even with `<style>` allowed through DOMPurify, an old
  // SVG in the source-hash cache (rendered before that fix) might lack
  // mermaid's color rules. Force readable defaults so existing cached
  // diagrams render with visible text instead of black-on-black labels.
  // Once the cache cycles through, mermaid's own theme rules take over.
  ".cm-mermaid-embed svg text, .cm-mermaid-embed svg .nodeLabel, .cm-mermaid-embed svg .edgeLabel, .cm-mermaid-embed svg .cluster-label":
    {
      fill: "var(--color-text-primary, #e5e5e5)",
      color: "var(--color-text-primary, #e5e5e5)",
    },
  ".cm-mermaid-embed svg .edgePath path, .cm-mermaid-embed svg .flowchart-link":
    {
      stroke: "var(--color-text-secondary, #a0a0a0)",
    },
  ".cm-mermaid-embed svg .arrowheadPath, .cm-mermaid-embed svg .marker": {
    fill: "var(--color-text-secondary, #a0a0a0)",
    stroke: "var(--color-text-secondary, #a0a0a0)",
  },
  ".cm-mermaid-loading": {
    fontSize: "12px",
    color: "var(--color-text-muted)",
    fontFamily: "var(--editor-font-family)",
    textAlign: "center",
    padding: "16px",
  },
  ".cm-mermaid-error": {
    borderColor:
      "color-mix(in srgb, var(--color-danger, #e06c75) 50%, var(--color-border))",
    textAlign: "left",
    cursor: "text",
  },
  ".cm-mermaid-error-msg": {
    fontSize: "12px",
    color: "var(--color-danger, #e06c75)",
    fontFamily: "var(--editor-font-family)",
    whiteSpace: "pre-wrap",
    wordBreak: "break-word",
  },
});

export function mermaidRender() {
  const field = StateField.define<DecorationSet>({
    create(state) {
      return buildDecorations(state);
    },
    update(value, tr: Transaction) {
      const modeToggle = tr.effects.some((e) => e.is(setEditMode));
      if (!tr.docChanged && !tr.selection && !modeToggle) return value;
      return buildDecorations(tr.state);
    },
    provide: (f) => EditorView.decorations.from(f),
  });

  return [field, mermaidTheme];
}
