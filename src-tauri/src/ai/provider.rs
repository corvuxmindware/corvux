// LLMProvider trait surface; some methods exposed for future provider implementations
#![allow(dead_code)]

//! Provider abstraction for LLM chat.
//!
//! - `LLMProvider`: cloud Gemini / OpenRouter / Ollama, plus local
//!   llama.cpp on desktop.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Runtime, Wry};

use super::tool_protocol::{StopReason, StreamEvent, ToolCall, ToolDef};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default, specta::Type)]
#[serde(rename_all = "lowercase")]
pub enum ChatRole {
    #[default]
    User,
    Model,
    /// Tool response message. Carries the result of a previous
    /// assistant tool call. Required for the native function-calling
    /// path; ignored by the legacy XML protocol (which uses synthetic
    /// `User` turns to feed tool results back to the model).
    Tool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, specta::Type)]
pub struct ChatMessage {
    pub role: ChatRole,
    pub content: String,
    /// Set when `role == Tool`. Binds this response to the assistant
    /// turn's [`ToolCall::id`] so providers that require pairing
    /// (Anthropic, OpenAI) can round-trip it. `None` for User/Model.
    /// `#[serde(default)]` so pre-2.A chat-session JSON on disk
    /// deserializes cleanly without the field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_use_id: Option<String>,
    /// Set when `role == Tool`. Tool name — some providers want it
    /// alongside the response for debuggability (OpenAI does;
    /// Anthropic doesn't). Always populated when `tool_use_id` is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    /// Tool calls emitted by the model in this assistant turn. Empty
    /// on User and Tool turns. On Model turns with tool use, the
    /// `content` field holds any prose preamble (often empty) and
    /// `tool_calls` carries the structured calls. `#[serde(default)]`
    /// for backward compat with pre-2.A persisted history.
    ///
    /// Skipped by specta — ToolCall.args is `serde_json::Value`, which
    /// specta-typescript walks into and trips its BigInt-forbid on
    /// `serde_json::Number`'s internal i64. The frontend's chat store
    /// declares its own ToolCall shape; this wire field is round-tripped
    /// untyped through `appInvoke` rather than the typed surface.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[specta(skip)]
    pub tool_calls: Vec<ToolCall>,
}

impl ChatMessage {
    /// Convenience for the common case of a plain text user message.
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: ChatRole::User,
            content: content.into(),
            tool_use_id: None,
            tool_name: None,
            tool_calls: Vec::new(),
        }
    }

    /// Convenience for a plain text assistant message (no tool calls).
    pub fn model(content: impl Into<String>) -> Self {
        Self {
            role: ChatRole::Model,
            content: content.into(),
            tool_use_id: None,
            tool_name: None,
            tool_calls: Vec::new(),
        }
    }

    /// Convenience for an assistant turn that includes tool calls.
    /// `content` may be empty (the model jumped straight to tools).
    pub fn model_with_tools(content: impl Into<String>, tool_calls: Vec<ToolCall>) -> Self {
        Self {
            role: ChatRole::Model,
            content: content.into(),
            tool_use_id: None,
            tool_name: None,
            tool_calls,
        }
    }

    /// Convenience for a tool response message bound to a prior
    /// assistant call.
    pub fn tool_response(
        tool_use_id: impl Into<String>,
        tool_name: impl Into<String>,
        content: impl Into<String>,
    ) -> Self {
        Self {
            role: ChatRole::Tool,
            content: content.into(),
            tool_use_id: Some(tool_use_id.into()),
            tool_name: Some(tool_name.into()),
            tool_calls: Vec::new(),
        }
    }
}

