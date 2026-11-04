import { For, Show } from "solid-js";
import { settingsStore, THEME_OPTIONS } from "../../stores/settings";
import { isMobile } from "../../lib/platform";
import EditorSection from "./EditorSection";

/**
 * Appearance — theme + editor typography. Merged from the previous
 * Appearance/Editor two-page split (2026-05-20): they're both
 * presentation concerns for the same surface, and users typically tune
 * them together. The drill-down stays one click from the settings index.
 */
export default function AppearanceSection() {
  return (
    <div class="space-y-8">
      <section>
        <h3 class="text-text-secondary text-xs font-semibold uppercase tracking-wider mb-3">
          Theme
        </h3>
      <div class="grid grid-cols-1 sm:grid-cols-2 gap-2">
        <For each={THEME_OPTIONS}>
          {(opt) => {
            const active = () => settingsStore.theme() === opt.id;
            return (
              <button
                onClick={() => settingsStore.setTheme(opt.id)}
                class={`flex items-center gap-3 px-3 py-2.5 rounded-lg border text-left transition-colors ${
                  active()
                    ? "border-accent bg-bg-hover"
                    : "border-border hover:border-border-strong bg-bg-secondary/60"
                }`}
                aria-pressed={active()}
              >
                <div
                  class="flex shrink-0 w-10 h-10 rounded-md overflow-hidden border border-border"
                  aria-hidden="true"
                >
                  <div class="w-1/3" style={{ "background-color": opt.swatch[0] }} />
                  <div class="w-1/3" style={{ "background-color": opt.swatch[1] }} />
                  <div class="w-1/3" style={{ "background-color": opt.swatch[2] }} />
                </div>
                <div class="min-w-0 flex-1">
                  <div
                    class={`text-sm truncate ${
                      active() ? "text-text-primary font-medium" : "text-text-secondary"
                    }`}
                  >
                    {opt.name}
                  </div>
                  <div class="text-[12px] text-text-muted truncate">
                    {opt.description}
                  </div>
                </div>
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
                  <path d="M20 6L9 17l-5-5" />
                </svg>
              </button>
            );
          }}
        </For>
      </div>
      </section>

      {/* Editor typography — rendered as a peer section so the h3
          hierarchy stays flat within the page. */}
      <EditorSection />

      {/* Startup — desktop-only. Mobile has no "launch at login" concept,
          and the autostart plugin isn't registered on Android/iOS, so the
          toggle is hidden there entirely. */}
      <Show when={!isMobile()}>
        <StartupSection />
      </Show>
    </div>
  );
}

/** A single labelled on/off toggle row, matching the Startup section's
 *  style. Extracted so the two startup toggles don't duplicate the
 *  switch markup. */
function ToggleRow(props: {
  checked: boolean;
  onChange: (v: boolean) => void;
  title: string;
  description: string;
}) {
  return (
    <label class="flex items-start gap-3 cursor-pointer group">
      <span class="relative inline-flex shrink-0 mt-0.5">
        <input
          type="checkbox"
          checked={props.checked}
          onChange={(e) => props.onChange(e.currentTarget.checked)}
          class="sr-only peer"
        />
        <span
          class={`w-9 h-5 rounded-full transition-colors ${
            props.checked ? "bg-purple" : "bg-bg-tertiary border border-border"
          } peer-focus-visible:ring-2 peer-focus-visible:ring-purple/40`}
          aria-hidden="true"
        />
        <span
          class={`absolute top-0.5 left-0.5 w-4 h-4 rounded-full bg-white shadow transition-transform ${
            props.checked ? "translate-x-4" : "translate-x-0"
          }`}
          aria-hidden="true"
        />
      </span>
      <div class="flex-1 min-w-0">
        <div class="text-sm text-text-primary">{props.title}</div>
        <div class="text-text-muted text-[11px] leading-relaxed mt-0.5">
          {props.description}
        </div>
      </div>
    </label>
  );
}

/** Startup toggles. Desktop-only (gated by the caller). "Launch at
 *  startup" reconciles the OS autostart registration in the Rust
 *  `save_settings` handler; "close to tray" is read live by the
 *  window-close handler — neither needs extra IPC here. */
function StartupSection() {
  const launchEnabled = () => settingsStore.settings().launch_at_startup;
  const trayEnabled = () => settingsStore.settings().close_to_tray;

  return (
    <section>
      <h3 class="text-text-secondary text-xs font-semibold uppercase tracking-wider mb-3">
        Startup
      </h3>
      <div class="space-y-4">
        <ToggleRow
          checked={launchEnabled()}
          onChange={(v) => void settingsStore.save({ launch_at_startup: v })}
          title="Launch Corvux when my computer starts"
          description="Opens automatically after you sign in, so your notes keep syncing in the background. Turn it off to start Corvux only when you open it yourself."
        />
        <ToggleRow
          checked={trayEnabled()}
          onChange={(v) => void settingsStore.save({ close_to_tray: v })}
          title="Keep Corvux running in the tray when I close the window"
          description="Pressing the window's close button hides Corvux to the system tray instead of quitting, so it stays online for sync and reopens instantly. Use the tray icon's Quit to exit fully."
        />
      </div>
    </section>
  );
}
