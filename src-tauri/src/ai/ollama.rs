//! Ollama chat provider — talks to a locally-running (or LAN) Ollama
//! server via its OpenAI-compatible `/v1/chat/completions` endpoint.
//!
//! Ollama hosts open-weight models (Llama, Qwen, Mistral, Gemma, …)
//! on-device, with no API key required for the default localhost setup.
//! We choose its OpenAI-compat endpoint over the native `/api/chat` so
//! the wire shape matches the OpenRouter provider line-for-line — tool
//! calling, streaming SSE, message roles, finish reasons are identical.
//!
//! Differences from `openrouter.rs`:
//! - `base_url` is a constructor arg, not a const (user can point at a
//!   remote Ollama, e.g. `http://10.0.0.5:11434`)
//! - `api_key` is `Option<String>` — `None` for the common localhost
//!   case; `Some(...)` when the user fronts Ollama with an auth proxy
//! - No `HTTP-Referer` / `X-Title` attribution headers (Ollama ignores
//!   them; sending wouldn't break anything but they're noise)
//! - No `sk-or-v1-` prefix check
//! - `is_local()` returns `true`
//!
//! Tool-use: Ollama's OpenAI-compat endpoint supports `tools` for models
//! that have a tool-use chat template (llama3.1+, qwen2.5+, etc.). Older
//! / smaller models will simply ignore the field. Same fallback behavior
//! as OpenRouter when routing to a non-tools model.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tauri_specta::Event;

use crate::events;

use super::provider::{ChatMessage, ChatRole, LLMProvider};
use super::streaming::consumer::SseEvents;
use super::streaming::openai_compat_chat::spawn_openai_compat_chat;
use super::tool_protocol::{StreamEvent, ToolDef};
// Test-only — same rationale as openrouter.rs.
#[cfg(test)]
use super::tool_protocol::ToolCall as ProtoToolCall;
use super::tool_translator::openai_compat::{
    build_openai_messages, build_openai_tools, ApiError, ChatToolsRequest,
};

/// Default base URL for a local Ollama install. Used as both the
/// settings default and the fallback when the user-supplied base is
/// empty or whitespace.
pub const OLLAMA_DEFAULT_BASE: &str = "http://localhost:11434";

/// Ollama is a desktop-only provider: it speaks cleartext `http://` (localhost
/// or a LAN host — see [`OLLAMA_DEFAULT_BASE`]), which on mobile would bypass
/// the WebView CSP and trip iOS App Transport Security. Single source of truth
/// for the mobile gate so the chat dispatch and the model-list probe can't
/// drift apart. (Audit 2026-06-08, LOW-9.)
pub(crate) const fn ollama_supported() -> bool {
    !cfg!(any(target_os = "android", target_os = "ios"))
}

pub struct OllamaChat {
    client: Arc<Client>,
    base_url: String,
    api_key: Option<String>,
    model: String,
}

impl OllamaChat {
    pub fn new(
        client: Arc<Client>,
        base_url: String,
        api_key: Option<String>,
        model: String,
    ) -> Self {
        let base = if base_url.trim().is_empty() {
            OLLAMA_DEFAULT_BASE.to_string()
        } else {
            // Strip trailing slashes so `{base}/v1/...` always renders cleanly
            // whether the user typed `http://x:11434` or `http://x:11434/`.
            base_url.trim_end_matches('/').to_string()
        };
        Self {
            client,
            base_url: base,
            api_key,
            model,
        }
    }
}

