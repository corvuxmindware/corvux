import { For } from "solid-js";
import { settingsStore, type EditorFont } from "../../stores/settings";
import {
  imageOptimizeEnabled,
  setImageOptimizeEnabled,
  MAX_EDGE_PX,
} from "../../lib/image-optimize";

const EDITOR_FONTS: EditorFont[] = [
  "mono",
  "sans",
  "serif",
  "system",
  "rounded",
  "slab",
];

// CSS stacks for the live preview in each picker tile so the user can see
// what the choice will look like before applying it. Kept in sync with
// FONT_MAP in stores/settings.ts.
const FONT_PREVIEW: Record<EditorFont, string> = {
  mono: "'JetBrains Mono', 'Fira Code', ui-monospace, monospace",
  sans: "'Inter', -apple-system, BlinkMacSystemFont, 'Segoe UI', sans-serif",
  serif: "'Georgia', 'Cambria', serif",
  system: "system-ui, -apple-system, BlinkMacSystemFont, 'Segoe UI', sans-serif",
  rounded: "ui-rounded, 'SF Pro Rounded', 'Quicksand', system-ui, sans-serif",
  slab: "'Roboto Slab', 'Source Serif Pro', Georgia, serif",
};

/**
 * Editor typography — font family, size, line-height. Folded into
 * AppearanceSection as of 2026-05-20 (one Appearance page covers theme
 * + typography, matching the typical "Look & feel" grouping in Slack /
 * Notion / Linear). Still exported as its own component so the merge
 * is a one-line render call upstream.
 */
export default function EditorSection() {
  return (
    <>
      <section>
      <h3 class="text-text-secondary text-xs font-semibold uppercase tracking-wider mb-3">
        Typography
      </h3>
      <div class="space-y-4">
        {/* Font family */}
        <div>
          <div class="flex items-center justify-between mb-2">
            <div>
              <div class="text-text-primary text-sm">Editor font</div>
              <div class="text-text-muted text-xs">Family used inside the editor</div>
            </div>
          </div>
          <div class="grid grid-cols-3 gap-2 text-xs w-full">
            <For each={EDITOR_FONTS}>
              {(f) => {
                const active = () => settingsStore.editorFont() === f;
                return (
                  <button
                    onClick={() => settingsStore.setEditorFont(f)}
                    aria-pressed={active()}
                    class={`flex flex-col items-center justify-center gap-0.5 rounded-lg border px-3 py-2 transition-colors ${
                      active()
                        ? "bg-accent text-bg-primary border-accent font-medium"
                        : "bg-bg-secondary border-border text-text-muted hover:text-text-secondary"
                    }`}
                  >
                    <span class="capitalize">{f}</span>
                    <span
                      class="text-[11px] opacity-80 leading-none"
                      style={{ "font-family": FONT_PREVIEW[f] }}
                    >
                      Aa
                    </span>
                  </button>
                );
              }}
            </For>
          </div>
        </div>

        {/* Font size */}
        <div>
          <div class="flex items-center justify-between mb-2">
            <div>
              <div class="text-text-primary text-sm">Font size</div>
              <div class="text-text-muted text-xs">{settingsStore.editorFontSize()}px</div>
            </div>
          </div>
          <input
            type="range"
            min="10"
            max="24"
            step="1"
            value={settingsStore.editorFontSize()}
            onInput={(e) =>
              settingsStore.setEditorFontSize(Number(e.currentTarget.value))
            }
            aria-label="Font size"
            class="w-full accent-accent"
          />
        </div>

        {/* Line height */}
        <div>
          <div class="flex items-center justify-between mb-2">
            <div>
              <div class="text-text-primary text-sm">Line height</div>
              <div class="text-text-muted text-xs">
                {settingsStore.editorLineHeight().toFixed(1)}
              </div>
            </div>
          </div>
          <input
            type="range"
            min="1.0"
            max="2.5"
            step="0.1"
            value={settingsStore.editorLineHeight()}
            onInput={(e) =>
              settingsStore.setEditorLineHeight(Number(e.currentTarget.value))
            }
            aria-label="Line height"
            class="w-full accent-accent"
          />
        </div>
      </div>
      </section>

      <section class="mt-6">
        <h3 class="text-text-secondary text-xs font-semibold uppercase tracking-wider mb-3">
          Images
        </h3>
        <ToggleRow
          checked={imageOptimizeEnabled()}
          onChange={setImageOptimizeEnabled}
          title="Optimize pasted images"
          description={`Downscale large images to ${MAX_EDGE_PX}px and re-encode to WebP before saving, to keep your roost small. Screenshots and text stay readable; SVGs, GIFs, and already-small images are left untouched.`}
        />
      </section>
    </>
  );
}

/** Labelled on/off toggle row, matching the Appearance section's switch. */
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
