import { createSignal, createRoot } from "solid-js";
import { commands, unwrap } from "../lib/invoke";
import type {
  Settings as WireSettings,
  DeviceProfile as WireDeviceProfile,
} from "../lib/bindings";

// The store's exported interfaces below are the *normalized* in-memory
// shape: every field present, every f32/f64 finite. The matching
// `Wire*` types from bindings.ts allow missing fields (Rust uses
// `#[serde(default)]` to fill defaults at serialize time) and surface
// f32/f64 as `number | null` (specta's NaN safety net).
//
// The `normalize*` helpers below collapse the wire shape into the
// in-memory shape at one point — every consumer of the store sees
// fully-populated objects with no nullables. If a field genuinely
// can be missing at runtime, this is where you'd surface that —
// not via 50 `?.` checks scattered across the panels.
import { lsGet, lsSet } from "../lib/storage";
import { log } from "../lib/log";
import { roostSettingsStore } from "./roostSettings";

export type Theme = "dark" | "light" | "nord" | "synthwave" | "parchment" | "corvid";
export type EditorFont = "mono" | "sans" | "serif" | "system" | "rounded" | "slab";

export interface ThemeOption {
  id: Theme;
  name: string;
  description: string;
  /** Swatch for the theme picker — [bg, accent, text] hex strings. */
  swatch: [string, string, string];
}

export const THEME_OPTIONS: ThemeOption[] = [
  { id: "dark", name: "Midnight", description: "Charcoal-purple — quiet default", swatch: ["#1a1a24", "#cba6f7", "#e4e4ef"] },
  { id: "parchment", name: "Parchment", description: "Cream + indigo — feels like writing on paper", swatch: ["#f4efe6", "#3b3b86", "#2d2a24"] },
  { id: "light", name: "Daylight", description: "Warm paper tones for sunny rooms", swatch: ["#eff1f5", "#8839ef", "#4c4f69"] },
  { id: "nord", name: "Nord", description: "Cool arctic blues — low-glare focus", swatch: ["#2e3440", "#88c0d0", "#eceff4"] },
  { id: "synthwave", name: "Synthwave '84", description: "Retro neon — pink, cyan, deep purple", swatch: ["#241b2f", "#ff7edb", "#f4eee4"] },
  { id: "corvid", name: "Corvid", description: "Editorial ink + brass — raven's eye accent", swatch: ["#1f2230", "#d4a254", "#e6e3dc"] },
];

/** Which LLM backend to use for chat. `auto` prefers local if loaded, else cloud. */
export type LlmBackend = "auto" | "cloud" | "local";

/** Which chat provider to use. `gemini`, `openrouter`, `openai`, and
 *  `anthropic` are cloud; `ollama` talks to a locally-running (or LAN)
 *  Ollama server. Local llama.cpp paths are gated by `llm_backend`
 *  separately. */
export type LlmProvider = "gemini" | "openrouter" | "ollama" | "openai" | "anthropic";

/** ML tier auto-detected from hardware capability — see ai/capability.rs. */
export type MlTier = "tier0" | "tier1" | "tier2" | "tier3";

export interface DeviceProfile {
  total_ram_gb: number;
  cpu_cores: number;
  free_disk_gb: number;
  tier: MlTier;
  platform: string;
}

export interface ModelStatus {
  name: string;
  installed: boolean;
  size_bytes: number;
  path: string;
}

