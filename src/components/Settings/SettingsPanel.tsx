import {
  For,
  Show,
  createMemo,
  createSignal,
  onCleanup,
  onMount,
  type JSX,
} from "solid-js";
import { isMobile } from "../../lib/platform";
import AppearanceSection from "./AppearanceSection";
// EditorSection is rendered inside AppearanceSection now (2026-05-20
// merge — theme + typography are the same "look & feel" concern).
import AISection from "./AISection";
import CorvuxAgentSection from "./CorvuxAgentSection";
import TasksSection from "./TasksSection";
import PrivacySection from "./PrivacySection";
import RoostSection from "./RoostSection";
import SyncSection from "./SyncSection";
import DiagnosticsSection from "./DiagnosticsSection";
import McpToolsSection from "./McpToolsSection";
import { updaterStore } from "../../stores/updater";
import { settingsStore } from "../../stores/settings";
import CorvuxLogo from "../Brand/CorvuxLogo";

export type PageId =
  | "index"
  | "appearance"
  | "ai"
  | "agent"
  | "tasks"
  | "privacy"
  | "roost"
  | "sync"
  | "mcpTools"
  | "diagnostics"
  | "about";

export interface SettingsPanelProps {
  onClose: () => void;
  /** Opens the full ShortcutOverlay cheat-sheet. */
  onOpenShortcuts?: () => void;
  /** Page to land on when first rendered. Defaults to "index" (the
   *  drill-down menu). Pass e.g. `"ai"` to deep-link the user
   *  straight to AI Models — used by the chat panel's "Add an API
   *  key" CTA so the user doesn't have to click through Settings
   *  index after we already know where they want to go. */
  initialPage?: PageId;
}

/**
 * Settings shell — drill-down nav, Obsidian-style.
 *
 * Responsive:
 * - Mobile (< sm): fills viewport edge-to-edge, no rounded corners,
 *   respects env(safe-area-inset-*) so the header isn't eaten by the
 *   notch/status bar.
 * - Desktop: centered modal, 560 px wide, 85 vh max-height, rounded.
 *
 * Escape: pops to index from a sub-page; closes from index.
 */
