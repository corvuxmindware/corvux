//! Anthropic (Claude) direct chat provider — native Messages API
//! (https://api.anthropic.com/v1/messages).
//!
//! Direct Claude, not via OpenRouter: a user's own Anthropic key, one
//! fewer party in the request path (Locked Decision #4), and no gateway
//! markup. This talks the native Messages API (NOT the OpenAI-compat shim
//! Anthropic also exposes) so tool use, streaming, and future features
//! behave exactly as Anthropic documents them.
//!
//! Wire-shape translation (request content blocks, tool round-trip, SSE
//! event types) lives in `tool_translator::anthropic`. This file owns
//! HTTP, auth headers, and the streaming drain — which is bespoke because
//! the Messages SSE envelope (`content_block_delta` etc.) differs from the
//! OpenAI `choices[].delta` shape the shared drain handles.
//!
//! Thinking is disabled on the request — see the translator module docs
//! for why (no thinking-block replay yet; keeps tool loops robust).

use std::collections::BTreeMap;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use async_trait::async_trait;
use reqwest::Client;
use tauri_specta::Event;

use crate::events;

use super::provider::{ChatMessage, ChatRole, LLMProvider};
use super::streaming::consumer::SseEvents;
use super::tool_protocol::{StopReason, StreamEvent, ToolCall as ProtoToolCall, ToolDef};
use super::tool_translator::anthropic::{
    build_anthropic_messages, build_anthropic_tools, AnthropicMessage, AnthropicRequest,
    PartialToolUse, ReqContentBlock, StreamEventWire, ThinkingConfig, ANTHROPIC_VERSION,
};

/// Anthropic API base. `/messages` and `/models` are appended.
pub const ANTHROPIC_BASE: &str = "https://api.anthropic.com/v1";

/// Output-token cap. Higher than the OpenAI path's 4096 because Claude
/// packs full note bodies into `tool_use.input` JSON on `roost_write`, and
/// 4096 truncates mid-write. 8192 is a cost-vs-truncation compromise;
/// `agent_loop_v2`'s MAX_TOKENS recovery still catches a hard cap.
const MAX_TOKENS: u32 = 8192;

pub struct AnthropicChat {
    client: Arc<Client>,
    api_key: String,
    model: String,
}

impl AnthropicChat {
    pub fn new(client: Arc<Client>, api_key: String, model: String) -> Self {
        Self {
            client,
            api_key,
            model,
        }
    }
}

#[async_trait]
impl<R: tauri::Runtime + 'static> LLMProvider<R> for AnthropicChat {
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

/// Non-reversible key fingerprint for logs — first 8 hex of blake3(key).
fn fingerprint(key: &str) -> String {
    let trimmed = key.trim();
    if trimmed.is_empty() {
        return "<empty>".to_string();
    }
    let hash = blake3::hash(trimmed.as_bytes());
    format!("hash:{}", &hash.to_hex().as_str()[..8])
}

/// Attach the three headers every Messages API call needs.
fn auth_headers(req: reqwest::RequestBuilder, api_key: &str) -> reqwest::RequestBuilder {
    req.header("x-api-key", api_key)
        .header("anthropic-version", ANTHROPIC_VERSION)
        .header("content-type", "application/json")
}

// ── Legacy (text-only) path ─────────────────────────────────────────
// Escape-hatch for `use_native_tools = false`. Maps every role to a
// plain text turn (Tool → user) — no tool_use/tool_result blocks, since
// the legacy XML protocol carries tool results inline as user text.