export interface Settings {
  gemini_api_key: string | null;
  model: string;
  llm_backend: LlmBackend;
  /** Active agent profile id ("default", "quicklookup", "research", "writer"). */
  agent_profile: string;
  /** Active cloud chat provider — Gemini direct or OpenRouter gateway. */
  llm_provider: LlmProvider;
  /** OpenRouter API key (separate from Gemini so users can keep both). */
  openrouter_api_key: string | null;
  /** Active OpenRouter model id, e.g. `anthropic/claude-sonnet-4.6`. */
  openrouter_model: string;
  /** Base URL for the user's Ollama server. Defaults to
   *  `http://localhost:11434`. Can point at a LAN host. */
  ollama_base_url: string;
  /** Active Ollama model tag (e.g. `llama3.2:3b`). Empty until the
   *  user picks a model from the dropdown. */
  ollama_model: string;
  /** Optional bearer token for users who front Ollama with an auth
   *  proxy. `null` is the common localhost case. */
  ollama_api_key: string | null;
  /** OpenAI API key (direct provider). Separate from the other keys. */
  openai_api_key: string | null;
  /** Active OpenAI model id, e.g. `gpt-4.1`. */
  openai_model: string;
  /** Anthropic (Claude) API key (direct provider). */
  anthropic_api_key: string | null;
  /** Active Claude model id, e.g. `claude-sonnet-5`. */
  anthropic_model: string;
  /** Where the in-app "Report a problem" flow routes the bug bundle.
   *  `mailto:` opens the user's default mail client; `https://` opens
   *  a browser tab (typically a GitHub Issue template or support form).
   *  When null the modal still works — user copies the bundle manually. */
  support_endpoint: string | null;
  /** Agent autonomy controls — what the agent is allowed to do without
   *  asking. Defaults: read tools execute, write/destructive pause for
   *  approval. */
  agent_autonomy: AgentAutonomy;
  /** Phase 2.A feature flag — route the agent loop through each
   *  provider's native function-calling API (Gemini Tools, OpenAI
   *  tools) instead of the legacy XML-in-prompt protocol. Default
   *  false during burn-in. */
  use_native_tools?: boolean;
  /** Desktop-only: launch Corvux automatically when the user signs into
   *  their computer. Default true. The toggle lives in
   *  Settings → Appearance → Startup and is hidden on mobile (no OS
   *  equivalent). Saving it reconciles the OS autostart registration
   *  backend-side. */
  launch_at_startup: boolean;
  /** Desktop-only: hide the window to the system tray when the close (X)
   *  button is pressed, instead of quitting — the app keeps running so
   *  sync stays live and reopening is instant. Default true (X hides);
   *  turn it off to make X quit. The toggle lives in Settings →
   *  Appearance → Startup and is hidden on mobile (no tray). The tray's
   *  "Quit" item is always the real exit. */
  close_to_tray: boolean;
}

export interface AgentAutonomy {
  auto_approve_reads: boolean;
  auto_approve_writes: boolean;
  /** Per-tool allowlist. Tool names that match `Tool::name()` skip the
   *  approval gate even when `auto_approve_writes` is false. Populated
   *  by clicking "Always approve <tool>" on a pending tool card. */
  auto_approve_tools: string[];
  /** Gitignore-style glob patterns. Any write/destructive tool that
   *  touches a path matching one of these is hard-rejected by the
   *  agent loop BEFORE the approval card is shown. Patterns are
   *  matched against roost-relative paths. See `ai/tools/glob.rs`
   *  for the matcher's supported syntax (`*`, `**`, `?`, trailing-
   *  slash folder form). */
  agent_blocked_paths: string[];
}

/** Wire shape returned by `list_agent_profiles` — drives the picker UI. */
export interface AgentProfileInfo {
  id: string;
  label: string;
  summary: string;
}

/** Default values used by `normalizeSettings` when the wire shape
 *  omits a field. Mirrors `impl Default for Settings` on the Rust
 *  side (`commands/ai/types.rs`). Single source of fallback so the
 *  store's initial state and a wire-fill use the same defaults. */
const DEFAULT_SETTINGS: Settings = {
  gemini_api_key: null,
  model: "gemini-2.5-flash",
  llm_backend: "auto",
  agent_profile: "default",
  llm_provider: "gemini",
  openrouter_api_key: null,
  openrouter_model: "anthropic/claude-sonnet-4.6",
  ollama_base_url: "http://localhost:11434",
  ollama_model: "",
  ollama_api_key: null,
  openai_api_key: null,
  openai_model: "gpt-5.5",
  anthropic_api_key: null,
  anthropic_model: "claude-sonnet-5",
  support_endpoint: "mailto:support@meetcorvux.com",
  agent_autonomy: {
    auto_approve_reads: true,
    auto_approve_writes: false,
    auto_approve_tools: [],
    agent_blocked_paths: [],
  },
  use_native_tools: true,
  launch_at_startup: true,
  close_to_tray: true,
};

/** Hydrate the wire-shape `Settings` into the store's required-fields
 *  view. `#[serde(default)]` on the Rust side already fills missing
 *  fields at serialize time, but the bindings type marks them
 *  optional; this normalizer reasserts the contract. */
