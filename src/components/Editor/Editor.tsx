import { onMount, onCleanup, createEffect, on, type Accessor } from "solid-js";
import { isMobile } from "../../lib/platform";
import { keyboardInset } from "../../lib/keyboard-inset";
import { formatBarInset } from "../../lib/format-bar-inset";
import { log } from "../../lib/log";
import { Compartment, EditorState } from "@codemirror/state";
import {
  EditorView,
  keymap,
  highlightActiveLine,
  drawSelection,
  rectangularSelection,
} from "@codemirror/view";
import {
  defaultKeymap,
  history,
  historyKeymap,
  indentWithTab,
} from "@codemirror/commands";
import { markdown, markdownLanguage } from "@codemirror/lang-markdown";
import { languages } from "@codemirror/language-data";
import { oneDarkHighlightStyle } from "@codemirror/theme-one-dark";
import {
  syntaxHighlighting,
  defaultHighlightStyle,
  bracketMatching,
  indentOnInput,
  ensureSyntaxTree,
} from "@codemirror/language";
import { closeBrackets, closeBracketsKeymap } from "@codemirror/autocomplete";
import { searchKeymap, highlightSelectionMatches } from "@codemirror/search";
import { wikilinks } from "./extensions/wikilink";
import { externalLinks } from "./extensions/external-link";
import { tagHighlight } from "./extensions/tag-highlight";
import { calloutHighlight } from "./extensions/callout";
import { linkAutocomplete, type NoteInfo } from "./extensions/link-autocomplete";
import { frontmatterHide } from "./extensions/frontmatter-hide";
import { imageEmbed, type ImageLoader } from "./extensions/image-embed";
import { audioEmbed, type AudioLoader, type AudioDeleter } from "./extensions/audio-embed";
import { livePreview } from "./extensions/live-preview";
import { tableRender } from "./extensions/table-render";
import { editMode } from "./extensions/edit-mode";
import { mermaidRender, preloadMermaid } from "./extensions/mermaid-render";
import { toastStore } from "../Toast/Toast";

interface EditorProps {
  content: string;
  onChange: (value: string) => void;
  onNavigate?: (target: string) => void;
  getNotes?: () => NoteInfo[];
  /** When this value changes, scroll to the given line number (1-based). */
  goToLine?: Accessor<number>;
  /** Called when user pastes an image — returns the filename to insert */
  onImagePaste?: (file: File) => Promise<string | null>;
  /** Resolves a roost-relative image path to a loadable URL (e.g. data URL). */
  onLoadImage?: ImageLoader;
  /** Resolves a roost-relative audio path to a data URL for inline playback. */
  onLoadAudio?: AudioLoader;
  /** Optional handler: delete the underlying audio file (move to trash). */
  onDeleteAudio?: AudioDeleter;
  /** When true, editor is locked — typing disabled (read mode). */
  readOnly?: Accessor<boolean>;
  /** Fired once after the CodeMirror EditorView is created. The parent
   *  stores the ref to dispatch programmatic commands (e.g. the mobile
   *  format bar). NOT re-fired on tab switch — `view.setState` swaps the
   *  doc but keeps the same EditorView instance, so the ref is stable
   *  for the Editor component's lifetime. */
  onReady?: (view: EditorView) => void;
}

function readOnlyExtension(ro: boolean) {
  return [EditorState.readOnly.of(ro), EditorView.editable.of(!ro)];
}

const corvuxTheme = EditorView.theme({
  "&": {
    backgroundColor: "var(--color-bg-primary)",
    color: "var(--color-text-primary)",
    height: "100%",
    fontFamily: "var(--editor-font-family)",
    fontSize: "var(--editor-font-size)",
  },
  ".cm-content": {
    fontFamily: "var(--editor-font-family)",
    fontSize: "var(--editor-font-size)",
    lineHeight: "var(--editor-line-height)",
  },
  ".cm-activeLine": {
    backgroundColor: "color-mix(in srgb, var(--color-bg-secondary) 50%, transparent)",
  },
  ".cm-cursor": {
    borderLeftColor: "var(--color-accent)",
    borderLeftWidth: "2px",
  },
  // Selection. `drawSelection()` paints this layer BEHIND the text, so a
  // translucent tint darkens the background without touching glyph contrast.
  //
  // These used to be --color-bg-hover / --color-bg-active, the 6%/12% pointer
  // tints. At those alphas selected text was almost indistinguishable from
  // unselected text — you could not see what you had grabbed. The dedicated
  // --color-selection tokens run ~2.5x that; see the note in global.css.
  // `!important` beats CodeMirror's own baseTheme rule for this class.
  ".cm-selectionBackground": {
    backgroundColor: "var(--color-selection-inactive) !important",
  },
  "&.cm-focused .cm-selectionBackground": {
    backgroundColor: "var(--color-selection) !important",
  },
});

