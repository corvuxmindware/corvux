//! Explicit, per-provider consent before chat content leaves the device for an
//! AI service (App Store guideline 5.1.2(i), Nov 2025: name the AI service and
//! get permission BEFORE sending personal data to it).
//!
//! Enforcement: `chat::build_chat_provider` — the only place a production
//! chat provider is constructed (ratchet: `tests/ai_consent_guard.rs`) —
//! returns it wrapped in [`ConsentGated`], which re-checks consent before
//! EVERY request. A provider object lives for a whole agent run (up to 8
//! model round-trips, reused by subagents), so a once-at-build check would let
//! tool results keep flowing after the user withdrew consent mid-run.
//!
//! The frontend asks first (via [`ai_consent_check`] + [`ai_consent_grant`])
//! for a good prompt; the backend wrapper is what makes "sent without
//! permission" unrepresentable for any current or future caller.
//!
//! Storage is a separate `ai_consent.json` next to `settings.json`, NOT a
//! `Settings` field: `save_settings` replaces the whole struct with the
//! frontend's copy, so a stale Settings page could silently re-grant a
//! consent the user just revoked. Reads and writes here are serialized by
//! [`CONSENT_LOCK`] and written atomically.
//!
//! Absent file = nothing granted, so every install (new or upgraded) is asked
//! once per provider. Deliberate: the prompt is visible and non-destructive
//! (the user taps OK and the message is sent), unlike a silent behaviour flip.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, State};

use super::types::{LlmProvider, Settings, SettingsState};
use crate::ai::{ChatMessage, LLMProvider, StreamEvent, ToolDef};

/// Prefix of the error returned when the gate refuses. The frontend matches
/// on it (`src/lib/chat-error.ts`).
pub const CONSENT_REQUIRED_PREFIX: &str = "AI_CONSENT_REQUIRED:";

const CONSENT_FILE: &str = "ai_consent.json";

/// Serializes every read-modify-write of the consent file.
static CONSENT_LOCK: Mutex<()> = Mutex::new(());

/// Where a chat would go, as shown in the consent prompt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
pub struct AiConsentTarget {
    /// Stable consent key: `gemini`, `openrouter`, `openai`, `anthropic`, or
    /// `ollama:<host>` (`ollama:ollama.com` for Ollama cloud models).
    pub id: String,
    /// Human name for the prompt, e.g. "Google Gemini".
    pub provider_name: String,
    /// Host the request goes to, e.g. `generativelanguage.googleapis.com`.
    pub host: String,
    /// Provider-specific sentence for the prompt (may be empty), e.g. that
    /// OpenRouter forwards to the model's maker.
    pub detail: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct ConsentFile {
    #[serde(default)]
    granted: BTreeSet<String>,
}

/// Mirrors `ai::ollama::OLLAMA_DEFAULT_BASE`: `OllamaChat::new` sends to it
/// when the configured base URL is blank.
fn ollama_effective_base(base: &str) -> &str {
    if base.trim().is_empty() {
        crate::ai::ollama::OLLAMA_DEFAULT_BASE
    } else {
        base.trim()
    }
}

/// Ollama names cloud-hosted models `<name>:<size>-cloud` (or `:cloud`); the
/// local daemon forwards those to ollama.com.
fn is_ollama_cloud_model(model: &str) -> bool {
    let m = model.trim().to_ascii_lowercase();
    m.ends_with("-cloud") || m.ends_with(":cloud")
}

/// Who receives the chat for the given provider configuration. Every provider
/// needs consent — including an Ollama server on this machine, because the
/// local daemon can forward to ollama.com (fail closed: ask once rather than
/// guess from a model name).
pub fn required_consent(
    provider: LlmProvider,
    ollama_base: &str,
    ollama_model: &str,
    openrouter_model: &str,
) -> AiConsentTarget {
    let t = |id: &str, name: &str, host: &str, detail: String| AiConsentTarget {
        id: id.to_string(),
        provider_name: name.to_string(),
        host: host.to_string(),
        detail,
    };
    match provider {
        LlmProvider::Gemini => t(
            "gemini",
            "Google Gemini",
            "generativelanguage.googleapis.com",
            String::new(),
        ),
        LlmProvider::Openrouter => {
            let maker = openrouter_model.split('/').next().unwrap_or("").trim();
            let detail = if maker.is_empty() {
                "OpenRouter passes it on to the company that makes the model you picked.".into()
            } else {
                format!(
                    "OpenRouter passes it on to the company that makes the model you picked ({maker})."
                )
            };
            t("openrouter", "OpenRouter", "openrouter.ai", detail)
        }
        LlmProvider::Openai => t("openai", "OpenAI", "api.openai.com", String::new()),
        LlmProvider::Anthropic => t(
            "anthropic",
            "Anthropic (Claude)",
            "api.anthropic.com",
            String::new(),
        ),
        LlmProvider::Ollama => {
            if is_ollama_cloud_model(ollama_model) {
                return t(
                    "ollama:ollama.com",
                    "Ollama Cloud",
                    "ollama.com",
                    "Cloud models (names ending in \"-cloud\") run on ollama.com, not on your device."
                        .into(),
                );
            }
            let host = url::Url::parse(ollama_effective_base(ollama_base))
                .ok()
                .and_then(|u| u.host_str().map(|h| h.to_ascii_lowercase()))
                .unwrap_or_else(|| "unknown-host".to_string());
            let local = matches!(
                host.trim_start_matches('[').trim_end_matches(']'),
                "localhost" | "127.0.0.1" | "::1"
            );
            let detail = if local {
                "Ollama runs on this computer, but it can forward requests to other servers (for example its cloud models)."
                    .to_string()
            } else {
                String::new()
            };
            AiConsentTarget {
                id: format!("ollama:{host}"),
                provider_name: format!("the Ollama server at {host}"),
                host,
                detail,
            }
        }
    }
}

fn target_for(settings: &Settings) -> AiConsentTarget {
    required_consent(
        settings.llm_provider,
        &settings.ollama_base_url,
        &settings.ollama_model,
        &settings.openrouter_model,
    )
}

pub(crate) fn consent_path(settings_path: &Path) -> PathBuf {
    settings_path.with_file_name(CONSENT_FILE)
}

/// Unreadable or corrupt file = nothing granted (fail closed: the user is
/// asked again rather than data being sent without a recorded consent).
fn read_granted(path: &Path) -> BTreeSet<String> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str::<ConsentFile>(&s).ok())
        .map(|f| f.granted)
        .unwrap_or_default()
}

