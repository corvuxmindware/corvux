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
//! Privacy: the IPC takes the key the user just typed. When that field is
//! empty ("test the key I already saved") the backend substitutes the saved
//! key itself — the webview never holds a saved key (sharp-edges F11). A saved
//! key is only ever sent to the provider's own fixed host, or, for Ollama, to
//! the saved server address. No persistence, no logging —
//! `tracing::debug!` only logs the latency + status, never the key or
//! response body.
//!
//! Every probe here uses the no-redirect client (`ai::http::no_redirect_client`,
//! review AK-1/AK-6): reqwest strips `Authorization` on a cross-host redirect
//! but NOT the `x-goog-api-key` / `x-api-key` headers, and for Ollama the
//! address itself is renderer-supplied. Every probe has a total timeout, and
//! every body — success or error — is read with a size cap.

use std::time::{Duration, Instant};

use serde::de::IgnoredAny;
use serde::{Deserialize, Serialize};
use tauri::State;

use super::ollama_models::{
    authorize_ollama_probe, resolve_ollama_probe, saved_settings, OLLAMA_MOBILE_UNSUPPORTED,
    OLLAMA_PROBE_TIMEOUT, PROBE_APPROVALS,
};
use super::settings::effective_probe_key;
use super::types::SettingsState;
use crate::ai::http::{
    bounded_json, error_body, no_redirect_client, redirect_refused, MAX_MODEL_LIST_BYTES,
    MAX_SMALL_JSON_BYTES,
};
use crate::commands::native_confirm::OsDialog;

/// Total budget for a cloud-provider key probe (connect to last body byte).
/// Generous enough for OpenRouter's full catalogue on a slow mobile link.
const CLOUD_PROBE_TIMEOUT: Duration = Duration::from_secs(20);

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

/// The model-catalogue shapes the cloud probes count. Entries are skipped, not
/// materialized: only the length matters.
#[derive(Debug, Default, Deserialize)]
struct ModelCatalogue {
    /// Gemini: `{"models": [...]}`.
    #[serde(default)]
    models: Vec<IgnoredAny>,
    /// OpenRouter / OpenAI / Anthropic: `{"data": [...]}`.
    #[serde(default)]
    data: Vec<IgnoredAny>,
}

#[derive(Debug, Clone, Copy)]
enum CatalogueField {
    Models,
    Data,
}