#[async_trait]
impl<R: tauri::Runtime + 'static> LLMProvider<R> for OllamaChat {
    async fn stream_chat(
        &self,
        app: &tauri::AppHandle<R>,
        messages: Vec<ChatMessage>,
        system_context: Option<String>,
        cancel: Arc<AtomicBool>,
    ) -> Result<String, String> {
        stream_chat_impl(
            &self.client,
            app,
            &self.base_url,
            self.api_key.as_deref(),
            &self.model,
            messages,
            system_context,
            cancel,
        )
        .await
    }

    async fn stream_chat_with_tools(
        &self,
        app: &tauri::AppHandle<R>,
        messages: Vec<ChatMessage>,
        system_context: Option<String>,
        tools: Vec<ToolDef>,
        cancel: Arc<AtomicBool>,
    ) -> Result<tokio::sync::mpsc::Receiver<StreamEvent>, String> {
        stream_chat_with_tools_impl(
            self.client.clone(),
            app.clone(),
            self.base_url.clone(),
            self.api_key.clone(),
            self.model.clone(),
            messages,
            system_context,
            tools,
            cancel,
        )
        .await
    }

    fn model_name(&self) -> &str {
        &self.model
    }

    fn is_local(&self) -> bool {
        // Ollama runs on the user's machine (or LAN). No data leaves the
        // user's network — matches the "local" semantic the embedding
        // routing code uses for ONNX.
        true
    }
}

// ── Request / response types (OpenAI-compatible) ────────────────────

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<OpenAiMessage>,
    stream: bool,
    temperature: f32,
    /// Halt generation at any of these literal strings. Matches the
    /// OpenRouter path: `</tool_call>` keeps the model from hallucinating
    /// fake `tool_result` blocks past its own tool-call boundary. The
    /// agent loop re-appends the closing tag before the regex parses.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    stop: Vec<String>,
}

#[derive(Serialize)]
struct OpenAiMessage {
    role: &'static str,
    content: String,
}

#[derive(Deserialize, Debug)]
struct StreamChunk {
    #[serde(default)]
    choices: Vec<StreamChoice>,
    #[serde(default)]
    error: Option<ApiError>,
}

#[derive(Deserialize, Debug)]
struct StreamChoice {
    #[serde(default)]
    delta: Option<Delta>,
}

#[derive(Deserialize, Debug)]
struct Delta {
    #[serde(default)]
    content: Option<String>,
}

// AUDIT-057: `ApiError` moved to `crate::ai::tool_translator::openai_compat`
// and imported above.

/// Attach the bearer header only when the user has supplied an API key.
/// Localhost Ollama doesn't need auth, so omitting the header is the
/// default. When set we treat it as a generic bearer token — works
/// with auth-proxy setups (e.g. Caddy / nginx in front of Ollama).
fn maybe_auth(req: reqwest::RequestBuilder, api_key: Option<&str>) -> reqwest::RequestBuilder {
    match api_key {
        Some(key) if !key.trim().is_empty() => req.bearer_auth(key.trim()),
        _ => req,
    }
}

