/**
 * Mobile markdown formatting toolbar.
 *
 * A compact row of buttons that floats just above the on-screen keyboard
 * when the user is editing a note on a touch device — Obsidian-style.
 * Each tap dispatches one CodeMirror transaction via the lifted view ref
 * (see `Editor.tsx`'s `onReady` prop) and refocuses the editor so the
 * keyboard stays open.
 *
 * Visibility: only when ALL of —
 *   - touch platform (`isMobile()` covers phones, iPad, Android tablets),
 *   - a file tab is active (not graph view),
 *   - editor is unlocked (write mode). Read mode keeps the chrome clean,
 *   - chrome isn't auto-hidden by scrolling (matches MobileBottomBar).
 *
 * It deliberately does NOT require the keyboard to be up. It used to, and
 * that made the whole bar undiscoverable: non-technical testers reported
 * "I can't add checkboxes / bullets" because the only way to see the
 * buttons was to already be typing, and the only way to know markdown was
 * to already know markdown. Now the buttons are on screen as soon as a
 * note is editable, so tapping "checkbox" is a discoverable first move.
 * Every command routes through `applySpec`, which calls `view.focus()` —
 * so a tap with the keyboard down inserts the markup AND opens the
 * keyboard, and the bar re-docks to it.
 *
 * Positioning: three cases, because "keyboard is open" and "keyboard
 * covers part of the layout viewport" are different questions (see
 * `keyboard-inset.ts`).
 *   - Overlay-mode keyboard (`keyboardInset() > 0`): dock on top of the
 *     keyboard, full width. The CSS var updates per-frame during the
 *     open/close animation so the bar slides along with it.
 *   - Resize-mode keyboard (`keyboardOpen()` but inset 0): the viewport
 *     already ends at the keyboard, so sit at its bottom. MobileBottomBar
 *     hides whenever the keyboard is open, so that space is free.
 *   - Idle: a floating pill parked above MobileBottomBar.
 *     `BOTTOM_BAR_CLEARANCE` is that bar's own footprint — it docks at
 *     `safe-area + 10px` and its tallest control is `w-12 h-12` inside
 *     `py-1`, so 48 + 8 + 10 = 66px, plus an 8px breathing gap.
 *
 * v1 button set (Core, ~8 + overflow): H / B / I / list / checkbox /
 * quote / code / wikilink, plus an overflow (⋮) sheet with tag, hr,
 * fenced code block, image, undo, redo. Per the v1 design decision,
 * tablet-landscape falls under desktop chrome and is hidden by the
 * `isMobile()` gate; tightening that is a one-line follow-up.
 */

import { Show, createSignal, createEffect, onCleanup, For, type JSX } from "solid-js";
import { EditorView } from "@codemirror/view";
import { undo, redo } from "@codemirror/commands";
import { isMobile } from "../../lib/platform";
import { keyboardInset, keyboardOpen } from "../../lib/keyboard-inset";
import { setFormatBarInset } from "../../lib/format-bar-inset";
import { shouldShowFormatBar } from "./visibility";
import { tabStore, isFileTab } from "../../stores/tabs";
import {
  wrapSelection,
  cycleHeading,
  toggleLinePrefix,
  toggleCheckbox,
  insertAtCursor,
} from "./format-commands";

export interface MobileFormatBarProps {
  /** Live CodeMirror view, lifted from the Editor via `onReady`. Null
   *  before the editor mounts / after it unmounts (graph tab open). */
  view: () => EditorView | null;
  /** Optional callback used by the overflow "insert image" entry. On
   *  desktop this lives in the TabBar; on mobile we surface it here. */
  onInsertImage?: () => void;
  /** True while the user is scrolling. Mirrors the `hidden` prop on
   *  MobileBottomBar so the two pieces of bottom chrome slide away and
   *  return together instead of one hovering over a hidden other. Only
   *  consulted when the keyboard is down — while typing the bar is docked
   *  to the keyboard and must never disappear mid-edit. */
  chromeHidden?: () => boolean;
}

/** Vertical space MobileBottomBar occupies above the safe area, plus an
 *  8px gap. See the positioning note in the file header. */
const BOTTOM_BAR_CLEARANCE = "74px";