fn write_granted(path: &Path, granted: &BTreeSet<String>) -> Result<(), String> {
    use std::io::Write;
    let json = serde_json::to_string_pretty(&ConsentFile {
        granted: granted.clone(),
    })
    .map_err(|e| format!("serialize AI consent: {e}"))?;
    let tmp = path.with_extension("json.tmp");
    let mut file =
        std::fs::File::create(&tmp).map_err(|e| format!("open AI consent temp file: {e}"))?;
    file.write_all(json.as_bytes())
        .and_then(|_| file.sync_all())
        .map_err(|e| format!("write AI consent temp file: {e}"))?;
    drop(file);
    super::settings::atomic_replace(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("replace AI consent file: {e}")
    })
}

fn lock() -> Result<std::sync::MutexGuard<'static, ()>, String> {
    CONSENT_LOCK
        .lock()
        .map_err(|e| format!("AI consent lock error: {e}"))
}

fn is_granted(settings_path: &Path, id: &str) -> Result<bool, String> {
    let _guard = lock()?;
    Ok(read_granted(&consent_path(settings_path)).contains(id))
}

fn check_granted(settings_path: &Path, id: &str) -> Result<(), String> {
    if is_granted(settings_path, id)? {
        Ok(())
    } else {
        Err(format!("{CONSENT_REQUIRED_PREFIX}{id}"))
    }
}

/// A chat provider that re-checks the user's consent before every request.
pub struct ConsentGated {
    inner: Box<dyn LLMProvider>,
    settings_path: PathBuf,
    consent_id: String,
}

impl ConsentGated {
    fn check(&self) -> Result<(), String> {
        check_granted(&self.settings_path, &self.consent_id)
    }
}

/// Wrap a freshly built provider. Fails immediately (before any other chat
/// work) if consent is missing; afterwards every request re-checks.
pub(crate) fn gate(
    inner: Box<dyn LLMProvider>,
    settings_path: &Path,
    target: &AiConsentTarget,
) -> Result<Box<dyn LLMProvider>, String> {
    let gated = ConsentGated {
        inner,
        settings_path: settings_path.to_path_buf(),
        consent_id: target.id.clone(),
    };
    gated.check()?;
    Ok(Box::new(gated))
}

#[async_trait]
impl LLMProvider for ConsentGated {
    async fn stream_chat(
        &self,
        app: &AppHandle,
        messages: Vec<ChatMessage>,
        system_context: Option<String>,
        cancel: Arc<AtomicBool>,
    ) -> Result<String, String> {
        self.check()?;
        self.inner
            .stream_chat(app, messages, system_context, cancel)
            .await
    }

    async fn stream_chat_with_tools(
        &self,
        app: &AppHandle,
        messages: Vec<ChatMessage>,
        system_context: Option<String>,
        tools: Vec<ToolDef>,
        cancel: Arc<AtomicBool>,
    ) -> Result<tokio::sync::mpsc::Receiver<StreamEvent>, String> {
        self.check()?;
        self.inner
            .stream_chat_with_tools(app, messages, system_context, tools, cancel)
            .await
    }

