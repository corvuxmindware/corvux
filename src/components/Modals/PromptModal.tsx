import { Show, createRoot, createSignal, onCleanup, onMount } from "solid-js";

// `window.prompt()` is non-functional in Tauri WebView2 on Windows (silently
// returns null without showing a dialog), so every UI surface that needs an
// "ask the user for a string" affordance routes through this store + modal.
// API is intentionally async-promise-shaped so callsites read like the
// original `const name = await promptStore.ask(...)`.

export interface PromptOptions {
  title: string;
  /** Optional one-liner above the input — context for what's being asked. */
  description?: string;
  placeholder?: string;
  /** Pre-filled value (e.g. for "rename"). */
  defaultValue?: string;
  /** Label on the confirm button. Defaults to "OK". */
  confirmLabel?: string;
  /** Return an error string to block submission, or null to allow it.
   *  Called on every input change so the error can render live. */
  validate?: (value: string) => string | null;
}

interface PendingPrompt extends PromptOptions {
  resolve: (value: string | null) => void;
}

function createPromptStore() {
  const [active, setActive] = createSignal<PendingPrompt | null>(null);

  function ask(opts: PromptOptions): Promise<string | null> {
    return new Promise((resolve) => {
      // If another prompt is already up, cancel it first — the new one wins.
      // Stacked prompts make no sense (the user would have to dismiss them
      // in reverse order without knowing the queue exists).
      const existing = active();
      if (existing) existing.resolve(null);
      setActive({ ...opts, resolve });
    });
  }

  function submit(value: string) {
    const p = active();
    if (!p) return;
    setActive(null);
    p.resolve(value);
  }

  function cancel() {
    const p = active();
    if (!p) return;
    setActive(null);
    p.resolve(null);
  }

  return { active, ask, submit, cancel };
}

export const promptStore = createRoot(createPromptStore);

export default function PromptModal() {
  return (
    <Show when={promptStore.active()}>
      {(prompt) => <PromptModalInner opts={prompt()} />}
    </Show>
  );
}

function PromptModalInner(props: { opts: PendingPrompt }) {
  const [value, setValue] = createSignal(props.opts.defaultValue ?? "");
  const error = () => props.opts.validate?.(value()) ?? null;
  const canSubmit = () => value().trim().length > 0 && error() === null;

  function handleKey(e: KeyboardEvent) {
    if (e.key === "Escape") {
      e.preventDefault();
      promptStore.cancel();
    } else if (e.key === "Enter" && canSubmit()) {
      e.preventDefault();
      promptStore.submit(value().trim());
    }
  }

  onMount(() => {
    document.addEventListener("keydown", handleKey);
    onCleanup(() => document.removeEventListener("keydown", handleKey));
  });

  return (
    <div class="fixed inset-0 z-[80] flex items-center justify-center p-4">
      <div class="absolute inset-0 bg-black/60" onClick={() => promptStore.cancel()} />
      <div
        class="relative w-full max-w-md bg-bg-primary border border-border rounded-lg shadow-xl overflow-hidden"
        role="dialog"
        aria-modal="true"
        aria-labelledby="prompt-modal-title"
      >
        <div class="px-4 py-3 border-b border-border">
          <h2 id="prompt-modal-title" class="text-text-primary text-base font-medium">
            {props.opts.title}
          </h2>
        </div>

        <div class="p-4 space-y-2">
          <Show when={props.opts.description}>
            <div class="text-text-secondary text-xs">{props.opts.description}</div>
          </Show>

          <input
            type="text"
            value={value()}
            onInput={(e) => setValue(e.currentTarget.value)}
            placeholder={props.opts.placeholder}
            autofocus
            class="w-full px-3 py-2 bg-bg-secondary border border-border rounded text-text-primary text-sm placeholder:text-text-muted focus:outline-none focus:border-accent"
          />

          <Show when={error()}>
            {(msg) => <div class="text-red-400 text-xs">{msg()}</div>}
          </Show>
        </div>

        <div class="flex justify-end gap-2 px-4 py-3 border-t border-border">
          <button
            onClick={() => promptStore.cancel()}
            class="px-3 py-2 text-sm rounded border border-border text-text-primary hover:bg-bg-hover"
          >
            Cancel
          </button>
          <button
            onClick={() => promptStore.submit(value().trim())}
            disabled={!canSubmit()}
            class="px-4 py-2 text-sm rounded bg-accent text-white hover:bg-accent-hover disabled:opacity-50 disabled:cursor-not-allowed"
          >
            {props.opts.confirmLabel ?? "OK"}
          </button>
        </div>
      </div>
    </div>
  );
}
