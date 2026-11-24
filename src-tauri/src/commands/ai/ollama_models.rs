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
//!
//! Both Ollama probes (`list_ollama_models`, `test_ollama_connection`) dial an
//! address the webview supplies, so they share one policy (review AK-1,
//! 2026-11-12): the address is validated like `save_settings` does
//! ([`resolve_ollama_probe`]); a host on the open internet that is NOT the saved
//! server needs the user's answer in a native OS dialog
//! ([`authorize_ollama_probe`]); the request uses the no-redirect client
//! (`ai::http::no_redirect_client`) with a 3 s total timeout; every body read is
//! size-capped.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tauri::State;

use super::provider_url::{validate_provider_base_url, HostClass};
use super::types::{Settings, SettingsState};
use crate::commands::native_confirm::{dialog_safe, NativeConfirm, OsDialog};

/// Total budget for one Ollama probe, connect to last body byte. Keeps the UI
/// snappy when Ollama is down (an unbounded request would hang ~30 s or more).
pub(super) const OLLAMA_PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// Returned when the user presses Cancel on the probe confirmation. Contains
/// "confirmation was cancelled", one of the markers the frontend's
/// `isSecurityCancel` recognizes, so a caller can show it as a choice rather
/// than a failure. (Today `OllamaProvider.tsx` shows it inline as the
/// test/model-list error text.)
pub(crate) const PROBE_DECLINED: &str =
    "The test request confirmation was cancelled — nothing was sent.";

/// How long a "yes" to one probe address is remembered, so the Test button's
/// follow-up model-list refresh (same address, a moment later) does not raise a
/// second dialog — which the native-confirm cool-down would answer "no".
pub(super) const PROBE_APPROVAL_TTL: Duration = Duration::from_secs(10 * 60);

/// The one probe address the user most recently allowed, and when. Kept in
/// Rust: the webview can only reuse an answer the user really gave, for the
/// exact same normalized address (a different path or host asks again).
pub(super) struct ProbeApprovals(Mutex<Option<(String, Instant)>>);

impl ProbeApprovals {
    pub(super) const fn new() -> Self {
        Self(Mutex::new(None))
    }

    /// A poisoned lock reads as "not approved": ask again (fail closed).
    fn is_approved(&self, url: &str, now: Instant) -> bool {
        self.0
            .lock()
            .map(|g| {
                g.as_ref().is_some_and(|(u, at)| {
                    u == url && now.saturating_duration_since(*at) < PROBE_APPROVAL_TTL
                })
            })
            .unwrap_or(false)
    }

    fn record(&self, url: &str, now: Instant) {
        if let Ok(mut g) = self.0.lock() {
            *g = Some((url.to_string(), now));
        }
    }
}

/// Process-wide memory used by both probe commands.
pub(super) static PROBE_APPROVALS: ProbeApprovals = ProbeApprovals::new();

/// Everything a probe needs, decided before any network I/O. `Debug` is
/// hand-written so the token can never reach a log line.
pub(super) struct OllamaProbe {
    /// Validated, normalized base URL (no trailing slash).
    pub url: String,
    /// Lowercased host, for the dialog.
    pub host: String,
    /// `http://` — the dialog says the request is unencrypted.
    pub plaintext: bool,
    /// Bearer token to attach, if any (see [`resolve_ollama_probe`]).
    pub key: Option<String>,
    /// A public host that is not the saved server: ask the user first.
    pub needs_confirm: bool,
}

impl std::fmt::Debug for OllamaProbe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OllamaProbe")
            .field("url", &self.url)
            .field("host", &self.host)
            .field("plaintext", &self.plaintext)
            .field("key", &self.key.as_ref().map(|_| "<redacted>"))
            .field("needs_confirm", &self.needs_confirm)
            .finish()
    }
}

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
/// * A host on the open internet that is not the saved server needs the user's
///   native confirmation (review AK-1): otherwise a script in the webview could
///   use the probe as a silent fetch of any internet URL, with note text in the
///   path. The saved server was confirmed when it was saved; loopback and
///   private-network hosts stay prompt-free, matching `save_settings` (the
///   by-name classing is the documented AK-8 residual).
pub(super) fn resolve_ollama_probe(
    base_url: &str,
    typed_key: Option<&str>,
    saved: &Settings,
) -> Result<OllamaProbe, String> {
    let target = validate_provider_base_url(base_url)
        .map_err(|e| format!("The Ollama server address {e}"))?;
    let is_saved_server = validate_provider_base_url(&saved.ollama_base_url)
        .ok()
        .is_some_and(|s| s.url == target.url);
    let typed = typed_key.map(str::trim).filter(|k| !k.is_empty());
    let key = match typed {
        Some(k) => Some(k.to_string()),
        None if is_saved_server => saved
            .ollama_api_key
            .as_deref()
            .map(|k| k.trim().to_string())
            .filter(|k| !k.is_empty()),
        None => None,
    };
    Ok(OllamaProbe {
        needs_confirm: target.class == HostClass::Public && !is_saved_server,
        plaintext: target.plaintext,
        host: target.host,
        url: target.url,
        key,
    })
}

