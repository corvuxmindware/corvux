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

use super::types::HttpClient;

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
) -> Result<Vec<OllamaModelInfo>, String> {
    // Ollama is desktop-only — its base URL is usually a plaintext http://
    // LAN/localhost host, which on mobile would bypass the WebView CSP and
    // `usesCleartextTraffic="false"`. Don't probe it from a phone.
    // (Audit 2026-06-08, LOW-9.)
    if !crate::ai::ollama::ollama_supported() {
        return Err("Ollama is a desktop-only provider and isn't available on mobile.".into());
    }

    let base = base_url.trim().trim_end_matches('/');
    let base = if base.is_empty() {
        crate::ai::ollama::OLLAMA_DEFAULT_BASE
    } else {
        base
    };
    let url = format!("{}/api/tags", base);

    let mut req = http_state
        .0
        .get(&url)
        .timeout(std::time::Duration::from_secs(3));
    if let Some(key) = api_key.as_deref() {
        let trimmed = key.trim();
        if !trimmed.is_empty() {
            req = req.bearer_auth(trimmed);
        }
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
