import { createSignal, For, Show } from "solid-js";
import { appInvoke, commands, isTauri, setBrowserRoost, unwrap } from "../../lib/invoke";
import { errMsg } from "../../lib/error";
import { log } from "../../lib/log";
import { isAndroid as platformIsAndroid, isIOS as platformIsIOS } from "../../lib/platform";
import { lsSet } from "../../lib/storage";
import { roostStore } from "../../stores/roost";
import { settingsStore, THEME_OPTIONS, type Theme } from "../../stores/settings";
import { tabStore } from "../../stores/tabs";
import CorvuxLogo from "../Brand/CorvuxLogo";

interface OnboardingProps {
  /** Pass `openChat: true` to have App open the AI panel right after the
   *  modal closes, so the user lands in the place the final step just
   *  taught them about — instead of a blank workspace. */
  onComplete: (openChat?: boolean) => void;
}

import type { ConnectionTestResult } from "../../lib/bindings";

type Provider = "gemini" | "openrouter" | "skip";

/** Welcome-note content seeded into the user's roost on first run.
 *
 *  Doubles as the "tips" surface — the previous flow had a separate
 *  shortcut-cheat-sheet step that the user dismissed without
 *  internalizing. Folding the same info into a real markdown file in
 *  the roost makes it discoverable forever (re-open the note any time)
 *  and contextualizes "this is what a note looks like; you can edit
 *  it." Keep this short — overlong welcome notes get deleted before
 *  they're read. */
const WELCOME_NOTE = `# Welcome to Corvux

This is a real note in your roost — edit it, delete it, link from it. Everything you write goes to plain markdown files on disk, organized however you want.

## Try these

- **Type \`[[\`** anywhere to link to another note. The other note will see this one in its backlinks panel.
- **Type \`#\`** to add a tag. Tags surface in search and the graph view.
- **Press \`Ctrl+L\`** to open Corvux Agent — ask questions about anything in your roost.
- **Press \`Ctrl+D\`** to open today's daily note.
- **Press \`Ctrl+P\`** for the command palette.
- **Press \`Ctrl+/\`** for the full keyboard shortcut cheat sheet.

## What makes Corvux different

- **Local-first.** Your notes are markdown files on your disk. You can open them in any other editor, sync them with any cloud storage, or \`git commit\` the folder.
- **Graph-aware.** Every link and tag is indexed in a graph database, so the AI can follow backlinks and connections — not just keyword matches — to find relevant context.
- **AI is opt-in.** No telemetry, no analytics. When you send a chat, your prompt and any note snippets the agent retrieved go to your chosen cloud provider (Gemini or OpenRouter today). See **Settings → Privacy** for exactly what leaves your device given your current setup.
- **You approve every change.** When the agent wants to edit or create a note, it shows the full before-and-after diff first. Nothing is written until you click Approve.

## Heads-up

Deleted notes go to \`.corvux-trash/\` inside your roost folder. Restore them from your file manager any time.

## Next

Try creating a note about something you're working on. Link to this one with \`[[Welcome to Corvux]]\` and you'll see it appear in this note's backlinks panel.
`;

// Android has a real folder picker (SAF), iOS has the doc-picker plugin.
// The shared platform helpers handle iPad-masquerading-as-Mac so the iPad
// picks the iOS branch instead of falling into the desktop dialog.
const isAndroid = platformIsAndroid();
const isIOS = platformIsIOS();

