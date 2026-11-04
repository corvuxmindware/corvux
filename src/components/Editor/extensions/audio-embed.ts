/**
 * CodeMirror 6 extension that renders markdown audio references inline as
 * an <audio controls> widget *in place of* the source text.
 *
 * Audio embeds replace the `![alt](.attachments/...)` span with the player
 * widget. The raw link is non-informative for audio, and showing both the
 * player and the markdown side-by-side reads as a broken UI. To delete the
 * embed, backspace at the start of the next line or select the surrounding
 * region.
 *
 * Markdown form: `![alt](.attachments/recording-XXX.webm)` — the same shape
 * the voice-memo flow inserts. Recognises common audio extensions; anything
 * else falls through (image-embed handles it or it stays raw).
 *
 * NOTE: decorations come from a `StateField` (not a `ViewPlugin`) to stay
 * compatible with CM's block-decoration constraint that image-embed also
 * follows. Replace decorations themselves don't require this, but keeping
 * the pattern symmetric makes it easier to swap in a block-replace later.
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
import { render } from "solid-js/web";
import { toastStore } from "../../Toast/Toast";
import AudioPlayer from "../../Audio/AudioPlayer";
import type { AudioResource } from "../../../lib/attachments";
import { syncStatusStore } from "../../../stores/sync";
import { visibleAttachmentLabel } from "./attachment-label";
import { attachmentMissingNotice } from "./attachment-missing-notice";

/// Dispatched when an audio blob arrives via sync, so its embed re-runs the
/// loader and the player appears (instead of the "not synced yet" notice).
/// Mirrors image-embed's invalidation; see invalidateAudioInEditors.
const invalidateAudioEffect = StateEffect.define<string>();

/// Live editor views mounting the audio extension.
const editorsWithAudio = new Set<EditorView>();

/// Re-decorate the audio embed for `rel` in every open editor (a small
/// audio clip just synced to disk). Called alongside the image equivalent
/// from the sync event handler.
export function invalidateAudioInEditors(rel: string): void {
  for (const view of editorsWithAudio) {
    view.dispatch({ effects: invalidateAudioEffect.of(rel) });
  }
}

export type AudioLoader = (relPath: string) => Promise<AudioResource | null>;

/**
 * Optional delete handler. Called when the user clicks the embed's X.
 * Implementation moves the underlying file to the roost trash (recoverable)
 * and resolves `true` on success — the widget then removes the matching
 * `![...](relPath)` line from the editor doc. Resolving `false` (e.g. the
 * user cancelled, or the IPC failed) leaves the doc untouched.
 */
export type AudioDeleter = (relPath: string) => Promise<boolean>;

// `![alt](path)` — same regex as image-embed; the extension test below filters
// by file extension so the two extensions don't fight over the same line.
const MEDIA_RE = /!\[([^\]\n]*)\]\(([^)\n]+)\)/g;

