import type { JSX } from "solid-js";

interface TouchIconButtonProps {
  /** Accessible name — required because the button is icon-only. */
  label: string;
  onClick: (e: MouseEvent) => void;
  /**
   * Desktop size. Mobile ALWAYS gets a 44px (`w-11 h-11`) tap target — the
   * iOS HIG / Android floor the codex review flagged. At the `sm` breakpoint
   * and up the button shrinks back to this compact size so desktop chrome is
   * unchanged. Default "7" matches the old `w-7 h-7` controls.
   */
  desktopSize?: "7" | "8" | "9";
  /** Accent highlight (e.g. an active mode/toggle). */
  active?: boolean;
  disabled?: boolean;
  title?: string;
  ariaPressed?: boolean;
  /** Extra classes appended last (avoid clashing rounding/sizing utilities). */
  class?: string;
  children: JSX.Element;
}

/**
 * Map the desktop size to a full, literal `sm:` class pair so Tailwind's
 * content scanner keeps them (it can't see interpolated class fragments).
 */
const DESKTOP_SIZE: Record<NonNullable<TouchIconButtonProps["desktopSize"]>, string> = {
  "7": "sm:w-7 sm:h-7",
  "8": "sm:w-8 sm:h-8",
  "9": "sm:w-9 sm:h-9",
};

/**
 * Icon button with a 44px mobile tap target that collapses to a compact
 * desktop size at the `sm` breakpoint. Centralizes the touch-target rule so
 * mobile headers (Graph, Chat, sheets) stop using sub-44px `w-7 h-7` buttons
 * without changing how the same controls render on desktop.
 */
export default function TouchIconButton(props: TouchIconButtonProps) {
  return (
    <button
      onClick={(e) => props.onClick(e)}
      aria-label={props.label}
      aria-pressed={props.ariaPressed}
      title={props.title}
      disabled={props.disabled}
      class={`w-11 h-11 ${DESKTOP_SIZE[props.desktopSize ?? "7"]} flex items-center justify-center rounded shrink-0 transition-colors ${
        props.disabled
          ? "text-text-muted opacity-40 cursor-not-allowed"
          : props.active
            ? "text-accent bg-accent/15"
            : "text-text-secondary hover:text-text-primary hover:bg-bg-hover"
      } ${props.class ?? ""}`}
    >
      {props.children}
    </button>
  );
}
