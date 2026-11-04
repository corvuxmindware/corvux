//! State types and IPC structs for the AI subsystem. Held in Tauri state
//! so every handler in `commands::ai::*` can reach them without rebuilding.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use crate::ai::llamacpp::LlamaCppModel;

/// Shared reqwest HTTP client held in Tauri state so every provider
/// (Gemini now, future cloud backends later) reuses the same connection
/// pool instead of spinning up a new TLS stack per call.
pub struct HttpClient(pub Arc<reqwest::Client>);

/// Lazy-loaded local LLM model. `None` until the user downloads the model
/// and triggers load. Loading takes 2-5s (GGUF → GPU VRAM), then cached.
pub struct LlamaCppState(pub Mutex<Option<Arc<LlamaCppModel>>>);

/// Which LLM backend to use for chat. `Auto` tries local first (if model is
/// loaded), falls back to cloud.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, specta::Type)]
#[serde(rename_all = "lowercase")]
pub enum LlmBackend {
    #[default]
    Auto,
    Cloud,
    Local,
}

/// Which LLM provider to use. Gemini + OpenRouter are cloud; Ollama is
/// a locally-running (or LAN) inference server the user manages. Local
/// llama.cpp paths are gated by `llm_backend` separately and aren't
/// part of this enum.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, specta::Type)]
#[serde(rename_all = "lowercase")]
pub enum LlmProvider {
    #[default]
    Gemini,
    Openrouter,
    Ollama,
    /// OpenAI direct — the user's own OpenAI key against api.openai.com.
    /// Same models OpenRouter can reach, but no gateway markup and one
    /// fewer party in the request path.
    Openai,
    /// Anthropic (Claude) direct — native Messages API against
    /// api.anthropic.com with the user's own Anthropic key.
    Anthropic,
}