    fn model_name(&self) -> &str {
        self.inner.model_name()
    }

    fn is_local(&self) -> bool {
        self.inner.is_local()
    }
}

/// Grant consent, but only for the target the CURRENT settings would send to.
/// Rejects anything else (a stale prompt after a provider switch, or an id the
/// UI never showed), so the file can only ever hold ids the user was asked
/// about.
fn grant_current(settings_path: &Path, settings: &Settings, id: &str) -> Result<(), String> {
    if target_for(settings).id != id {
        return Err(
            "This permission request is out of date. Send your message again to review it."
                .to_string(),
        );
    }
    let _guard = lock()?;
    let path = consent_path(settings_path);
    let mut set = read_granted(&path);
    if set.insert(id.to_string()) {
        write_granted(&path, &set)?;
    }
    Ok(())
}

fn revoke(settings_path: &Path, id: &str) -> Result<(), String> {
    let _guard = lock()?;
    let path = consent_path(settings_path);
    let mut set = read_granted(&path);
    if set.remove(id) {
        write_granted(&path, &set)?;
    }
    Ok(())
}

/// Whether the active, fully configured provider still needs consent.
/// Returns `None` when consent is granted or the provider isn't configured
/// yet (missing key/model) — in that last case `chat_send`'s own "no API key"
/// error is the useful message, not a consent prompt.
fn pending_consent(settings: &Settings, granted: &BTreeSet<String>) -> Option<AiConsentTarget> {
    let configured = match settings.llm_provider {
        LlmProvider::Gemini => settings.gemini_api_key.is_some(),
        LlmProvider::Openrouter => settings.openrouter_api_key.is_some(),
        LlmProvider::Openai => settings.openai_api_key.is_some(),
        LlmProvider::Anthropic => settings.anthropic_api_key.is_some(),
        LlmProvider::Ollama => !settings.ollama_model.trim().is_empty(),
    };
    if !configured {
        return None;
    }
    Some(target_for(settings)).filter(|t| !granted.contains(&t.id))
}

fn settings_snapshot(state: &State<'_, SettingsState>) -> Result<Settings, String> {
    Ok(state
        .settings
        .lock()
        .map_err(|e| format!("Lock error: {e}"))?
        .clone())
}

#[tauri::command]
#[specta::specta]
pub fn ai_consent_check(
    state: State<'_, SettingsState>,
) -> Result<Option<AiConsentTarget>, String> {
    let settings = settings_snapshot(&state)?;
    let _guard = lock()?;
    Ok(pending_consent(
        &settings,
        &read_granted(&consent_path(&state.path)),
    ))
}

#[tauri::command]
#[specta::specta]
pub fn ai_consent_grant(id: String, state: State<'_, SettingsState>) -> Result<(), String> {
    let settings = settings_snapshot(&state)?;
    grant_current(&state.path, &settings, &id)
}

#[tauri::command]
#[specta::specta]
pub fn ai_consent_revoke(id: String, state: State<'_, SettingsState>) -> Result<(), String> {
    revoke(&state.path, &id)
}

