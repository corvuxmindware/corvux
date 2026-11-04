import {
  type JSX,
  Show,
  createSignal,
  onCleanup,
  onMount,
} from "solid-js";
import { isAndroid } from "../../lib/platform";

export interface BottomSheetProps {
  /** Fires when the user taps the backdrop, hits Escape, or taps the close button. */
  onClose: () => void;
  /** Optional header title. When provided, the sheet renders a header row with a close button. */
  title?: string;
  /**
   * Layout mode:
   * - `"sheet"` (default): slides up from the bottom on mobile, centers on >= sm.
   * - `"fullscreen"`: covers the whole viewport — use for the tab-switcher grid.
   */
  variant?: "sheet" | "fullscreen";
  /** Max width for the centered/desktop variant. Default: `sm:w-[420px]`. */
  maxWidthClass?: string;
  /** ARIA label for the dialog root. Falls back to `title` if provided. */
  ariaLabel?: string;
  /** Optional sticky footer content. Renders below the scroll area. */
  footer?: JSX.Element;
  /**
   * Show a drag-handle pill at the top (mobile slide-up sheets only). Opt-in
   * so existing consumers are visually untouched. No-op on desktop (`sm:hidden`)
   * and for `variant="fullscreen"`.
   */
  grabHandle?: boolean;
  /**
   * Make Android's hardware back close the sheet (instead of leaving the app).
   * Opt-in — pushes one history entry on mount and pops it on close. Leave off
   * for sheets that aren't the top-most Android-back target.
   */
  handleAndroidBack?: boolean;
  /**
   * Where the sheet sits. `"bottom"` (default) slides up from the bottom on
   * mobile and centers on >= sm. `"center"` centers on ALL sizes — a compact
   * dialog for short forms where a full-width bottom sheet feels oversized.
   * Center placement also lifts above the on-screen keyboard via the
   * `--corvux-keyboard-inset` var (set in lib/keyboard-inset.ts).
   */
  placement?: "bottom" | "center";
  children: JSX.Element;
}

/**
 * Mobile-first sheet / modal shell. One primitive for every non-toast
 * overlay — ThemePicker, TabSwitcher, NewNote prompt, future settings
 * sub-sheets — so dismiss / ARIA / transition behaviour is consistent.
 *
 * Dismiss pattern (matches the fix in commit 8c26574):
 *   - Backdrop owns the `onClick={onClose}` handler.
 *   - Content root is `position: relative` above the backdrop, so taps
 *     on it don't bubble to the dismiss handler.
 *   - Escape key also closes.
 *
 * Transition: opacity + Y-translate via a `mounted` signal flipped on
 * the first animation frame. Keeps bundle-size zero (no motion library).
 */
export default function BottomSheet(props: BottomSheetProps) {
  const [mounted, setMounted] = createSignal(false);
  let contentRef!: HTMLDivElement;

  onMount(() => {
    // rAF ensures the initial opacity-0 / translate state paints before
    // the `opacity-100` class is applied, so we actually see the
    // transition on mount (rather than snapping to the final state).
    requestAnimationFrame(() => setMounted(true));

    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.preventDefault();
        props.onClose();
      }
    };
    document.addEventListener("keydown", onKey);
    onCleanup(() => document.removeEventListener("keydown", onKey));

    // Android hardware back closes the sheet first (same pattern as
    // SettingsPanel). Push a sentinel entry; a back press pops it and fires
    // onClose. On programmatic close we consume our own entry via back().
    if (props.handleAndroidBack && isAndroid()) {
      let poppedByBack = false;
      history.pushState({ corvuxSheet: true }, "");
      const onPop = () => {
        poppedByBack = true;
        props.onClose();
      };
      window.addEventListener("popstate", onPop);
      onCleanup(() => {
        window.removeEventListener("popstate", onPop);
        if (!poppedByBack) history.back();
      });
    }

    // Focus the content root so screen readers announce the dialog and
    // the Escape key works without the user tapping first.
    contentRef?.focus?.();
  });

  const variant = () => props.variant ?? "sheet";
  const placement = () => props.placement ?? "bottom";

  const positionClasses = () =>
    variant() === "fullscreen"
      ? "inset-0"
      : placement() === "center"
        ? "inset-0 flex items-center justify-center p-4"
        : "inset-0 flex items-end sm:items-center justify-center";

  const contentClasses = () =>
    variant() === "fullscreen"
      ? `absolute inset-0 flex flex-col bg-bg-primary transition-opacity duration-150 ${
          mounted() ? "opacity-100" : "opacity-0"
        }`
      : placement() === "center"
        ? `relative w-full max-w-[400px] max-h-[85vh] flex flex-col bg-bg-secondary border border-border rounded-2xl shadow-2xl transition-all duration-150 ${
            mounted() ? "opacity-100 translate-y-0" : "opacity-0 translate-y-2"
          }`
        : `relative w-full ${props.maxWidthClass ?? "sm:w-[420px]"} max-h-[80vh] flex flex-col bg-bg-secondary border border-border rounded-t-2xl sm:rounded-xl shadow-2xl transition-all duration-150 ${
            mounted()
              ? "opacity-100 translate-y-0"
              : "opacity-0 translate-y-2 sm:translate-y-0"
          }`;

  return (
    <div
      class={`fixed z-50 ${positionClasses()}`}
      style={
        placement() === "center"
          ? { "padding-bottom": "calc(1rem + var(--corvux-keyboard-inset, 0px))" }
          : undefined
      }
    >
      <div
        class={`absolute inset-0 bg-black/50 transition-opacity duration-150 ${
          mounted() ? "opacity-100" : "opacity-0"
        }`}
        onClick={props.onClose}
        aria-hidden="true"
      />
      <div
        ref={contentRef}
        role="dialog"
        aria-modal="true"
        aria-label={props.ariaLabel ?? props.title ?? "Dialog"}
        tabindex="-1"
        class={contentClasses()}
      >
        <Show when={variant() === "sheet" && props.grabHandle}>
          {/* Drag-handle affordance — mobile slide-up sheets only. */}
          <div class="sm:hidden flex justify-center pt-2 pb-1 shrink-0" aria-hidden="true">
            <div class="w-9 h-1 rounded-full bg-border-strong" />
          </div>
        </Show>
        <Show when={props.title}>
          <div class="flex items-center justify-between px-4 py-2 sm:py-3 border-b border-border shrink-0">
            <h3 class="text-text-primary font-semibold text-sm">{props.title}</h3>
            <button
              onClick={props.onClose}
              class="w-11 h-11 sm:w-7 sm:h-7 flex items-center justify-center rounded hover:bg-bg-hover text-text-muted hover:text-text-primary transition-colors"
              aria-label="Close"
            >
              <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
                <path d="M18 6L6 18M6 6l12 12" />
              </svg>
            </button>
          </div>
        </Show>

        <div class="flex-1 overflow-y-auto">{props.children}</div>

        <Show when={props.footer}>
          <div class="shrink-0 border-t border-border">{props.footer}</div>
        </Show>
      </div>
    </div>
  );
}