#[derive(Debug, Clone, Serialize, Deserialize, specta::Type)]
pub struct Settings {
    #[serde(default)]
    pub gemini_api_key: Option<String>,
    #[serde(default = "default_model")]
    pub model: String,
    #[serde(default)]
    pub llm_backend: LlmBackend,
    /// Active agent profile — selects which `ToolBundle`s the chat
    /// loop has, plus iteration / token budget. Stored as the stable
    /// id (`"default"`, `"quicklookup"`, `"research"`, `"writer"`)
    /// so `AgentProfile::from_str` can resolve it.
    #[serde(default = "default_agent_profile")]
    pub agent_profile: String,
    /// Cloud chat provider. Gemini = direct Gemini API; OpenRouter =
    /// the unified gateway (multi-model). Defaults to Gemini so existing
    /// installs are unchanged.
    #[serde(default)]
    pub llm_provider: LlmProvider,
    /// OpenRouter API key. Independent from `gemini_api_key` so users
    /// can keep both keys configured and switch providers without
    /// re-pasting.
    #[serde(default)]
    pub openrouter_api_key: Option<String>,
    /// Active OpenRouter model id, in `provider/model-name` form (e.g.
    /// `anthropic/claude-sonnet-4.6`). Treated as opaque text so the
    /// user can paste any model id OpenRouter advertises — we don't
    /// hardcode an allowlist.
    #[serde(default = "default_openrouter_model")]
    pub openrouter_model: String,
    /// Base URL for the user's Ollama server. Defaults to the local
    /// `http://localhost:11434`; can be pointed at a LAN host for
    /// users who run Ollama on a beefier desktop and chat from a laptop.
    /// Treated as opaque text — validation happens at request time.
    #[serde(default = "default_ollama_base_url")]
    pub ollama_base_url: String,
    /// Active Ollama model tag (e.g. `llama3.2:3b`, `qwen2.5:14b`).
    /// Empty until the user picks one from the model dropdown.
    #[serde(default)]
    pub ollama_model: String,
    /// Optional bearer token for users who front their Ollama server
    /// with an auth proxy. `None` is the common localhost case.
    #[serde(default)]
    pub ollama_api_key: Option<String>,
    /// OpenAI API key (direct provider). Independent from the other
    /// provider keys so users can keep several configured and switch
    /// without re-pasting. Kept in the OS keychain (Locked Decision #10).
    #[serde(default)]
    pub openai_api_key: Option<String>,
    /// Active OpenAI model id (e.g. `gpt-5.5`, `gpt-5.4-nano`). Opaque
    /// text — we don't hardcode an allowlist so the user can paste any
    /// model their key can reach.
    #[serde(default = "default_openai_model")]
    pub openai_model: String,
    /// Anthropic (Claude) API key (direct provider). Independent from the
    /// other provider keys; stored in the OS keychain.
    #[serde(default)]
    pub anthropic_api_key: Option<String>,
    /// Active Claude model id (e.g. `claude-sonnet-5`, `claude-opus-4-8`).
    /// Opaque text — no hardcoded allowlist.
    #[serde(default = "default_anthropic_model")]
    pub anthropic_model: String,
    /// Where the in-app "Report a problem" flow sends users. Either a
    /// `mailto:` URL or an `https://` URL (a GitHub Issue template, a
    /// support form, etc.). When unset the bug-report modal still works
    /// — the user can copy the diagnostic bundle and route it manually.
    /// Stored as opaque text so we don't lock the user into one channel.
    #[serde(default)]
    pub support_endpoint: Option<String>,
    /// Agent autonomy controls — what the agent is allowed to do without
    /// asking. Defaults to "read tools execute, write/destructive tools
    /// pause for approval." Per-tool always-approve list lets a user
    /// promote individual tools after seeing them work a few times.
    #[serde(default)]
    pub agent_autonomy: AgentAutonomy,
    /// Phase 2.A feature flag — route the agent loop through the
    /// native function-calling path (`stream_chat_with_tools`) instead
    /// of the legacy XML-in-prompt protocol. Default `true` post burn-in;
    /// the system prompt's tool-registry blurb adapts (no `<tool_call>`
    /// XML instructions on the native path) so Gemini doesn't get the
    /// conflicting signal that caused empty model bubbles. Removing the
    /// flag (and the XML path) is Phase 2.A's definition-of-done.
    #[serde(default = "default_true")]
    pub use_native_tools: bool,
    /// Desktop-only: register Corvux to launch automatically when the
    /// user signs into their computer. Default `true` (ON) — including
    /// for installs upgraded from a settings.json written before this
    /// field existed, since `#[serde(default = "default_true")]` fills
    /// the missing field as `true`. Reconciled against the OS autostart
    /// registry on every launch and whenever the toggle is saved (see
    /// `reconcile_autostart` in lib.rs). No-op on Android/iOS — the
    /// field still (de)serializes, but the OS reconcile is compiled out.
    #[serde(default = "default_true")]
    pub launch_at_startup: bool,
    /// Desktop-only: when the window's close (X) button is pressed, hide
    /// the window to the system tray instead of quitting — the process
    /// (and the sync engine, mDNS, peer connections) keeps running so the
    /// device stays reachable for incoming deltas, and reopening from the
    /// tray is instant. Default `true` (ON), matching `launch_at_startup`
    /// — a synced second brain benefits from staying live after the window
    /// is closed; `#[serde(default = "default_true")]` also flips it ON for
    /// installs upgraded from a settings.json written before the field
    /// existed. Turn it off in Settings → Appearance → Startup to make X
    /// quit. The tray's "Quit" item is always the real exit, routed
    /// through the same shutdown. No effect on mobile (no tray surface) —
    /// the field still (de)serializes so settings.json round-trips across
    /// platforms.
    #[serde(default = "default_true")]
    pub close_to_tray: bool,
}

/// Per-user agent-autonomy preferences. Exposed via `get_settings` /
/// `save_settings` so the Settings → AI panel can render them.
#[derive(Debug, Clone, Serialize, Deserialize, specta::Type)]
pub struct AgentAutonomy {
    /// Read tools always execute when true (the default). Off-switch
    /// is rare — included for completeness so users can audit every
    /// single tool call if they want.
    #[serde(default = "default_true")]
    pub auto_approve_reads: bool,
    /// Write tools execute without approval when true. Default false —
    /// the trust gate. Users opt in explicitly per the harness vision.
    #[serde(default)]
    pub auto_approve_writes: bool,
    /// Per-tool allowlist. Names that match `Tool::name()` skip the
    /// approval gate even when `auto_approve_writes` is false. Populated
    /// by clicking "Always approve this tool" on a pending tool card.
    #[serde(default)]
    pub auto_approve_tools: Vec<String>,
    /// Gitignore-style glob patterns. Write/destructive tools that
    /// touch a path matching ANY pattern here are hard-rejected by the
    /// agent loop BEFORE the approval card is shown — the user can't
    /// even click Approve. Lets users carve out folders the agent
    /// must never touch (e.g. `Journal/`, `Finance/`, `**/secret.md`).
    /// Read tools are unaffected — reading a note isn't destructive.
    /// See `ai/tools/glob.rs` for the matcher's supported syntax.
    #[serde(default)]
    pub agent_blocked_paths: Vec<String>,
}