#[allow(clippy::too_many_arguments)]
async fn stream_chat_impl<R: tauri::Runtime>(
    client: &Client,
    app: &tauri::AppHandle<R>,
    base_url: &str,
    api_key: Option<&str>,
    model: &str,
    messages: Vec<ChatMessage>,
    system_context: Option<String>,
    cancel: Arc<AtomicBool>,
) -> Result<String, String> {
    let url = format!("{}/v1/chat/completions", base_url);

    tracing::info!(
        model = model,
        base_url = %base_url,
        has_api_key = api_key.is_some_and(|k| !k.trim().is_empty()),
        "ollama: dispatching chat"
    );

    // OpenAI-style messages: system prompt is a separate message with
    // role "system", not a top-level field like Gemini.
    let mut chat: Vec<OpenAiMessage> = Vec::with_capacity(messages.len() + 1);
    if let Some(ctx) = system_context {
        if !ctx.is_empty() {
            chat.push(OpenAiMessage {
                role: "system",
                content: ctx,
            });
        }
    }
    for m in messages {
        chat.push(OpenAiMessage {
            role: match m.role {
                ChatRole::User => "user",
                ChatRole::Model => "assistant",
                // Legacy XML path maps Tool messages onto "user" — same
                // as openrouter.rs. Native path (below) uses the proper
                // OpenAI `"tool"` role with `tool_call_id`.
                ChatRole::Tool => "user",
            },
            content: m.content,
        });
    }

    let body = ChatRequest {
        model,
        messages: chat,
        stream: true,
        temperature: 0.7,
        // No max_tokens — Ollama's OpenAI-compat layer treats omission
        // as "model default", which respects the per-model context length
        // configured at `ollama pull` time. Setting a low fixed cap would
        // chop off long responses for users who run larger models.
        stop: vec!["</tool_call>".to_string()],
    };

    let response = maybe_auth(client.post(&url), api_key)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("HTTP request failed: {e}"))?;

    if !response.status().is_success() {
        let status = response.status();
        let body_text = response.text().await.unwrap_or_default();
        tracing::error!(
            status = %status,
            model = model,
            base_url = %base_url,
            body = %body_text,
            "ollama: request rejected"
        );
        return Err(format!("Ollama API error ({}): {}", status, body_text));
    }

    // AUDIT-057: shared cancel-aware SSE consumer replaces the prior
    // inline loop. Per-event parsing stays here.
    let mut events = SseEvents::new(response.bytes_stream(), cancel.clone());
    let mut full_text = String::new();

    while let Some(event) = events.next_event().await? {
        for line in event.lines() {
            let Some(data) = line.strip_prefix("data: ") else {
                continue;
            };
            let data = data.trim();
            if data.is_empty() || data == "[DONE]" {
                continue;
            }

            let resp: StreamChunk = match serde_json::from_str(data) {
                Ok(r) => r,
                Err(_) => continue,
            };

            if let Some(err) = resp.error {
                events::AiError(err.message.clone()).emit(app).ok();
                return Err(err.message);
            }

            let Some(text) = resp
                .choices
                .first()
                .and_then(|c| c.delta.as_ref())
                .and_then(|d| d.content.as_deref())
            else {
                continue;
            };
            if text.is_empty() {
                continue;
            }
            full_text.push_str(text);
            events::AiChunk(text.to_string()).emit(app).ok();
        }
    }

    events::AiDone(full_text.clone()).emit(app).ok();
    Ok(full_text)
}

// AUDIT-057: `find_sse_boundary` moved to `crate::ai::streaming::sse` and
// imported at the top of this file.

// ── Native function-calling path ────────────────────
//
// Mirrors openrouter.rs's `stream_chat_with_tools_impl` — same OpenAI
// tool-call shape on the wire, same per-index accumulator, same
// finish-reason logic. The big difference is that not every Ollama
// model has a tool-use chat template; for those models, Ollama returns
// a 400 with a clear message. We surface that error verbatim so the
// user can switch to a tools-capable model.

// AUDIT-057: all of `ChatToolsRequest`, `OpenAiToolDef`,
// `OpenAiFunctionDef`, `OpenAiToolsMessage`, `OpenAiOutgoingToolCall`,
// `OpenAiOutgoingFunction`, `ToolsStreamChunk`, `ToolsStreamChoice`,
// `ToolsDelta`, `ToolsDeltaCall`, `ToolsDeltaFunction`, `PartialCall`,
// `build_openai_tools`, and `build_openai_messages` moved to
// `crate::ai::tool_translator::openai_compat` (imported above). They were
// byte-identical to openrouter.rs except for the `max_tokens` field — the
// shared struct now models that with `max_tokens: Option<u32>` (Ollama
// passes `None`, the call site below).

