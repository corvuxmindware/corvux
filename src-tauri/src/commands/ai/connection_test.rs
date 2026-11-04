//! "Test connection" IPCs for the Settings → AI panel.
//!
//! Both probes hit each provider's `/models` endpoint with the user-
//! supplied key. That validates authentication WITHOUT spending any
//! generation tokens — the request is metered as a list call (free
//! tier on Gemini, no charge on OpenRouter even on a paid key).
//!
//! Why we don't reuse the existing `LLMProvider::stream` path: that
//! requires building an entire `Vec<ChatMessage>` + a system prompt and
//! consuming the first SSE chunk before we know whether auth worked.
//! It would also burn 1+ output tokens per click. The /models probe
//! returns a definitive 200 / 401 / 403 in one round-trip.
//!
//! Result shape is intentionally narrow — `ok` + `latency_ms` +
//! `error_message` + a small descriptive `summary`. Frontend renders
//! green/red inline, doesn't try to parse the summary.
//!
//! Privacy: the IPC takes the key directly from the frontend (it's
//! already in memory there from the input field). No persistence, no
//! logging — `tracing::debug!` only logs the latency + status, never
//! the key or response body.

use serde::Serialize;
use std::time::Instant;
use tauri::State;

use super::types::HttpClient;

#[derive(Debug, Clone, Serialize, specta::Type)]
pub struct ConnectionTestResult {
    /// True when the probe got a 200 from the provider's models endpoint.
    pub ok: bool,
    /// Round-trip wall-clock to the first byte. Useful for spotting
    /// "it's slow but working" vs. "it's failing fast" — different
    /// triage paths.
    #[specta(type = u32)]
    pub latency_ms: u64,
    /// Short human-readable label the panel shows next to the latency.
    /// On success: number of models the provider exposes for this key.
    /// On failure: a one-liner derived from the HTTP status / error.
    pub summary: String,
    /// Populated only when `ok` is false. Contains the raw error
    /// (status line, network error message) so the user can include
    /// it in a bug report — but kept narrow enough to render in a
    /// settings panel without scrolling.
    pub error_message: Option<String>,
}

impl ConnectionTestResult {
    fn ok_with(latency_ms: u64, summary: String) -> Self {
        Self {
            ok: true,
            latency_ms,
            summary,
            error_message: None,
        }
    }
    fn fail(latency_ms: u64, summary: String, error: impl Into<String>) -> Self {
        Self {
            ok: false,
            latency_ms,
            summary,
            error_message: Some(error.into()),
        }
    }
}

/// Probe the Gemini API key against `models.list`. Hits
/// `GET https://generativelanguage.googleapis.com/v1beta/models` with
/// the key in the `x-goog-api-key` header — same auth pattern the
/// chat + embed providers use, so a successful probe == the same
/// auth surface chat will use.
#[tauri::command]
#[specta::specta]
pub async fn test_gemini_connection(
    api_key: String,
    http_state: State<'_, HttpClient>,
) -> Result<ConnectionTestResult, String> {
    let key = api_key.trim();
    if key.is_empty() {
        return Ok(ConnectionTestResult::fail(
            0,
            "No key provided".into(),
            "API key field is empty",
        ));
    }
    let url = "https://generativelanguage.googleapis.com/v1beta/models";
    let started = Instant::now();
    match http_state
        .0
        .get(url)
        .header("x-goog-api-key", key)
        .send()
        .await
    {
        Ok(resp) => {
            let latency_ms = started.elapsed().as_millis() as u64;
            let status = resp.status();
            if status.is_success() {
                let body: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);
                let count = body
                    .get("models")
                    .and_then(|v| v.as_array())
                    .map(|a| a.len())
                    .unwrap_or(0);
                tracing::debug!(latency_ms, models = count, "test_gemini: ok");
                Ok(ConnectionTestResult::ok_with(
                    latency_ms,
                    format!(
                        "{count} model{} reachable",
                        if count == 1 { "" } else { "s" }
                    ),
                ))
            } else {
                let body_excerpt = resp.text().await.unwrap_or_default();
                tracing::warn!(latency_ms, status = %status, "test_gemini: rejected");
                Ok(ConnectionTestResult::fail(
                    latency_ms,
                    explain_status(status.as_u16()),
                    truncate(body_excerpt, 240),
                ))
            }
        }
        Err(e) => {
            let latency_ms = started.elapsed().as_millis() as u64;
            tracing::warn!(latency_ms, error = %e, "test_gemini: network error");
            Ok(ConnectionTestResult::fail(
                latency_ms,
                "Network error".into(),
                e.to_string(),
            ))
        }
    }
}