export default function SettingsPanel(props: SettingsPanelProps) {
  const [page, setPage] = createSignal<PageId>(props.initialPage ?? "index");
  const mobile = isMobile();

  function handleKey(e: KeyboardEvent) {
    if (e.key !== "Escape") return;
    e.preventDefault();
    if (page() !== "index") setPage("index");
    else props.onClose();
  }

  // Android hardware back button → treat like Escape: pop one level if
  // we're on a sub-page, otherwise close the modal. Without this handler
  // the Android back button calls history.back() on an empty history
  // stack and the OS quits the whole app.
  //
  // Pattern: push one history entry per modal open. When back fires, a
  // popstate event is emitted for THIS entry. We intercept it, do our
  // own dismiss logic, and never actually leave our app's history root.
  function handlePopState() {
    if (page() !== "index") {
      // Re-push so the next back press can also be captured — otherwise
      // the second tap would exit the app.
      setPage("index");
      history.pushState({ corvuxSettings: true }, "");
    } else {
      props.onClose();
    }
  }

  onMount(() => {
    document.addEventListener("keydown", handleKey);
    // Seed a history entry so the first Android back press is ours to
    // handle via popstate. Browser still treats us as "at the page";
    // we just have one extra entry to consume.
    history.pushState({ corvuxSettings: true }, "");
    window.addEventListener("popstate", handlePopState);
  });
  onCleanup(() => {
    document.removeEventListener("keydown", handleKey);
    window.removeEventListener("popstate", handlePopState);
  });

  const currentTitle = createMemo<string>(() => {
    const p = page();
    if (p === "index") return "Settings";
    const found = PAGES.find((x) => x.id === p);
    return found ? found.label : "Settings";
  });

  // Body JSX is inlined (not wrapped in a factory function) so SolidJS
  // can track the <Show> memos inside the component's reactive owner.
  // Using `const body = () => (<>...</>)` + `{body()}` breaks ownership:
  // the Show memos get created in a detached context, `page()` reads
  // don't register, and every Show evaluates once at mount and stays
  // frozen — which is the "empty body" symptom the user hit, with the
  // "computations created outside a createRoot" warning in the console.
  //
  // Same applies to <Header/> — we pass reactive bindings directly as
  // props so the header re-renders on page() changes without a factory
  // call site.

  // Body content — plain function, reactive because `page()` is read
  // inside it. When interpolated in JSX as `{renderBody()}`, SolidJS
  // wraps it in a tracking context that re-runs on page changes.
  //
  // Components called as plain functions (Index({...}) instead of
  // <Index .../>) — bypasses `createComponent` which was silently
  // failing to invoke Index in the previous pass (confirmed via
  // adb logcat: memo ran, Index body never did).
  function renderBody(): JSX.Element {
    const p = page();
    // Every component invoked as a plain function, not via JSX. On this
    // build Solid's `createComponent` silently no-ops for locally-defined
    // components (confirmed via adb logcat: `[settings] Index invoked`
    // never fired while other logs in the same component did). Direct
    // function calls bypass the template cache and always run the
    // current module's function reference.
    switch (p) {
      case "appearance":
        return <div class="p-5">{AppearanceSection()}</div>;
      case "ai":
        return <div class="p-5">{AISection()}</div>;
      case "agent":
        return <div class="p-5">{CorvuxAgentSection()}</div>;
      case "tasks":
        return <div class="p-5">{TasksSection()}</div>;
      case "privacy":
        return <div class="p-5">{PrivacySection()}</div>;
      case "roost":
        return <div class="p-5">{RoostSection()}</div>;
      case "sync":
        return <div class="p-5">{SyncSection()}</div>;
      case "mcpTools":
        return <div class="p-5">{McpToolsSection()}</div>;
      case "diagnostics":
        return <div class="p-5">{DiagnosticsSection()}</div>;
      case "about":
        return About();
      case "index":
      default:
        return Index({
          onSelect: (id) => setPage(id),
          onOpenShortcuts: props.onOpenShortcuts,
        });
    }
  }

  const header = (
    <Header
      title={currentTitle()}
      onBack={page() !== "index" ? () => setPage("index") : undefined}
      onClose={props.onClose}
    />
  );

  // Mobile: ONE scroll container (no flex, no nested heights). The header
  // sits inside it as `sticky top-0`. Safe-area padding goes on the
  // scroll container itself. Any previous flex-column + flex-1 nesting
  // collapsed to zero height on Android WebView, which was the root
  // cause of the "settings is empty" symptom.
  if (mobile) {
    return (
      <div
        class="fixed inset-0 z-50 bg-bg-primary overflow-y-auto overscroll-contain"
        role="dialog"
        aria-modal="true"
        aria-label="Settings"
        style={{
          // NO padding-top here. With viewport-fit=cover the iOS status bar
          // overlays the webview; a padding-top on the SCROLL container
          // scrolls away, so on scroll/overscroll the content (and the
          // sticky header) renders under the status bar. The top inset must
          // live on the sticky header instead (below) — its opaque bg covers
          // the status-bar strip and the header pins beneath it.
          "padding-bottom": "env(safe-area-inset-bottom)",
          "padding-left": "env(safe-area-inset-left)",
          "padding-right": "env(safe-area-inset-right)",
        }}
      >
        <div
          class="sticky top-0 z-10 bg-bg-primary"
          style={{ "padding-top": "env(safe-area-inset-top)" }}
        >
          {header}
        </div>
        {renderBody()}
      </div>
    );
  }

  // Desktop: centered modal. Single scroll container on the modal body,
  // header sticky inside. Matches the mobile structure exactly minus
  // the fullscreen positioning.
  return (
    <div
      class="fixed inset-0 z-50 flex items-center justify-center"
      role="dialog"
      aria-modal="true"
      aria-label="Settings"
    >
      <div
        class="absolute inset-0 bg-black/50"
        onClick={props.onClose}
        aria-hidden="true"
      />
      <div class="relative w-[560px] max-w-[calc(100vw-32px)] max-h-[85vh] overflow-y-auto bg-bg-primary border border-border rounded-xl shadow-2xl">
        <div class="sticky top-0 z-10 bg-bg-primary">{header}</div>
        {renderBody()}
      </div>
    </div>
  );
}

