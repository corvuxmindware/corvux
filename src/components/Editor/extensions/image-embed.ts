/**
 * CodeMirror 6 extension that renders markdown images (`![alt](path)`) inline
 * in place of the source token.
 *
 * Behaviour:
 *  - The raw markdown remains in the document but is hidden behind a replace
 *    decoration so pasted screenshots read as screenshots, not filenames.
 *  - Multiple images on one line render as separate widgets.
 *  - The `loader` fetches the actual bytes. Typically wired to a Tauri IPC
 *    that returns a base64 data URL — this keeps us off the asset protocol,
 *    which is finicky in WSL Tauri-dev and mocked-browser modes.
 *  - Results are cached per-path inside the extension closure, so switching
 *    notes or scrolling doesn't re-fetch. Cache lives for the editor's
 *    lifetime; call `invalidateImageCache(extension, path)` if you need to
 *    force a reload (not wired up yet — add when rename/delete starts lying).
 *
 * NOTE: Block decorations MUST come from a `StateField`, not a `ViewPlugin`,
 * or CodeMirror throws "Block decorations may not be specified via plugins".
 * See frontmatter-hide.ts for the same pattern.
 */

import {
  Decoration,
  DecorationSet,
  EditorView,
  ViewPlugin,
  WidgetType,
} from "@codemirror/view";
import {
  StateEffect,
  StateField,
  type Text,
  type Transaction,
} from "@codemirror/state";
import { syncStatusStore } from "../../../stores/sync";
import { attachmentMissingNotice } from "./attachment-missing-notice";
import { visibleAttachmentLabel } from "./attachment-label";

export type ImageLoader = (relPath: string) => Promise<string | null>;

/// Dispatched when a `.attachments/...` blob arrives via sync, so the embed
/// for that path re-runs its loader and flips "📎 Not synced yet" to the
/// image — WITHOUT a doc change (reloading the note would drop the mobile
/// keyboard). Carries the roost-relative attachment path.
const invalidateImageEffect = StateEffect.define<string>();

/// Live editor views that mount the image extension. A `ViewPlugin` keeps
/// this current so `invalidateImageInEditors` can reach every open editor
/// without the sync layer knowing about CodeMirror internals.
const editorsWithImages = new Set<EditorView>();

/// Tell every open editor to re-decorate the embed for `rel` (a blob just
/// landed on disk). Safe to call when no editor shows the path — the
/// StateField only rebuilds if the path was actually cached.
export function invalidateImageInEditors(rel: string): void {
  for (const view of editorsWithImages) {
    view.dispatch({ effects: invalidateImageEffect.of(rel) });
  }
}

// Match `![alt](path)` — alt may be empty; path can't contain `)` or newline.
// Kept intentionally simple; nested parens in paths are not a real-world issue
// for pasted attachments (which produce `.attachments/paste-<ts>.png`).
const IMAGE_RE = /!\[([^\]\n]*)\]\(([^)\n]+)\)/g;

// Audio files share the `![]()` syntax (voice-memo feature). audio-embed.ts
// handles those; this extension skips them so a recording doesn't render as
// a broken image. Keep in sync with audio-embed.ts AUDIO_EXTS.
const AUDIO_EXTS = new Set([
  "webm",
  "opus",
  "m4a",
  "mp4",
  "mp3",
  "wav",
  "ogg",
  "aac",
]);

