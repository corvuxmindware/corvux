//! OpenAI direct chat provider — OpenAI's `chat/completions` API
//! (https://api.openai.com/v1/chat/completions).
//!
//! This is byte-for-byte the same wire shape OpenRouter speaks (OpenRouter
//! is an OpenAI-compatible gateway), so this file reuses the shared
//! `tool_translator::openai_compat` builders and the shared
//! `streaming::openai_compat_chat` drain. The only per-provider differences
//! versus `openrouter.rs` are:
//!   - base URL (`api.openai.com` instead of `openrouter.ai`),
//!   - no `HTTP-Referer` / `X-Title` attribution headers (OpenRouter-only),
//!   - auth is the user's own OpenAI key.
//!
//! Why direct OpenAI *and* OpenRouter: OpenRouter reaches OpenAI models
//! too, but a direct key avoids the gateway markup and keeps one fewer
//! third party in the request path (privacy-first — Locked Decision #4).
//!
//! GPT-5.x compatibility: OpenAI's current models (gpt-5.x) reject the
//! legacy `max_tokens` and `temperature` parameters — they require
//! `max_completion_tokens` and don't take a sampling temperature. So this
//! provider sends `max_completion_tokens` and omits both `max_tokens` and
//! `temperature`. `max_completion_tokens` is accepted by the older
//! GPT-4.x/4o lineup too, so one shape works for every OpenAI model.

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
use super::tool_translator::openai_compat::{
    build_openai_messages, build_openai_tools, ApiError, ChatToolsRequest,
};

/// Default OpenAI base URL. Endpoint path (`/chat/completions`,
/// `/models`) is appended. Kept as a const (not a setting) — users who
/// need a custom base should use OpenRouter or another gateway; "OpenAI
/// direct" means api.openai.com.
pub const OPENAI_BASE: &str = "https://api.openai.com/v1";

/// Output-token cap sent as `max_completion_tokens`. On GPT-5.x reasoning
/// models this budget covers reasoning tokens too, so it's set generously
/// (8192) — too low and the model can spend the whole budget thinking and
/// return an empty visible answer. `agent_loop_v2`'s MAX_TOKENS recovery
/// still catches a hard cap.
const MAX_COMPLETION_TOKENS: u32 = 8192;

pub struct OpenAiChat {
    client: Arc<Client>,
    api_key: String,
    model: String,
}

impl OpenAiChat {
    pub fn new(client: Arc<Client>, api_key: String, model: String) -> Self {
        Self {
            client,
            api_key,
            model,
        }
    }
}

#[async_trait]
impl<R: tauri::Runtime + 'static> LLMProvider<R> for OpenAiChat {
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
            &self.api_key,
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
        false
    }
}

// ── Legacy (text-only) request/response types ───────────────────────
// Mirror openrouter.rs. Used only on the `use_native_tools = false`
// escape-hatch path.

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<OpenAiMessage>,
    stream: bool,
    // No `temperature` — GPT-5.x rejects it. `max_completion_tokens` (not
    // the legacy `max_tokens`) for the same reason.
    max_completion_tokens: u32,
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

/// Non-reversible key fingerprint for logs — first 8 hex of blake3(key).
/// Never logs any real bytes of the secret. Mirrors `openrouter::fingerprint`
/// (private there; duplicated rather than widening that module's surface).
fn fingerprint(key: &str) -> String {
    let trimmed = key.trim();
    if trimmed.is_empty() {
        return "<empty>".to_string();
    }
    let hash = blake3::hash(trimmed.as_bytes());
    format!("hash:{}", &hash.to_hex().as_str()[..8])
}

async fn stream_chat_impl<R: tauri::Runtime>(
    client: &Client,
    app: &tauri::AppHandle<R>,
    api_key: &str,
    model: &str,
    messages: Vec<ChatMessage>,
    system_context: Option<String>,
    cancel: Arc<AtomicBool>,
) -> Result<String, String> {
    let url = format!("{}/chat/completions", OPENAI_BASE);

    tracing::info!(
        model = model,
        key_fingerprint = %fingerprint(api_key),
        key_len = api_key.trim().len(),
        "openai: dispatching chat"
    );

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
                // Legacy path inlines tool results as user turns, same as
                // openrouter.rs. The native path uses the proper "tool" role.
                ChatRole::Tool => "user",
            },
            content: m.content,
        });
    }

    let body = ChatRequest {
        model,
        messages: chat,
        stream: true,
        max_completion_tokens: MAX_COMPLETION_TOKENS,
        stop: vec!["</tool_call>".to_string()],
    };

    let response = client
        .post(&url)
        .header("Authorization", format!("Bearer {api_key}"))
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
            key_fingerprint = %fingerprint(api_key),
            body = %body_text,
            "openai: request rejected"
        );
        return Err(format!("OpenAI API error ({}): {}", status, body_text));
    }

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

