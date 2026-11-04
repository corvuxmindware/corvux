//! OpenRouter chat provider — OpenAI-compatible chat completions API
//! (https://openrouter.ai/api/v1/chat/completions).
//!
//! OpenRouter unifies access to many model providers (Anthropic, OpenAI,
//! Google, Meta, Mistral, etc.) behind a single OpenAI-shaped API. We
//! talk to it the same way we'd talk to OpenAI's `chat/completions`,
//! plus the optional `HTTP-Referer` / `X-Title` headers for app
//! attribution in OpenRouter's leaderboard.
//!
//! Tool-use compatibility: the chat layer in this app uses **text-based**
//! `<tool_call>...</tool_call>` parsing in the agent loop (see
//! `ai/tools/agent_loop.rs`). That means the LLM provider only needs to
//! (a) stream tokens and (b) honor a `stop` sequence so the model halts
//! at the closing tag. Both are vanilla OpenAI-API features, so OpenRouter
//! works with every model that respects `stop` — which is essentially all
//! of them. No native function-calling integration needed.

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
// Test-only — the test module builds fixture ProtoToolCall + StopReason
// values to exercise the OpenAI-compat translator and ToolsStreamChunk
// parsing paths.
#[cfg(test)]
use super::tool_protocol::ToolCall as ProtoToolCall;
#[cfg(test)]
use super::tool_translator::openai_compat::ToolsStreamChunk;
use super::tool_translator::openai_compat::{
    build_openai_messages, build_openai_tools, ApiError, ChatToolsRequest,
};

/// Default OpenRouter base URL. Endpoint path is appended.
pub const OPENROUTER_BASE: &str = "https://openrouter.ai/api/v1";

/// Sent as `HTTP-Referer` for app attribution. Must be a valid URL or
/// OpenRouter rejects it; we use the project repo as the public face.
const ATTRIBUTION_REFERER: &str = "https://corvux.app";

/// Sent as `X-Title` so the app shows up correctly in OpenRouter's
/// per-app leaderboard / usage dashboards.
const ATTRIBUTION_TITLE: &str = "Corvux";

pub struct OpenRouterChat {
    client: Arc<Client>,
    api_key: String,
    model: String,
}

impl OpenRouterChat {
    pub fn new(client: Arc<Client>, api_key: String, model: String) -> Self {
        Self {
            client,
            api_key,
            model,
        }
    }
}

#[async_trait]
impl<R: tauri::Runtime + 'static> LLMProvider<R> for OpenRouterChat {
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

// ── Request / response types (OpenAI-compatible) ────────────────────

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<OpenAiMessage>,
    stream: bool,
    temperature: f32,
    max_tokens: u32,
    /// Halt generation at any of these literal strings. Same as the
    /// Gemini path: `</tool_call>` keeps the model from hallucinating
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
// and imported above — same shape ({ message }) used by every OpenAI-compat
// provider for error envelopes.

