import { Show, createSignal, type JSX } from "solid-js";
import { tabStore } from "../../stores/tabs";
import { roostStore } from "../../stores/roost";
import CorvuxLogo from "../Brand/CorvuxLogo";
import TabSwitcher from "./TabSwitcher";
import NewNotePrompt from "./NewNotePrompt";

export interface MobileBottomBarProps {
  onOpenQuickSwitcher: () => void;
  onToggleGraph: () => void;
  onToggleChat: () => void;
  graphOpen: boolean;
  /** Pixels reserved by chrome on the left of the editor area (left
   *  sidebar + icon rail). The bar's centre is shifted right by half
   *  of this so it visually centres in the editor, not in the window. */
  leftOffset?: number;
  /** Pixels reserved by chrome on the right (right sidebar). The bar's
   *  centre is shifted left by half of this for the same reason. */
  rightOffset?: number;
  /** When true, slide the bar down off-screen + fade it out (auto-hide
   *  while the user scrolls the note; reveals again when scrolling stops). */
  hidden?: boolean;
}

interface BarButtonProps {
  label: string;
  onClick: () => void;
  disabled?: boolean;
  active?: boolean;
  children: JSX.Element;
}

/**
 * Single bar-button — rounded pill, uniform sizing, disabled-state styling.
 * Kept inline rather than exported because it's only used here; a shared
 * `<IconButton>` would need a broader audit across the app first.
 */
function BarButton(props: BarButtonProps) {
  return (
    <button
      onClick={props.onClick}
      aria-label={props.label}
      disabled={props.disabled}
      class={`w-11 h-11 flex items-center justify-center rounded-full transition-colors ${
        props.disabled
          ? "text-text-muted opacity-40"
          : props.active
            ? "text-accent bg-accent/15"
            : "text-text-secondary hover:text-text-primary active:bg-bg-hover"
      }`}
    >
      {props.children}
    </button>
  );
}

/**
 * Obsidian-style floating bottom toolbar for mobile.
 *
 * Hosts the PRIMARY mobile actions only (trimmed 2026-06-03): open-note
 * (quick switcher), new-note, graph, agent, tab-switcher (live count
 * badge), overflow (⋮). Editing actions (add image, voice memo) and the
 * less-frequent commands moved into the overflow menu — a phone bar should
 * be ~5-6 buttons, not 10.
 *
 * Hidden when:
 * - No Roost is open (file actions make no sense yet).
 * - A modal/sheet is above (handled by z-index layering — the sheets
 *   have z-50, this bar sits at z-40).
 *
 * The bar itself is a dumb presentational shell. Popups it opens
 * (tab-switcher, new-note prompt) live in their own components so
 * they're easy to reuse and reason about in isolation.
 */