function isAudioPath(path: string): boolean {
  const lastDot = path.lastIndexOf(".");
  if (lastDot < 0) return false;
  const ext = path
    .slice(lastDot + 1)
    .toLowerCase()
    .replace(/[?#].*$/, "");
  return AUDIO_EXTS.has(ext);
}

interface ImageMatch {
  alt: string;
  path: string;
}

// Cap the per-editor image cache. Roost-wide attachment counts can run into
// the hundreds; data-URL strings are ~1.33× the binary size, so without a
// bound, browsing a roost with image-heavy notes accumulates tens to hundreds
// of MB of base64 in JS heap that GC will never reclaim. 64 covers the
// working set for a session of normal note-switching while bounding the
// worst case to a few MB.
const IMAGE_CACHE_MAX = 64;

export function imageEmbed(loader: ImageLoader) {
  // Per-editor LRU cache. Map iteration order = insertion order, so we delete
  // and re-insert on hit to bump to "newest", and evict from the front when
  // the size cap is exceeded. Same pattern as a textbook LRU.
  const cache = new Map<string, Promise<string | null>>();

  function getImage(relPath: string): Promise<string | null> {
    const existing = cache.get(relPath);
    if (existing) {
      cache.delete(relPath);
      cache.set(relPath, existing);
      return existing;
    }
    const p = loader(relPath).catch(() => null);
    cache.set(relPath, p);
    if (cache.size > IMAGE_CACHE_MAX) {
      const oldest = cache.keys().next().value;
      if (oldest !== undefined) cache.delete(oldest);
    }
    return p;
  }

  class ImageWidget extends WidgetType {
    constructor(
      private readonly image: ImageMatch,
      // Bumped on blob-arrival invalidation so `eq` returns false and
      // CodeMirror rebuilds the DOM (re-running the loader) even though the
      // markdown path is unchanged.
      private readonly generation: number,
    ) {
      super();
    }

    eq(other: ImageWidget): boolean {
      if (other.generation !== this.generation) return false;
      return (
        other.image.path === this.image.path &&
        other.image.alt === this.image.alt
      );
    }

    toDOM(view: EditorView): HTMLElement {
      const wrap = document.createElement("span");
      wrap.className = "cm-image-embed";
      wrap.contentEditable = "false";

      // Images load asynchronously — without re-measuring, CodeMirror's
      // line-position cache stays sized to the empty <img> placeholder
      // and clicks below this widget land on the wrong line. Same fix
      // pattern as mermaid-render.ts. Observer is torn down in destroy().
      const ro = new ResizeObserver(() => view.requestMeasure());
      ro.observe(wrap);
      (wrap as { __cmRO?: ResizeObserver }).__cmRO = ro;

      const { alt, path } = this.image;
      const figure = document.createElement("figure");
      figure.className = "cm-image-embed-figure";

      const img = document.createElement("img");
      img.className = "cm-image-embed-img";
      img.alt = alt;
      img.loading = "lazy";
      img.draggable = false;
      figure.appendChild(img);

      let captionEl: HTMLElement | null = null;
      const ensureCaption = (): HTMLElement => {
        if (captionEl) return captionEl;
        const caption = document.createElement("figcaption");
        caption.className = "cm-image-embed-caption";
        figure.appendChild(caption);
        captionEl = caption;
        return caption;
      };

      const label = visibleAttachmentLabel(alt, path);
      if (label) ensureCaption().textContent = label;

      getImage(path).then((url) => {
        if (url) {
          img.src = url;
        } else {
          // File isn't on disk. If sync is on, it is most likely still syncing.
          const notice = attachmentMissingNotice({
            syncOn:
              syncStatusStore.listening() ||
              syncStatusStore.connectedPeers() > 0,
            path,
            kind: "image",
          });
          figure.classList.add(
            notice.variant === "unsynced"
              ? "cm-image-embed-unsynced"
              : "cm-image-embed-error",
          );
          ensureCaption().textContent = notice.text;
        }
      });

      wrap.appendChild(figure);

      return wrap;
    }

    destroy(dom: HTMLElement): void {
      (dom as { __cmRO?: ResizeObserver }).__cmRO?.disconnect();
    }

    // Allow clicks/selection on the preview (opens file, etc. — future).
    ignoreEvent(): boolean {
      return false;
    }
  }

  // Bumped each time a synced blob invalidates this editor's cache, so
  // rebuilt widgets compare unequal and re-run their loader.
  let generation = 0;

  function buildDecorations(doc: Text): DecorationSet {
    const ranges: Array<{ from: number; to: number; value: Decoration }> = [];

    for (let i = 1; i <= doc.lines; i++) {
      const line = doc.line(i);
      const text = line.text;
      IMAGE_RE.lastIndex = 0;
      let match: RegExpExecArray | null;
      while ((match = IMAGE_RE.exec(text)) !== null) {
        const path = match[2].trim();
        // Skip audio paths — audio-embed.ts owns those. Without this gate
        // both extensions would attach a widget to the same line, and the
        // image one would render a broken <img>.
        if (isAudioPath(path)) continue;
        const from = line.from + match.index;
        const to = from + match[0].length;
        const image: ImageMatch = { alt: match[1], path };
        ranges.push({
          from,
          to,
          value: Decoration.replace({
            widget: new ImageWidget(image, generation),
          }),
        });
      }
    }

    return Decoration.set(
      ranges.map((r) => r.value.range(r.from, r.to)),
      true,
    );
  }

  const field = StateField.define<DecorationSet>({
    create(state) {
      return buildDecorations(state.doc);
    },
    update(deco: DecorationSet, tr: Transaction) {
      // A synced blob landed: drop its (likely null) cached load and bump
      // the generation so the widget rebuilds and re-runs the loader.
      let invalidated = false;
      for (const e of tr.effects) {
        // Only rebuild if THIS editor actually has the path cached (i.e.
        // shows the image) — editors not displaying it stay untouched.
        if (e.is(invalidateImageEffect) && cache.has(e.value)) {
          cache.delete(e.value);
          invalidated = true;
        }
      }
      if (invalidated) {
        generation++;
        return buildDecorations(tr.state.doc);
      }
      if (!tr.docChanged) return deco;
      return buildDecorations(tr.state.doc);
    },
    provide: (f) => EditorView.decorations.from(f),
  });

  // Track this editor's view so `invalidateImageInEditors` can reach it.
  const registry = ViewPlugin.define((view) => {
    editorsWithImages.add(view);
    return {
      destroy() {
        editorsWithImages.delete(view);
      },
    };
  });

  return [field, registry, imageTheme];
}

const imageTheme = EditorView.baseTheme({
  // Padding (not margin) for vertical breathing room — CM measures padding
  // in line/widget heights but excludes margin. Margin here drifted click-
  // to-position math below every image. The styled box is the figure inside,
  // so extending the wrapper's padding has no visual side effect.
  ".cm-image-embed": {
    display: "inline-block",
    width: "100%",
    verticalAlign: "top",
    paddingTop: "4px",
    paddingBottom: "12px",
  },
  ".cm-image-embed-figure": {
    margin: "0",
    padding: "6px",
    borderRadius: "6px",
    background: "var(--color-bg-secondary)",
    border: "1px solid var(--color-border)",
    maxWidth: "100%",
    display: "inline-block",
  },
  ".cm-image-embed-img": {
    display: "block",
    maxWidth: "100%",
    maxHeight: "480px",
    borderRadius: "4px",
    objectFit: "contain",
    background: "var(--color-bg-primary)",
  },
  ".cm-image-embed-caption": {
    marginTop: "6px",
    fontSize: "11px",
    color: "var(--color-text-muted)",
    fontFamily: "var(--editor-font-family)",
    wordBreak: "break-all",
  },
  ".cm-image-embed-error": {
    borderColor:
      "color-mix(in srgb, var(--color-danger, #e06c75) 60%, var(--color-border))",
  },
  ".cm-image-embed-error .cm-image-embed-img": {
    display: "none",
  },
  // "Not synced yet" — neutral, not an error. Hide the broken <img> but keep
  // the default (muted) caption colour; no danger border.
  ".cm-image-embed-unsynced .cm-image-embed-img": {
    display: "none",
  },
});