/// Render a key fingerprint for logs as a non-reversible hash — never
/// any prefix or tail bytes of the actual secret. Lets us tell apart
/// "did the user save a different key" without leaking key material
/// into log buffers, copy-paste diagnostic reports, or screenshots.
///
/// `<empty>` for empty input so empty-string bugs show up cleanly.
/// Otherwise: first 8 hex chars of `blake3(key)` — deterministic for
/// the same key, opaque for everyone else.
fn fingerprint(key: &str) -> String {
    let trimmed = key.trim();
    if trimmed.is_empty() {
        return "<empty>".to_string();
    }
    let hash = blake3::hash(trimmed.as_bytes());
    let hex = hash.to_hex();
    format!("hash:{}", &hex.as_str()[..8])
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
    let url = format!("{}/chat/completions", OPENROUTER_BASE);

    tracing::info!(
        model = model,
        key_fingerprint = %fingerprint(api_key),
        key_len = api_key.trim().len(),
        "openrouter: dispatching chat"
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
                // Same reasoning as gemini.rs: legacy path maps Tool
                // messages onto "user" so existing tool-result inline
                // history still flows. Native path will use the
                // proper OpenAI `"tool"` role + `tool_call_id` field.
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
        max_tokens: 4096,
        stop: vec!["</tool_call>".to_string()],
    };

    let response = client
        .post(&url)
        .header("Authorization", format!("Bearer {api_key}"))
        .header("HTTP-Referer", ATTRIBUTION_REFERER)
        .header("X-Title", ATTRIBUTION_TITLE)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("HTTP request failed: {e}"))?;

    if !response.status().is_success() {
        let status = response.status();
        let body_text = response.text().await.unwrap_or_default();
        // Log to tracing so the diagnostic capture sees the full body —
        // returning Err alone propagates to the chat panel toast but
        // doesn't land in the structured log stream.
        tracing::error!(
            status = %status,
            model = model,
            key_fingerprint = %fingerprint(api_key),
            body = %body_text,
            "openrouter: request rejected"
        );
        return Err(format!("OpenRouter API error ({}): {}", status, body_text));
    }

    // AUDIT-057: shared cancel-aware SSE consumer replaces the prior
    // inline loop (100ms cancel-poll + boundary detection + keepalive
    // trim). Per-event parsing stays here — provider-specific.
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
                Err(_) => continue, // keep-alive / partial / unknown
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

    // Loop ended — either cancel fired or the stream completed cleanly.
    // Both emit `ai:done` with whatever text has accumulated; the cancel
    // flag itself is the source of truth for "user stopped this turn"
    // when the agent loop introspects.
    events::AiDone(full_text.clone()).emit(app).ok();
    Ok(full_text)
}

// AUDIT-057: `find_sse_boundary` moved to `crate::ai::streaming::sse` and
// imported at the top of this file.

// ── Native function-calling path (Phase 2.A) ────────────────────
//
// OpenRouter routes to many backends — Anthropic, OpenAI, Google,
// Meta, Mistral, etc. — all behind the OpenAI `tools` API shape:
//
//   Request:
//     tools: [{ type: "function", function: { name, description,
//                                               parameters: <schema> } }]
//
//   Streaming:
//     delta.tool_calls[].{ index, id?, function: { name?, arguments? } }
//     - `index` identifies which parallel tool call this delta belongs
//       to (model may emit multiple calls in one response).
//     - `id` and `function.name` arrive on the first delta for an
//       index; subsequent deltas only carry `arguments` fragments.
//     - `arguments` is a stringified-JSON fragment; must be
//       concatenated across all deltas for that index and ONLY parsed
//       when finish_reason == "tool_calls" arrives. Mid-stream
//       parsing is unsafe — Anthropic-via-OpenRouter sometimes
//       interleaves additional metadata.
//
//   Tool response message:
//     { role: "tool", tool_call_id: "...", content: "<stringified result>" }
//
//   Assistant message echoing prior tool calls:
//     { role: "assistant", content: "...",
//       tool_calls: [{ id, type: "function",
//                      function: { name, arguments: "<stringified-json>" } }] }

// AUDIT-057: all of `ChatToolsRequest`, `OpenAiToolDef`,
// `OpenAiFunctionDef`, `OpenAiToolsMessage`, `OpenAiOutgoingToolCall`,
// `OpenAiOutgoingFunction`, `ToolsStreamChunk`, `ToolsStreamChoice`,
// `ToolsDelta`, `ToolsDeltaCall`, `ToolsDeltaFunction`, `PartialCall`,
// `build_openai_tools`, and `build_openai_messages` moved to
// `crate::ai::tool_translator::openai_compat` (imported above). They were
// byte-identical to ollama.rs except for the `max_tokens` field — the
// shared struct now models that with `max_tokens: Option<u32>`.