#[allow(clippy::too_many_arguments)]
async fn stream_chat_with_tools_impl<R: tauri::Runtime + 'static>(
    client: Arc<Client>,
    app: tauri::AppHandle<R>,
    base_url: String,
    api_key: Option<String>,
    model: String,
    messages: Vec<ChatMessage>,
    system_context: Option<String>,
    tools: Vec<ToolDef>,
    cancel: Arc<AtomicBool>,
) -> Result<tokio::sync::mpsc::Receiver<StreamEvent>, String> {
    let url = format!("{}/v1/chat/completions", base_url);

    tracing::info!(
        model = %model,
        base_url = %base_url,
        has_api_key = api_key.as_deref().is_some_and(|k| !k.trim().is_empty()),
        tools = tools.len(),
        "ollama: dispatching chat_with_tools"
    );

    let chat = build_openai_messages(system_context, &messages);
    let body = ChatToolsRequest {
        model: &model,
        messages: chat,
        stream: true,
        temperature: Some(0.7),
        // Ollama respects per-model defaults from `ollama pull` — passing
        // a cap here would override the model's intended context budget.
        max_tokens: None,
        max_completion_tokens: None,
        tools: build_openai_tools(&tools),
    };

    let response = maybe_auth(client.post(&url), api_key.as_deref())
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("HTTP request failed: {e}"))?;

    if !response.status().is_success() {
        let status = response.status();
        let body_text = response.text().await.unwrap_or_default();
        tracing::error!(
            status = %status,
            model = %model,
            base_url = %base_url,
            body = %body_text,
            "ollama: request rejected"
        );
        return Err(format!("Ollama API error ({}): {}", status, body_text));
    }

    // AUDIT-057: shared spawn + drain — see streaming::openai_compat_chat.
    Ok(spawn_openai_compat_chat(response, app, cancel))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn new_strips_trailing_slash_from_base_url() {
        let client = Arc::new(reqwest::Client::new());
        let chat = OllamaChat::new(
            client,
            "http://localhost:11434/".to_string(),
            None,
            "llama3.2".to_string(),
        );
        assert_eq!(chat.base_url, "http://localhost:11434");
    }

    #[test]
    fn ollama_is_gated_off_on_mobile() {
        // The gate is compile-time per target; assert both senses so removing
        // the cfg (or flipping it) fails here. The iOS value itself is locked
        // by `cargo check --target aarch64-apple-ios`.
        #[cfg(any(target_os = "android", target_os = "ios"))]
        assert!(!ollama_supported(), "Ollama must be gated off on mobile");
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        assert!(ollama_supported(), "Ollama must be available on desktop");
    }

    #[test]
    fn new_empty_base_url_falls_back_to_default() {
        // An empty / whitespace base_url is treated as "use default" so
        // a fresh settings.json with the field defaulted to "" still
        // does the sensible thing.
        let client = Arc::new(reqwest::Client::new());
        let chat = OllamaChat::new(client, "   ".to_string(), None, "llama3.2".to_string());
        assert_eq!(chat.base_url, OLLAMA_DEFAULT_BASE);
    }

    #[test]
    fn request_serializes_with_openai_compatible_schema() {
        let req = ChatRequest {
            model: "llama3.2:3b",
            messages: vec![
                OpenAiMessage {
                    role: "system",
                    content: "You are a careful assistant.".into(),
                },
                OpenAiMessage {
                    role: "user",
                    content: "Hi".into(),
                },
            ],
            stream: true,
            temperature: 0.7,
            stop: vec!["</tool_call>".into()],
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["model"], "llama3.2:3b");
        assert_eq!(json["stream"], true);
        assert_eq!(json["messages"][0]["role"], "system");
        assert_eq!(json["messages"][1]["role"], "user");
        assert_eq!(json["stop"][0], "</tool_call>");
        // No max_tokens — Ollama respects per-model defaults.
        assert!(json.get("max_tokens").is_none());
    }

    #[test]
    fn stream_chunk_parses_ollama_openai_delta() {
        // Ollama's /v1/chat/completions wire shape matches OpenAI's
        // exactly. Sample chunk lifted from an `ollama serve` capture.
        let raw = r#"{
            "id": "chatcmpl-abc",
            "object": "chat.completion.chunk",
            "model": "llama3.2:3b",
            "choices": [{
                "index": 0,
                "delta": { "role": "assistant", "content": "Hello" }
            }]
        }"#;
        let parsed: StreamChunk = serde_json::from_str(raw).unwrap();
        assert_eq!(
            parsed
                .choices
                .first()
                .and_then(|c| c.delta.as_ref())
                .and_then(|d| d.content.as_deref()),
            Some("Hello")
        );
    }

    #[test]
    fn stream_chunk_handles_empty_choices() {
        let raw = r#"{"id":"chatcmpl-end","object":"chat.completion.chunk"}"#;
        let parsed: StreamChunk = serde_json::from_str(raw).unwrap();
        assert!(parsed.choices.is_empty());
    }

    #[test]
    fn stream_chunk_surfaces_api_error() {
        // Ollama returns a clean error object when the requested model
        // isn't pulled, e.g. `{"error":{"message":"model 'foo' not found"}}`.
        let raw = r#"{"error":{"message":"model 'foo' not found"}}"#;
        let parsed: StreamChunk = serde_json::from_str(raw).unwrap();
        assert_eq!(parsed.error.unwrap().message, "model 'foo' not found");
    }

    // AUDIT-057: `sse_boundary_detection` test removed — coverage moved
    // to `crate::ai::streaming::sse::tests` (the function itself moved
    // there too).

    #[test]
    fn build_openai_tools_wraps_in_function_envelope() {
        let defs = vec![ToolDef {
            name: "roost_read".into(),
            description: "Read notes".into(),
            input_schema: json!({"type":"object","required":["paths"]}),
        }];
        let out = build_openai_tools(&defs);
        let json = serde_json::to_value(&out).unwrap();
        assert_eq!(json[0]["type"], "function");
        assert_eq!(json[0]["function"]["name"], "roost_read");
    }

    #[test]
    fn model_with_tool_calls_stringifies_arguments() {
        // OpenAI contract: `function.arguments` is a JSON STRING, not
        // a nested object. Ollama's OpenAI-compat shim honors the same
        // contract, so this test pins the same behavior we have for
        // OpenRouter.
        let call = ProtoToolCall {
            id: "call_1".into(),
            name: "roost_search".into(),
            args: json!({"query":"fitness"}),
            provider_metadata: None,
        };
        let msgs = build_openai_messages(
            None,
            &[ChatMessage::model_with_tools("Looking up.", vec![call])],
        );
        let json = serde_json::to_value(&msgs[0]).unwrap();
        assert_eq!(json["role"], "assistant");
        assert!(json["tool_calls"][0]["function"]["arguments"].is_string());
        let args_str = json["tool_calls"][0]["function"]["arguments"]
            .as_str()
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(args_str).unwrap();
        assert_eq!(parsed, json!({"query":"fitness"}));
    }

    #[test]
    fn tool_response_uses_tool_role_with_tool_call_id() {
        let msg = ChatMessage::tool_response("call_1", "roost_search", r#"{"hits":[]}"#);
        let msgs = build_openai_messages(None, &[msg]);
        let json = serde_json::to_value(&msgs[0]).unwrap();
        assert_eq!(json["role"], "tool");
        assert_eq!(json["tool_call_id"], "call_1");
    }

    #[test]
    fn maybe_auth_skips_header_when_key_absent() {
        // Smoke test: a None / empty key should not add an Authorization
        // header, so localhost Ollama (no auth) works out of the box.
        let client = reqwest::Client::new();
        let req_no_key = maybe_auth(
            client.post("http://localhost:11434/v1/chat/completions"),
            None,
        );
        let built = req_no_key.build().unwrap();
        assert!(built.headers().get("authorization").is_none());

        let req_blank_key = maybe_auth(
            client.post("http://localhost:11434/v1/chat/completions"),
            Some("   "),
        );
        let built_blank = req_blank_key.build().unwrap();
        assert!(built_blank.headers().get("authorization").is_none());
    }

    #[test]
    fn maybe_auth_attaches_bearer_when_key_present() {
        let client = reqwest::Client::new();
        let req = maybe_auth(
            client.post("http://localhost:11434/v1/chat/completions"),
            Some("secret-token"),
        );
        let built = req.build().unwrap();
        let header = built.headers().get("authorization").expect("bearer header");
        assert_eq!(header.to_str().unwrap(), "Bearer secret-token");
    }
}