function createExtensions(
  onChange: (value: string) => void,
  onNavigate: (target: string) => void,
  getNotes: () => NoteInfo[],
  readOnlyCompartment: Compartment,
  initialReadOnly: boolean,
  onImagePaste?: (file: File) => Promise<string | null>,
  onLoadImage?: ImageLoader,
  onLoadAudio?: AudioLoader,
  onDeleteAudio?: AudioDeleter,
) {
  return [
    readOnlyCompartment.of(readOnlyExtension(initialReadOnly)),
    highlightActiveLine(),
    drawSelection(),
    rectangularSelection(),
    indentOnInput(),
    bracketMatching(),
    closeBrackets(),
    highlightSelectionMatches(),
    history(),
    keymap.of([
      ...defaultKeymap,
      ...historyKeymap,
      ...closeBracketsKeymap,
      ...searchKeymap,
      indentWithTab,
    ]),
    markdown({
      base: markdownLanguage,
      codeLanguages: languages,
    }),
    syntaxHighlighting(defaultHighlightStyle, { fallback: true }),
    syntaxHighlighting(oneDarkHighlightStyle),
    corvuxTheme,
    // Wiki-link decorations + Ctrl+Click navigation
    wikilinks(onNavigate),
    // External links: render [text](url) Obsidian-style + click-to-open in
    // the system browser (markdown links, autolinks, and bare URLs).
    externalLinks(),
    // Inline #tag highlighting
    tagHighlight(),
    // Obsidian-style callout blocks
    calloutHighlight(),
    // [[ autocomplete with roost notes
    linkAutocomplete(getNotes),
    // Double-click-to-edit gate used by live-preview / wiki-links / tables
    editMode(),
    // Collapse YAML frontmatter — the Properties panel already renders it
    frontmatterHide(),
    // Render GFM tables as real HTML tables when the cursor isn't editing them
    tableRender(onNavigate),
    // Obsidian-style live preview: hide markers, scale headings, style emphasis
    livePreview(),
    // Render ```mermaid fenced blocks as SVG below the source. Mermaid
    // is dynamic-imported on first sighting so notes without diagrams
    // never pay the bundle cost.
    mermaidRender(),
    // Inline-render markdown image references (![alt](.attachments/...))
    // as a block widget below the source line. Web-based loader (returns a
    // data URL) avoids the Tauri asset-protocol capability dance.
    ...(onLoadImage ? [imageEmbed(onLoadImage)] : []),
    // Same shape as image-embed, but for audio extensions. image-embed
    // already filters out audio paths so the two extensions don't fight.
    ...(onLoadAudio ? [audioEmbed(onLoadAudio, onDeleteAudio)] : []),
    EditorView.updateListener.of((update) => {
      if (update.docChanged) {
        onChange(update.state.doc.toString());
      }
    }),
    // Keep the caret clear of the on-screen keyboard AND of the mobile
    // format bar. Under `interactive-widget=overlays-content` (Android) the
    // layout viewport no longer shrinks for the keyboard, so CM would
    // consider a caret sitting behind the keyboard "already visible" and
    // never scroll it up. Reporting the height as a bottom scroll margin
    // makes every scroll-into-view (which CM runs on user input) land the
    // caret above it. The format bar stacks on top of the keyboard, so its
    // height adds to the same margin — without it the caret hides behind the
    // toolbar on the last visible line. Returns null when neither is present
    // so desktop is untouched.
    EditorView.scrollMargins.of(() => {
      const inset = keyboardInset() + formatBarInset();
      return inset > 0 ? { bottom: inset } : null;
    }),
    EditorView.lineWrapping,
    // Image paste handler — save to roost/.attachments and insert a plain
    // markdown link. No inline rendering for this phase; just capture + link.
    EditorView.domEventHandlers({
      paste(event, view) {
        if (!onImagePaste) return false;
        const cd = event.clipboardData;
        if (!cd) return false;

        // Some OSes/browsers put pasted images in `files` rather than `items`
        // (e.g. drag-and-drop into clipboard, screenshots on Windows), so check
        // both. Prefer a real file object — `item.getAsFile()` returns null for
        // items whose kind is "string".
        let file: File | null = null;
        if (cd.files && cd.files.length > 0) {
          for (const f of cd.files) {
            if (f.type.startsWith("image/")) {
              file = f;
              break;
            }
          }
        }
        if (!file && cd.items) {
          for (const item of cd.items) {
            if (item.kind === "file" && item.type.startsWith("image/")) {
              file = item.getAsFile();
              if (file) break;
            }
          }
        }
        if (!file) return false;

        event.preventDefault();

        // Unique placeholder token — survives multiple concurrent pastes
        // without the earlier `indexOf("![uploading...]()")` collision.
        const token = `${Date.now()}-${Math.random().toString(36).slice(2, 8)}`;
        const placeholder = `![uploading-${token}]()`;
        const cursor = view.state.selection.main.head;
        view.dispatch({
          changes: { from: cursor, insert: placeholder },
        });

        onImagePaste(file)
          .then((filename) => {
            const doc = view.state.doc.toString();
            const start = doc.indexOf(placeholder);
            if (start === -1) return;

            // Standard markdown — works with or without an embed renderer,
            // and the relative path makes the saved file discoverable.
            const replacement = filename
              ? `![${filename}](.attachments/${filename})`
              : "";
            view.dispatch({
              changes: {
                from: start,
                to: start + placeholder.length,
                insert: replacement,
              },
            });

            if (filename) {
              toastStore.show(`Image saved: ${filename}`, "success", 2500);
            } else {
              toastStore.show("Failed to save pasted image", "error");
            }
          })
          .catch((err) => {
            const doc = view.state.doc.toString();
            const start = doc.indexOf(placeholder);
            if (start !== -1) {
              view.dispatch({
                changes: { from: start, to: start + placeholder.length, insert: "" },
              });
            }
            log.error("editor", "image paste failed", err);
            toastStore.show(`Paste failed: ${err}`, "error");
          });
        return true;
      },
    }),
  ];
}

