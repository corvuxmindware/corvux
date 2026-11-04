import { For, Show } from "solid-js";
import CorvuxLogo from "../Brand/CorvuxLogo";
import { tabStore } from "../../stores/tabs";
import { roostStore } from "../../stores/roost";
import { promptStore } from "../Modals/PromptModal";
import { isMobile } from "../../lib/platform";

interface WelcomeProps {
  onOpenQuickSwitcher?: () => void;
  onOpenCommandPalette?: () => void;
}

/**
 * Empty-state shown when no tab is active. Three primary actions as
 * clickable links over a quiet logo mark — mirrors Obsidian's New Tab
 * pattern so the empty state reads as workspace, not landing page.
 */
export default function Welcome(props: WelcomeProps) {
  async function createNewNote() {
    const name = await promptStore.ask({
      title: "New note",
      placeholder: "Note name",
      confirmLabel: "Create",
    });
    if (!name) return;
    const path = await roostStore.createNewNote(name);
    // Brand-new empty note — always land in write mode.
    if (path) tabStore.openTab(path, { readOnly: false });
  }

  const actions = [
    { key: "Ctrl+N", label: "Create new note", onClick: createNewNote },
    { key: "Ctrl+O", label: "Go to file", onClick: () => props.onOpenQuickSwitcher?.() },
    { key: "Ctrl+P", label: "Command palette", onClick: () => props.onOpenCommandPalette?.() },
  ];

  return (
    <div class="h-full flex items-center justify-center px-6">
      <div class="text-center">
        <CorvuxLogo
          alt=""
          class="w-10 h-10 mx-auto mb-4 opacity-25"
        />
        <ul class="space-y-2">
          <For each={actions}>
            {(a) => (
              <li>
                {/* Mobile: no physical keyboard, so the shortcut hint is
                    noise — hide it and give the action a 44px-min tap
                    area (Apple HIG / Material minimum). Desktop keeps
                    the compact link + kbd-hint style. */}
                <button
                  onClick={a.onClick}
                  class={
                    isMobile()
                      ? "text-accent text-base min-h-[44px] px-4 inline-flex items-center justify-center"
                      : "text-accent hover:underline text-sm"
                  }
                >
                  {a.label}
                  <Show when={!isMobile()}>
                    {" "}
                    <span class="text-text-muted text-xs opacity-60">
                      ({a.key})
                    </span>
                  </Show>
                </button>
              </li>
            )}
          </For>
        </ul>
      </div>
    </div>
  );
}
