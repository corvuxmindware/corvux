import { Show, type JSX } from "solid-js";

interface SheetActionRowProps {
  /** 20px leading glyph. */
  icon?: JSX.Element;
  label: string;
  onClick: () => void;
  /** Accent text + a trailing dot for the active/selected row. */
  active?: boolean;
  /** Attention dot on the RIGHT edge (e.g. unread suggestions). */
  badge?: boolean;
}

/**
 * One 48px tappable row for bottom-sheet menus: leading icon, label, and a
 * right-aligned badge/active dot. Replaces the cramped ~32px floating-menu
 * rows the codex review flagged, and moves badges off the icon to the row's
 * trailing edge.
 */
export default function SheetActionRow(props: SheetActionRowProps) {
  return (
    <button
      onClick={props.onClick}
      class={`w-full h-12 flex items-center gap-3 px-4 text-left text-sm transition-colors ${
        props.active
          ? "text-accent bg-bg-hover"
          : "text-text-secondary hover:bg-bg-hover hover:text-text-primary"
      }`}
    >
      <Show when={props.icon}>
        <span class="w-5 h-5 shrink-0 flex items-center justify-center">
          {props.icon}
        </span>
      </Show>
      <span class="flex-1 truncate">{props.label}</span>
      <Show when={props.badge}>
        <span class="w-1.5 h-1.5 rounded-full bg-yellow shrink-0" />
      </Show>
      <Show when={props.active}>
        <span class="w-1.5 h-1.5 rounded-full bg-accent shrink-0" />
      </Show>
    </button>
  );
}
