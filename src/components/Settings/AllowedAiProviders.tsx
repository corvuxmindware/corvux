import { For, Match, Show, Switch, createResource } from "solid-js";
import { commands, unwrap } from "../../lib/invoke";
import { toastStore } from "../Toast/Toast";

/** Readable label for a consent id from `commands/ai/consent.rs`. */
export function consentLabel(id: string): string {
  switch (id) {
    case "gemini":
      return "Google Gemini";
    case "openrouter":
      return "OpenRouter";
    case "openai":
      return "OpenAI";
    case "anthropic":
      return "Anthropic (Claude)";
    case "ollama:ollama.com":
      return "Ollama Cloud (ollama.com)";
    default:
      return id.startsWith("ollama:") ? `Ollama at ${id.slice("ollama:".length)}` : id;
  }
}

/** AI providers the user allowed to receive chat content (guideline
 *  5.1.2(i) consent), each with a way to withdraw it. Withdrawing takes
 *  effect on the next request, even mid-chat (`ConsentGated` re-checks). */
export default function AllowedAiProviders() {
  const [granted, { refetch }] = createResource(() => unwrap(commands.aiConsentList()));
  const withdraw = async (id: string) => {
    try {
      await unwrap(commands.aiConsentRevoke(id));
      toastStore.show(`${consentLabel(id)} will ask again before your next chat.`, "info");
    } catch (e) {
      toastStore.showError("Couldn't withdraw permission", e);
    }
    void refetch();
  };
  // Read the accessor only in the ready/refreshing branch: a Solid resource
  // rethrows its error on read, and while it is still loading the list is
  // unknown — showing "none yet" then would be wrong for users who granted.
  const list = () => granted.latest ?? [];
  return (
    <div class="pt-1 space-y-1" data-testid="allowed-ai-providers">
      <Switch>
        <Match when={granted.error}>
          <p class="text-[11px] text-text-muted">
            Couldn't load the AI providers you've allowed. Reopen Settings to try again.
          </p>
        </Match>
        <Match when={granted.state === "ready" || granted.state === "refreshing"}>
          <p class="text-[11px] text-text-muted">
            Allowed to receive your chats:{" "}
            <Show when={list().length === 0}>
              <span>none yet. Corvux asks before the first message to each provider.</span>
            </Show>
          </p>
          <For each={list()}>
            {(id) => (
              <div class="flex items-center justify-between gap-2 text-xs">
                <span class="text-text-secondary">{consentLabel(id)}</span>
                <button
                  type="button"
                  class="px-2 py-0.5 rounded-md border border-border text-text-secondary hover:text-text-primary text-[11px]"
                  onClick={() => void withdraw(id)}
                >
                  Withdraw
                </button>
              </div>
            )}
          </For>
        </Match>
      </Switch>
    </div>
  );
}