/// Native function-calling path. Builds the OpenAI-compat request and
/// hands the response to the shared drain (`spawn_openai_compat_chat`).
#[allow(clippy::too_many_arguments)]
async fn stream_chat_with_tools_impl<R: tauri::Runtime + 'static>(
    client: Arc<Client>,
    app: tauri::AppHandle<R>,
    api_key: String,
    model: String,
    messages: Vec<ChatMessage>,
    system_context: Option<String>,
    tools: Vec<ToolDef>,
    cancel: Arc<AtomicBool>,
) -> Result<tokio::sync::mpsc::Receiver<StreamEvent>, String> {
    let url = format!("{}/chat/completions", OPENAI_BASE);

    tracing::info!(
        model = %model,
        key_fingerprint = %fingerprint(&api_key),
        key_len = api_key.trim().len(),
        tools = tools.len(),
        "openai: dispatching chat_with_tools"
    );

    let chat = build_openai_messages(system_context, &messages);
    let body = ChatToolsRequest {
        model: &model,
        messages: chat,
        stream: true,
        // GPT-5.x rejects `temperature` and `max_tokens`; `max_completion_tokens`
        // is accepted by both the GPT-4.x and GPT-5.x lines, so use it
        // universally and omit the other two. See module docs.
        temperature: None,
        max_tokens: None,
        max_completion_tokens: Some(MAX_COMPLETION_TOKENS),
        tools: build_openai_tools(&tools),
    };

    let response = client
        .post(&url)
        .header("Authorization", format!("Bearer {}", api_key))
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
            key_fingerprint = %fingerprint(&api_key),
            body = %body_text,
            "openai: request rejected"
        );
        return Err(format!("OpenAI API error ({}): {}", status, body_text));
    }

    Ok(spawn_openai_compat_chat(response, app, cancel))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_request_serializes_with_openai_schema() {
        let req = ChatRequest {
            model: "gpt-5.5",
            messages: vec![
                OpenAiMessage {
                    role: "system",
                    content: "sys".into(),
                },
                OpenAiMessage {
                    role: "user",
                    content: "hi".into(),
                },
            ],
            stream: true,
            max_completion_tokens: 8192,
            stop: vec!["</tool_call>".into()],
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["model"], "gpt-5.5");
        assert_eq!(json["stream"], true);
        assert_eq!(json["messages"][0]["role"], "system");
        assert_eq!(json["max_completion_tokens"], 8192);
        // GPT-5.x rejects these — must be absent.
        assert!(json.get("temperature").is_none());
        assert!(json.get("max_tokens").is_none());
        assert_eq!(json["stop"][0], "</tool_call>");
    }

    #[test]
    fn stream_chunk_parses_content_delta() {
        let raw = r#"{"choices":[{"index":0,"delta":{"role":"assistant","content":"Hi"}}]}"#;
        let parsed: StreamChunk = serde_json::from_str(raw).unwrap();
        assert_eq!(
            parsed
                .choices
                .first()
                .and_then(|c| c.delta.as_ref())
                .and_then(|d| d.content.as_deref()),
            Some("Hi")
        );
    }

    #[test]
    fn stream_chunk_surfaces_error_envelope() {
        let raw = r#"{"error":{"message":"invalid_api_key"}}"#;
        let parsed: StreamChunk = serde_json::from_str(raw).unwrap();
        assert_eq!(parsed.error.unwrap().message, "invalid_api_key");
    }

    #[test]
    fn fingerprint_never_leaks_key_bytes() {
        let fp = fingerprint("sk-secret-value-1234567890");
        assert!(fp.starts_with("hash:"));
        assert!(!fp.contains("secret"));
        assert_eq!(fingerprint("").as_str(), "<empty>");
    }

    #[test]
    fn base_url_is_openai() {
        assert_eq!(OPENAI_BASE, "https://api.openai.com/v1");
    }
}