/// Probe the OpenRouter key against `GET /api/v1/models` with
/// `Authorization: Bearer …`. Anonymous-shape endpoint, returns the
/// full model catalogue on success.
#[tauri::command]
#[specta::specta]
pub async fn test_openrouter_connection(
    api_key: String,
    http_state: State<'_, HttpClient>,
) -> Result<ConnectionTestResult, String> {
    let key = api_key.trim();
    if key.is_empty() {
        return Ok(ConnectionTestResult::fail(
            0,
            "No key provided".into(),
            "API key field is empty",
        ));
    }
    // Cheap structural pre-check — catches the paste-truncation footgun
    // before we even hit the network. Mirrors the inline validator on
    // the OpenRouter input.
    if !key.starts_with("sk-or-v1-") {
        return Ok(ConnectionTestResult::fail(
            0,
            "Key prefix wrong".into(),
            "Expected key to start with `sk-or-v1-`. Re-paste the full key from openrouter.ai/keys.",
        ));
    }

    let started = Instant::now();
    let resp = http_state
        .0
        .get(format!("{}/models", crate::ai::openrouter::OPENROUTER_BASE))
        .bearer_auth(key)
        .send()
        .await;

    match resp {
        Ok(resp) => {
            let latency_ms = started.elapsed().as_millis() as u64;
            let status = resp.status();
            if status.is_success() {
                let body: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);
                let count = body
                    .get("data")
                    .and_then(|v| v.as_array())
                    .map(|a| a.len())
                    .unwrap_or(0);
                tracing::debug!(latency_ms, models = count, "test_openrouter: ok");
                Ok(ConnectionTestResult::ok_with(
                    latency_ms,
                    format!(
                        "{count} model{} reachable",
                        if count == 1 { "" } else { "s" }
                    ),
                ))
            } else {
                let body_excerpt = resp.text().await.unwrap_or_default();
                tracing::warn!(latency_ms, status = %status, "test_openrouter: rejected");
                Ok(ConnectionTestResult::fail(
                    latency_ms,
                    explain_status(status.as_u16()),
                    truncate(body_excerpt, 240),
                ))
            }
        }
        Err(e) => {
            let latency_ms = started.elapsed().as_millis() as u64;
            tracing::warn!(latency_ms, error = %e, "test_openrouter: network error");
            Ok(ConnectionTestResult::fail(
                latency_ms,
                "Network error".into(),
                e.to_string(),
            ))
        }
    }
}

/// Probe a user's Ollama server. Hits `GET {base}/api/version` —
/// the cheapest reachability check, doesn't require any model to be
/// loaded, and returns `{"version": "0.x.y"}` on success. A 3s timeout
/// keeps the UI snappy when Ollama is down (default reqwest timeout
/// would hang ~30s).
///
/// `api_key` is optional — supplied only when the user fronts Ollama
/// with an auth proxy. Localhost setups leave it `None`.
#[tauri::command]
#[specta::specta]
pub async fn test_ollama_connection(
    base_url: String,
    api_key: Option<String>,
    http_state: State<'_, HttpClient>,
) -> Result<ConnectionTestResult, String> {
    let base = base_url.trim().trim_end_matches('/');
    let base = if base.is_empty() {
        crate::ai::ollama::OLLAMA_DEFAULT_BASE
    } else {
        base
    };
    let url = format!("{}/api/version", base);

    let started = Instant::now();
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

    match req.send().await {
        Ok(resp) => {
            let latency_ms = started.elapsed().as_millis() as u64;
            let status = resp.status();
            if status.is_success() {
                let body: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);
                let version = body
                    .get("version")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                tracing::debug!(latency_ms, version, "test_ollama: ok");
                Ok(ConnectionTestResult::ok_with(
                    latency_ms,
                    format!("Ollama {} reachable", version),
                ))
            } else {
                let body_excerpt = resp.text().await.unwrap_or_default();
                tracing::warn!(latency_ms, status = %status, "test_ollama: rejected");
                Ok(ConnectionTestResult::fail(
                    latency_ms,
                    explain_status(status.as_u16()),
                    truncate(body_excerpt, 240),
                ))
            }
        }
        Err(e) => {
            let latency_ms = started.elapsed().as_millis() as u64;
            tracing::warn!(latency_ms, error = %e, "test_ollama: network error");
            // The likely error is "not running" — call that out explicitly
            // so the user knows to start `ollama serve` rather than chase
            // a generic network gremlin.
            let hint = if e.is_connect() || e.is_timeout() {
                format!("Couldn't reach Ollama at {base}. Make sure `ollama serve` is running.")
            } else {
                "Network error".to_string()
            };
            Ok(ConnectionTestResult::fail(latency_ms, hint, e.to_string()))
        }
    }
}

