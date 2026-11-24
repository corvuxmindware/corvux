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
            let parsed = url::Url::parse(ollama_effective_base(ollama_base)).ok();
            let host = parsed
                .as_ref()
                .and_then(|u| u.host_str().map(|h| h.to_ascii_lowercase()))
                .unwrap_or_else(|| "unknown-host".to_string());
            // The grant is for an ORIGIN, not just a host name (review AK-8): a
            // "yes" to `http://localhost:11434` must not carry over to
            // `https://localhost:8443` or to another service on the same host.
            // Path is deliberately not part of it (a reverse-proxy prefix is the
            // same server).
            let origin = parsed.as_ref().map(|u| {
                format!(
                    "{}://{host}:{}",
                    u.scheme(),
                    u.port_or_known_default().unwrap_or(0)
                )
            });
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
            let (id, shown) = match origin {
                Some(o) => (format!("ollama:{o}"), o),
                // Unparseable base: one shared "unknown" id, as before.
                None => (format!("ollama:{host}"), host.clone()),
            };
            AiConsentTarget {
                id,
                provider_name: format!("the Ollama server at {shown}"),
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

/// The host of a current-format Ollama id `ollama:<scheme>://<host>:<port>`;
/// `None` for every other id (the fixed provider ids, the legacy `ollama:<host>`
/// form, `ollama:ollama.com`, or anything malformed). The host is returned
/// exactly as it is in the id (bracketed for IPv6), so it can be compared for
/// EQUALITY with a legacy grant's host — never as a suffix or pattern.
fn origin_id_host(id: &str) -> Option<&str> {
    let rest = id.strip_prefix("ollama:")?;
    let (scheme, authority) = rest.split_once("://")?;
    let (host, port) = authority.rsplit_once(':')?;
    let well_formed = matches!(scheme, "http" | "https")
        && !host.is_empty()
        && !host.contains('/')
        && !port.is_empty()
        && port.bytes().all(|b| b.is_ascii_digit());
    well_formed.then_some(host)
}

/// Before review AK-8 an Ollama grant was keyed `ollama:<host>` with no scheme or
/// port, and it meant "chats to that host, on whatever scheme and port is
/// configured". Existing users keep exactly that meaning (migration safety, rule
/// 7): a legacy grant covers every scheme and port for EXACTLY that host. New
/// grants are written in the stricter `ollama:<scheme>://<host>:<port>` form and
/// cover only that origin.
fn legacy_host_id(host: &str) -> String {
    format!("ollama:{host}")
}

/// Whether `granted` covers `id`: the id itself, or the legacy grant for the same
/// host. The ONLY membership test — `is_granted` and `pending_consent` must agree
/// or the prompt and the gate would disagree. Host matching is exact string
/// equality: no wildcards, no suffix or subdomain matching.
fn covers(granted: &BTreeSet<String>, id: &str) -> bool {
    granted.contains(id)
        || origin_id_host(id).is_some_and(|host| granted.contains(&legacy_host_id(host)))
}

fn is_granted(settings_path: &Path, id: &str) -> Result<bool, String> {
    let _guard = lock()?;
    Ok(covers(&read_granted(&consent_path(settings_path)), id))
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
    let mut changed = set.remove(id);
    if let Some(host) = origin_id_host(id) {
        // Withdrawing a current-format id also withdraws the legacy grant for
        // that host that still counts for it (see `covers`), or it would stay
        // granted.
        changed |= set.remove(&legacy_host_id(host));
    } else if let Some(host) = id
        .strip_prefix("ollama:")
        .filter(|h| !h.is_empty() && !h.contains("://"))
    {
        // Withdrawing a legacy `ollama:<host>` grant also withdraws every
        // current-format grant for exactly that host (whatever scheme/port).
        let before = set.len();
        set.retain(|granted| origin_id_host(granted) != Some(host));
        changed |= set.len() != before;
    }
    if changed {
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
    Some(target_for(settings)).filter(|t| !covers(granted, &t.id))
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
            assert_eq!(t.id, format!("ollama:http://{host}:11434"), "{base:?}");
            assert_eq!(t.host, host, "{base:?}");
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
        assert_eq!(lan.id, "ollama:http://192.168.1.20:11434");
        // Userinfo can't disguise a remote host as localhost.
        let sneaky = required_consent(
            LlmProvider::Ollama,
            "http://localhost:1@evil.example",
            "m",
            "",
        );
        assert_eq!(sneaky.id, "ollama:http://evil.example:80");
        assert_eq!(sneaky.host, "evil.example");
        assert!(required_consent(LlmProvider::Ollama, "not a url", "m", "")
            .id
            .starts_with("ollama:"));
    }

    #[test]
    fn the_ollama_consent_id_names_scheme_and_port_but_not_the_path() {
        // AK-8: a "yes" is for an origin. Two services on one host, or the same
        // host over a different scheme, are different recipients.
        let id = |base: &str| required_consent(LlmProvider::Ollama, base, "m", "").id;
        let plain = id("http://box.example:11434");
        assert_eq!(plain, "ollama:http://box.example:11434");
        assert_ne!(plain, id("http://box.example:8080"));
        assert_ne!(plain, id("https://box.example:11434"));
        assert_ne!(id("https://box.example"), id("http://box.example"));
        assert_eq!(id("https://box.example"), "ollama:https://box.example:443");
        // Same server: path prefix, trailing slash and host case do not matter.
        assert_eq!(plain, id("http://BOX.example:11434/"));
        assert_eq!(plain, id("http://box.example:11434/ollama"));
        // The cloud id is unchanged.
        assert_eq!(
            required_consent(
                LlmProvider::Ollama,
                "http://box.example:11434",
                "x:cloud",
                ""
            )
            .id,
            "ollama:ollama.com"
        );
        // The prompt shows where the chat goes, scheme and port included.
        let t = required_consent(LlmProvider::Ollama, "http://box.example:11434", "m", "");
        assert!(
            t.provider_name.contains("http://box.example:11434"),
            "{t:?}"
        );
        assert_eq!(t.host, "box.example");
    }

    fn grants(ids: &[&str]) -> BTreeSet<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn an_old_format_grant_covers_that_host_on_any_scheme_and_port() {
        // Migration safety (rule 7): before AK-8 `ollama:<host>` meant "chats to
        // that host, whatever scheme/port is configured". It still does.
        let dir = tempfile::tempdir().unwrap();
        let sp = settings_path(&dir);
        write_granted(
            &consent_path(&sp),
            &grants(&["ollama:localhost", "ollama:box.example"]),
        )
        .unwrap();
        for covered in [
            "ollama:http://localhost:11434",
            "ollama:http://localhost:8080",
            "ollama:https://localhost:8443",
            "ollama:http://box.example:11434",
            "ollama:https://box.example:443",
            "ollama:https://box.example:8443",
            "ollama:http://box.example:80",
        ] {
            assert!(check_granted(&sp, covered).is_ok(), "{covered}");
        }
        // Exact host only: another host, a look-alike suffix/prefix host, a
        // sub-domain, a malformed id and the cloud id are all NOT covered.
        for not_covered in [
            "ollama:http://other.example:11434",
            "ollama:https://other.example:8443",
            "ollama:http://notbox.example:11434",
            "ollama:http://evilbox.example:11434",
            "ollama:https://box.example.evil.net:443",
            "ollama:https://a.box.example:443",
            "ollama:http://localhost.evil.example:11434",
            "ollama:http://:11434",
            "ollama:ftp://box.example:21",
            "ollama:http://box.example",
            "ollama:ollama.com",
        ] {
            assert!(check_granted(&sp, not_covered).is_err(), "{not_covered}");
        }
        // The prompt agrees with the gate: no re-ask for a covered target.
        let mut s = settings(LlmProvider::Ollama);
        s.ollama_model = "llama3.2".into();
        let granted = read_granted(&consent_path(&sp));
        for base in [
            "http://localhost:11434",
            "https://localhost:8443",
            "https://box.example:8443",
        ] {
            s.ollama_base_url = base.into();
            assert_eq!(pending_consent(&s, &granted), None, "{base}");
        }
        // (Kept off the `for` line: the raw-fs ratchet's line counter reads `//`
        // inside a string as a comment and would lose this loop's opening brace.)
        let uncovered = ["http://other.example:11434", "https://box.example.evil.net"];
        for base in uncovered {
            s.ollama_base_url = base.into();
            assert!(pending_consent(&s, &granted).is_some(), "{base}");
        }
    }

    #[test]
    fn a_new_style_grant_covers_only_its_own_origin() {
        // New grants are strict: scheme + host + port. Nothing widens them and
        // nothing writes a legacy host-wide grant on their behalf.
        let dir = tempfile::tempdir().unwrap();
        let sp = settings_path(&dir);
        write_granted(
            &consent_path(&sp),
            &grants(&["ollama:http://box.example:11434"]),
        )
        .unwrap();
        assert!(check_granted(&sp, "ollama:http://box.example:11434").is_ok());
        for not_covered in [
            "ollama:https://box.example:11434",
            "ollama:http://box.example:8080",
            "ollama:https://box.example:443",
            "ollama:http://other.example:11434",
            "ollama:http://notbox.example:11434",
            "ollama:http://a.box.example:11434",
            "ollama:ollama.com",
        ] {
            assert!(check_granted(&sp, not_covered).is_err(), "{not_covered}");
        }
        // Granting through the normal path stores the strict id only.
        let dir2 = tempfile::tempdir().unwrap();
        let sp2 = settings_path(&dir2);
        let mut s = settings(LlmProvider::Ollama);
        s.ollama_model = "llama3.2".into();
        s.ollama_base_url = "https://box.example:8443".into();
        let id = target_for(&s).id;
        grant_current(&sp2, &s, &id).unwrap();
        assert_eq!(
            read_granted(&consent_path(&sp2)),
            grants(&["ollama:https://box.example:8443"])
        );
        assert!(check_granted(&sp2, "ollama:https://box.example:8443").is_ok());
        assert!(check_granted(&sp2, "ollama:http://box.example:11434").is_err());
    }

    #[test]
    fn origin_id_host_reads_only_current_format_ids() {
        assert_eq!(
            origin_id_host("ollama:http://box.example:11434"),
            Some("box.example")
        );
        assert_eq!(origin_id_host("ollama:https://[::1]:8443"), Some("[::1]"));
        assert_eq!(
            origin_id_host("ollama:http://192.168.1.2:11434"),
            Some("192.168.1.2")
        );
        for not_an_origin in [
            "ollama:localhost",
            "ollama:ollama.com",
            "gemini",
            "ollama:http://box.example",
            "ollama:http://box.example:",
            "ollama:http://box.example:80a",
            "ollama:http://:80",
            "ollama:ftp://box.example:21",
            "ollama:http://a://b:80",
            "http://box.example:80",
            "",
        ] {
            assert_eq!(origin_id_host(not_an_origin), None, "{not_an_origin}");
        }
    }

    #[test]
    fn revoking_either_form_withdraws_the_other_for_that_host() {
        let dir = tempfile::tempdir().unwrap();
        let sp = settings_path(&dir);
        // New id revoked → the legacy host grant behind it goes too.
        write_granted(&consent_path(&sp), &grants(&["ollama:localhost"])).unwrap();
        assert!(check_granted(&sp, "ollama:http://localhost:11434").is_ok());
        revoke(&sp, "ollama:http://localhost:11434").unwrap();
        assert!(check_granted(&sp, "ollama:http://localhost:11434").is_err());
        assert!(check_granted(&sp, "ollama:https://localhost:8443").is_err());
        assert!(read_granted(&consent_path(&sp)).is_empty());

        // Legacy id revoked → every new-style grant for exactly that host goes
        // too (any scheme/port), and other hosts are left alone.
        write_granted(
            &consent_path(&sp),
            &grants(&[
                "ollama:box.example",
                "ollama:http://box.example:11434",
                "ollama:https://box.example:8443",
                "ollama:http://notbox.example:11434",
                "ollama:other.example",
                "gemini",
            ]),
        )
        .unwrap();
        revoke(&sp, "ollama:box.example").unwrap();
        assert_eq!(
            read_granted(&consent_path(&sp)),
            grants(&[
                "ollama:http://notbox.example:11434",
                "ollama:other.example",
                "gemini",
            ])
        );
        assert!(check_granted(&sp, "ollama:https://box.example:8443").is_err());
        assert!(check_granted(&sp, "ollama:https://other.example:443").is_ok());

        // Revoking a new-style id leaves OTHER origins' new-style grants alone.
        write_granted(
            &consent_path(&sp),
            &grants(&["ollama:http://a.example:1", "ollama:http://a.example:2"]),
        )
        .unwrap();
        revoke(&sp, "ollama:http://a.example:1").unwrap();
        assert_eq!(
            read_granted(&consent_path(&sp)),
            grants(&["ollama:http://a.example:2"])
        );
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
