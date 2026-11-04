import { For } from "solid-js";
import { settingsStore, THEME_OPTIONS, type Theme } from "../../stores/settings";
import BottomSheet from "../BottomSheet/BottomSheet";

export interface ThemePickerProps {
  /** Called when the user dismisses — tap backdrop, hit Escape, or tap close. */
  onClose: () => void;
  /**
   * When `true`, selecting a theme closes the picker automatically.
   * Default `true` — users expect "pick and done" on mobile.
   */
  closeOnSelect?: boolean;
}

/**
 * Theme picker — replaces the previous cycle-on-tap behaviour that
 * forced users to cycle all four themes to see what they wanted.
 *
 * Displayed as a BottomSheet with each theme as a tappable row: swatch
 * (bg / accent / text chips), name, description, and a checkmark on
 * the currently-active theme. Same data as Settings > Appearance; one
 * source of truth is `THEME_OPTIONS` in `stores/settings.ts`.
 */
export default function ThemePicker(props: ThemePickerProps) {
  const closeOnSelect = () => props.closeOnSelect ?? true;

  function handleSelect(theme: Theme) {
    settingsStore.setTheme(theme);
    if (closeOnSelect()) props.onClose();
  }

  return (
    <BottomSheet
      onClose={props.onClose}
      title="Theme"
      ariaLabel="Choose a theme"
    >
      <div class="py-1" role="radiogroup" aria-label="Theme selection">
        <For each={THEME_OPTIONS}>
          {(opt) => {
            const active = () => settingsStore.theme() === opt.id;
            return (
              <button
                onClick={() => handleSelect(opt.id)}
                role="radio"
                aria-checked={active()}
                class={`w-full flex items-center gap-3 px-4 py-3 transition-colors text-left ${
                  active() ? "bg-bg-hover" : "hover:bg-bg-hover"
                }`}
              >
                {/* Three-chip swatch — matches the Settings panel picker */}
                <div
                  class="flex shrink-0 w-10 h-10 rounded-lg overflow-hidden border border-border"
                  aria-hidden="true"
                >
                  <div
                    class="w-1/3"
                    style={{ "background-color": opt.swatch[0] }}
                  />
                  <div
                    class="w-1/3"
                    style={{ "background-color": opt.swatch[1] }}
                  />
                  <div
                    class="w-1/3"
                    style={{ "background-color": opt.swatch[2] }}
                  />
                </div>
                <div class="min-w-0 flex-1">
                  <div
                    class={`text-sm truncate ${
                      active()
                        ? "text-accent font-medium"
                        : "text-text-primary"
                    }`}
                  >
                    {opt.name}
                  </div>
                  <div class="text-[12px] text-text-muted truncate">
                    {opt.description}
                  </div>
                </div>
                {/* Checkmark — visible only on the active option */}
                <svg
                  width="18"
                  height="18"
                  viewBox="0 0 24 24"
                  fill="none"
                  stroke="currentColor"
                  stroke-width="2.2"
                  stroke-linecap="round"
                  stroke-linejoin="round"
                  class={`shrink-0 text-accent transition-opacity ${
                    active() ? "opacity-100" : "opacity-0"
                  }`}
                  aria-hidden="true"
                >
                  <path d="M5 13l4 4L19 7" />
                </svg>
              </button>
            );
          }}
        </For>
      </div>
    </BottomSheet>
  );
}
