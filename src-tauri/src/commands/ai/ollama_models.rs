//! IPC for listing models the user has pulled into their Ollama install.
//!
//! Backs the Settings → AI Ollama dropdown. Querying `/api/tags` is
//! the right move (instead of asking the user to type a model id):
//!  - Avoids typos like `llama3.2:3b` vs `llama3.2-3b`
//!  - Shows the user what they actually have available
//!  - Tells us the size so we can hint "this one's 4.7 GB"
//!
//! Network failures are reported as `Err(..)` so the frontend can
//! degrade to a freeform text input (matches OpenRouter UX).

use serde::{Deserialize, Serialize};
use tauri::State;

use super::provider_url::validate_provider_base_url;
use super::types::{HttpClient, Settings, SettingsState};

/// Wire shape returned to the frontend. Narrow on purpose — we only
/// surface what the picker needs to render. Extra fields from
/// `/api/tags` (digest, modified_at, details) stay on the floor.
#[derive(Debug, Clone, Serialize, specta::Type)]
pub struct OllamaModelInfo {
    pub name: String,
    #[specta(type = u32)]
    pub size_bytes: u64,
}

/// Ollama's `/api/tags` response shape. Only the fields we forward
/// are pulled out; the rest are dropped on the floor via serde's
/// default deny-extra behavior on missing fields (we let extras pass).
#[derive(Debug, Deserialize)]
struct TagsResponse {
    #[serde(default)]
    models: Vec<TagsModel>,
}

#[derive(Debug, Deserialize)]
struct TagsModel {
    name: String,
    #[serde(default)]
    size: u64,
}

/// Validate the server address a probe is about to dial and pick the bearer
/// token it may carry (sharp-edges F2/F11).
///
/// * The address goes through the same validator as `save_settings` — a probe
///   must not be a way around it.
/// * A token the user just typed is theirs to send anywhere they typed it.
/// * The SAVED token is attached only when the probe targets the saved server:
///   the webview no longer holds saved keys, so "test with the saved key"
///   cannot be allowed to mean "send the saved key to this URL I supplied".
pub(super) fn resolve_ollama_probe(
    base_url: &str,
    typed_key: Option<&str>,
    saved: &Settings,
) -> Result<(String, Option<String>), String> {
    let target = validate_provider_base_url(base_url)
        .map_err(|e| format!("The Ollama server address {e}"))?;
    let typed = typed_key.map(str::trim).filter(|k| !k.is_empty());
    let key = match typed {
        Some(k) => Some(k.to_string()),
        None => {
            let saved_target = validate_provider_base_url(&saved.ollama_base_url).ok();
            if saved_target.as_ref().map(|s| s.url.as_str()) == Some(target.url.as_str()) {
                saved
                    .ollama_api_key
                    .as_deref()
                    .map(|k| k.trim().to_string())
                    .filter(|k| !k.is_empty())
            } else {
                None
            }
        }
    };
    Ok((target.url, key))
}

/// Snapshot of the saved settings for a probe. A poisoned lock falls back to
/// defaults (no saved key is attached — fail closed).
pub(super) fn saved_settings(state: &SettingsState) -> Settings {
    state.settings.lock().map(|g| g.clone()).unwrap_or_default()
}