// Audio extensions we recognise. Mirrors the Rust sanitize_extension allowlist
// (commands/attachments.rs) — keep in sync if formats are added.
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
  const ext = path.slice(lastDot + 1).toLowerCase();
  // Strip any URL fragment / query that might tag along ("file.webm?v=1").
  const clean = ext.replace(/[?#].*$/, "");
  return AUDIO_EXTS.has(clean);
}

interface AudioMatch {
  alt: string;
  path: string;
  from: number;
  to: number;
}

// Cap matches the image-embed cache rationale — each data URL is ~1.33× the
// raw audio bytes; a 25 MB clip becomes a ~33 MB string in JS heap. 16 covers
// a session of normal note-switching while keeping worst-case heap bounded.
const AUDIO_CACHE_MAX = 16;

export function audioEmbed(loader: AudioLoader, onDelete?: AudioDeleter) {
  const cache = new Map<string, Promise<AudioResource | null>>();
  // Forward-declared so AudioWidget's click handler can read the live
  // decoration set at click time (A-07). The StateField object below
  // assigns into this slot; the closure capture means widgets created
  // by `buildDecorations` see the same StateField instance forever.
  let field: StateField<DecorationSet>;

  function getAudio(relPath: string): Promise<AudioResource | null> {
    const existing = cache.get(relPath);
    if (existing) {
      cache.delete(relPath);
      cache.set(relPath, existing);
      return existing;
    }
    const p = loader(relPath).catch(() => null);
    cache.set(relPath, p);
    if (cache.size > AUDIO_CACHE_MAX) {
      const oldest = cache.keys().next().value;
      if (oldest !== undefined) {
        // Revoke the evicted entry's blob URL — loader returns `blob:`
        // URLs (see attachments.ts loadAudio) which retain their Blob
        // bytes until revoke. Without this, every eviction permanently
        // leaks the underlying audio buffer.
        const evicted = cache.get(oldest);
        cache.delete(oldest);
        evicted?.then((res) => {
          if (res?.url && res.url.startsWith("blob:")) {
            try { URL.revokeObjectURL(res.url); } catch { /* ignore */ }
          }
        });
      }
    }
    return p;
  }

  class AudioWidget extends WidgetType {
    constructor(
      private readonly clip: AudioMatch,
      // Bumped on blob-arrival invalidation so eq() returns false and CM
      // rebuilds the widget (re-running the loader) for an unchanged path.
      private readonly generation: number,
    ) {
      super();
    }

    eq(other: AudioWidget): boolean {
      return (
        other.generation === this.generation &&
        other.clip.path === this.clip.path &&
        other.clip.alt === this.clip.alt
      );
    }

    toDOM(view: EditorView): HTMLElement {
      const wrap = document.createElement("span");
      wrap.className = "cm-audio-embed";
      // Replace decorations render where the original text was. The span
      // stays inline (block: false on the decoration) so editing around
      // it doesn't fight CM's selection model.
      wrap.contentEditable = "false";

      const ro = new ResizeObserver(() => view.requestMeasure());
      ro.observe(wrap);
      (wrap as { __cmRO?: ResizeObserver }).__cmRO = ro;

      // Disposal flag — the getAudio() promise can resolve AFTER CM has
      // already torn the widget down (note switch, line deleted in
      // flight). Without this guard `render()` mounts a Solid root into
      // an orphan node, the AudioContext spins up forever, and the
      // dispose handle goes nowhere because destroy() already ran.
      const disposed = { v: false };
      (wrap as { __cmDisposed?: { v: boolean } }).__cmDisposed = disposed;

      const clipPath = this.clip.path;
      const captionLabel = visibleAttachmentLabel(this.clip.alt, clipPath);

      // Mount point for the Solid AudioPlayer component. We need a Solid
      // tree here because CodeMirror widgets are imperative DOM and we
      // can't ship JSX directly — `render()` bridges the two worlds.
      // The dispose handle is stashed on the wrapper so destroy() can
      // tear it down without leaking signals.
      const mount = document.createElement("span");
      mount.style.display = "block";
      wrap.appendChild(mount);

      // Resolve the URL (or render an inline error state) then mount the
      // Solid player against the resolved URL. We can't render before
      // resolving because AudioPlayer assumes a valid src at mount.
      // Resolve THIS widget's live document range at click time, not at
      // construction time. CM keeps the widget DOM across rebuilds when
      // WidgetType.eq() returns true (same path + alt), so the closure-
      // captured `this.clip.from/to` can drift relative to the live
      // decoration. Worse: two embeds of the same file would both match
      // a text-based lookup. `view.posAtDOM(wrap)` resolves the
      // widget's live document position; the StateField's decoration
      // set then tells us which range houses it.
      const resolveLiveRange = (): { from: number; to: number } | null => {
        try {
          const pos = view.posAtDOM(wrap);
          const decoSet = view.state.field(field, false);
          if (!decoSet) return null;
          let hit: { from: number; to: number } | null = null;
          decoSet.between(Math.max(0, pos - 1), pos + 1, (from, to) => {
            hit = { from, to };
            return false; // stop on first
          });
          return hit;
        } catch {
          return null;
        }
      };

      const handleDelete = onDelete
        ? async () => {
            const fname = clipPath.split("/").pop() ?? clipPath;
            // No confirm: window.confirm() is unreliable in Tauri's
            // WebView and the file lands in .corvux-trash anyway.
            const removed = await onDelete(clipPath);
            if (!removed) {
              toastStore.show(`Failed to delete ${fname}`, "error");
              return;
            }
            toastStore.show(
              `Deleted ${fname} — recoverable from trash`,
              "info",
              3500,
            );

            // Prefer the live decoration range — handles the
            // duplicate-embed case correctly. Fall back to text search
            // if posAtDOM throws (widget detached mid-click).
            const doc = view.state.doc;
            const live = resolveLiveRange();
            let from: number;
            let to: number;
            if (live) {
              const line = doc.lineAt(live.from);
              const wholeMatch = doc.sliceString(live.from, live.to);
              const removeWholeLine = line.text.trim() === wholeMatch.trim();
              from = removeWholeLine ? line.from : live.from;
              to = removeWholeLine
                ? Math.min(doc.length, line.to + 1)
                : live.to;
            } else {
              const text = doc.toString();
              const needle = `(${clipPath})`;
              const idx = text.indexOf(needle);
              if (idx < 0) return;
              let mdStart = idx;
              while (mdStart > 0 && text[mdStart] !== "!") mdStart--;
              if (text[mdStart] !== "!" || text[mdStart + 1] !== "[") return;
              const mdEnd = idx + needle.length;
              const line = doc.lineAt(mdStart);
              const wholeMatch = text.slice(mdStart, mdEnd);
              const removeWholeLine = line.text.trim() === wholeMatch.trim();
              from = removeWholeLine ? line.from : mdStart;
              to = removeWholeLine
                ? Math.min(doc.length, line.to + 1)
                : mdEnd;
            }
            view.dispatch({ changes: { from, to, insert: "" } });
          }
        : undefined;

      getAudio(clipPath).then((res) => {
        if (disposed.v) return; // widget gone before load resolved
        if (!res) {
          // Build the "missing file" notice via textContent, not innerHTML:
          // clipPath comes from untrusted note content (a synced/imported note
          // could craft `![x](<img onerror=...>.webm)`), so interpolating it
          // into innerHTML is a stored-XSS sink. Security audit 2026-05-30.
          const missing = document.createElement("span");
          missing.className = "corvux-audio";
          missing.style.padding = "8px 12px";
          const missingCaption = document.createElement("span");
          missingCaption.className = "corvux-audio-caption";
          missingCaption.style.paddingLeft = "0";
          // If sync is on, an absent clip is most likely still in transit or
          // waiting on the source device. Say so gently rather than the
          // alarming "missing"; textContent keeps the XSS-safe path intact.
          missingCaption.textContent = attachmentMissingNotice({
            syncOn:
              syncStatusStore.listening() ||
              syncStatusStore.connectedPeers() > 0,
            path: clipPath,
            kind: "audio",
          }).text;
          missing.appendChild(missingCaption);
          mount.replaceChildren(missing);
          return;
        }
        const dispose = render(
          () =>
            AudioPlayer({
              url: res.url,
              buffer: res.buffer,
              label: captionLabel,
              onDelete: handleDelete,
            }),
          mount,
        );
        (wrap as { __cmDispose?: () => void }).__cmDispose = dispose;
      });

      return wrap;
    }

    destroy(dom: HTMLElement): void {
      // Flip first — any in-flight getAudio().then now bails before it
      // mounts a Solid root into a detached node.
      const dis = (dom as { __cmDisposed?: { v: boolean } }).__cmDisposed;
      if (dis) dis.v = true;
      (dom as { __cmRO?: ResizeObserver }).__cmRO?.disconnect();
      // Solid mount lifetime is bound to the widget — without this the
      // signals + AudioContext refs keep ticking after the widget is
      // unmounted (note switched, line deleted) and leak.
      (dom as { __cmDispose?: () => void }).__cmDispose?.();
      // A-11 note: we intentionally do NOT evict the cache entry here.
      // The cache is per-editor and benefits re-renders / note-back-and-
      // forth (the LRU bound caps worst-case retained set at
      // AUDIO_CACHE_MAX entries). The reviewer's concern was about
      // never-resolved promises; those still occupy a slot but they're
      // bounded by the LRU and revoked on eviction. Re-fetching on every
      // note open would dwarf the bounded-leak cost.
    }

    // Audio controls need pointer/keyboard events; let CM forward them.
    ignoreEvent(): boolean {
      return false;
    }
  }

  // Bumped when a synced audio blob invalidates this editor's cache.
  let generation = 0;

  // Drop a cached entry and revoke its blob URL (mirrors the LRU evict) so
  // re-decoration re-runs the loader and we don't leak the old Blob.
  function evictAudio(relPath: string): boolean {
    const existing = cache.get(relPath);
    if (!existing) return false;
    cache.delete(relPath);
    existing.then((res) => {
      if (res?.url && res.url.startsWith("blob:")) {
        try {
          URL.revokeObjectURL(res.url);
        } catch {
          /* ignore */
        }
      }
    });
    return true;
  }

  function buildDecorations(doc: Text): DecorationSet {
    const ranges: Array<{ from: number; to: number; value: Decoration }> = [];

    for (let i = 1; i <= doc.lines; i++) {
      const line = doc.line(i);
      const text = line.text;
      MEDIA_RE.lastIndex = 0;
      let match: RegExpExecArray | null;
      while ((match = MEDIA_RE.exec(text)) !== null) {
        const path = match[2].trim();
        if (!isAudioPath(path)) continue;
        const from = line.from + match.index;
        const to = from + match[0].length;
        const clip: AudioMatch = { alt: match[1], path, from, to };
        ranges.push({
          from,
          to,
          value: Decoration.replace({ widget: new AudioWidget(clip, generation) }),
        });
      }
    }

    return Decoration.set(
      ranges.map((r) => r.value.range(r.from, r.to)),
      true,
    );
  }

  field = StateField.define<DecorationSet>({
    create(state) {
      return buildDecorations(state.doc);
    },
    update(deco: DecorationSet, tr: Transaction) {
      // A synced audio blob landed: evict its cached (null) load and bump
      // the generation so the widget rebuilds and re-runs the loader.
      let invalidated = false;
      for (const e of tr.effects) {
        if (e.is(invalidateAudioEffect) && evictAudio(e.value)) {
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

  // Track this editor's view so invalidateAudioInEditors can reach it.
  const registry = ViewPlugin.define((view) => {
    editorsWithAudio.add(view);
    return {
      destroy() {
        editorsWithAudio.delete(view);
      },
    };
  });

  return [field, registry, audioTheme];
}

// Minimal CM-side theming — the actual player chrome lives in
// styles/global.css under `.corvux-audio*` so the Solid AudioPlayer can
// share it. We only need the outer wrapper sizing here.
const audioTheme = EditorView.baseTheme({
  ".cm-audio-embed": {
    display: "inline-block",
    width: "100%",
    verticalAlign: "top",
    paddingTop: "4px",
    paddingBottom: "8px",
  },
});