/// Consent ids currently granted, for Settings → Privacy.
#[tauri::command]
#[specta::specta]
pub fn ai_consent_list(state: State<'_, SettingsState>) -> Result<Vec<String>, String> {
    let _guard = lock()?;
    Ok(read_granted(&consent_path(&state.path))
        .into_iter()
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings_path(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().join("settings.json")
    }

    fn settings(provider: LlmProvider) -> Settings {
        let mut s: Settings = serde_json::from_str("{}").unwrap();
        s.llm_provider = provider;
        s
    }

    const ALL: [LlmProvider; 5] = [
        LlmProvider::Gemini,
        LlmProvider::Openrouter,
        LlmProvider::Openai,
        LlmProvider::Anthropic,
        LlmProvider::Ollama,
    ];

    #[test]
    fn every_provider_requires_consent_and_names_its_host() {
        for p in ALL {
            let t = required_consent(p, "", "llama3.2", "anthropic/claude-sonnet");
            assert!(!t.id.is_empty() && !t.provider_name.is_empty(), "{t:?}");
            assert!(!t.host.is_empty(), "{t:?}");
        }
    }

    #[test]
    fn ollama_targets_follow_where_the_request_really_goes() {
        // Local daemon: still asks (it can forward), id names the host.
        for (base, host) in [
            ("http://localhost:11434", "localhost"),
            ("http://127.0.0.1:11434", "127.0.0.1"),
            ("http://[::1]:11434", "[::1]"),
            ("", "localhost"), // blank base → OllamaChat's default
        ] {
            let t = required_consent(LlmProvider::Ollama, base, "llama3.2", "");
            assert_eq!(t.id, format!("ollama:{host}"), "{base:?}");
            assert!(!t.detail.is_empty(), "local Ollama must explain forwarding");
        }
        // Cloud models go to ollama.com whatever the base URL is.
        for model in ["gpt-oss:120b-cloud", "qwen3-coder:480b-CLOUD", "kimi:cloud"] {
            let t = required_consent(LlmProvider::Ollama, "http://localhost:11434", model, "");
            assert_eq!(t.id, "ollama:ollama.com", "{model}");
            assert_eq!(t.host, "ollama.com");
        }
        let lan = required_consent(
            LlmProvider::Ollama,
            "http://192.168.1.20:11434",
            "llama3.2",
            "",
        );
        assert_eq!(lan.id, "ollama:192.168.1.20");
        // Userinfo can't disguise a remote host as localhost.
        let sneaky = required_consent(
            LlmProvider::Ollama,
            "http://localhost:1@evil.example",
            "m",
            "",
        );
        assert_eq!(sneaky.id, "ollama:evil.example");
        assert!(required_consent(LlmProvider::Ollama, "not a url", "m", "")
            .id
            .starts_with("ollama:"));
    }

    #[test]
    fn openrouter_prompt_names_the_model_maker() {
        let t = required_consent(LlmProvider::Openrouter, "", "", "anthropic/claude-sonnet-4");
        assert!(t.detail.contains("anthropic"), "{t:?}");
    }

    #[test]
    fn grant_only_accepts_the_current_target_and_revoke_withdraws_it() {
        let dir = tempfile::tempdir().unwrap();
        let sp = settings_path(&dir);
        let gemini = settings(LlmProvider::Gemini);
        for p in ALL {
            assert!(check_granted(&sp, &required_consent(p, "", "m", "").id).is_err());
        }
        // Stale / foreign ids are refused and nothing is written.
        for bad in ["openrouter", "", "ollama:evil", "../settings"] {
            assert!(grant_current(&sp, &gemini, bad).is_err(), "{bad:?}");
        }
        assert!(!consent_path(&sp).exists());
        grant_current(&sp, &gemini, "gemini").unwrap();
        assert!(check_granted(&sp, "gemini").is_ok());
        assert!(
            check_granted(&sp, "openrouter").is_err(),
            "consent is per provider"
        );
        revoke(&sp, "gemini").unwrap();
        let err = check_granted(&sp, "gemini").unwrap_err();
        assert_eq!(err, format!("{CONSENT_REQUIRED_PREFIX}gemini"));
    }

    #[test]
    fn gate_fails_closed_on_corrupt_file() {
        let dir = tempfile::tempdir().unwrap();
        let sp = settings_path(&dir);
        std::fs::write(consent_path(&sp), "{not json").unwrap();
        assert!(check_granted(&sp, "gemini").is_err());
        grant_current(&sp, &settings(LlmProvider::Gemini), "gemini").unwrap();
        assert!(check_granted(&sp, "gemini").is_ok());
    }

    #[test]
    fn gated_provider_rechecks_consent_on_every_request() {
        struct Never;
        #[async_trait]
        impl LLMProvider for Never {
            async fn stream_chat(
                &self,
                _: &AppHandle,
                _: Vec<ChatMessage>,
                _: Option<String>,
                _: Arc<AtomicBool>,
            ) -> Result<String, String> {
                unreachable!("not called in this test")
            }
            fn model_name(&self) -> &str {
                "never"
            }
            fn is_local(&self) -> bool {
                false
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let sp = settings_path(&dir);
        let target = required_consent(LlmProvider::Gemini, "", "", "");
        assert!(
            gate(Box::new(Never), &sp, &target).is_err(),
            "no consent → no provider"
        );

        grant_current(&sp, &settings(LlmProvider::Gemini), "gemini").unwrap();
        let gated = ConsentGated {
            inner: Box::new(Never),
            settings_path: sp.clone(),
            consent_id: target.id.clone(),
        };
        assert!(gated.check().is_ok());
        // Withdrawn mid-run: the very next request is refused.
        revoke(&sp, "gemini").unwrap();
        assert!(gated
            .check()
            .unwrap_err()
            .starts_with(CONSENT_REQUIRED_PREFIX));
        assert_eq!(gated.model_name(), "never");
    }

    #[test]
    fn pending_consent_waits_for_a_configured_provider() {
        let mut s = settings(LlmProvider::Gemini);
        s.gemini_api_key = None;
        assert_eq!(pending_consent(&s, &BTreeSet::new()), None);
        s.gemini_api_key = Some("k".into());
        assert_eq!(
            pending_consent(&s, &BTreeSet::new()).map(|t| t.id),
            Some("gemini".to_string())
        );
        let granted: BTreeSet<String> = ["gemini".to_string()].into();
        assert_eq!(pending_consent(&s, &granted), None);
    }
}