/// Dialog text for a probe that needs confirming. Host and address are
/// renderer-influenced, so both go through `dialog_safe`.
fn probe_confirm_body(probe: &OllamaProbe) -> String {
    let mut body = format!(
        "Send a test request to {}?\n\n\
         Full address: {}\n\n\
         This is not your saved Ollama server — it is a host on the open internet. \
         Continue only if you just typed this address in Settings \u{2192} AI yourself. \
         If you did not, press Cancel.",
        dialog_safe(&probe.host, 80),
        dialog_safe(&probe.url, 160),
    );
    if probe.plaintext {
        body.push_str("\n\nThe address uses plain http://, so the request is UNENCRYPTED.");
    }
    if probe.key.is_some() {
        body.push_str("\n\nThe access token you typed will be sent with it.");
    }
    body
}

/// Ask the user before a probe that needs it. `Ok(())` = go ahead; a declined
/// (or suppressed) dialog is [`PROBE_DECLINED`] and nothing is sent.
pub(super) async fn authorize_ollama_probe<C: NativeConfirm + ?Sized>(
    probe: &OllamaProbe,
    confirm: &C,
    approvals: &ProbeApprovals,
) -> Result<(), String> {
    if !probe.needs_confirm || approvals.is_approved(&probe.url, Instant::now()) {
        return Ok(());
    }
    let allowed = confirm
        .confirm(
            "Confirm test request",
            &probe_confirm_body(probe),
            "Send test request",
        )
        .await;
    tracing::info!(
        allowed,
        "ollama probe to a non-saved internet host: user answered"
    );
    if !allowed {
        return Err(PROBE_DECLINED.to_string());
    }
    approvals.record(&probe.url, Instant::now());
    Ok(())
}