/** Where MobileBottomBar itself sits when it IS on screen. The format bar
 *  takes this spot while typing, since the bottom bar is hidden then. */
const BOTTOM_DOCK = "10px";

/** Single toolbar button. Same shape as `BarButton` in MobileBottomBar
 *  but local — the bottom-bar one is inlined and not exported, and a
 *  shared <IconButton> would need a broader audit. */
function FormatButton(props: {
  label: string;
  onClick: () => void;
  disabled?: boolean;
  children: JSX.Element;
}) {
  return (
    <button
      type="button"
      // CRITICAL — never let the tap move focus off the editor. The
      // default action of pointerdown on a button is to focus it,
      // which blurs the editor and kicks off the keyboard-dismiss
      // animation on Android before `view.focus()` in applySpec can
      // pull it back. Preventing the default keeps focus put, so the
      // keyboard never twitches and view.focus() becomes a safety net
      // rather than the mechanism. `touch-action: manipulation` also
      // kills the legacy 300ms tap delay on mobile.
      onPointerDown={(e) => e.preventDefault()}
      onClick={props.onClick}
      disabled={props.disabled}
      aria-label={props.label}
      class="flex-1 min-w-0 h-11 flex items-center justify-center rounded-md text-text-secondary active:bg-bg-hover active:text-text-primary transition-colors disabled:opacity-30 [touch-action:manipulation]"
    >
      {props.children}
    </button>
  );
}

// Glyph components kept inline + tiny. Stroke-based so they inherit
// `currentColor` and adapt to the active theme automatically.
function IconHeading() {
  return (
    <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
      <path d="M6 4v16M18 4v16M6 12h12" />
    </svg>
  );
}
function IconBold() {
  return (
    <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round">
      <path d="M7 5h6a3.5 3.5 0 0 1 0 7H7zM7 12h7a3.5 3.5 0 0 1 0 7H7z" />
    </svg>
  );
}
function IconItalic() {
  return (
    <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
      <line x1="19" y1="5" x2="10" y2="5" />
      <line x1="14" y1="19" x2="5" y2="19" />
      <line x1="15" y1="5" x2="9" y2="19" />
    </svg>
  );
}
function IconList() {
  return (
    <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
      <line x1="9" y1="6" x2="20" y2="6" />
      <line x1="9" y1="12" x2="20" y2="12" />
      <line x1="9" y1="18" x2="20" y2="18" />
      <circle cx="4.5" cy="6" r="1.2" fill="currentColor" />
      <circle cx="4.5" cy="12" r="1.2" fill="currentColor" />
      <circle cx="4.5" cy="18" r="1.2" fill="currentColor" />
    </svg>
  );
}
function IconCheckbox() {
  return (
    <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
      <rect x="3.5" y="3.5" width="17" height="17" rx="3" />
      <polyline points="8 12.5 11 15.5 16.5 9" />
    </svg>
  );
}
function IconQuote() {
  return (
    <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
      <line x1="3" y1="6" x2="21" y2="6" />
      <line x1="3" y1="12" x2="14" y2="12" />
      <line x1="3" y1="18" x2="18" y2="18" />
    </svg>
  );
}
function IconCode() {
  return (
    <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
      <polyline points="8 7 3 12 8 17" />
      <polyline points="16 7 21 12 16 17" />
    </svg>
  );
}
function IconWikilink() {
  return (
    <svg width="20" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
      {/* Stylised [[ ]] — two pairs of brackets, the link glyph. */}
      <path d="M7 5L3 12l4 7" />
      <path d="M5 5L1 12l4 7" opacity="0.45" />
      <path d="M17 5l4 7-4 7" />
      <path d="M19 5l4 7-4 7" opacity="0.45" />
    </svg>
  );
}
function IconOverflow() {
  return (
    <svg width="18" height="18" viewBox="0 0 24 24" fill="currentColor">
      <circle cx="12" cy="5" r="1.8" />
      <circle cx="12" cy="12" r="1.8" />
      <circle cx="12" cy="19" r="1.8" />
    </svg>
  );
}
function IconUndo() {
  return (
    <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
      <path d="M3 12a9 9 0 1 1 3 6.7" />
      <polyline points="3 4 3 10 9 10" />
    </svg>
  );
}
function IconRedo() {
  return (
    <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
      <path d="M21 12a9 9 0 1 0-3 6.7" />
      <polyline points="21 4 21 10 15 10" />
    </svg>
  );
}