/// Trait for LLM chat providers (cloud Gemini, local llama.cpp).
///
/// Generic over Tauri runtime `R` (defaults to `Wry`). Production code
/// uses the default and reads `LLMProvider` unchanged; tests pass
/// `LLMProvider<tauri::test::MockRuntime>` so the agent loop can drive
/// a fake provider without a real Tauri app.
#[async_trait]
pub trait LLMProvider<R: Runtime + 'static = Wry>: Send + Sync {
    /// Stream a chat completion. Tokens are emitted via
    /// `app.emit("ai:chunk", &text)`. The provider is responsible for
    /// honoring `cancel` and emitting `ai:done` / `ai:error` as appropriate.
    ///
    /// Returns the full accumulated response text so the caller can
    /// post-process (e.g. parse tool calls, extract citations, etc.).
    ///
    /// **Legacy path.** The XML-in-prompt protocol uses this. New code
    /// should call [`Self::stream_chat_with_tools`] which gives the
    /// caller structured tool-call events instead of a text blob to
    /// regex-parse. This method stays until the XML path is removed
    /// (Phase 2.A definition of done).
    async fn stream_chat(
        &self,
        app: &AppHandle<R>,
        messages: Vec<ChatMessage>,
        system_context: Option<String>,
        cancel: Arc<AtomicBool>,
    ) -> Result<String, String>;

    /// Native function-calling stream. Returns a receiver of
    /// [`StreamEvent`]s — `TextChunk` for streamed prose, `ToolCall`
    /// for each structured tool call the model makes, and `Stop` to
    /// terminate. The receiver is closed by the provider when the
    /// stream ends; callers drain until the channel is closed.
    ///
    /// Providers MUST emit `app.emit("ai:chunk", ...)` for streaming
    /// text just like the legacy `stream_chat` does; the receiver's
    /// `TextChunk` events exist for the agent loop's history
    /// accumulation, not for re-display.
    ///
    /// # Default implementation
    ///
    /// Bridges to [`Self::stream_chat`] for providers that have not
    /// yet been migrated. The default ignores `tools` (the legacy
    /// API has no surface for them), buffers the full response, and
    /// emits one `TextChunk` followed by `Stop(EndTurn)` so the agent
    /// loop sees a clean — but tool-less — turn.
    ///
    /// Providers that DO support native tools (Gemini, OpenRouter)
    /// override this with a streaming implementation that translates
    /// `tools` to the provider's native shape and emits structured
    /// `ToolCall` events as they arrive.
    async fn stream_chat_with_tools(
        &self,
        app: &AppHandle<R>,
        messages: Vec<ChatMessage>,
        system_context: Option<String>,
        _tools: Vec<ToolDef>,
        cancel: Arc<AtomicBool>,
    ) -> Result<tokio::sync::mpsc::Receiver<StreamEvent>, String> {
        // Default: degrade to the text-only path. `_tools` is unused
        // because there's no slot for it in the legacy API. Providers
        // that override this method use the tools list properly.
        let full = self
            .stream_chat(app, messages, system_context, cancel)
            .await?;
        let (tx, rx) = tokio::sync::mpsc::channel(2);
        // Send + close. Channel capacity is 2 — both sends are
        // guaranteed to succeed without awaiting on a consumer because
        // the receiver hasn't been polled yet but the buffer fits both.
        let _ = tx.send(StreamEvent::TextChunk(full)).await;
        let _ = tx.send(StreamEvent::Stop(StopReason::EndTurn)).await;
        // tx drops at end of function → receiver sees channel close
        // once both buffered events are consumed.
        Ok(rx)
    }

    fn model_name(&self) -> &str;

    fn is_local(&self) -> bool;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn user_message_serializes_without_tool_fields() {
        // Skip-if-empty on the new fields keeps user/model messages
        // looking exactly like they did pre-2.A — important so
        // existing session JSON on disk reads back unchanged AND new
        // session JSON stays compact.
        let m = ChatMessage::user("hello");
        let s = serde_json::to_string(&m).unwrap();
        assert_eq!(s, r#"{"role":"user","content":"hello"}"#);
    }

    #[test]
    fn model_message_serializes_without_tool_fields() {
        let m = ChatMessage::model("hi");
        let s = serde_json::to_string(&m).unwrap();
        assert_eq!(s, r#"{"role":"model","content":"hi"}"#);
    }

    #[test]
    fn pre_2a_chat_message_still_deserializes() {
        // Regression: old chat-session JSON on disk has only
        // role + content. New optional fields default to None / [].
        let json = r#"{"role":"user","content":"old format"}"#;
        let m: ChatMessage = serde_json::from_str(json).unwrap();
        assert_eq!(m.role, ChatRole::User);
        assert_eq!(m.content, "old format");
        assert!(m.tool_use_id.is_none());
        assert!(m.tool_name.is_none());
        assert!(m.tool_calls.is_empty());
    }

    #[test]
    fn model_with_tool_calls_roundtrip() {
        let call = ToolCall {
            id: "toolu_1".into(),
            name: "roost_read".into(),
            args: json!({"paths":["x"]}),
            provider_metadata: None,
        };
        let m = ChatMessage::model_with_tools("", vec![call.clone()]);
        let ser = serde_json::to_string(&m).unwrap();
        let back: ChatMessage = serde_json::from_str(&ser).unwrap();
        assert_eq!(back.role, ChatRole::Model);
        assert_eq!(back.tool_calls.len(), 1);
        assert_eq!(back.tool_calls[0].id, call.id);
        assert_eq!(back.tool_calls[0].name, call.name);
        assert_eq!(back.tool_calls[0].args, call.args);
    }

    #[test]
    fn tool_response_roundtrip() {
        let m = ChatMessage::tool_response("toolu_1", "roost_read", "{\"ok\":1}");
        let ser = serde_json::to_string(&m).unwrap();
        let back: ChatMessage = serde_json::from_str(&ser).unwrap();
        assert_eq!(back.role, ChatRole::Tool);
        assert_eq!(back.tool_use_id.as_deref(), Some("toolu_1"));
        assert_eq!(back.tool_name.as_deref(), Some("roost_read"));
        assert_eq!(back.content, "{\"ok\":1}");
        assert!(back.tool_calls.is_empty());
    }

    // Note: `default_stream_chat_with_tools_bridges_text` would belong
    // here to exercise the trait's default impl, but it needs an
    // `AppHandle` which requires `tauri::test::MockRuntime`. That
    // surface isn't built in this configuration (see comments at the
    // bottom of `ai/tools/agent_loop.rs`). The default impl is small
    // and the build-time wiring is type-checked by the compiler, so
    // we accept the gap for Chunk 1; full-loop tests against a mock
    // provider come in Chunk 4.
}