/// GET `{base}/api/tags` with the no-redirect client and bounded reads.
async fn fetch_ollama_tags(
    client: &reqwest::Client,
    base: &str,
    key: Option<&str>,
) -> Result<Vec<OllamaModelInfo>, String> {
    let url = format!("{}/api/tags", base);
    let mut req = client.get(&url).timeout(OLLAMA_PROBE_TIMEOUT);
    if let Some(key) = key {
        req = req.bearer_auth(key);
    }

    let resp = req
        .send()
        .await
        .map_err(|e| format!("Ollama not reachable at {base}: {e}"))?;

    let status = resp.status();
    if let Some(msg) = crate::ai::http::redirect_refused(status) {
        return Err(format!("Ollama at {base}: {msg}"));
    }
    if !status.is_success() {
        let mut body = crate::ai::http::error_body(resp).await;
        crate::ai::tools::truncate_utf8_with(&mut body, 240, "\u{2026}");
        return Err(format!("Ollama returned {status}: {body}"));
    }

    let parsed: TagsResponse = crate::ai::http::bounded_json(
        resp,
        crate::ai::http::MAX_MODEL_LIST_BYTES,
        OLLAMA_PROBE_TIMEOUT,
    )
    .await
    .map_err(|e| format!("Failed to read the /api/tags response: {e}"))?;

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

/// Snapshot of the saved settings for a probe. A poisoned lock falls back to
/// defaults (no saved key is attached — fail closed).
pub(super) fn saved_settings(state: &SettingsState) -> Settings {
    state.settings.lock().map(|g| g.clone()).unwrap_or_default()
}

/// Message both Ollama probes return on mobile.
pub(super) const OLLAMA_MOBILE_UNSUPPORTED: &str =
    "Ollama is a desktop-only provider and isn't available on mobile.";

/// List the models pulled into the user's Ollama install. Returns
/// `Err(..)` on network / parse failure so the frontend can fall back
/// to a freeform text input. The base URL falls back to the default
/// localhost if the supplied value is empty.
#[tauri::command]
#[specta::specta]
pub async fn list_ollama_models(
    app: tauri::AppHandle,
    base_url: String,
    api_key: Option<String>,
    settings_state: State<'_, SettingsState>,
) -> Result<Vec<OllamaModelInfo>, String> {
    // Ollama is desktop-only — its base URL is usually a plaintext http://
    // LAN/localhost host, which on mobile would bypass the WebView CSP and
    // `usesCleartextTraffic="false"`. Don't probe it from a phone.
    // (Audit 2026-06-08, LOW-9.)
    if !crate::ai::ollama::ollama_supported() {
        return Err(OLLAMA_MOBILE_UNSUPPORTED.into());
    }

    let probe = resolve_ollama_probe(
        &base_url,
        api_key.as_deref(),
        &saved_settings(&settings_state),
    )?;
    authorize_ollama_probe(&probe, &OsDialog(app), &PROBE_APPROVALS).await?;
    let client = crate::ai::http::no_redirect_client()?;
    fetch_ollama_tags(&client, &probe.url, probe.key.as_deref()).await
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
        let p = resolve_ollama_probe("https://Ollama.Mine.Example/", None, &s).unwrap();
        assert_eq!(p.url, "https://ollama.mine.example");
        assert_eq!(p.key.as_deref(), Some("SAVED-TOKEN"));
        // A different server never receives it — the exfil shape from the audit.
        let p = resolve_ollama_probe("https://attacker.example", None, &s).unwrap();
        assert_eq!(p.key, None);
    }

    #[test]
    fn a_typed_token_wins_and_is_trimmed() {
        let s = saved("http://localhost:11434", Some("SAVED"));
        let p = resolve_ollama_probe("", Some("  typed "), &s).unwrap();
        assert_eq!(p.key.as_deref(), Some("typed"));
    }

    #[test]
    fn empty_address_means_the_default_server_and_the_saved_token_applies_there() {
        let s = saved("", Some("SAVED"));
        let p = resolve_ollama_probe("   ", None, &s).unwrap();
        assert_eq!(p.url, "http://localhost:11434");
        assert_eq!(p.key.as_deref(), Some("SAVED"));
    }

    #[test]
    fn no_saved_token_means_no_bearer() {
        let s = saved("http://localhost:11434", None);
        assert_eq!(resolve_ollama_probe("", None, &s).unwrap().key, None);
        let blank = saved("http://localhost:11434", Some("  "));
        assert_eq!(resolve_ollama_probe("", None, &blank).unwrap().key, None);
    }

    // ── AK-1: which probes need the user's native confirmation ──────────

    #[test]
    fn only_a_public_host_that_is_not_the_saved_server_needs_confirmation() {
        let s = saved("https://ollama.mine.example", None);
        let needs = |url: &str| resolve_ollama_probe(url, None, &s).unwrap().needs_confirm;

        // Public and not saved → ask (both schemes, and a path carrying text).
        assert!(needs("https://attacker.example"));
        assert!(needs("http://attacker.example:11434"));
        assert!(needs("https://attacker.example/secret-note-text"));
        // A different path on the saved host is a different address → ask.
        assert!(needs("https://ollama.mine.example/other"));
        assert!(needs("https://x.tail1234.ts.net"));

        // The saved server (any cosmetic spelling) → silent.
        assert!(!needs("https://ollama.mine.example"));
        assert!(!needs("https://Ollama.Mine.Example/"));
        // Loopback and private network → silent (AK-8 residual, unchanged).
        for url in [
            "",
            "http://localhost:11434",
            "http://127.0.0.1:11434",
            "http://192.168.1.9:11434",
            "http://10.0.0.5:11434",
            "http://my-pc:11434",
            "http://box.lan:11434",
        ] {
            assert!(!needs(url), "{url}");
        }
    }

    #[test]
    fn with_no_usable_saved_server_every_public_probe_asks() {
        // A grandfathered saved value that no longer validates matches nothing.
        let s = saved("ftp://old", None);
        assert!(
            resolve_ollama_probe("https://h.example", None, &s)
                .unwrap()
                .needs_confirm
        );
    }

    fn probe(url: &str, key: Option<&str>) -> OllamaProbe {
        resolve_ollama_probe(url, key, &saved("http://localhost:11434", None)).unwrap()
    }

    #[test]
    fn the_dialog_names_the_host_and_warns_about_plain_http_and_the_token() {
        let body = probe_confirm_body(&probe("http://attacker.example/x", Some("SECRET-9")));
        assert!(
            body.contains("Send a test request to attacker.example?"),
            "{body}"
        );
        assert!(body.contains("http://attacker.example/x"), "{body}");
        assert!(body.contains("UNENCRYPTED"), "{body}");
        assert!(body.contains("access token"), "{body}");
        assert!(!body.contains("SECRET-9"), "the token value is never shown");
        let https = probe_confirm_body(&probe("https://attacker.example", None));
        assert!(
            !https.contains("UNENCRYPTED") && !https.contains("token"),
            "{https}"
        );
    }

    #[tokio::test]
    async fn a_declined_probe_returns_a_clear_error_and_is_not_remembered() {
        use crate::commands::native_confirm::test_support::Scripted;
        let approvals = ProbeApprovals::new();
        let p = probe("https://declined.example", None);
        let deny = Scripted::deny();
        let e = authorize_ollama_probe(&p, &deny, &approvals)
            .await
            .unwrap_err();
        assert_eq!(e, PROBE_DECLINED);
        assert!(
            e.contains("confirmation was cancelled"),
            "matches isSecurityCancel"
        );
        assert_eq!(deny.times_asked(), 1);
        // Asked again next time — a "no" is not cached as anything.
        let _ = authorize_ollama_probe(&p, &deny, &approvals).await;
        assert_eq!(deny.times_asked(), 2);
    }

    #[tokio::test]
    async fn probes_that_need_no_confirmation_never_show_a_dialog() {
        use crate::commands::native_confirm::test_support::Scripted;
        let approvals = ProbeApprovals::new();
        let deny = Scripted::deny();
        for url in ["", "http://192.168.1.9:11434", "http://localhost:11434"] {
            authorize_ollama_probe(&probe(url, None), &deny, &approvals)
                .await
                .unwrap();
        }
        assert_eq!(deny.times_asked(), 0);
    }

    #[tokio::test]
    async fn an_allowed_address_is_remembered_for_that_exact_address_only() {
        use crate::commands::native_confirm::test_support::Scripted;
        let approvals = ProbeApprovals::new();
        let allow = Scripted::allow();
        let p = probe("https://mine.example", None);
        authorize_ollama_probe(&p, &allow, &approvals)
            .await
            .unwrap();
        // Test → model-list refresh on the same address: no second dialog.
        authorize_ollama_probe(&p, &allow, &approvals)
            .await
            .unwrap();
        assert_eq!(allow.times_asked(), 1);
        // A different path (e.g. note text smuggled in) asks again.
        let other = probe("https://mine.example/other", None);
        authorize_ollama_probe(&other, &allow, &approvals)
            .await
            .unwrap();
        assert_eq!(allow.times_asked(), 2);
    }

    #[test]
    fn a_remembered_approval_expires() {
        let approvals = ProbeApprovals::new();
        let t0 = Instant::now();
        approvals.record("https://mine.example", t0);
        assert!(approvals.is_approved("https://mine.example", t0));
        assert!(!approvals.is_approved("https://other.example", t0));
        assert!(!approvals.is_approved("https://mine.example", t0 + PROBE_APPROVAL_TTL));
    }

    // ── AK-1 / AK-6: redirects and body caps on the wire ────────────────

    #[tokio::test]
    async fn the_tags_probe_reports_a_redirect_and_never_follows_it() {
        use crate::ai::http::test_server::{assert_never_dialed, redirect_trap};
        let (first, target, hits) = redirect_trap().await;
        let client = crate::ai::http::no_redirect_client().unwrap();
        let e = fetch_ollama_tags(&client, &format!("http://{first}"), None)
            .await
            .unwrap_err();
        assert!(e.contains("redirect") && e.contains("302"), "{e}");
        assert!(!e.contains(&target.to_string()), "{e}");
        assert_never_dialed(&hits).await;
    }

    #[tokio::test]
    async fn the_tags_probe_bounds_error_and_success_bodies() {
        use crate::ai::http::test_server::{serve_once, wire};
        let client = crate::ai::http::no_redirect_client().unwrap();

        // A large error body is capped and then shortened for the UI.
        let addr = serve_once(wire("500 Oops", &[b'e'; 10_000], false)).await;
        let e = fetch_ollama_tags(&client, &format!("http://{addr}"), None)
            .await
            .unwrap_err();
        assert!(e.starts_with("Ollama returned 500"), "{e}");
        assert!(e.len() < 400, "{} bytes", e.len());

        // A success body over the model-list cap is refused, not parsed.
        let addr = serve_once(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
                crate::ai::http::MAX_MODEL_LIST_BYTES + 1
            )
            .into_bytes(),
        )
        .await;
        let e = fetch_ollama_tags(&client, &format!("http://{addr}"), None)
            .await
            .unwrap_err();
        assert!(e.contains("larger than"), "{e}");

        // A normal list still parses and sorts.
        let body = br#"{"models":[{"name":"b","size":2},{"name":"a","size":1}]}"#;
        let addr = serve_once(wire("200 OK", body, true)).await;
        let models = fetch_ollama_tags(&client, &format!("http://{addr}"), None)
            .await
            .unwrap();
        assert_eq!(models[0].name, "a");
        assert_eq!(models[1].size_bytes, 2);
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