/// List the models pulled into the user's Ollama install. Returns
/// `Err(..)` on network / parse failure so the frontend can fall back
/// to a freeform text input. The base URL falls back to the default
/// localhost if the supplied value is empty.
#[tauri::command]
#[specta::specta]
pub async fn list_ollama_models(
    base_url: String,
    api_key: Option<String>,
    http_state: State<'_, HttpClient>,
    settings_state: State<'_, SettingsState>,
) -> Result<Vec<OllamaModelInfo>, String> {
    // Ollama is desktop-only — its base URL is usually a plaintext http://
    // LAN/localhost host, which on mobile would bypass the WebView CSP and
    // `usesCleartextTraffic="false"`. Don't probe it from a phone.
    // (Audit 2026-06-08, LOW-9.)
    if !crate::ai::ollama::ollama_supported() {
        return Err("Ollama is a desktop-only provider and isn't available on mobile.".into());
    }

    let (base, key) = resolve_ollama_probe(
        &base_url,
        api_key.as_deref(),
        &saved_settings(&settings_state),
    )?;
    let url = format!("{}/api/tags", base);

    let mut req = http_state
        .0
        .get(&url)
        .timeout(std::time::Duration::from_secs(3));
    if let Some(key) = key.as_deref() {
        req = req.bearer_auth(key);
    }

    let resp = req
        .send()
        .await
        .map_err(|e| format!("Ollama not reachable at {base}: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("Ollama returned {status}: {body}"));
    }

    let parsed: TagsResponse = resp
        .json()
        .await
        .map_err(|e| format!("Failed to parse /api/tags response: {e}"))?;

    let mut out: Vec<OllamaModelInfo> = parsed
        .models
        .into_iter()
        .map(|m| OllamaModelInfo {
            name: m.name,
            size_bytes: m.size,
        })
        .collect();
    // Sort alphabetically for stable picker order — Ollama returns
    // models in pull order otherwise, which is meaningless to the user.
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_response_parses_real_payload() {
        // Sample payload from `curl http://localhost:11434/api/tags`
        // (Ollama 0.5+) — pinned here so a wire-shape change is caught
        // by a test before users hit it.
        let raw = r#"{
            "models": [
                {
                    "name": "llama3.2:3b",
                    "modified_at": "2025-01-15T10:00:00Z",
                    "size": 2019393189,
                    "digest": "abc123",
                    "details": {"format": "gguf"}
                },
                {
                    "name": "qwen2.5:14b",
                    "modified_at": "2025-01-20T10:00:00Z",
                    "size": 9000000000,
                    "digest": "def456"
                }
            ]
        }"#;
        let parsed: TagsResponse = serde_json::from_str(raw).unwrap();
        assert_eq!(parsed.models.len(), 2);
        assert_eq!(parsed.models[0].name, "llama3.2:3b");
        assert_eq!(parsed.models[0].size, 2_019_393_189);
        assert_eq!(parsed.models[1].name, "qwen2.5:14b");
    }

    #[test]
    fn tags_response_handles_empty_models() {
        // Fresh Ollama install with no models pulled — frontend should
        // render an empty dropdown and prompt the user to `ollama pull`.
        let raw = r#"{"models": []}"#;
        let parsed: TagsResponse = serde_json::from_str(raw).unwrap();
        assert!(parsed.models.is_empty());
    }

    // ── resolve_ollama_probe (sharp-edges F2 / F11) ─────────────────────

    fn saved(base: &str, key: Option<&str>) -> Settings {
        Settings {
            ollama_base_url: base.to_string(),
            ollama_api_key: key.map(Into::into),
            ..Settings::default()
        }
    }

    #[test]
    fn probe_rejects_what_save_settings_would_reject() {
        let s = saved("http://localhost:11434", Some("tok"));
        let e = resolve_ollama_probe("http://localhost:11434@evil.example", None, &s).unwrap_err();
        assert!(e.contains("user:password@"), "{e}");
        assert!(resolve_ollama_probe("ftp://x", None, &s).is_err());
    }

    #[test]
    fn saved_token_is_attached_only_to_the_saved_server() {
        let s = saved("https://ollama.mine.example", Some("SAVED-TOKEN"));
        // Same server (trailing slash / case differences normalize away).
        let (base, key) = resolve_ollama_probe("https://Ollama.Mine.Example/", None, &s).unwrap();
        assert_eq!(base, "https://ollama.mine.example");
        assert_eq!(key.as_deref(), Some("SAVED-TOKEN"));
        // A different server never receives it — the exfil shape from the audit.
        let (_, key) = resolve_ollama_probe("https://attacker.example", None, &s).unwrap();
        assert_eq!(key, None);
    }

    #[test]
    fn a_typed_token_wins_and_is_trimmed() {
        let s = saved("http://localhost:11434", Some("SAVED"));
        let (_, key) = resolve_ollama_probe("", Some("  typed "), &s).unwrap();
        assert_eq!(key.as_deref(), Some("typed"));
    }

    #[test]
    fn empty_address_means_the_default_server_and_the_saved_token_applies_there() {
        let s = saved("", Some("SAVED"));
        let (base, key) = resolve_ollama_probe("   ", None, &s).unwrap();
        assert_eq!(base, "http://localhost:11434");
        assert_eq!(key.as_deref(), Some("SAVED"));
    }

    #[test]
    fn no_saved_token_means_no_bearer() {
        let s = saved("http://localhost:11434", None);
        assert_eq!(resolve_ollama_probe("", None, &s).unwrap().1, None);
        let blank = saved("http://localhost:11434", Some("  "));
        assert_eq!(resolve_ollama_probe("", None, &blank).unwrap().1, None);
    }

    #[test]
    fn tags_response_tolerates_missing_size() {
        // Defensive — older Ollama versions may omit the size field.
        // Serde default lands at 0 so the picker still shows the model.
        let raw = r#"{
            "models": [
                {"name": "phi3:mini", "modified_at": "2025-01-01T00:00:00Z"}
            ]
        }"#;
        let parsed: TagsResponse = serde_json::from_str(raw).unwrap();
        assert_eq!(parsed.models[0].name, "phi3:mini");
        assert_eq!(parsed.models[0].size, 0);
    }
}