/// Send one fixed-host key probe and turn the answer into a result. Shared by
/// the four cloud probes so the timeout, redirect refusal and body caps cannot
/// drift between them.
async fn run_catalogue_probe(
    name: &'static str,
    req: reqwest::RequestBuilder,
    field: CatalogueField,
) -> ConnectionTestResult {
    let started = Instant::now();
    match req.timeout(CLOUD_PROBE_TIMEOUT).send().await {
        Ok(resp) => {
            let latency_ms = started.elapsed().as_millis() as u64;
            let status = resp.status();
            if let Some(msg) = redirect_refused(status) {
                tracing::warn!(latency_ms, status = %status, probe = name, "connection probe: redirect refused");
                return ConnectionTestResult::fail(
                    latency_ms,
                    "Redirected — not followed".into(),
                    msg,
                );
            }
            if status.is_success() {
                // An unreadable or oversized list still proves the key works.
                let catalogue: ModelCatalogue =
                    bounded_json(resp, MAX_MODEL_LIST_BYTES, CLOUD_PROBE_TIMEOUT)
                        .await
                        .unwrap_or_default();
                let count = match field {
                    CatalogueField::Models => catalogue.models.len(),
                    CatalogueField::Data => catalogue.data.len(),
                };
                tracing::debug!(
                    latency_ms,
                    models = count,
                    probe = name,
                    "connection probe: ok"
                );
                ConnectionTestResult::ok_with(
                    latency_ms,
                    format!(
                        "{count} model{} reachable",
                        if count == 1 { "" } else { "s" }
                    ),
                )
            } else {
                let body_excerpt = error_body(resp).await;
                tracing::warn!(latency_ms, status = %status, probe = name, "connection probe: rejected");
                ConnectionTestResult::fail(
                    latency_ms,
                    explain_status(status.as_u16()),
                    truncate(body_excerpt, 240),
                )
            }
        }
        Err(e) => {
            let latency_ms = started.elapsed().as_millis() as u64;
            tracing::warn!(latency_ms, error = %e, probe = name, "connection probe: network error");
            ConnectionTestResult::fail(latency_ms, "Network error".into(), e.to_string())
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
    settings_state: State<'_, SettingsState>,
) -> Result<ConnectionTestResult, String> {
    let saved = settings_state
        .settings
        .lock()
        .map(|g| g.gemini_api_key.clone())
        .unwrap_or(None);
    let key = effective_probe_key(&api_key, &saved);
    let key = key.as_str();
    if key.is_empty() {
        return Ok(ConnectionTestResult::fail(
            0,
            "No key provided".into(),
            "API key field is empty",
        ));
    }
    let url = "https://generativelanguage.googleapis.com/v1beta/models";
    let client = no_redirect_client()?;
    // AK-9: the key header is marked sensitive (kept out of Debug output).
    let req = crate::ai::http::secret_header(client.get(url), "x-goog-api-key", key)?;
    Ok(run_catalogue_probe("gemini", req, CatalogueField::Models).await)
}

/// Probe the OpenRouter key against `GET /api/v1/models` with
/// `Authorization: Bearer …`. Anonymous-shape endpoint, returns the
/// full model catalogue on success.
#[tauri::command]
#[specta::specta]
pub async fn test_openrouter_connection(
    api_key: String,
    settings_state: State<'_, SettingsState>,
) -> Result<ConnectionTestResult, String> {
    let saved = settings_state
        .settings
        .lock()
        .map(|g| g.openrouter_api_key.clone())
        .unwrap_or(None);
    let key = effective_probe_key(&api_key, &saved);
    let key = key.as_str();
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

    let client = no_redirect_client()?;
    let req = client
        .get(format!("{}/models", crate::ai::openrouter::OPENROUTER_BASE))
        .bearer_auth(key);
    Ok(run_catalogue_probe("openrouter", req, CatalogueField::Data).await)
}

/// Ollama's `/api/version` answer. Only the version string is read.
#[derive(Debug, Deserialize)]
struct OllamaVersion {
    #[serde(default)]
    version: Option<String>,
}

/// GET `{base}/api/version` with the no-redirect client, a total timeout and
/// bounded reads. `base` has already been validated and, where needed,
/// confirmed by the user.
async fn probe_ollama_version(
    client: &reqwest::Client,
    base: &str,
    key: Option<&str>,
) -> ConnectionTestResult {
    let url = format!("{}/api/version", base);
    let started = Instant::now();
    let mut req = client.get(&url).timeout(OLLAMA_PROBE_TIMEOUT);
    if let Some(key) = key {
        req = req.bearer_auth(key);
    }

    match req.send().await {
        Ok(resp) => {
            let latency_ms = started.elapsed().as_millis() as u64;
            let status = resp.status();
            if let Some(msg) = redirect_refused(status) {
                tracing::warn!(latency_ms, status = %status, "test_ollama: redirect refused");
                return ConnectionTestResult::fail(
                    latency_ms,
                    "Redirected — not followed".into(),
                    msg,
                );
            }
            if status.is_success() {
                let version =
                    bounded_json::<OllamaVersion>(resp, MAX_SMALL_JSON_BYTES, OLLAMA_PROBE_TIMEOUT)
                        .await
                        .ok()
                        .and_then(|v| v.version)
                        .unwrap_or_else(|| "unknown".to_string());
                let version = truncate(version, 64);
                tracing::debug!(latency_ms, "test_ollama: ok");
                ConnectionTestResult::ok_with(latency_ms, format!("Ollama {} reachable", version))
            } else {
                let body_excerpt = error_body(resp).await;
                tracing::warn!(latency_ms, status = %status, "test_ollama: rejected");
                ConnectionTestResult::fail(
                    latency_ms,
                    explain_status(status.as_u16()),
                    truncate(body_excerpt, 240),
                )
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
            ConnectionTestResult::fail(latency_ms, hint, e.to_string())
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
///
/// Same policy as `list_ollama_models` (review AK-1): desktop only; the address
/// is validated like `save_settings`; an internet host that is not the saved
/// server needs the user's native confirmation; redirects are not followed.
#[tauri::command]
#[specta::specta]
pub async fn test_ollama_connection(
    app: tauri::AppHandle,
    base_url: String,
    api_key: Option<String>,
    settings_state: State<'_, SettingsState>,
) -> Result<ConnectionTestResult, String> {
    // Desktop-only, like `list_ollama_models` and chat (Audit 2026-06-08,
    // LOW-9): a phone must not dial a plaintext LAN address from Rust.
    if !crate::ai::ollama::ollama_supported() {
        return Err(OLLAMA_MOBILE_UNSUPPORTED.into());
    }
    // Same address policy as save_settings; the saved token only ever goes to
    // the saved server (see `resolve_ollama_probe`).
    let probe = match resolve_ollama_probe(
        &base_url,
        api_key.as_deref(),
        &saved_settings(&settings_state),
    ) {
        Ok(v) => v,
        Err(e) => {
            return Ok(ConnectionTestResult::fail(
                0,
                "Server address can't be used".into(),
                e,
            ))
        }
    };
    authorize_ollama_probe(&probe, &OsDialog(app), &PROBE_APPROVALS).await?;
    let client = no_redirect_client()?;
    Ok(probe_ollama_version(&client, &probe.url, probe.key.as_deref()).await)
}

/// Probe an OpenAI key against `GET /v1/models` with `Authorization:
/// Bearer …`. Returns the model catalogue count on success — same auth
/// surface the chat provider uses, so a green probe means chat will
/// authenticate too.
#[tauri::command]
#[specta::specta]
pub async fn test_openai_connection(
    api_key: String,
    settings_state: State<'_, SettingsState>,
) -> Result<ConnectionTestResult, String> {
    let saved = settings_state
        .settings
        .lock()
        .map(|g| g.openai_api_key.clone())
        .unwrap_or(None);
    let key = effective_probe_key(&api_key, &saved);
    let key = key.as_str();
    if key.is_empty() {
        return Ok(ConnectionTestResult::fail(
            0,
            "No key provided".into(),
            "API key field is empty",
        ));
    }
    let client = no_redirect_client()?;
    let req = client
        .get(format!("{}/models", crate::ai::openai::OPENAI_BASE))
        .bearer_auth(key);
    Ok(run_catalogue_probe("openai", req, CatalogueField::Data).await)
}

/// Probe an Anthropic (Claude) key against `GET /v1/models` with the
/// native `x-api-key` + `anthropic-version` headers — the same auth the
/// Messages API chat provider uses.
#[tauri::command]
#[specta::specta]
pub async fn test_anthropic_connection(
    api_key: String,
    settings_state: State<'_, SettingsState>,
) -> Result<ConnectionTestResult, String> {
    let saved = settings_state
        .settings
        .lock()
        .map(|g| g.anthropic_api_key.clone())
        .unwrap_or(None);
    let key = effective_probe_key(&api_key, &saved);
    let key = key.as_str();
    if key.is_empty() {
        return Ok(ConnectionTestResult::fail(
            0,
            "No key provided".into(),
            "API key field is empty",
        ));
    }
    let client = no_redirect_client()?;
    // AK-9: the key header is marked sensitive (kept out of Debug output).
    let req = crate::ai::http::secret_header(
        client.get(format!("{}/models", crate::ai::anthropic::ANTHROPIC_BASE)),
        "x-api-key",
        key,
    )?
    .header(
        "anthropic-version",
        crate::ai::tool_translator::anthropic::ANTHROPIC_VERSION,
    );
    Ok(run_catalogue_probe("anthropic", req, CatalogueField::Data).await)
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
    crate::ai::tools::truncate_utf8_with(&mut s, max, "…");
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::http::test_server::{assert_never_dialed, redirect_trap, serve_once, wire};

    #[tokio::test]
    async fn the_ollama_probe_reports_a_redirect_and_never_follows_it() {
        let (first, target, hits) = redirect_trap().await;
        let client = no_redirect_client().unwrap();
        let r = probe_ollama_version(&client, &format!("http://{first}"), None).await;
        assert!(!r.ok);
        let err = r.error_message.unwrap_or_default();
        assert!(err.contains("redirect") && err.contains("302"), "{err}");
        assert!(!err.contains(&target.to_string()), "{err}");
        assert_never_dialed(&hits).await;
    }

    #[tokio::test]
    async fn a_key_probe_never_carries_its_header_through_a_redirect() {
        // reqwest strips `Authorization` on a cross-host hop but not
        // `x-api-key` / `x-goog-api-key` — with no redirect there is no hop.
        let (first, _target, hits) = redirect_trap().await;
        let client = no_redirect_client().unwrap();
        let req = client
            .get(format!("http://{first}/v1/models"))
            .header("x-api-key", "sk-ant-secret");
        let r = run_catalogue_probe("test", req, CatalogueField::Data).await;
        assert!(!r.ok);
        assert_eq!(r.summary, "Redirected — not followed");
        assert_never_dialed(&hits).await;
    }

    #[tokio::test]
    async fn probe_bodies_are_bounded() {
        let client = no_redirect_client().unwrap();

        // Error body: capped, then cut to one line for the panel.
        let addr = serve_once(wire("401 Unauthorized", &[b'x'; 100_000], true)).await;
        let r = run_catalogue_probe(
            "test",
            client.get(format!("http://{addr}/models")),
            CatalogueField::Data,
        )
        .await;
        assert!(!r.ok);
        assert!(r.error_message.unwrap_or_default().len() <= 240 + 3);

        // Success body over the cap: refused (counted as 0), never buffered.
        let addr = serve_once(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
                MAX_MODEL_LIST_BYTES + 1
            )
            .into_bytes(),
        )
        .await;
        let r = run_catalogue_probe(
            "test",
            client.get(format!("http://{addr}/models")),
            CatalogueField::Data,
        )
        .await;
        assert!(r.ok);
        assert_eq!(r.summary, "0 models reachable");

        // Normal catalogue still counts.
        let addr = serve_once(wire(
            "200 OK",
            br#"{"data":[{"id":"a"},{"id":"b"}]}"#,
            false,
        ))
        .await;
        let r = run_catalogue_probe(
            "test",
            client.get(format!("http://{addr}/models")),
            CatalogueField::Data,
        )
        .await;
        assert_eq!(r.summary, "2 models reachable");

        // Ollama version: an oversized /api/version reads as "unknown", and a
        // long version string is cut.
        let addr = serve_once(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
                MAX_SMALL_JSON_BYTES + 1
            )
            .into_bytes(),
        )
        .await;
        let r = probe_ollama_version(&client, &format!("http://{addr}"), None).await;
        assert_eq!(r.summary, "Ollama unknown reachable");
        let long = format!(r#"{{"version":"{}"}}"#, "9".repeat(5_000));
        let addr = serve_once(wire("200 OK", long.as_bytes(), false)).await;
        let r = probe_ollama_version(&client, &format!("http://{addr}"), None).await;
        assert!(r.ok && r.summary.len() < 100, "{}", r.summary);
    }

    #[test]
    fn truncates_provider_errors_at_utf8_boundaries() {
        assert_eq!(
            super::truncate("\u{754c}".repeat(100), 239),
            format!("{}\u{2026}", "\u{754c}".repeat(79))
        );
        assert_eq!(super::truncate("\u{1f600}".into(), 0), "\u{2026}");
    }
}