export default function Editor(props: EditorProps) {
  let containerRef!: HTMLDivElement;
  let view: EditorView | undefined;
  let skipNextUpdate = false;
  const readOnlyCompartment = new Compartment();

  const handleNavigate = (target: string) => {
    props.onNavigate?.(target);
  };

  const handleGetNotes = (): NoteInfo[] => {
    return props.getNotes?.() ?? [];
  };

  // Tab switching is the dominant hot path. Re-running `createExtensions`
  // per switch was instantiating ~20 extensions including the markdown
  // language parser (+ language-data, + the live-preview/table/mermaid
  // wrappers) every time — 150–300ms of WebView main-thread work on
  // Android. Cache once per editor instance and reuse on every
  // setState. Safety: the closures captured here read `props` lazily
  // (Solid prop getters), so prop updates are picked up at call time;
  // `readOnly` mutations go through `readOnlyCompartment`, not extension
  // recreation. Lifetime is the Editor component instance — when the
  // component unmounts, the cache goes with it.
  let cachedExtensions: ReturnType<typeof createExtensions> | null = null;
  function getExtensions() {
    if (cachedExtensions === null) {
      cachedExtensions = createExtensions(
        (val) => {
          skipNextUpdate = true;
          props.onChange(val);
        },
        handleNavigate,
        handleGetNotes,
        readOnlyCompartment,
        props.readOnly?.() ?? false,
        props.onImagePaste,
        props.onLoadImage,
        props.onLoadAudio,
        props.onDeleteAudio,
      );
    }
    return cachedExtensions;
  }

  // Mobile WebView budgets are tighter — let the parser block longer on
  // desktop where the user is more sensitive to "first paint shows raw
  // markdown for a beat", and accept progressive highlighting on Android
  // where 150ms blocking is multiple frames of jank.
  const SYNTAX_TREE_DEADLINE_MS = isMobile() ? 50 : 150;

  onMount(() => {
    const state = EditorState.create({
      doc: props.content,
      extensions: getExtensions(),
    });

    view = new EditorView({
      state,
      parent: containerRef,
    });

    // Pre-warm mermaid only when the initial doc actually contains a
    // fenced diagram. Overlaps the ~800 KB dynamic-import with
    // CodeMirror's own parse so the first render doesn't stack the
    // two waits serially. Notes without diagrams still pay nothing.
    if (/^```\s*mermaid\b/im.test(props.content)) {
      void preloadMermaid();
    }

    // Nudge CodeMirror to finish parsing the first chunk of markdown
    // before the first paint — otherwise live-preview, tables, and
    // mermaid render as raw source for a second or two while the
    // incremental parser catches up. Desktop gets 150ms (well under a
    // frame budget at 60 Hz on real hardware); mobile drops to 50ms
    // and accepts progressive highlighting that fills in async.
    ensureSyntaxTree(state, state.doc.length, SYNTAX_TREE_DEADLINE_MS);

    // Hand the live EditorView to the parent so it can dispatch
    // programmatic commands (mobile format bar, palette actions). No-op
    // when no callback is supplied (desktop with no parent command surface).
    props.onReady?.(view);
  });

  // React to read-only toggles from the parent without rebuilding the editor.
  createEffect(
    on(
      () => props.readOnly?.() ?? false,
      (ro) => {
        if (!view) return;
        view.dispatch({
          effects: readOnlyCompartment.reconfigure(readOnlyExtension(ro)),
        });
      },
      { defer: true }
    )
  );

  // When content changes from outside (file switch), replace editor content
  createEffect(
    on(
      () => props.content,
      (newContent) => {
        if (skipNextUpdate) {
          skipNextUpdate = false;
          return;
        }
        if (view && newContent !== view.state.doc.toString()) {
          // Tab swap = full doc replace. Using `view.dispatch({changes})`
          // here records the swap in the undo history *and* leaves the
          // previous file's history sitting in memory; after ~20 switches
          // through non-trivial markdown the heap balloons into the GBs
          // and the WebView main thread starves. `setState` rebuilds a
          // fresh EditorState — undo stack, viewport, selection all reset
          // to per-file defaults, which is the correct semantic anyway
          // (you don't want Ctrl-Z in file B to revert an edit from A).
          // Reuse the cached extension array — see `getExtensions`
          // comment above. `setState` still creates a fresh `EditorState`
          // (clearing the previous file's history), which is what the
          // memory comment below preserves; only the extension
          // instantiation cost is amortized.
          view.setState(
            EditorState.create({
              doc: newContent,
              extensions: getExtensions(),
            })
          );
          // Force CM6 to recompute viewport + line geometry after layout
          // settles. Without this, switching tabs leaves CM6 with a stale
          // viewport — editor body renders blank.
          //
          // Single rAF works for plain click-to-switch (only activeTabPath
          // changes). Closing a tab triggers THREE simultaneous DOM updates
          // (setTabs shrinks the tab bar, setActiveTabPath swaps content,
          // Properties re-renders for the new frontmatter) — those layout
          // shifts can cascade across two frames. Nested rAF fires once
          // after the first paint settles AND once after the second so
          // both code paths land on a correctly-measured editor.
          const v = view;
          requestAnimationFrame(() => {
            v.requestMeasure();
            requestAnimationFrame(() => v.requestMeasure());
          });
          // Same warm-up as onMount — applies when the user switches
          // to a different note with diagrams.
          if (/^```\s*mermaid\b/im.test(newContent)) {
            void preloadMermaid();
          }
          ensureSyntaxTree(view.state, view.state.doc.length, SYNTAX_TREE_DEADLINE_MS);
        }
      }
    )
  );

  // Scroll to a specific line when goToLine signal changes
  createEffect(
    on(
      () => props.goToLine?.(),
      (lineNum) => {
        if (!view || !lineNum || lineNum < 1) return;
        const doc = view.state.doc;
        const line = Math.min(lineNum, doc.lines);
        const lineInfo = doc.line(line);
        view.dispatch({
          selection: { anchor: lineInfo.from },
          scrollIntoView: true,
          effects: EditorView.scrollIntoView(lineInfo.from, { y: "center" }),
        });
        view.focus();
      }
    )
  );

  // When the keyboard opens (inset grows), the caret may already be sitting
  // behind it — CM only re-scrolls on transactions, so without a nudge the
  // caret stays hidden until the next keystroke. Scroll it into view once on
  // the open transition. We only react to meaningful growth (not shrink or
  // per-frame jitter) so this never fights the user's own scrolling.
  let prevInset = 0;
  createEffect(
    on(keyboardInset, (inset) => {
      const grew = inset > prevInset + 8;
      prevInset = inset;
      if (!grew || !view || !view.hasFocus) return;
      const head = view.state.selection.main.head;
      requestAnimationFrame(() => {
        view?.dispatch({
          effects: EditorView.scrollIntoView(head, { y: "nearest" }),
        });
      });
    })
  );

  onCleanup(() => {
    view?.destroy();
  });

  return <div ref={containerRef} class="h-full w-full overflow-hidden" />;
}