async fn stream_chat_impl<R: tauri::Runtime>(
    client: &Client,
    app: &tauri::AppHandle<R>,
    api_key: &str,
    model: &str,
    messages: Vec<ChatMessage>,
    system_context: Option<String>,
    cancel: Arc<AtomicBool>,
) -> Result<String, String> {
    let url = format!("{}/messages", ANTHROPIC_BASE);

    tracing::info!(
        model = model,
        key_fingerprint = %fingerprint(api_key),
        key_len = api_key.trim().len(),
        "anthropic: dispatching chat"
    );

    let msgs: Vec<AnthropicMessage> = messages
        .into_iter()
        .map(|m| AnthropicMessage {
            role: match m.role {
                ChatRole::Model => "assistant",
                // User + Tool both render as user turns on the legacy path.
                _ => "user",
            },
            content: vec![ReqContentBlock::Text { text: m.content }],
        })
        .collect();

    let body = AnthropicRequest {
        model,
        max_tokens: MAX_TOKENS,
        stream: true,
        system: system_context.filter(|s| !s.is_empty()),
        messages: msgs,
        tools: Vec::new(),
        thinking: ThinkingConfig::disabled(),
    };

    let response = auth_headers(client.post(&url), api_key)
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
            "anthropic: request rejected"
        );
        return Err(format!("Anthropic API error ({}): {}", status, body_text));
    }

    let mut events = SseEvents::new(response.bytes_stream(), cancel.clone());
    let mut full_text = String::new();

    while let Some(event) = events.next_event().await? {
        for line in event.lines() {
            let Some(data) = line.strip_prefix("data: ") else {
                continue;
            };
            let data = data.trim();
            if data.is_empty() {
                continue;
            }
            let ev: StreamEventWire = match serde_json::from_str(data) {
                Ok(e) => e,
                Err(_) => continue,
            };
            match ev {
                StreamEventWire::Error { error } => {
                    events::AiError(error.message.clone()).emit(app).ok();
                    return Err(error.message);
                }
                StreamEventWire::ContentBlockDelta { delta, .. } => {
                    if delta.delta_type == "text_delta" {
                        if let Some(t) = delta.text {
                            if !t.is_empty() {
                                full_text.push_str(&t);
                                events::AiChunk(t).emit(app).ok();
                            }
                        }
                    }
                }
                StreamEventWire::MessageStop => break,
                _ => {}
            }
        }
    }

    events::AiDone(full_text.clone()).emit(app).ok();
    Ok(full_text)
}