export default function MobileFormatBar(props: MobileFormatBarProps) {
  const [showOverflow, setShowOverflow] = createSignal(false);

  /** The keyboard physically overlaps the layout viewport, so the bar can
   *  dock onto its top edge. False on WebViews that resize instead. */
  const overlapsKeyboard = () => keyboardInset() > 0;
  /** The user is typing, by either keyboard mode. Drives the bar's shape
   *  (full-width slab vs floating pill) and its scroll-hide exemption. */
  const typing = () => keyboardOpen();

  // Visibility gate. Each signal is reactive; Solid re-runs this on any
  // change. We DON'T render at all when false so taps can't fall through
  // to the editor and dismiss the keyboard mid-format. The rule itself is
  // pure and lives in `visibility.ts` so it can be tested directly.
  const visible = () =>
    shouldShowFormatBar({
      mobile: isMobile(),
      fileTab: isFileTab(tabStore.activeTab()),
      readOnly: tabStore.isReadOnly(),
      keyboardUp: typing(),
      chromeHidden: props.chromeHidden?.() ?? false,
    });

  // Publish the bar's height so the editor can keep its last lines and its
  // caret clear of it (see `lib/format-bar-inset.ts`). Measured in a rAF so
  // `offsetHeight` is read after layout, not during the same tick the node
  // was created in.
  let barEl: HTMLDivElement | undefined;
  createEffect(() => {
    if (!visible()) {
      setFormatBarInset(0);
      return;
    }
    // Touch `typing()` so a dock/park transition re-measures — the two
    // states differ in border and radius, which can differ by a pixel.
    typing();
    requestAnimationFrame(() => {
      if (barEl) setFormatBarInset(barEl.offsetHeight);
    });
  });
  onCleanup(() => setFormatBarInset(0));

  // Each button's handler bails when the view ref is null (editor
  // unmounted). Defensive — `visible()` already filters this out, but a
  // race during tab close could leave the bar mounted for a frame.
  const run = (fn: (v: EditorView) => void) => {
    const v = props.view();
    if (!v) return;
    fn(v);
  };

  return (
    <Show when={visible()}>
      {/* The bar itself. `position: fixed` so it overlays the scrollable
          editor; `bottom: var(--corvux-keyboard-inset)` pins it to the
          top edge of the keyboard. z-39 sits one layer under the
          MobileBottomBar (z-40) — but when the keyboard is up, the
          bottom bar is hidden behind the keyboard so the two never
          compete for the same pixels. */}
      <div
        ref={barEl}
        role="toolbar"
        aria-label="Markdown formatting"
        // Typing: a full-width slab, rounded on top only, reading as an
        // extension of the keyboard. Idle: a floating pill that reads as
        // chrome, matching MobileBottomBar's language.
        class={`fixed left-1 right-1 z-39 flex items-stretch gap-0.5 px-1 py-1 bg-bg-secondary/95 border-border/60 backdrop-blur shadow-lg ${
          typing() ? "rounded-t-xl border-t border-x" : "rounded-xl border"
        }`}
        style={{
          // Three cases — see the positioning note in the file header.
          bottom: overlapsKeyboard()
            ? "var(--corvux-keyboard-inset, 0px)"
            : typing()
              ? `calc(env(safe-area-inset-bottom, 0px) + ${BOTTOM_DOCK})`
              : `calc(env(safe-area-inset-bottom, 0px) + ${BOTTOM_BAR_CLEARANCE})`,
          // Smooth slide with the keyboard open/close. The CSS var
          // updates per-frame during the animation; this transition
          // smooths the few ms of latency between visualViewport events.
          transition: "bottom 120ms linear, opacity 120ms ease",
          "padding-bottom": "calc(4px + env(safe-area-inset-bottom, 0px) * 0)",
        }}
      >
        <FormatButton label="Heading" onClick={() => run(cycleHeading)}>
          <IconHeading />
        </FormatButton>
        <FormatButton label="Bold" onClick={() => run((v) => wrapSelection(v, "**"))}>
          <IconBold />
        </FormatButton>
        <FormatButton label="Italic" onClick={() => run((v) => wrapSelection(v, "*"))}>
          <IconItalic />
        </FormatButton>
        <FormatButton label="Bulleted list" onClick={() => run((v) => toggleLinePrefix(v, "- "))}>
          <IconList />
        </FormatButton>
        <FormatButton label="Checkbox" onClick={() => run(toggleCheckbox)}>
          <IconCheckbox />
        </FormatButton>
        <FormatButton label="Quote" onClick={() => run((v) => toggleLinePrefix(v, "> "))}>
          <IconQuote />
        </FormatButton>
        <FormatButton label="Inline code" onClick={() => run((v) => wrapSelection(v, "`"))}>
          <IconCode />
        </FormatButton>
        <FormatButton
          label="Wikilink"
          onClick={() => run((v) => wrapSelection(v, "[[", "]]"))}
        >
          <IconWikilink />
        </FormatButton>
        <FormatButton
          label="More formatting"
          onClick={() => setShowOverflow(true)}
          disabled={!props.view()}
        >
          <IconOverflow />
        </FormatButton>
      </div>

      {/* Overflow sheet. Modal-style overlay so the user can see what
          they're picking without the keyboard eating the tap. Tapping
          outside dismisses. Each entry is a real <button> for a11y. */}
      <Show when={showOverflow()}>
        <div
          class="fixed inset-0 z-50 flex items-end justify-center"
          onClick={() => setShowOverflow(false)}
        >
          {/* Dim layer — taps here close the sheet. */}
          <div class="absolute inset-0 bg-black/30" />
          <div
            role="menu"
            aria-label="More formatting options"
            onClick={(e) => e.stopPropagation()}
            class="relative w-full max-w-md mb-[calc(var(--corvux-keyboard-inset,0px)+8px)] mx-2 rounded-2xl bg-bg-secondary border border-border/60 shadow-2xl overflow-hidden"
          >
            <For
              each={[
                {
                  label: "Tag",
                  hint: "#tag",
                  run: () => run((v) => insertAtCursor(v, "#", 1)),
                  disabled: !props.view(),
                },
                {
                  label: "Horizontal rule",
                  hint: "---",
                  run: () =>
                    run((v) => insertAtCursor(v, "\n\n---\n\n")),
                  disabled: !props.view(),
                },
                {
                  label: "Code block",
                  hint: "```",
                  run: () =>
                    run((v) => wrapSelection(v, "```\n", "\n```")),
                  disabled: !props.view(),
                },
                {
                  label: "Insert image",
                  hint: "",
                  run: () => props.onInsertImage?.(),
                  disabled: !props.onInsertImage,
                },
                {
                  label: "Undo",
                  hint: "",
                  run: () => run((v) => undo(v)),
                  disabled: !props.view(),
                },
                {
                  label: "Redo",
                  hint: "",
                  run: () => run((v) => redo(v)),
                  disabled: !props.view(),
                },
              ]}
            >
              {(item) => (
                <button
                  type="button"
                  role="menuitem"
                  // Same focus-preservation as FormatButton — see there
                  // for why. Matters most for undo/redo, which don't go
                  // through applySpec and therefore don't even have the
                  // view.focus() safety net. With this preventDefault
                  // the editor never blurs in the first place, so the
                  // keyboard stays up across the sheet open → action →
                  // dismiss round-trip.
                  onPointerDown={(e) => e.preventDefault()}
                  disabled={item.disabled}
                  onClick={() => {
                    item.run();
                    setShowOverflow(false);
                  }}
                  class="w-full px-4 py-3 flex items-center justify-between text-left text-[15px] text-text-primary hover:bg-bg-hover active:bg-bg-hover disabled:opacity-30 transition-colors border-b border-border/30 last:border-b-0 [touch-action:manipulation]"
                >
                  <span>{item.label}</span>
                  {item.hint ? (
                    <code class="text-xs text-text-muted font-mono">{item.hint}</code>
                  ) : null}
                </button>
              )}
            </For>
          </div>
        </div>
      </Show>
    </Show>
  );
}
