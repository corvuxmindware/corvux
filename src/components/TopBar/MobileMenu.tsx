import { For, Show, type JSX } from "solid-js";
import BottomSheet from "../BottomSheet/BottomSheet";
import SheetActionRow from "../BottomSheet/SheetActionRow";

interface MobileMenuItem {
  id: string;
  label?: string;
  icon?: JSX.Element;
  onClick?: () => void;
  active?: boolean;
  badge?: boolean;
  /** Renders a thin group separator instead of a button. */
  divider?: boolean;
}

export interface MobileMenuProps {
  onClose: () => void;
  onOpenQuickSwitcher: () => void;
  onOpenCommandPalette: () => void;
  onOpenSettings: () => void;
  onToggleGraph: () => void;
  onToggleChat: () => void;
  onToggleOutline?: () => void;
  onOpenPathFinder?: () => void;
  onShowBacklinks?: () => void;
  onInsertImage?: () => void;
  onRecordVoice?: () => void;
  graphOpen: boolean;
  chatOpen: boolean;
  outlineOpen?: boolean;
}

const icon = (d: string) => (
  <svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round">
    <path d={d} />
  </svg>
);

/**
 * Overflow ("More") menu for mobile, rendered as a bottom sheet (codex
 * finding #6). Collects every icon-rail action plus the keyboard shortcuts
 * that are unreachable without a keyboard — Quick Switcher (Ctrl+O) and
 * Command Palette (Ctrl+P). On mobile this is the primary discovery surface
 * for non-visible commands. Its trigger lives in the top bar; dismiss / ARIA
 * / Escape / Android-back behaviour is owned by BottomSheet.
 */
export default function MobileMenu(props: MobileMenuProps) {
  const run = (fn: () => void) => () => {
    fn();
    props.onClose();
  };

  const items = (): MobileMenuItem[] => {
    // Grouped by intent, with thin dividers between groups. New note / Daily
    // note are intentionally NOT here — both live in the bottom-bar pill AND
    // the sidebar action row, so duplicating them here was pure clutter.
    // Open note / Graph / Agent likewise live in the bottom bar. This menu is
    // strict OVERFLOW: only actions no other surface exposes.

    // Group 1 — the current note: insert content, then inspect its structure.
    const thisNote: MobileMenuItem[] = [
      ...(props.onInsertImage
        ? [{
            id: "add-image",
            label: "Add image",
            icon: (
              <svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round">
                <rect x="3" y="3" width="18" height="18" rx="2" />
                <circle cx="8.5" cy="8.5" r="1.5" />
                <path d="M21 15l-5-5L5 21" />
              </svg>
            ),
            onClick: run(props.onInsertImage),
          }]
        : []),
      ...(props.onRecordVoice
        ? [{
            id: "voice-memo",
            label: "Voice memo",
            icon: (
              <svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round">
                <rect x="9" y="2" width="6" height="12" rx="3" />
                <path d="M5 10v2a7 7 0 0 0 14 0v-2" />
                <line x1="12" y1="19" x2="12" y2="22" />
                <line x1="8" y1="22" x2="16" y2="22" />
              </svg>
            ),
            onClick: run(props.onRecordVoice),
          }]
        : []),
      ...(props.onToggleOutline
        ? [{
            id: "outline",
            label: "Outline",
            icon: icon("M4 6h16M4 12h10M4 18h14"),
            onClick: run(props.onToggleOutline),
            active: props.outlineOpen,
          }]
        : []),
      ...(props.onShowBacklinks
        ? [{
            id: "backlinks",
            label: "Backlinks",
            icon: icon("M10 13a5 5 0 007.54.54l3-3a5 5 0 00-7.07-7.07l-1.72 1.71M14 11a5 5 0 00-7.54-.54l-3 3a5 5 0 007.07 7.07l1.71-1.71"),
            onClick: run(props.onShowBacklinks),
          }]
        : []),
    ];

    // Group 2 — discover / graph + AI.
    const discover: MobileMenuItem[] = [
      ...(props.onOpenPathFinder
        ? [{
            id: "find-path",
            label: "Find path between notes…",
            icon: (
              <svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round">
                <circle cx="6" cy="6" r="2.5" />
                <circle cx="18" cy="18" r="2.5" />
                <path d="M8 7l8 10" stroke-dasharray="2 2" />
              </svg>
            ),
            onClick: run(props.onOpenPathFinder),
          }]
        : []),
    ];

    // Group 3 — app/meta. The keyboard Shortcuts cheat sheet is meaningless
    // without a hardware keyboard, so it's omitted. Settings IS here (in
    // addition to the drawer footer gear) so a phone user can reach it from
    // the always-visible top-bar overflow without opening the drawer first.
    const app: MobileMenuItem[] = [
      {
        id: "command-palette",
        label: "Run command…",
        icon: icon("M4 17l6-6-6-6M12 19h8"),
        onClick: run(props.onOpenCommandPalette),
      },
      {
        id: "settings",
        label: "Settings",
        icon: (
          <svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round">
            <circle cx="12" cy="12" r="3" />
            <path d="M19.4 15a1.65 1.65 0 00.33 1.82l.06.06a2 2 0 01-2.83 2.83l-.06-.06a1.65 1.65 0 00-1.82-.33 1.65 1.65 0 00-1 1.51V21a2 2 0 01-4 0v-.09a1.65 1.65 0 00-1-1.51 1.65 1.65 0 00-1.82.33l-.06.06a2 2 0 01-2.83-2.83l.06-.06a1.65 1.65 0 00.33-1.82 1.65 1.65 0 00-1.51-1H3a2 2 0 010-4h.09a1.65 1.65 0 001.51-1 1.65 1.65 0 00-.33-1.82l-.06-.06a2 2 0 012.83-2.83l.06.06a1.65 1.65 0 001.82.33H9a1.65 1.65 0 001-1.51V3a2 2 0 014 0v.09a1.65 1.65 0 001 1.51 1.65 1.65 0 001.82-.33l.06-.06a2 2 0 012.83 2.83l-.06.06a1.65 1.65 0 00-.33 1.82V9a1.65 1.65 0 001.51 1H21a2 2 0 010 4h-.09a1.65 1.65 0 00-1.51 1z" />
          </svg>
        ),
        onClick: run(props.onOpenSettings),
      },
    ];

    // Join non-empty groups with a divider between each.
    return [thisNote, discover, app]
      .filter((g) => g.length > 0)
      .flatMap((g, i) =>
        i === 0 ? g : [{ id: `divider-${i}`, divider: true }, ...g],
      );
  };

  return (
    <BottomSheet
      onClose={props.onClose}
      title="More"
      variant="sheet"
      grabHandle
      handleAndroidBack
    >
      <div class="py-1">
        <For each={items()}>
          {(item) => (
            <Show
              when={!item.divider}
              fallback={<div class="my-1 mx-4 border-t border-border" />}
            >
              <SheetActionRow
                icon={item.icon}
                label={item.label ?? ""}
                active={item.active}
                badge={item.badge}
                onClick={() => item.onClick?.()}
              />
            </Show>
          )}
        </For>
      </div>
    </BottomSheet>
  );
}
