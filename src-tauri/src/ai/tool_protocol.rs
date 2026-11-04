//! Wire-format types for native function-calling.
//!
//! Provider-agnostic shape used by `LLMProvider::stream_chat_with_tools`.
//! Each provider impl (`gemini.rs`, `openrouter.rs`, future Anthropic)
//! translates these to the on-the-wire format the provider's API
//! expects:
//!
//! - **Gemini**: `tools: [{ functionDeclarations: [...] }]` request;
//!   `candidates[].content.parts[].functionCall` response.
//! - **OpenRouter / OpenAI**: `tools: [{ type:"function", function: {...} }]`
//!   request; `choices[].message.tool_calls[]` streamed in deltas.
//! - **Anthropic** (future): `tools: [{ name, description, input_schema }]`
//!   request; `content[].type == "tool_use"` blocks in response.
//!
//! See `docs/phase-2-native-tools-and-skills-plan.md` for the migration
//! plan and `docs/agent-tool-calling-architecture.md` for context on
//! why this exists (the XML-in-prompt protocol it replaces).
//!
//! The types here are **internal**. They are not part of the public
//! Tauri IPC surface today — `ToolCall` is the only one that may leak
//! to the frontend (via the future `ai:tool-call` event), and it has
//! `Serialize`/`Deserialize` derives for that reason.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Tool definition shipped to the provider at chat-start time.
///
/// Built once at the start of `run_agent_loop` from the active
/// [`ToolRegistry`]. Each entry maps directly to one row in the
/// provider's tools array.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    /// JSON Schema for the tool's `args` object. Comes verbatim from
    /// [`super::tools::registry::Tool::input_schema`]. Each provider
    /// is responsible for any necessary adaptation (e.g. Gemini
    /// requires `parameters` instead of `input_schema`; OpenAI wants
    /// the same field nested under `function.parameters`).
    pub input_schema: Value,
}

/// A tool call emitted by the model.
///
/// `id` is provider-assigned; treat as opaque. Used to round-trip the
/// pairing between an assistant call and the matching tool response —
/// required by Anthropic and OpenAI, synthesized for Gemini.
///
/// `provider_metadata` carries opaque provider-specific blobs that
/// MUST round-trip on the next request. Today's only consumer is
/// Gemini 2.5's `thoughtSignature` (sibling of `functionCall` in the
/// API response; required when replaying assistant history) — see
/// `ai/gemini.rs`. Other providers leave this `None`. Forward-compat:
/// adding a new provider that needs round-trip metadata is an append
/// into this `Value`, not a struct change.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub args: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_metadata: Option<Value>,
}

/// Why the provider stopped streaming. The agent loop uses this to
/// distinguish "model gave a final answer" from "model wants to call
/// tools and continue."
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    /// Text complete, no tool calls — terminal.
    EndTurn,
    /// One or more tool calls emitted; loop should dispatch them and
    /// re-stream with the results.
    ToolUse,
    /// Anything else (`max_tokens`, content filter, transport error).
    /// String carries the provider-reported reason for surfacing in
    /// logs; not parsed.
    Other(String),
}

/// Streamed events from `stream_chat_with_tools`. Replaces the
/// text-blob + regex-parse pattern used by the legacy XML protocol.
///
/// Providers send through an `mpsc::channel(16)` receiver; the agent
/// loop drains until [`StreamEvent::Stop`] arrives. Bounded channel
/// gives backpressure if the loop is slow to consume.
#[derive(Debug, Clone)]
pub enum StreamEvent {
    /// Partial text chunk. Forwarded to the frontend as `ai:chunk`
    /// inside the provider impl (so the user sees streaming prose);
    /// the agent loop also accumulates these into the assistant turn
    /// it appends to history.
    TextChunk(String),
    /// A complete tool call. Providers that stream call arguments in
    /// pieces (OpenAI shape) accumulate internally and emit ONE event
    /// per call once the arguments are fully assembled.
    ToolCall(ToolCall),
    /// Stream terminated. No further events on this receiver.
    Stop(StopReason),
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tool_def_roundtrip() {
        let def = ToolDef {
            name: "roost_read".to_string(),
            description: "Read notes".to_string(),
            input_schema: json!({"type":"object","required":["paths"]}),
        };
        let ser = serde_json::to_string(&def).unwrap();
        let back: ToolDef = serde_json::from_str(&ser).unwrap();
        assert_eq!(back.name, def.name);
        assert_eq!(back.description, def.description);
        assert_eq!(back.input_schema, def.input_schema);
    }

    #[test]
    fn tool_call_roundtrip() {
        let call = ToolCall {
            id: "toolu_abc".to_string(),
            name: "roost_search".to_string(),
            args: json!({"query":"fitness"}),
            provider_metadata: None,
        };
        let ser = serde_json::to_string(&call).unwrap();
        let back: ToolCall = serde_json::from_str(&ser).unwrap();
        assert_eq!(back.id, call.id);
        assert_eq!(back.name, call.name);
        assert_eq!(back.args, call.args);
        assert!(back.provider_metadata.is_none());
    }

    #[test]
    fn tool_call_provider_metadata_roundtrips_when_set() {
        // Pin the wire shape Gemini's thoughtSignature relies on:
        // the field serializes when present, deserializes back as-is,
        // and is absent on the wire when None (skip_serializing_if).
        let call = ToolCall {
            id: "id".into(),
            name: "roost_read".into(),
            args: json!({}),
            provider_metadata: Some(json!({ "gemini_thought_signature": "abc" })),
        };
        let ser = serde_json::to_string(&call).unwrap();
        assert!(ser.contains("provider_metadata"));
        assert!(ser.contains("gemini_thought_signature"));
        let back: ToolCall = serde_json::from_str(&ser).unwrap();
        assert_eq!(
            back.provider_metadata
                .as_ref()
                .and_then(|v| v.get("gemini_thought_signature"))
                .and_then(|v| v.as_str()),
            Some("abc"),
        );
    }

    #[test]
    fn tool_call_provider_metadata_absent_when_none() {
        let call = ToolCall {
            id: "id".into(),
            name: "x".into(),
            args: json!({}),
            provider_metadata: None,
        };
        let ser = serde_json::to_string(&call).unwrap();
        assert!(
            !ser.contains("provider_metadata"),
            "skip_serializing_if must omit None blobs from the wire"
        );
    }

    #[test]
    fn tool_call_deserializes_legacy_payload_without_metadata_field() {
        // Pre-fix chat sessions saved to disk before provider_metadata
        // existed must deserialize cleanly — the serde default handles
        // it. Regression test for forward-compat with persisted JSON.
        let legacy = r#"{"id":"x","name":"roost_search","args":{}}"#;
        let back: ToolCall = serde_json::from_str(legacy).unwrap();
        assert!(back.provider_metadata.is_none());
    }

    #[test]
    fn tool_call_args_can_be_arbitrary_json() {
        // Some tools have complex nested args (roost_write with body,
        // tags, frontmatter). Round-trip must preserve them exactly.
        let nested = json!({
            "title": "note",
            "body": "# hi\nline 2",
            "tags": ["a", "b"],
            "frontmatter": { "type": "daily" }
        });
        let call = ToolCall {
            id: "id".into(),
            name: "roost_write".into(),
            args: nested.clone(),
            provider_metadata: None,
        };
        let back: ToolCall = serde_json::from_str(&serde_json::to_string(&call).unwrap()).unwrap();
        assert_eq!(back.args, nested);
    }
}