// ─── Pages metadata ────────────────────────────────────────────────────────
// Typed-only data — plain strings, no JSX. Icons are rendered by a switch
// in the row component itself, which is the one place that cares about
// what `id` an entry maps to. Keeping JSX out of module-level arrays
// sidesteps a SolidJS quirk where module-level templates can reference
// DOM nodes already mounted by a prior HMR cycle.

interface PageMeta {
  id: Exclude<PageId, "index">;
  label: string;
  description: string;
  group: "options" | "advanced";
}

const PAGES: PageMeta[] = [
  { id: "appearance", label: "Appearance", description: "Theme, font, size, line height", group: "options" },
  { id: "ai", label: "AI Models", description: "Provider, API keys, chat model, embeddings", group: "options" },
  { id: "agent", label: "Corvux Agent", description: "Approvals, roost conventions, tool history", group: "options" },
  { id: "tasks", label: "Tasks", description: "Folders excluded from the Tasks pane", group: "options" },
  { id: "privacy", label: "Privacy", description: "What leaves your device, what stays", group: "options" },
  { id: "roost", label: "Roost", description: "Active Roost and folders", group: "options" },
  { id: "sync", label: "Sync (beta)", description: "Paired devices, peer-to-peer", group: "options" },
  { id: "mcpTools", label: "Connect AI tools", description: "Claude Desktop, Cursor, Claude Code", group: "options" },
  { id: "diagnostics", label: "Diagnostics", description: "Logs and bug report bundle", group: "advanced" },
  { id: "about", label: "About", description: "Version and credits", group: "advanced" },
];

// ─── Icons (each a call, never a stored node) ──────────────────────────────

function iconFor(id: PageMeta["id"] | "shortcuts"): JSX.Element {
  switch (id) {
    case "appearance":
      return (
        <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round">
          <circle cx="13.5" cy="6.5" r=".5" fill="currentColor" />
          <circle cx="17.5" cy="10.5" r=".5" fill="currentColor" />
          <circle cx="8.5" cy="7.5" r=".5" fill="currentColor" />
          <circle cx="6.5" cy="12.5" r=".5" fill="currentColor" />
          <path d="M12 2C6.5 2 2 6.5 2 12s4.5 10 10 10c.8 0 1.4-.3 1.9-.7.3-.3.5-.7.5-1.1 0-.3-.1-.6-.3-.9-.2-.3-.3-.6-.3-.9 0-.6.4-1 .9-1.2.9-.3 2.3-.9 2.3-2.4 0-2.9-3-5.2-7-5.2S4 9.2 4 12c0 4.4 3.6 8 8 8" />
        </svg>
      );
    // "editor" page removed 2026-05-20 — typography merged into Appearance.
    case "ai":
      return (
        <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round">
          <path d="M12 2a7 7 0 00-4 12.7c.6.4.9 1 .9 1.7V17h6.2v-.6c0-.7.3-1.3.9-1.7A7 7 0 0012 2z" />
          <path d="M10 22h4" />
        </svg>
      );
    case "agent":
      return (
        <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round">
          <circle cx="12" cy="12" r="3" />
          <path d="M19.4 15a1.65 1.65 0 00.33 1.82l.06.06a2 2 0 010 2.83 2 2 0 01-2.83 0l-.06-.06a1.65 1.65 0 00-1.82-.33 1.65 1.65 0 00-1 1.51V21a2 2 0 01-2 2 2 2 0 01-2-2v-.09a1.65 1.65 0 00-1-1.51 1.65 1.65 0 00-1.82.33l-.06.06a2 2 0 01-2.83 0 2 2 0 010-2.83l.06-.06a1.65 1.65 0 00.33-1.82 1.65 1.65 0 00-1.51-1H3a2 2 0 01-2-2 2 2 0 012-2h.09a1.65 1.65 0 001.51-1 1.65 1.65 0 00-.33-1.82l-.06-.06a2 2 0 010-2.83 2 2 0 012.83 0l.06.06a1.65 1.65 0 001.82.33h0a1.65 1.65 0 001-1.51V3a2 2 0 012-2 2 2 0 012 2v.09a1.65 1.65 0 001 1.51 1.65 1.65 0 001.82-.33l.06-.06a2 2 0 012.83 0 2 2 0 010 2.83l-.06.06a1.65 1.65 0 00-.33 1.82v0a1.65 1.65 0 001.51 1H21a2 2 0 012 2 2 2 0 01-2 2h-.09a1.65 1.65 0 00-1.51 1z" />
        </svg>
      );
    case "tasks":
      return (
        <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round">
          <polyline points="9 11 12 14 22 4" />
          <path d="M21 12v7a2 2 0 01-2 2H5a2 2 0 01-2-2V5a2 2 0 012-2h11" />
        </svg>
      );
    case "privacy":
      return (
        <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round">
          <path d="M12 22s8-4 8-10V5l-8-3-8 3v7c0 6 8 10 8 10z" />
          <path d="M9 12l2 2 4-4" />
        </svg>
      );
    case "roost":
      return (
        <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round">
          <path d="M3 7v10a2 2 0 002 2h14a2 2 0 002-2V9a2 2 0 00-2-2h-6l-2-2H5a2 2 0 00-2 2z" />
        </svg>
      );
    case "sync":
      return (
        <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round">
          <path d="M21 12a9 9 0 11-3.3-6.9" />
          <path d="M21 3v6h-6" />
        </svg>
      );
    case "mcpTools":
      return (
        <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round">
          <path d="M9 2v6M15 2v6" />
          <path d="M6 8h12v3a6 6 0 01-12 0V8z" />
          <path d="M12 17v5" />
        </svg>
      );
    case "diagnostics":
      return (
        <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round">
          <path d="M22 12h-4l-3 9L9 3l-3 9H2" />
        </svg>
      );
    case "about":
      return (
        <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round">
          <path d="M12 22c5.5 0 10-4.5 10-10S17.5 2 12 2 2 6.5 2 12s4.5 10 10 10z" />
          <path d="M12 8v4" />
          <path d="M12 16h.01" />
        </svg>
      );
    case "shortcuts":
      return (
        <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round">
          <rect x="3" y="6" width="18" height="12" rx="2" />
          <path d="M7 10h.01M11 10h.01M15 10h.01M7 14h10" />
        </svg>
      );
  }
}