fn default_true() -> bool {
    true
}

impl Default for AgentAutonomy {
    fn default() -> Self {
        Self {
            auto_approve_reads: true,
            auto_approve_writes: false,
            auto_approve_tools: Vec::new(),
            agent_blocked_paths: Vec::new(),
        }
    }
}

fn default_model() -> String {
    // Q-04: aligned with frontend `DEFAULT_SETTINGS.model` in
    // `src/stores/settings.ts`. Current production model is
    // gemini-2.5-flash (see CLAUDE.md tech stack). The previous default
    // `gemini-3.1-flash-lite` drifted from the frontend, so a user
    // hitting a settings-load failure would see a different model
    // selected in the UI than the backend actually used.
    "gemini-2.5-flash".to_string()
}

fn default_agent_profile() -> String {
    "default".to_string()
}

fn default_openrouter_model() -> String {
    "anthropic/claude-sonnet-4.6".to_string()
}

fn default_ollama_base_url() -> String {
    "http://localhost:11434".to_string()
}

fn default_openai_model() -> String {
    // GPT-5.5 is OpenAI's current flagship (Apr 2026). Users change it to
    // gpt-5.4-nano (cheap) or any model their key can reach.
    "gpt-5.5".to_string()
}

fn default_anthropic_model() -> String {
    // Sonnet 5 = balanced/current, cheaper than Opus for a default chat
    // assistant. Users can switch to claude-opus-4-8 for max capability.
    "claude-sonnet-5".to_string()
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            gemini_api_key: None,
            model: default_model(),
            llm_backend: LlmBackend::Auto,
            agent_profile: default_agent_profile(),
            llm_provider: LlmProvider::default(),
            openrouter_api_key: None,
            openrouter_model: default_openrouter_model(),
            ollama_base_url: default_ollama_base_url(),
            ollama_model: String::new(),
            ollama_api_key: None,
            openai_api_key: None,
            openai_model: default_openai_model(),
            anthropic_api_key: None,
            anthropic_model: default_anthropic_model(),
            support_endpoint: Some("mailto:support@meetcorvux.com".to_string()),
            agent_autonomy: AgentAutonomy::default(),
            // Native function calling (V2) is the default as of the
            // Phase 2.A burn-in flip. Faster and fewer parsing mistakes
            // than the legacy XML-tag path, which stays wired behind the
            // toggle (`use_native_tools = false`) as an escape hatch.
            // Kept aligned with frontend `DEFAULT_SETTINGS.use_native_tools`
            // in `src/stores/settings.ts` — change BOTH sides together
            // (the `defaults_match_frontend_canonical_values` test pins this).
            use_native_tools: true,
            // Launch-at-startup ships ON by default (product decision
            // 2026-06-04): a synced second brain benefits from staying
            // live across reboots. Users can flip it off in
            // Settings → Appearance → Startup. Desktop-only — ignored on
            // mobile where the autostart plugin isn't registered.
            launch_at_startup: true,
            // Close-to-tray ships ON (same rationale as launch_at_startup):
            // a synced second brain stays live after the window is closed,
            // so the device keeps receiving deltas. Users who want X to
            // quit outright flip it off in Settings → Appearance; the
            // tray's "Quit" is always available regardless.
            close_to_tray: true,
        }
    }
}

pub struct SettingsState {
    pub path: PathBuf,
    pub settings: Mutex<Settings>,
}

/// Holds the cancellation flag and the JoinHandle of the active chat task.
/// Storing the handle lets us abort a stale stream when the user sends a
/// new message before the previous one finishes.
pub struct ChatCancel {
    pub flag: Arc<AtomicBool>,
    pub task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

#[cfg(test)]
mod default_settings_tests {
    use super::*;