export default function MobileBottomBar(props: MobileBottomBarProps) {
  const [showTabs, setShowTabs] = createSignal(false);
  const [showNewNote, setShowNewNote] = createSignal(false);

  const tabCount = () => tabStore.tabs().length;

  return (
    <Show when={roostStore.roostPath()}>
      <div
        role="toolbar"
        aria-label="Navigation toolbar"
        class="fixed z-40 flex items-center gap-0.5 px-2 py-1 rounded-full bg-bg-secondary/90 border border-border/60 shadow-lg backdrop-blur"
        style={{
          // Dock just above the home indicator. The note StatusBar no
          // longer renders on mobile (save-state moved to the top bar), so
          // the bar owns the bottom — a small 10px margin above the safe
          // area is all it needs.
          bottom: "calc(env(safe-area-inset-bottom, 0px) + 10px)",
          // Centre between (leftOffset, viewport-rightOffset) — without
          // these, the bar centres on the full viewport and drifts off
          // the visual centre of the editor when the sidebar or chat
          // pane opens. The math: half the asymmetry shifts the centre,
          // then translate(-50%) re-anchors.
          left: `calc(50% + ${((props.leftOffset ?? 0) - (props.rightOffset ?? 0)) / 2}px)`,
          // translateX(-50%) re-anchors the centre; the Y translation slides
          // the bar fully below the screen while scrolling. Combined with
          // opacity + pointer-events so it's both invisible and inert when
          // hidden. The transition makes the hide/reveal smooth.
          transform: props.hidden
            ? "translateX(-50%) translateY(180%)"
            : "translateX(-50%)",
          opacity: props.hidden ? "0" : "1",
          "pointer-events": props.hidden ? "none" : "auto",
          transition: "transform 240ms ease, opacity 240ms ease",
        }}
      >
        {/* Layout: [Open] [Graph] [ + ] [Agent] [Tabs]. New-note sits in the
            CENTER as the strongest button — the codex review flagged six
            equal-weight icons reading as flat. Overflow (⋮) moved out of the
            bar to the top-bar; tabs keep their live count badge. */}
        <BarButton label="Open note" onClick={props.onOpenQuickSwitcher}>
          {/* File-search composite — distinct from the Search panel's plain
              magnifying glass. A small magnifier inside a document outline
              communicates "find/jump to a specific file" rather than
              "full-text search inside notes". */}
          <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round">
            <path d="M14 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8z" />
            <polyline points="14 2 14 8 20 8" />
            <circle cx="11.5" cy="15" r="1.8" />
            <line x1="13" y1="16.5" x2="14.7" y2="18.2" />
          </svg>
        </BarButton>

        {/* Graph view — same dense 5-node X glyph as desktop IconRail. */}
        <BarButton
          label="Graph view"
          onClick={props.onToggleGraph}
          active={props.graphOpen}
        >
          <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round">
            <circle cx="12" cy="12" r="2.5" fill="currentColor" opacity="0.8" />
            <circle cx="5" cy="5" r="2" />
            <circle cx="19" cy="7" r="2" />
            <circle cx="6" cy="19" r="2" />
            <circle cx="19" cy="17" r="2" />
            <line x1="10" y1="10.5" x2="7" y2="6.5" />
            <line x1="14" y1="11" x2="17" y2="8.5" />
            <line x1="10.5" y1="13.5" x2="8" y2="17.5" />
            <line x1="14" y1="13.5" x2="17.5" y2="15.5" />
          </svg>
        </BarButton>

        {/* New note — CENTER, strongest button: 48px accent-filled circle. */}
        <button
          onClick={() => setShowNewNote(true)}
          aria-label="New note"
          class="w-12 h-12 mx-0.5 flex items-center justify-center rounded-full bg-accent text-bg-primary shadow-md active:scale-95 transition-transform"
        >
          <svg width="22" height="22" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round">
            <path d="M12 5v14M5 12h14" />
          </svg>
        </button>

        {/* AI chat — Corvux brand mark, same as desktop IconRail.
            Bar hides when chat is open so no active state needed. */}
        <BarButton label="Corvux Agent" onClick={props.onToggleChat}>
          <CorvuxLogo alt="" width="20" height="20" class="object-contain" />
        </BarButton>

        {/* Tab switcher with count badge — isolated from BarButton because
            the SVG + centered number needs its own layout. */}
        <button
          onClick={() => setShowTabs(true)}
          aria-label={`Switch tab (${tabCount()} open)`}
          class="w-11 h-11 flex items-center justify-center rounded-full text-text-secondary hover:text-text-primary active:bg-bg-hover transition-colors"
        >
          <div class="relative flex items-center justify-center">
            <svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round">
              <rect x="4" y="4" width="16" height="16" rx="3" />
            </svg>
            <span class="absolute inset-0 flex items-center justify-center text-[11px] font-semibold tabular-nums">
              {tabCount()}
            </span>
          </div>
        </button>
      </div>

      <Show when={showTabs()}>
        <TabSwitcher
          onClose={() => setShowTabs(false)}
          onNewNote={() => setShowNewNote(true)}
        />
      </Show>
      <Show when={showNewNote()}>
        <NewNotePrompt onClose={() => setShowNewNote(false)} />
      </Show>
    </Show>
  );
}