// ─── Header ────────────────────────────────────────────────────────────────

function Header(props: {
  title: string;
  onBack?: () => void;
  onClose: () => void;
}) {
  return (
    <div class="flex items-center gap-2 px-3 py-3 border-b border-border shrink-0">
      <Show
        when={props.onBack}
        fallback={<div class="w-9 h-9" aria-hidden="true" />}
      >
        <button
          onClick={props.onBack}
          class="w-9 h-9 flex items-center justify-center rounded-full hover:bg-bg-hover text-text-secondary hover:text-text-primary transition-colors"
          aria-label="Back to settings index"
        >
          <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round">
            <path d="M15 18l-6-6 6-6" />
          </svg>
        </button>
      </Show>
      <h2 class="flex-1 text-center text-text-primary font-semibold text-base truncate">
        {props.title}
      </h2>
      <button
        onClick={props.onClose}
        class="w-9 h-9 flex items-center justify-center rounded-full hover:bg-bg-hover text-text-secondary hover:text-text-primary transition-colors"
        aria-label="Close settings"
      >
        <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
          <path d="M18 6L6 18M6 6l12 12" />
        </svg>
      </button>
    </div>
  );
}

// ─── Index ─────────────────────────────────────────────────────────────────

function Index(props: {
  onSelect: (id: Exclude<PageId, "index">) => void;
  onOpenShortcuts?: () => void;
}) {
  // "Connect AI tools" (MCP → Claude Desktop / Cursor / Claude Code) needs the
  // bundled corvux-mcp sidecar, which is excluded from mobile builds
  // (externalBin: [] in tauri.ios/android.conf.json). Hide the row on mobile —
  // it advertises something that physically can't run on a phone.
  const options = PAGES.filter(
    (p) => p.group === "options" && !(isMobile() && p.id === "mcpTools"),
  );
  const advanced = PAGES.filter((p) => p.group === "advanced");

  // Inline row renderer — plain function returning JSX. Called directly
  // as `row({...})`, not via `<Row .../>` JSX, because local components
  // via JSX were silently not invoking on this build.
  function row(item: {
    icon: JSX.Element;
    label: string;
    description?: string;
    trailing?: JSX.Element;
    onClick: () => void;
  }): JSX.Element {
    return (
      <button
        onClick={item.onClick}
        class="w-full flex items-center gap-3 px-4 py-3 text-left hover:bg-bg-hover transition-colors"
      >
        <span class="shrink-0 text-text-secondary" aria-hidden="true">
          {item.icon}
        </span>
        <span class="min-w-0 flex-1">
          <span class="block text-text-primary text-sm truncate">
            {item.label}
          </span>
          <Show when={item.description}>
            <span class="block text-text-muted text-[11px] truncate">
              {item.description}
            </span>
          </Show>
        </span>
        <Show when={item.trailing}>
          <span class="shrink-0">{item.trailing}</span>
        </Show>
        <svg
          width="16"
          height="16"
          viewBox="0 0 24 24"
          fill="none"
          stroke="currentColor"
          stroke-width="2"
          stroke-linecap="round"
          class="shrink-0 text-text-muted"
          aria-hidden="true"
        >
          <path d="M9 18l6-6-6-6" />
        </svg>
      </button>
    );
  }

  return (
    <div class="px-4 py-4 space-y-5">
      {/* Options group — inlined. No SettingsGroup component wrapper
          because local-component JSX was failing to invoke. */}
      <section>
        <h3 class="text-text-muted text-[11px] font-semibold uppercase tracking-wider mb-2 px-2">
          Options
        </h3>
        <div class="rounded-xl bg-bg-secondary/60 border border-border/60 overflow-hidden divide-y divide-border/50">
          <For each={options}>
            {(p) =>
              row({
                icon: iconFor(p.id),
                label: p.label,
                description: p.description,
                onClick: () => props.onSelect(p.id),
              })
            }
          </For>
          <Show when={props.onOpenShortcuts}>
            {row({
              icon: iconFor("shortcuts"),
              label: "Keyboard shortcuts",
              description: "Full cheat sheet",
              trailing: (
                <kbd class="px-1.5 py-0.5 rounded bg-bg-tertiary text-[10px] border border-border font-mono text-text-muted">
                  Ctrl+/
                </kbd>
              ),
              onClick: () => props.onOpenShortcuts?.(),
            })}
          </Show>
        </div>
      </section>

      {/* About group */}
      <section>
        <h3 class="text-text-muted text-[11px] font-semibold uppercase tracking-wider mb-2 px-2">
          About
        </h3>
        <div class="rounded-xl bg-bg-secondary/60 border border-border/60 overflow-hidden divide-y divide-border/50">
          <For each={advanced}>
            {(p) =>
              row({
                icon: iconFor(p.id),
                label: p.label,
                description: p.description,
                onClick: () => props.onSelect(p.id),
              })
            }
          </For>
        </div>
      </section>
    </div>
  );
}