function normalizeSettings(raw: WireSettings): Settings {
  return {
    gemini_api_key: raw.gemini_api_key ?? DEFAULT_SETTINGS.gemini_api_key,
    model: raw.model ?? DEFAULT_SETTINGS.model,
    llm_backend: raw.llm_backend ?? DEFAULT_SETTINGS.llm_backend,
    agent_profile: raw.agent_profile ?? DEFAULT_SETTINGS.agent_profile,
    llm_provider: raw.llm_provider ?? DEFAULT_SETTINGS.llm_provider,
    openrouter_api_key: raw.openrouter_api_key ?? DEFAULT_SETTINGS.openrouter_api_key,
    openrouter_model: raw.openrouter_model ?? DEFAULT_SETTINGS.openrouter_model,
    ollama_base_url: raw.ollama_base_url ?? DEFAULT_SETTINGS.ollama_base_url,
    ollama_model: raw.ollama_model ?? DEFAULT_SETTINGS.ollama_model,
    ollama_api_key: raw.ollama_api_key ?? DEFAULT_SETTINGS.ollama_api_key,
    openai_api_key: raw.openai_api_key ?? DEFAULT_SETTINGS.openai_api_key,
    openai_model: raw.openai_model ?? DEFAULT_SETTINGS.openai_model,
    anthropic_api_key: raw.anthropic_api_key ?? DEFAULT_SETTINGS.anthropic_api_key,
    anthropic_model: raw.anthropic_model ?? DEFAULT_SETTINGS.anthropic_model,
    support_endpoint: raw.support_endpoint ?? DEFAULT_SETTINGS.support_endpoint,
    agent_autonomy: raw.agent_autonomy
      ? {
          auto_approve_reads:
            raw.agent_autonomy.auto_approve_reads ??
            DEFAULT_SETTINGS.agent_autonomy.auto_approve_reads,
          auto_approve_writes:
            raw.agent_autonomy.auto_approve_writes ??
            DEFAULT_SETTINGS.agent_autonomy.auto_approve_writes,
          auto_approve_tools:
            raw.agent_autonomy.auto_approve_tools ??
            DEFAULT_SETTINGS.agent_autonomy.auto_approve_tools,
          agent_blocked_paths:
            raw.agent_autonomy.agent_blocked_paths ??
            DEFAULT_SETTINGS.agent_autonomy.agent_blocked_paths,
        }
      : DEFAULT_SETTINGS.agent_autonomy,
    use_native_tools: raw.use_native_tools ?? DEFAULT_SETTINGS.use_native_tools,
    launch_at_startup: raw.launch_at_startup ?? DEFAULT_SETTINGS.launch_at_startup,
    close_to_tray: raw.close_to_tray ?? DEFAULT_SETTINGS.close_to_tray,
  };
}

/** Hydrate the wire-shape `DeviceProfile` into the store's view.
 *  f32 fields surface as `number | null` (specta's NaN safety net);
 *  finite RAM/disk values always survive, so coalescing to 0 is
 *  safe and lets every UI consumer drop the `?? 0` boilerplate. */
function normalizeDeviceProfile(raw: WireDeviceProfile): DeviceProfile {
  return {
    total_ram_gb: raw.total_ram_gb ?? 0,
    cpu_cores: raw.cpu_cores,
    free_disk_gb: raw.free_disk_gb ?? 0,
    tier: raw.tier,
    platform: raw.platform,
  };
}

const FONT_MAP: Record<EditorFont, string> = {
  mono: "'JetBrains Mono', 'Fira Code', 'Cascadia Code', ui-monospace, monospace",
  sans: "'Inter', 'SF Pro', -apple-system, BlinkMacSystemFont, 'Segoe UI', sans-serif",
  serif: "'Georgia', 'Cambria', 'Times New Roman', serif",
  // System UI font of the host OS — native look on each platform.
  system: "system-ui, -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, 'Helvetica Neue', sans-serif",
  // Rounded sans — SF Pro Rounded on Apple, falls back to system rounded
  // alternatives or system-ui elsewhere.
  rounded: "ui-rounded, 'SF Pro Rounded', 'Hiragino Maru Gothic ProN', 'Quicksand', 'Comfortaa', system-ui, sans-serif",
  // Slab serif — falls back to Georgia where the named faces are absent.
  slab: "'Roboto Slab', 'Source Serif Pro', 'Bitter', 'Rockwell', Georgia, serif",
};