/// Probe an OpenAI key against `GET /v1/models` with `Authorization:
/// Bearer …`. Returns the model catalogue count on success — same auth
/// surface the chat provider uses, so a green probe means chat will
/// authenticate too.
#[tauri::command]
#[specta::specta]
pub async fn test_openai_connection(
    api_key: String,
    http_state: State<'_, HttpClient>,
) -> Result<ConnectionTestResult, String> {
    let key = api_key.trim();
    if key.is_empty() {
        return Ok(ConnectionTestResult::fail(
            0,
            "No key provided".into(),
            "API key field is empty",
        ));
    }
    let started = Instant::now();
    let resp = http_state
        .0
        .get(format!("{}/models", crate::ai::openai::OPENAI_BASE))
        .bearer_auth(key)
        .send()
        .await;

    match resp {
        Ok(resp) => {
            let latency_ms = started.elapsed().as_millis() as u64;
            let status = resp.status();
            if status.is_success() {
                let body: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);
                let count = body
                    .get("data")
                    .and_then(|v| v.as_array())
                    .map(|a| a.len())
                    .unwrap_or(0);
                tracing::debug!(latency_ms, models = count, "test_openai: ok");
                Ok(ConnectionTestResult::ok_with(
                    latency_ms,
                    format!(
                        "{count} model{} reachable",
                        if count == 1 { "" } else { "s" }
                    ),
                ))
            } else {
                let body_excerpt = resp.text().await.unwrap_or_default();
                tracing::warn!(latency_ms, status = %status, "test_openai: rejected");
                Ok(ConnectionTestResult::fail(
                    latency_ms,
                    explain_status(status.as_u16()),
                    truncate(body_excerpt, 240),
                ))
            }
        }
        Err(e) => {
            let latency_ms = started.elapsed().as_millis() as u64;
            tracing::warn!(latency_ms, error = %e, "test_openai: network error");
            Ok(ConnectionTestResult::fail(
                latency_ms,
                "Network error".into(),
                e.to_string(),
            ))
        }
    }
}

/// Probe an Anthropic (Claude) key against `GET /v1/models` with the
/// native `x-api-key` + `anthropic-version` headers — the same auth the
/// Messages API chat provider uses.
#[tauri::command]
#[specta::specta]
pub async fn test_anthropic_connection(
    api_key: String,
    http_state: State<'_, HttpClient>,
) -> Result<ConnectionTestResult, String> {
    let key = api_key.trim();
    if key.is_empty() {
        return Ok(ConnectionTestResult::fail(
            0,
            "No key provided".into(),
            "API key field is empty",
        ));
    }
    let started = Instant::now();
    let resp = http_state
        .0
        .get(format!("{}/models", crate::ai::anthropic::ANTHROPIC_BASE))
        .header("x-api-key", key)
        .header(
            "anthropic-version",
            crate::ai::tool_translator::anthropic::ANTHROPIC_VERSION,
        )
        .send()
        .await;

    match resp {
        Ok(resp) => {
            let latency_ms = started.elapsed().as_millis() as u64;
            let status = resp.status();
            if status.is_success() {
                let body: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);
                let count = body
                    .get("data")
                    .and_then(|v| v.as_array())
                    .map(|a| a.len())
                    .unwrap_or(0);
                tracing::debug!(latency_ms, models = count, "test_anthropic: ok");
                Ok(ConnectionTestResult::ok_with(
                    latency_ms,
                    format!(
                        "{count} model{} reachable",
                        if count == 1 { "" } else { "s" }
                    ),
                ))
            } else {
                let body_excerpt = resp.text().await.unwrap_or_default();
                tracing::warn!(latency_ms, status = %status, "test_anthropic: rejected");
                Ok(ConnectionTestResult::fail(
                    latency_ms,
                    explain_status(status.as_u16()),
                    truncate(body_excerpt, 240),
                ))
            }
        }
        Err(e) => {
            let latency_ms = started.elapsed().as_millis() as u64;
            tracing::warn!(latency_ms, error = %e, "test_anthropic: network error");
            Ok(ConnectionTestResult::fail(
                latency_ms,
                "Network error".into(),
                e.to_string(),
            ))
        }
    }
}

/// Translate the HTTP status into a one-line summary the user can act on.
/// Falls through to the raw status code for anything we haven't seen.
fn explain_status(code: u16) -> String {
    match code {
        400 => "Bad request — key format may be malformed".into(),
        401 => "Unauthorized — key is invalid or revoked".into(),
        403 => "Forbidden — key lacks permission for this endpoint".into(),
        404 => "Not found — endpoint path may have changed".into(),
        429 => "Rate-limited — too many requests on this key".into(),
        500..=599 => format!("Provider error (HTTP {code}) — try again shortly"),
        other => format!("Unexpected status (HTTP {other})"),
    }
}

fn truncate(mut s: String, max: usize) -> String {
    if s.len() <= max {
        return s;
    }
    s.truncate(max);
    s.push('…');
    s
}