// ─── About ─────────────────────────────────────────────────────────────────

function About() {
  const [version, setVersion] = createSignal<string>("0.1.0");
  const [supportEndpoint, setSupportEndpoint] = createSignal<string>(
    settingsStore.settings().support_endpoint ?? "",
  );
  const [supportSaveStatus, setSupportSaveStatus] = createSignal<
    "" | "saving" | "saved" | "invalid"
  >("");

  onMount(async () => {
    try {
      const { getVersion } = await import("@tauri-apps/api/app");
      const v = await getVersion();
      if (v) setVersion(v);
    } catch {
      // Browser dev / non-Tauri: keep the fallback. The build-time
      // package.json version is good enough here.
    }
  });

  function statusLabel(): string {
    const s = updaterStore.status();
    switch (s.kind) {
      case "checking":
        return "Checking…";
      case "downloading":
        return "Downloading update…";
      case "installing":
        return "Installing update…";
      case "available":
        return `Update available — v${s.version}`;
      case "up-to-date":
        return "You're up to date.";
      case "error":
        return `Couldn't check: ${s.message}`;
      case "idle":
      default:
        return "";
    }
  }

  return (
    <div class="p-6 flex flex-col items-center text-center gap-2">
      <div class="w-16 h-16 rounded-2xl bg-bg-secondary border border-border flex items-center justify-center mb-2 overflow-hidden">
        <CorvuxLogo class="w-11 h-11 object-contain" />
      </div>
      <div class="text-text-primary font-semibold text-base">Corvux</div>
      <div class="text-text-muted text-xs">
        AI-Native Knowledge OS · v{version()}
      </div>
      <div class="text-text-muted text-xs opacity-60 mt-1 mb-3">
        Built by Corvux Mindware
      </div>

      <Show when={updaterStore.isSupported()}>
        <div class="w-full max-w-xs flex flex-col items-center gap-2 mt-2">
          <button
            onClick={() => void updaterStore.check({ manual: true })}
            disabled={updaterStore.isChecking() || updaterStore.isInstalling()}
            class="px-3 py-1.5 rounded-lg border border-border bg-bg-secondary hover:bg-bg-hover text-text-primary text-xs disabled:opacity-50 disabled:cursor-not-allowed transition-colors"
          >
            Check for updates
          </button>
          <Show when={statusLabel()}>
            <div class="text-text-muted text-[11px]" aria-live="polite">
              {statusLabel()}
            </div>
          </Show>
          <Show when={updaterStore.available()}>
            {(info) => (
              <button
                onClick={() => void updaterStore.installAndRelaunch()}
                class="px-3 py-1.5 rounded-lg bg-accent hover:bg-accent-hover text-white text-xs transition-colors"
              >
                Restart &amp; install v{info().version}
              </button>
            )}
          </Show>
        </div>
      </Show>

      <div class="w-full max-w-xs flex flex-col gap-1.5 mt-6 text-left">
        <label class="text-text-secondary text-[11px] font-semibold uppercase tracking-wider">
          Support contact
        </label>
        <p class="text-text-muted text-[11px] leading-relaxed">
          Where the "Report a problem" flow sends bug reports. Use a
          <code class="px-1 py-0 mx-0.5 bg-bg-secondary rounded text-[10px]">
            mailto:
          </code>
          URL to open an email draft, or an
          <code class="px-1 py-0 mx-0.5 bg-bg-secondary rounded text-[10px]">
            https://
          </code>
          URL (e.g. a GitHub issue template). Leave blank to copy reports
          to the clipboard only.
        </p>
        <input
          type="text"
          placeholder="mailto:support@meetcorvux.com"
          value={supportEndpoint()}
          onInput={(e) => {
            setSupportEndpoint(e.currentTarget.value);
            setSupportSaveStatus("");
          }}
          class="px-3 py-1.5 rounded-lg border border-border bg-bg-secondary/60 text-text-primary text-xs placeholder:text-text-muted focus:outline-none focus:border-accent/60"
        />
        <div class="flex items-center gap-2">
          <button
            onClick={async () => {
              const trimmed = supportEndpoint().trim();
              if (
                trimmed &&
                !trimmed.startsWith("mailto:") &&
                !/^https?:\/\//i.test(trimmed)
              ) {
                setSupportSaveStatus("invalid");
                return;
              }
              setSupportSaveStatus("saving");
              try {
                await settingsStore.save({
                  support_endpoint: trimmed === "" ? null : trimmed,
                });
                setSupportSaveStatus("saved");
                setTimeout(() => setSupportSaveStatus(""), 2000);
              } catch {
                setSupportSaveStatus("invalid");
              }
            }}
            class="px-3 py-1 rounded-lg bg-accent text-white text-xs font-medium hover:bg-accent-hover transition-colors"
          >
            Save
          </button>
          <Show when={supportSaveStatus() === "saving"}>
            <span class="text-text-muted text-[11px]">Saving…</span>
          </Show>
          <Show when={supportSaveStatus() === "saved"}>
            <span class="text-green text-[11px]">Saved.</span>
          </Show>
          <Show when={supportSaveStatus() === "invalid"}>
            <span class="text-red text-[11px]">
              Use a mailto: or https:// URL.
            </span>
          </Show>
        </div>
      </div>
    </div>
  );
}