export default function Onboarding(props: OnboardingProps) {
  const [step, setStep] = createSignal(0);

  // Step 1 — provider selection + per-provider key fields.
  const [provider, setProvider] = createSignal<Provider>("gemini");
  const [geminiKey, setGeminiKey] = createSignal("");
  const [orKey, setOrKey] = createSignal("");
  const [testing, setTesting] = createSignal(false);
  const [testResult, setTestResult] = createSignal<ConnectionTestResult | null>(null);
  const [keySaveError, setKeySaveError] = createSignal<string | null>(null);

  // Step 2 — welcome note opt-IN (default off; user ticks to add it).
  const [includeWelcomeNote, setIncludeWelcomeNote] = createSignal(false);
  const [finishing, setFinishing] = createSignal(false);

  const [error, setError] = createSignal<string | null>(null);

  async function handleOpenRoost() {
    setError(null);
    if (!isTauri) {
      try {
        if ("showDirectoryPicker" in window) {
          const handle = await (window as Window & { showDirectoryPicker: (opts?: object) => Promise<FileSystemDirectoryHandle> }).showDirectoryPicker({ mode: "readwrite" });
          const roostPath = `/browser-roost/${handle.name}`;
          setBrowserRoost(handle, roostPath);
          await roostStore.openRoost(roostPath, handle.name, undefined, { skipWelcome: true });
        } else {
          await roostStore.openRoost("/mock-roost", undefined, undefined, { skipWelcome: true });
        }
        setStep(2);
      } catch (e) {
        const msg = errMsg(e);
        if (/abort/i.test(msg)) return;
        setError(msg);
      }
      return;
    }
    try {
      if (isAndroid) {
        // Real folder picker via SAF — lets the user pick a roost folder
        // they already have (e.g. the same `Brain-Dump` folder synced via
        // Syncthing, Google Drive, or just an existing local folder).
        const picked = await appInvoke<{ uri: string; displayName: string }>(
          "plugin:saf|pick_folder",
        );
        await roostStore.openRoost(picked.uri, picked.displayName, undefined, { skipWelcome: true });
        setStep(2);
        return;
      }
      if (isIOS) {
        // Real folder picker via UIDocumentPickerViewController — lets
        // the user pick any folder visible in Files.app (On My iPhone,
        // iCloud Drive, third-party storage providers). The bookmark
        // re-acquires scoped access across cold starts.
        const picked = await appInvoke<{
          path: string;
          displayName: string;
          bookmark: string;
        }>("plugin:ios-picker|pick_folder");
        await roostStore.openRoost(picked.path, picked.displayName, picked.bookmark, { skipWelcome: true });
        setStep(2);
        return;
      }
      const { open } = await import("@tauri-apps/plugin-dialog");
      const selected = await open({ directory: true, multiple: false });
      if (selected) {
        await roostStore.openRoost(selected as string, undefined, undefined, { skipWelcome: true });
        setStep(2);
      }
    } catch (e) {
      const msg = typeof e === "string" ? e : (e as Error).message;
      // Cancelled picker shouldn't look like a failure.
      if (/cancel/i.test(msg)) return;
      setError(msg);
    }
  }

  /** Run the appropriate test-connection IPC for the active provider's
   *  current input. Returns immediately (sets the result signal) — the
   *  step doesn't advance until the user clicks Continue. */
  async function handleTestConnection() {
    setTesting(true);
    setTestResult(null);
    try {
      const key = provider() === "openrouter" ? orKey().trim() : geminiKey().trim();
      const result = await unwrap(
        provider() === "openrouter"
          ? commands.testOpenrouterConnection(key)
          : commands.testGeminiConnection(key),
      );
      setTestResult(result);
    } catch (e) {
      const msg = errMsg(e);
      setTestResult({
        ok: false,
        latency_ms: 0,
        summary: "Test failed",
        error_message: msg,
      });
    } finally {
      setTesting(false);
    }
  }

  /** Persist whichever key was entered for the active provider, then
   *  advance to step 2. Skip path doesn't write anything — they can
   *  add a key later from Settings. Errors surface inline rather than
   *  silently failing. */
  async function handleSaveAndContinue() {
    setKeySaveError(null);
    const p = provider();
    if (p === "skip") {
      setStep(3);
      return;
    }
    const key = p === "openrouter" ? orKey().trim() : geminiKey().trim();
    if (!key) {
      // Empty input on a non-skip provider is treated as Skip — no
      // popup, just advance. The user can fill the key later from
      // Settings → AI without rerunning onboarding.
      setStep(3);
      return;
    }
    try {
      if (p === "gemini") {
        await settingsStore.save({ gemini_api_key: key, llm_provider: "gemini" });
      } else {
        await settingsStore.save({
          openrouter_api_key: key,
          llm_provider: "openrouter",
        });
      }
      setStep(3);
    } catch (e) {
      const msg = errMsg(e);
      setKeySaveError(msg);
    }
  }

  /** Seed the optional welcome note and advance to the agent-primer step.
   *  Welcome-note creation is best-effort — if it fails (read-only roost,
   *  SAF race on Android) we still advance so the user isn't stuck in a
   *  modal because of a starter file. The corvux:onboarded flag is set
   *  in `handleComplete` (the *real* finish), not here, so a user who
   *  bails on the primer can re-enter it on the next launch. */
  async function handleSeedAndAdvance() {
    setFinishing(true);
    try {
      if (includeWelcomeNote()) {
        try {
          const path = await roostStore.createNewNote("Welcome to Corvux");
          if (path) {
            await unwrap(commands.writeFile(path, WELCOME_NOTE));
            // Open the note in a tab so the user lands inside the note,
            // not on a blank workspace. Indexing happens via the normal
            // tabStore.setOnAfterSave path the next time they save —
            // good enough for a starter file.
            // Locked on purpose, on every platform: this is a primer to
            // READ, and read mode is what shows it fully rendered. The
            // padlock in the top bar unlocks it if they want to edit.
            await tabStore.openTab(path, { readOnly: true });
          }
        } catch (e) {
          log.warn("onboarding", "welcome note creation failed", e);
        }
      }
      setStep(4);
    } finally {
      setFinishing(false);
    }
  }

  /** Final exit from the modal. `openChat` is true when the user clicked
   *  "Try it now" on the agent-primer step — App will open the chat
   *  panel after the modal closes so the user lands in the empty state
   *  with the suggested-question pills they just read about. */
  function handleComplete(openChat: boolean) {
    lsSet("corvux:onboarded", "true");
    props.onComplete(openChat);
  }

  return (
    <div class="fixed inset-0 z-50 bg-bg-primary flex items-center justify-center">
      <div class="max-w-md w-full px-8">
        {/* Logo + title */}
        <div class="text-center mb-8">
          <CorvuxLogo
            alt="Corvux"
            class="w-20 h-20 mx-auto mb-4"
            style={{ filter: "drop-shadow(0 0 18px rgba(79,140,255,0.28))" }}
          />
          <h1 class="text-2xl font-bold text-text-primary">Welcome to Corvux</h1>
          <p class="text-text-muted text-sm mt-1">Your AI-native second brain</p>
        </div>

        {/* Step indicators */}
        <div class="flex items-center justify-center gap-2 mb-8">
          {[0, 1, 2, 3, 4].map((i) => (
            <div
              class={`h-1.5 rounded-full transition-all duration-300 ${
                i === step()
                  ? "w-8 bg-accent"
                  : i < step()
                    ? "w-4 bg-accent/40"
                    : "w-4 bg-bg-hover"
              }`}
            />
          ))}
        </div>

        {/* Step 0: Theme picker. Lives before roost so every later page
            uses the chosen theme; runStartupSequence pushes the choice
            INTO the roost on first install (instead of roost overriding
            device), so onboarding stays visually consistent. */}
        <Show when={step() === 0}>
          <div class="space-y-4">
            <div class="text-center space-y-2">
              <h2 class="text-lg font-semibold text-text-primary">
                Pick a look
              </h2>
              <p class="text-text-muted text-sm">
                Tap a theme to try it. You can change it anytime in Settings.
              </p>
            </div>
            <div class="grid grid-cols-2 gap-2">
              <For each={THEME_OPTIONS}>
                {(opt) => (
                  <button
                    onClick={() => settingsStore.setTheme(opt.id as Theme)}
                    class={`p-3 rounded-lg border text-left transition-colors ${
                      settingsStore.theme() === opt.id
                        ? "border-accent bg-bg-secondary"
                        : "border-border bg-bg-secondary/40 hover:border-border-strong"
                    }`}
                  >
                    <div class="flex items-center gap-1.5 mb-1.5">
                      <div
                        class="w-4 h-4 rounded-full border border-border/40"
                        style={{ background: opt.swatch[0] }}
                      />
                      <div
                        class="w-4 h-4 rounded-full border border-border/40"
                        style={{ background: opt.swatch[1] }}
                      />
                      <div
                        class="w-4 h-4 rounded-full border border-border/40"
                        style={{ background: opt.swatch[2] }}
                      />
                    </div>
                    <div class="text-sm font-medium text-text-primary">
                      {opt.name}
                    </div>
                    <div class="text-xs text-text-muted leading-snug">
                      {opt.description}
                    </div>
                  </button>
                )}
              </For>
            </div>
            <button
              onClick={() => setStep(1)}
              class="w-full py-3 px-4 rounded-lg bg-accent text-bg-primary font-medium hover:opacity-90 transition-opacity"
            >
              Continue
            </button>
          </div>
        </Show>

        {/* Step 1: Open roost */}
        <Show when={step() === 1}>
          <div class="text-center space-y-4">
            <div class="space-y-2">
              <h2 class="text-lg font-semibold text-text-primary">
                Choose your roost
              </h2>
              <p class="text-text-muted text-sm">
                {isIOS
                  ? "A roost is a folder of markdown files. Pick a folder from Files.app — iCloud Drive, On My iPhone, or any storage provider. If you use the same folder on desktop, sync will pair automatically."
                  : isAndroid
                    ? "A roost is a folder of markdown files. Pick an existing folder on your device — if you use the same folder name on desktop, sync will pair automatically."
                    : "A roost is a folder of markdown files. Pick an existing folder or create a new one."}
              </p>
            </div>
            <button
              onClick={handleOpenRoost}
              class="w-full py-3 px-4 rounded-lg bg-accent text-bg-primary font-medium hover:opacity-90 transition-opacity flex items-center justify-center gap-2"
            >
              <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
                <path d="M22 19a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h5l2 3h9a2 2 0 0 1 2 2z" />
              </svg>
              {isAndroid || isIOS ? "Pick Folder" : "Open Folder"}
            </button>
            <Show when={error()}>
              <p class="text-red-400 text-xs">{error()}</p>
            </Show>
            <p class="text-text-muted text-xs opacity-50">
              Your notes stay on your device — nothing is uploaded.
            </p>
          </div>
        </Show>

        {/* Step 1: Provider + API key (optional) */}
        <Show when={step() === 2}>
          <div class="space-y-4">
            <div class="text-center space-y-2">
              <h2 class="text-lg font-semibold text-text-primary">AI setup</h2>
              <p class="text-text-muted text-sm">
                Corvux Agent and semantic search need a model API key. Pick a
                provider — you can change or remove this in Settings later.
              </p>
            </div>

            {/* Provider toggle — three options as pills. The Skip pill
                doubles as "I'll add a key later." */}
            <div class="flex items-center bg-bg-secondary rounded-lg border border-border text-xs p-0.5">
              {(["gemini", "openrouter", "skip"] as const).map((p) => (
                <button
                  onClick={() => {
                    setProvider(p);
                    setTestResult(null);
                    setKeySaveError(null);
                  }}
                  class={`flex-1 py-1.5 rounded-md transition-colors ${
                    provider() === p
                      ? "bg-accent text-bg-primary font-medium"
                      : "text-text-muted hover:text-text-secondary"
                  }`}
                >
                  {p === "gemini" ? "Gemini" : p === "openrouter" ? "OpenRouter" : "Skip"}
                </button>
              ))}
            </div>

            <Show when={provider() === "gemini"}>
              <div class="space-y-1.5">
                <input
                  type="password"
                  placeholder="AIza..."
                  value={geminiKey()}
                  onInput={(e) => {
                    setGeminiKey(e.currentTarget.value);
                    setTestResult(null);
                  }}
                  onKeyDown={(e) => {
                    if (e.key === "Enter") void handleSaveAndContinue();
                  }}
                  class="w-full px-3 py-2.5 rounded-lg bg-bg-secondary border border-border text-text-primary text-sm placeholder-text-muted focus:outline-none focus:border-accent"
                />
                <a
                  href="https://aistudio.google.com/apikey"
                  target="_blank"
                  rel="noopener"
                  class="text-xs text-accent hover:underline inline-block"
                >
                  Get a free API key from Google AI Studio
                </a>
              </div>
            </Show>

            <Show when={provider() === "openrouter"}>
              <div class="space-y-1.5">
                <input
                  type="password"
                  placeholder="sk-or-v1-..."
                  value={orKey()}
                  onInput={(e) => {
                    setOrKey(e.currentTarget.value);
                    setTestResult(null);
                  }}
                  onKeyDown={(e) => {
                    if (e.key === "Enter") void handleSaveAndContinue();
                  }}
                  class="w-full px-3 py-2.5 rounded-lg bg-bg-secondary border border-border text-text-primary text-sm placeholder-text-muted focus:outline-none focus:border-accent"
                />
                <a
                  href="https://openrouter.ai/keys"
                  target="_blank"
                  rel="noopener"
                  class="text-xs text-accent hover:underline inline-block"
                >
                  Create a key at openrouter.ai
                </a>
              </div>
            </Show>

            <Show when={provider() === "skip"}>
              <div class="text-text-muted text-xs px-1">
                Corvux will work as a plain markdown editor — file tree, links,
                tags, search by keyword. AI features (chat, semantic search,
                concept extraction) stay disabled until you add a key in
                Settings → AI.
              </div>
            </Show>

            {/* Test result — only relevant when a real provider is chosen.
                Mirrors the AISection pattern so users see the same
                visual language across surfaces. */}
            <Show when={testResult()}>
              {(r) => (
                <div
                  class={`text-xs flex items-start gap-1.5 ${
                    r().ok ? "text-green" : "text-red"
                  }`}
                  role="status"
                  aria-live="polite"
                >
                  <span class="font-semibold">{r().ok ? "✓" : "✗"}</span>
                  <span class="flex-1">
                    {r().summary} · {r().latency_ms} ms
                    <Show when={!r().ok && r().error_message}>
                      <div class="text-text-muted mt-0.5 break-words font-mono text-[10px]">
                        {r().error_message}
                      </div>
                    </Show>
                  </span>
                </div>
              )}
            </Show>

            <Show when={keySaveError()}>
              {(err) => (
                <div class="text-xs text-red">
                  Save failed: {err()}
                </div>
              )}
            </Show>

            <div class="flex gap-2">
              <Show when={provider() !== "skip"}>
                <button
                  onClick={handleTestConnection}
                  disabled={
                    testing() ||
                    (provider() === "gemini" ? !geminiKey().trim() : !orKey().trim())
                  }
                  class="flex-1 py-2.5 rounded-lg border border-border text-text-secondary hover:bg-bg-hover disabled:opacity-30 disabled:cursor-not-allowed transition-colors text-sm"
                >
                  {testing() ? "Testing…" : "Test"}
                </button>
              </Show>
              <button
                onClick={handleSaveAndContinue}
                class="flex-1 py-2.5 rounded-lg bg-accent text-bg-primary font-medium hover:opacity-90 transition-opacity text-sm"
              >
                Continue
              </button>
            </div>
          </div>
        </Show>

        {/* Step 2: Welcome note + finish */}
        <Show when={step() === 3}>
          <div class="space-y-5">
            <div class="text-center space-y-2">
              <h2 class="text-lg font-semibold text-text-primary">Almost done</h2>
              <p class="text-text-muted text-sm">
                We can drop a starter note in your roost to show you what's
                possible — links, tags, Corvux Agent, the keyboard shortcuts you'll
                use most. Skip if you'd rather start with an empty workspace.
              </p>
            </div>

            <label class="flex items-start gap-3 px-3 py-2.5 rounded-lg bg-bg-secondary border border-border cursor-pointer hover:bg-bg-hover transition-colors">
              <input
                type="checkbox"
                checked={includeWelcomeNote()}
                onChange={(e) => setIncludeWelcomeNote(e.currentTarget.checked)}
                class="mt-0.5"
              />
              <span class="flex-1">
                <span class="block text-text-primary text-sm">
                  Create <span class="font-mono">Welcome to Corvux.md</span>
                </span>
                <span class="block text-text-muted text-xs mt-0.5">
                  ~30 lines of markdown showing core features. Edit or delete
                  any time.
                </span>
              </span>
            </label>

            <button
              onClick={handleSeedAndAdvance}
              disabled={finishing()}
              class="w-full py-3 rounded-lg bg-accent text-bg-primary font-medium hover:opacity-90 disabled:opacity-50 disabled:cursor-wait transition-opacity"
            >
              {finishing() ? "Setting up…" : "Continue"}
            </button>
          </div>
        </Show>

        {/* Step 3: Try the killer workflow */}
        <Show when={step() === 4}>
          <div class="space-y-5">
            <div class="text-center space-y-2">
              <h2 class="text-lg font-semibold text-text-primary">
                Now try asking your roost
              </h2>
              <p class="text-text-muted text-sm">
                Corvux's core loop: ask a question → get an answer with citations
                from your notes → optionally let the agent turn it into tasks or
                edits, with your approval before anything is written.
              </p>
            </div>

            <div class="space-y-2">
              <p class="text-text-muted text-xs uppercase tracking-wide">
                Try one of these
              </p>
              <For each={SAMPLE_PROMPTS}>
                {(prompt) => (
                  <div class="px-3 py-2 rounded-lg border border-border bg-bg-secondary text-text-secondary text-xs leading-relaxed">
                    "{prompt}"
                  </div>
                )}
              </For>
            </div>

            <div class="rounded-lg bg-bg-secondary/50 border border-border/50 px-3 py-2 space-y-1 text-[11px] text-text-muted leading-relaxed">
              <div>
                <span class="text-text-secondary">📄 Sources</span> — every
                answer cites the notes it read.
              </div>
              <div>
                <span class="text-text-secondary">✓ Approve</span> — the agent
                shows a diff before any write or edit.
              </div>
              <div>
                <span class="text-text-secondary">⌨️ Ctrl+L</span> opens chat
                from anywhere.
              </div>
            </div>

            <div class="flex gap-2">
              <button
                onClick={() => handleComplete(false)}
                class="flex-1 py-2.5 rounded-lg border border-border text-text-secondary hover:bg-bg-hover transition-colors text-sm"
              >
                Explore on my own
              </button>
              <button
                onClick={() => handleComplete(true)}
                class="flex-1 py-2.5 rounded-lg bg-accent text-bg-primary font-medium hover:opacity-90 transition-opacity text-sm"
              >
                Open chat & try it
              </button>
            </div>
          </div>
        </Show>
      </div>
    </div>
  );
}

/// Sample prompts shown on the agent-primer step. Phrased to flex the
/// killer loop (search → ground → action) rather than showcase a single
/// tool. The agent will fan out across `roost_search` / `roost_read` /
/// task tools depending on the prompt.
const SAMPLE_PROMPTS = [
  "What am I forgetting about projects I started this month?",
  "Find notes I have related to a recurring theme — pick one.",
  "Pull the action items out of my recent notes and list them.",
];