function preferredDeviceTheme(): Theme {
  if (typeof window === "undefined" || !window.matchMedia) return "dark";
  return window.matchMedia("(prefers-color-scheme: light)").matches ? "light" : "dark";
}

function createSettingsStore() {
  // Initial state mirrors the Rust `impl Default for Settings` so the
  // UI renders the same blanks-with-defaults on cold start as it does
  // after a successful `getSettings()` against a fresh prefs file.
  const [settings, setSettings] = createSignal<Settings>({ ...DEFAULT_SETTINGS });
  const [loaded, setLoaded] = createSignal(false);
  const [deviceProfile, setDeviceProfile] = createSignal<DeviceProfile | null>(null);
  const [llmModelStatus, setLlmModelStatus] = createSignal<ModelStatus | null>(null);
  const [llmLoaded, setLlmLoaded] = createSignal<boolean>(false);
  const [agentProfiles, setAgentProfiles] = createSignal<AgentProfileInfo[]>([]);

  // Theme — persisted in localStorage, applied via data-theme attribute
  const stored = lsGet("corvux:theme") as Theme | null;
  const [theme, setThemeSignal] = createSignal<Theme>(stored ?? preferredDeviceTheme());

  // Typography — persisted in localStorage, applied via CSS variables
  const [editorFont, setEditorFontSignal] = createSignal<EditorFont>(
    (lsGet("corvux:editorFont") as EditorFont) || "sans"
  );
  const [editorFontSize, setEditorFontSizeSignal] = createSignal<number>(
    Number(lsGet("corvux:editorFontSize")) || 14
  );
  const [editorLineHeight, setEditorLineHeightSignal] = createSignal<number>(
    Number(lsGet("corvux:editorLineHeight")) || 1.6
  );

  // Apply typography to DOM
  function applyTypography() {
    if (typeof document === "undefined") return;
    const root = document.documentElement;
    root.style.setProperty("--editor-font-family", FONT_MAP[editorFont()]);
    root.style.setProperty("--editor-font-size", `${editorFontSize()}px`);
    root.style.setProperty("--editor-line-height", `${editorLineHeight()}`);
  }
  applyTypography();

  // Apply theme to DOM immediately. Dark is the implicit default — no
  // attribute needed. Every other theme gets `data-theme="<id>"` so CSS
  // can swap variables.
  function applyTheme(t: Theme) {
    if (typeof document === "undefined") return;
    // Native scrollbars / form controls follow color-scheme, so any
    // light-background theme (light, parchment) must declare "light".
    const isLightBg = t === "light" || t === "parchment";
    document.documentElement.style.colorScheme = isLightBg ? "light" : "dark";
    if (t === "dark") {
      document.documentElement.removeAttribute("data-theme");
    } else {
      document.documentElement.setAttribute("data-theme", t);
    }
  }
  applyTheme(theme());

  function setTheme(t: Theme) {
    setThemeSignal(t);
    // Always persist to localStorage as the device default — covers roosts
    // that haven't set their own theme + the no-roost state.
    lsSet("corvux:theme", t);
    applyTheme(t);
    // Also write to the roost's `.corvux/settings.json` if one is open, so
    // the theme follows the roost to other devices.
    if (roostSettingsStore.loaded()) {
      // Partial appearance update — roostSettings.save merges over the
      // current state, so accent_color is preserved if previously set.
      void roostSettingsStore.save({ appearance: { theme: t } });
    }
  }

  // The theme the cycle button would switch to next, in THEME_OPTIONS
  // order. Single source of truth so the icon-rail label ("Switch to
  // <name>") and `toggleTheme` can't disagree about what a click does.
  function nextTheme(): ThemeOption {
    const idx = THEME_OPTIONS.findIndex((o) => o.id === theme());
    return THEME_OPTIONS[(idx + 1) % THEME_OPTIONS.length];
  }

  // Cycle through the available themes in THEME_OPTIONS order. Kept for
  // the icon-rail button and mobile menu; the Settings panel uses the
  // full swatch-picker for deliberate selection.
  function toggleTheme() {
    setTheme(nextTheme().id);
  }

  async function load() {
    try {
      const result = await unwrap(commands.getSettings());
      setSettings(normalizeSettings(result));
      setLoaded(true);
    } catch (e) {
      log.error("settings", "failed to load settings", e);
    }
    try {
      const profile = await unwrap(commands.getDeviceProfile());
      setDeviceProfile(normalizeDeviceProfile(profile));
    } catch (e) {
      // Non-fatal — just means we can't show hardware tier in UI
      log.warn("settings", "failed to detect device profile", e);
    }
    await loadLlmModelStatus();
    await loadAgentProfiles();
  }

  /** Fetch the available agent-profile descriptors so the picker can
   *  render labels + summaries from the source of truth in Rust. */
  async function loadAgentProfiles() {
    try {
      const profiles = await unwrap(commands.listAgentProfiles());
      setAgentProfiles(profiles);
    } catch (e) {
      log.warn("settings", "failed to fetch agent profiles", e);
    }
  }

  async function setAgentProfile(id: string) {
    await save({ agent_profile: id });
  }

  async function loadLlmModelStatus() {
    try {
      const status = await commands.getLlmModelStatus();
      setLlmModelStatus(status);
    } catch (e) {
      log.warn("settings", "failed to fetch LLM model status", e);
    }
  }

  async function loadLlmModel() {
    try {
      await unwrap(commands.loadLlmModel());
      setLlmLoaded(true);
    } catch (e) {
      log.error("settings", "failed to load LLM model", e);
      setLlmLoaded(false);
      throw e;
    }
  }

  async function unloadLlmModel() {
    try {
      await unwrap(commands.unloadLlmModel());
      setLlmLoaded(false);
    } catch (e) {
      log.error("settings", "failed to unload LLM model", e);
    }
  }

  let saveChain: Promise<void> = Promise.resolve();

  function save(updates: Partial<Settings>): Promise<void> {
    // Merge only when this write reaches the head of the queue. Two panels can
    // save concurrently (for example a toggle and an API-key test); merging
    // both against the same stale snapshot made the later response erase the
    // first field even though both IPC calls succeeded.
    const operation = saveChain.then(async () => {
      const merged: Settings = { ...settings(), ...updates };
      try {
        await unwrap(commands.saveSettings(merged));
        setSettings(merged);
      } catch (e) {
        log.error("settings", "failed to save settings", e);
        throw e;
      }
    });
    saveChain = operation.catch(() => undefined);
    return operation;
  }

  /** True if the *active* provider is "ready to chat" — has a key
   *  (Gemini, OpenRouter) or has a model selected (Ollama, which
   *  doesn't need a key on the localhost path). Used by the chat
   *  panel + onboarding to gate the ready state without inspecting
   *  the provider at every call site. */
  function hasApiKey(): boolean {
    const s = settings();
    if (s.llm_provider === "ollama") {
      return !!s.ollama_model && s.ollama_model.trim().length > 0;
    }
    const key =
      s.llm_provider === "openrouter"
        ? s.openrouter_api_key
        : s.llm_provider === "openai"
          ? s.openai_api_key
          : s.llm_provider === "anthropic"
            ? s.anthropic_api_key
            : s.gemini_api_key;
    return !!key && key.trim().length > 0;
  }

  function setEditorFont(f: EditorFont) {
    setEditorFontSignal(f);
    lsSet("corvux:editorFont", f);
    applyTypography();
  }

  function setEditorFontSize(s: number) {
    const clamped = Math.max(10, Math.min(24, s));
    setEditorFontSizeSignal(clamped);
    lsSet("corvux:editorFontSize", String(clamped));
    applyTypography();
  }

  function setEditorLineHeight(lh: number) {
    const clamped = Math.max(1.0, Math.min(2.5, lh));
    setEditorLineHeightSignal(clamped);
    lsSet("corvux:editorLineHeight", String(clamped));
    applyTypography();
  }

  return {
    settings, loaded, load, save, hasApiKey,
    deviceProfile,
    llmModelStatus, loadLlmModelStatus,
    llmLoaded, loadLlmModel, unloadLlmModel,
    agentProfiles, loadAgentProfiles, setAgentProfile,
    theme, setTheme, toggleTheme, nextTheme,
    editorFont, setEditorFont,
    editorFontSize, setEditorFontSize,
    editorLineHeight, setEditorLineHeight,
  };
}

export const settingsStore = createRoot(createSettingsStore);