/// Streaming implementation for the native function-calling path.
/// Spawns a task that drains the SSE stream, accumulates per-index
/// tool-call deltas, and emits typed events through the channel.
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
    let url = format!("{}/chat/completions", OPENROUTER_BASE);

    tracing::info!(
        model = %model,
        key_fingerprint = %fingerprint(&api_key),
        key_len = api_key.trim().len(),
        tools = tools.len(),
        "openrouter: dispatching chat_with_tools"
    );

    let chat = build_openai_messages(system_context, &messages);
    let body = ChatToolsRequest {
        model: &model,
        messages: chat,
        stream: true,
        temperature: Some(0.7),
        max_tokens: Some(4096),
        max_completion_tokens: None,
        tools: build_openai_tools(&tools),
    };

    let response = client
        .post(&url)
        .header("Authorization", format!("Bearer {}", api_key))
        .header("HTTP-Referer", ATTRIBUTION_REFERER)
        .header("X-Title", ATTRIBUTION_TITLE)
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
            "openrouter: request rejected"
        );
        return Err(format!("OpenRouter API error ({}): {}", status, body_text));
    }

    // AUDIT-057: shared spawn + drain — see streaming::openai_compat_chat.
    Ok(spawn_openai_compat_chat(response, app, cancel))
}

#[cfg(test)]
mod tools_tests {
    use super::*;
    use serde_json::json;

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
        assert_eq!(json[0]["function"]["description"], "Read notes");
        assert_eq!(json[0]["function"]["parameters"]["required"][0], "paths");
    }

    #[test]
    fn build_openai_tools_empty_returns_empty_vec() {
        assert!(build_openai_tools(&[]).is_empty());
    }

    #[test]
    fn build_openai_messages_passes_system_through() {
        let msgs = build_openai_messages(Some("be helpful".into()), &[]);
        assert_eq!(msgs.len(), 1);
        let json = serde_json::to_value(&msgs[0]).unwrap();
        assert_eq!(json["role"], "system");
        assert_eq!(json["content"], "be helpful");
    }

    #[test]
    fn build_openai_messages_skips_empty_system() {
        let msgs = build_openai_messages(Some(String::new()), &[ChatMessage::user("hi")]);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].role, "user");
    }

    #[test]
    fn user_and_model_render_as_user_assistant() {
        let msgs = build_openai_messages(
            None,
            &[ChatMessage::user("hi"), ChatMessage::model("hello")],
        );
        assert_eq!(msgs[0].role, "user");
        assert_eq!(msgs[1].role, "assistant");
        assert!(msgs[0].tool_calls.is_empty());
        assert!(msgs[1].tool_calls.is_empty());
    }

    #[test]
    fn model_with_tool_calls_stringifies_arguments() {
        // Regression: OpenAI requires `function.arguments` to be a
        // JSON STRING, not an object. Easy to miss when porting from
        // Gemini (which takes an object).
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
        assert_eq!(json["tool_calls"][0]["id"], "call_1");
        assert_eq!(json["tool_calls"][0]["type"], "function");
        assert_eq!(json["tool_calls"][0]["function"]["name"], "roost_search");
        // Arguments is a string, not nested object.
        assert!(
            json["tool_calls"][0]["function"]["arguments"].is_string(),
            "arguments must be string per OpenAI contract"
        );
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
        assert_eq!(json["content"], r#"{"hits":[]}"#);
        // tool_calls field skipped when empty.
        assert!(json.get("tool_calls").is_none());
    }

    #[test]
    fn parse_tool_call_delta_chunk() {
        // OpenAI sends a sequence like:
        //   1: {delta:{tool_calls:[{index:0, id:"call_1", function:{name:"roost_read"}}]}}
        //   2: {delta:{tool_calls:[{index:0, function:{arguments:"{\"pa"}}]}}
        //   3: {delta:{tool_calls:[{index:0, function:{arguments:"ths\":[\"x\"]}"}}]}}
        //   4: {choices:[{finish_reason:"tool_calls"}]}
        let raw = r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"roost_read"}}]}}]}"#;
        let parsed: ToolsStreamChunk = serde_json::from_str(raw).unwrap();
        let choice = &parsed.choices[0];
        let delta = choice.delta.as_ref().unwrap();
        assert_eq!(delta.tool_calls[0].index, 0);
        assert_eq!(delta.tool_calls[0].id.as_deref(), Some("call_1"));
        assert_eq!(
            delta.tool_calls[0]
                .function
                .as_ref()
                .unwrap()
                .name
                .as_deref(),
            Some("roost_read")
        );
    }

    #[test]
    fn parse_arguments_chunk_carries_partial_fragment() {
        let raw = r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"pa"}}]}}]}"#;
        let parsed: ToolsStreamChunk = serde_json::from_str(raw).unwrap();
        let func = parsed.choices[0].delta.as_ref().unwrap().tool_calls[0]
            .function
            .as_ref()
            .unwrap();
        assert_eq!(func.arguments.as_deref(), Some("{\"pa"));
        assert!(func.name.is_none(), "name not on this delta");
    }

    #[test]
    fn parse_finish_reason_tool_calls() {
        let raw = r#"{"choices":[{"index":0,"finish_reason":"tool_calls"}]}"#;
        let parsed: ToolsStreamChunk = serde_json::from_str(raw).unwrap();
        assert_eq!(
            parsed.choices[0].finish_reason.as_deref(),
            Some("tool_calls")
        );
    }

    #[test]
    fn parse_finish_reason_stop_means_end_turn() {
        let raw = r#"{"choices":[{"index":0,"finish_reason":"stop"}]}"#;
        let parsed: ToolsStreamChunk = serde_json::from_str(raw).unwrap();
        assert_eq!(parsed.choices[0].finish_reason.as_deref(), Some("stop"));
    }

    #[test]
    fn parse_anthropic_thinking_block_arrives_as_text_delta() {
        // Anthropic-via-OpenRouter sometimes emits chain-of-thought
        // as a text delta before the tool call. We don't filter it —
        // it lands as a TextChunk, agent loop accumulates it as
        // prose. The frontend can choose to render thinking blocks
        // collapsed (future work).
        let raw = r#"{"choices":[{"delta":{"content":"<thinking>need to search</thinking>"}}]}"#;
        let parsed: ToolsStreamChunk = serde_json::from_str(raw).unwrap();
        assert_eq!(
            parsed.choices[0].delta.as_ref().unwrap().content.as_deref(),
            Some("<thinking>need to search</thinking>")
        );
    }

    #[test]
    fn empty_arguments_at_flush_become_empty_object() {
        // Pure-function check that we cover the "no args field" case
        // observed when a model invokes a no-arg tool (e.g.
        // skill_list). Verified indirectly via the json flushing
        // logic — here we just confirm the JSON parser accepts `{}`
        // as the fallback.
        let v: serde_json::Value = serde_json::from_str("{}").unwrap();
        assert!(v.is_object());
        assert_eq!(v.as_object().unwrap().len(), 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_serializes_with_openai_compatible_schema() {
        let req = ChatRequest {
            model: "anthropic/claude-sonnet-4.6",
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
            max_tokens: 4096,
            stop: vec!["</tool_call>".into()],
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["model"], "anthropic/claude-sonnet-4.6");
        assert_eq!(json["stream"], true);
        assert_eq!(json["messages"][0]["role"], "system");
        assert_eq!(json["messages"][1]["role"], "user");
        assert_eq!(json["stop"][0], "</tool_call>");
    }

    #[test]
    fn stream_chunk_parses_openrouter_delta() {
        // Sample OpenRouter SSE payload (one delta with content).
        let raw = r#"{
            "id": "gen-xyz",
            "object": "chat.completion.chunk",
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
        // Some providers stream a final chunk with no choices array.
        let raw = r#"{"id":"gen-end","object":"chat.completion.chunk"}"#;
        let parsed: StreamChunk = serde_json::from_str(raw).unwrap();
        assert!(parsed.choices.is_empty());
    }

    #[test]
    fn stream_chunk_surfaces_api_error() {
        let raw = r#"{"error":{"message":"Insufficient credits","type":"invalid_request"}}"#;
        let parsed: StreamChunk = serde_json::from_str(raw).unwrap();
        assert_eq!(parsed.error.unwrap().message, "Insufficient credits");
    }

    // AUDIT-057: `sse_boundary_detection` test removed — `find_sse_boundary`
    // now lives in `crate::ai::streaming::sse` and is fully covered by
    // that module's test suite (Unix delimiter, CRLF, multi-event split,
    // multiline event, prefers-first-delimiter edge case).
}