    /// Q-04: the Rust `Settings::default()` shape is mirrored by the
    /// frontend `DEFAULT_SETTINGS` constant in
    /// `src/stores/settings.ts`. The two drifted in two visible ways
    /// (model name, `use_native_tools`) before this audit; this test
    /// pins the canonical values so future drift surfaces as a Rust
    /// test failure. If you intentionally change a default, update
    /// BOTH sides and the assertions below.
    #[test]
    fn defaults_match_frontend_canonical_values() {
        let s = Settings::default();
        assert_eq!(
            s.model, "gemini-2.5-flash",
            "default Gemini model must match frontend DEFAULT_SETTINGS.model"
        );
        assert_eq!(
            s.openrouter_model, "anthropic/claude-sonnet-4.6",
            "default OpenRouter model drift"
        );
        assert_eq!(
            s.ollama_base_url, "http://localhost:11434",
            "default Ollama base URL drift"
        );
        assert_eq!(s.ollama_model, "", "default Ollama model must be empty");
        assert_eq!(
            s.openai_model, "gpt-5.5",
            "default OpenAI model drift — keep in sync with frontend"
        );
        assert_eq!(
            s.anthropic_model, "claude-sonnet-5",
            "default Anthropic model drift — keep in sync with frontend"
        );
        assert_eq!(s.agent_profile, "default");
        assert!(
            s.use_native_tools,
            "default use_native_tools must be true (native function-calling, V2)"
        );
        assert!(s.gemini_api_key.is_none());
        assert!(s.openrouter_api_key.is_none());
        assert!(s.ollama_api_key.is_none());
        assert!(s.openai_api_key.is_none());
        assert!(s.anthropic_api_key.is_none());
        assert_eq!(
            s.support_endpoint.as_deref(),
            Some("mailto:support@meetcorvux.com"),
            "default support_endpoint must point at the Corvux support inbox so the bug-report modal has a default destination out of the box"
        );
        assert!(s.agent_autonomy.auto_approve_reads);
        assert!(!s.agent_autonomy.auto_approve_writes);
        assert!(s.agent_autonomy.auto_approve_tools.is_empty());
        assert!(s.agent_autonomy.agent_blocked_paths.is_empty());
        assert!(
            s.launch_at_startup,
            "launch_at_startup must default ON to match frontend DEFAULT_SETTINGS.launch_at_startup"
        );
        assert!(
            s.close_to_tray,
            "close_to_tray must default ON to match frontend DEFAULT_SETTINGS.close_to_tray — X hides to tray unless the user opts out"
        );
    }

    /// Existing installs have a settings.json written before
    /// `launch_at_startup` existed. Deserializing such a file must
    /// default the field to `true` so the ON-by-default behavior also
    /// applies on upgrade — not just to fresh installs. This pins the
    /// `#[serde(default = "default_true")]` contract that the whole
    /// "ON by default for everyone" product decision rests on.
    #[test]
    fn launch_at_startup_defaults_true_when_field_absent() {
        let s: Settings =
            serde_json::from_str("{}").expect("empty object deserializes via serde defaults");
        assert!(s.launch_at_startup);

        // An explicit `false` must still round-trip — the user who turned
        // it off doesn't get it silently re-enabled on the next launch.
        let off: Settings = serde_json::from_str(r#"{"launch_at_startup": false}"#)
            .expect("explicit false deserializes");
        assert!(!off.launch_at_startup);
    }

    /// `close_to_tray` is `#[serde(default = "default_true")]`, so a
    /// settings.json written before the field existed must deserialize as
    /// ON — hide-to-tray applies on upgrade, not just fresh installs. An
    /// explicit `false` must round-trip so a user who opted out doesn't
    /// get it silently re-enabled on the next launch.
    #[test]
    fn close_to_tray_defaults_true_when_field_absent() {
        let s: Settings =
            serde_json::from_str("{}").expect("empty object deserializes via serde defaults");
        assert!(s.close_to_tray);

        let off: Settings = serde_json::from_str(r#"{"close_to_tray": false}"#)
            .expect("explicit false deserializes");
        assert!(!off.close_to_tray);
    }
}