// ── Native function-calling path ────────────────────────────────────

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
    let url = format!("{}/messages", ANTHROPIC_BASE);

    tracing::info!(
        model = %model,
        key_fingerprint = %fingerprint(&api_key),
        key_len = api_key.trim().len(),
        tools = tools.len(),
        "anthropic: dispatching chat_with_tools"
    );

    let body = AnthropicRequest {
        model: &model,
        max_tokens: MAX_TOKENS,
        stream: true,
        system: system_context.filter(|s| !s.is_empty()),
        messages: build_anthropic_messages(&messages),
        tools: build_anthropic_tools(&tools),
        thinking: ThinkingConfig::disabled(),
    };

    let response = auth_headers(client.post(&url), &api_key)
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
            "anthropic: request rejected"
        );
        return Err(format!("Anthropic API error ({}): {}", status, body_text));
    }

    let (tx, rx) = tokio::sync::mpsc::channel::<StreamEvent>(16);

    tokio::spawn(async move {
        let mut events = SseEvents::new(response.bytes_stream(), cancel.clone());
        // content-block index → accumulating tool_use call. BTreeMap so
        // flush iterates in the order Claude emitted the blocks.
        let mut partials: BTreeMap<usize, PartialToolUse> = BTreeMap::new();
        let mut stop_reason: Option<String> = None;

        'outer: loop {
            let event = match events.next_event().await {
                Ok(Some(e)) => e,
                Ok(None) => break, // cancel or EOF → flush
                Err(e) => {
                    let _ = tx.send(StreamEvent::Stop(StopReason::Other(e))).await;
                    return;
                }
            };

            for line in event.lines() {
                let Some(data) = line.strip_prefix("data: ") else {
                    continue;
                };
                let data = data.trim();
                if data.is_empty() {
                    continue;
                }
                let parsed: StreamEventWire = match serde_json::from_str(data) {
                    Ok(p) => p,
                    Err(_) => continue,
                };

                match parsed {
                    StreamEventWire::Error { error } => {
                        let _ = events::AiError(error.message.clone()).emit(&app);
                        let _ = tx
                            .send(StreamEvent::Stop(StopReason::Other(error.message)))
                            .await;
                        return;
                    }
                    StreamEventWire::ContentBlockStart {
                        index,
                        content_block,
                    } => {
                        if content_block.block_type == "tool_use" {
                            partials.insert(
                                index,
                                PartialToolUse {
                                    id: content_block.id.unwrap_or_default(),
                                    name: content_block.name.unwrap_or_default(),
                                    json: String::new(),
                                },
                            );
                        }
                    }
                    StreamEventWire::ContentBlockDelta { index, delta } => {
                        match delta.delta_type.as_str() {
                            "text_delta" => {
                                if let Some(t) = delta.text {
                                    if !t.is_empty() {
                                        let _ = events::AiChunk(t.clone()).emit(&app);
                                        if tx.send(StreamEvent::TextChunk(t)).await.is_err() {
                                            break 'outer;
                                        }
                                    }
                                }
                            }
                            "input_json_delta" => {
                                if let Some(pj) = delta.partial_json {
                                    if let Some(p) = partials.get_mut(&index) {
                                        p.json.push_str(&pj);
                                    }
                                }
                            }
                            _ => {} // thinking_delta / signature_delta — ignored
                        }
                    }
                    StreamEventWire::MessageDelta { delta } => {
                        if let Some(r) = delta.stop_reason {
                            stop_reason = Some(r);
                        }
                    }
                    StreamEventWire::MessageStop => break 'outer,
                    StreamEventWire::Unknown => {}
                }
            }
        }

        // Flush accumulated tool_use blocks. Parse args only now — the
        // full JSON is assembled from all `input_json_delta` fragments.
        let emitted_tool_call = !partials.is_empty();
        for (idx, partial) in partials.into_iter() {
            let args_str = if partial.json.is_empty() {
                "{}".to_string()
            } else {
                partial.json
            };
            let args: serde_json::Value = serde_json::from_str(&args_str).unwrap_or_else(|_| {
                // Args didn't parse — surface a marker so the agent loop's
                // schema validator fails cleanly instead of dispatching a
                // half-built call. Raw string kept for debugging.
                serde_json::json!({ "_invalid_args_raw": args_str })
            });
            let id = if partial.id.is_empty() {
                format!("toolu_{idx}")
            } else {
                partial.id
            };
            let call = ProtoToolCall {
                id,
                name: partial.name,
                args,
                // Anthropic tool_use has no per-call round-trip blob — the
                // `id` carries everything the tool_result needs.
                provider_metadata: None,
            };
            if tx.send(StreamEvent::ToolCall(call)).await.is_err() {
                return;
            }
        }

        let stop = if emitted_tool_call {
            StopReason::ToolUse
        } else {
            match stop_reason.as_deref() {
                Some("tool_use") => StopReason::ToolUse,
                Some("end_turn") | Some("stop_sequence") | None => StopReason::EndTurn,
                Some(other) => StopReason::Other(other.to_string()),
            }
        };
        let _ = tx.send(StreamEvent::Stop(stop)).await;
    });

    Ok(rx)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_url_and_version_pinned() {
        assert_eq!(ANTHROPIC_BASE, "https://api.anthropic.com/v1");
        assert_eq!(ANTHROPIC_VERSION, "2023-06-01");
    }

    #[test]
    fn fingerprint_never_leaks_key_bytes() {
        let fp = fingerprint("sk-ant-secret-0987654321");
        assert!(fp.starts_with("hash:"));
        assert!(!fp.contains("secret"));
        assert_eq!(fingerprint("   ").as_str(), "<empty>");
    }

    #[test]
    fn model_name_returns_configured_model() {
        let c = AnthropicChat::new(
            Arc::new(reqwest::Client::new()),
            "k".into(),
            "claude-sonnet-5".into(),
        );
        assert_eq!(LLMProvider::<tauri::Wry>::model_name(&c), "claude-sonnet-5");
        assert!(!LLMProvider::<tauri::Wry>::is_local(&c));
    }
}
