//! OpenAI-compat wire-shape translator.
//!
//! "OpenAI-compat" is the de-facto industry standard for chat-with-tools
//! streaming: a `messages` array with `role: "system" | "user" | "assistant"
//! | "tool"`, function definitions wrapped under `tools: [{ type: "function",
//! function: { ... } }]`, and tool-call arguments shipped as **stringified
//! JSON** (not raw objects) across SSE deltas keyed by `index`.
//!
//! Used today by `openrouter.rs` and `ollama.rs`. Will be reused for OpenAI
//! direct and Anthropic's OpenAI-compat endpoint when those land.
//!
//! Gemini does NOT use this shape — its `functionDeclarations` envelope and
//! object-shape `args` live in `tool_translator/gemini.rs` (separate module
//! by design — collapsing them would be a false-economy abstraction).

use serde::{Deserialize, Serialize};

use crate::ai::provider::{ChatMessage, ChatRole};
use crate::ai::tool_protocol::ToolDef;

// ── Request types (outgoing wire shape) ──

/// Top-level streaming chat-with-tools request body.
///
/// `max_tokens` is optional because Ollama deliberately omits it (the cap
/// lives in the per-model `ollama pull` config). OpenRouter requires it.
#[derive(Serialize)]
pub struct ChatToolsRequest<'a> {
    pub model: &'a str,
    pub messages: Vec<OpenAiToolsMessage>,
    pub stream: bool,
    /// Optional so OpenAI direct can OMIT it — GPT-5.x reasoning models
    /// reject a `temperature` parameter (400 `unsupported_parameter`).
    /// OpenRouter and Ollama still pass `Some(0.7)`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    /// Legacy output cap. Omitted by Ollama; `Some(_)` on OpenRouter.
    /// Do NOT set for OpenAI direct — GPT-5.x rejects `max_tokens` and
    /// wants `max_completion_tokens` instead.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// Modern output cap. OpenAI direct sets this (GPT-4.x AND GPT-5.x
    /// both accept it); other providers leave it None.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_completion_tokens: Option<u32>,
    /// Tool definitions in OpenAI's wrapped shape. Empty array
    /// disables tool use for the turn (sending an empty `tools` field
    /// is allowed and equivalent to omitting it).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<OpenAiToolDef>,
}

#[derive(Serialize)]
pub struct OpenAiToolDef {
    #[serde(rename = "type")]
    pub kind: &'static str, // always "function"
    pub function: OpenAiFunctionDef,
}

#[derive(Serialize)]
pub struct OpenAiFunctionDef {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

/// Outgoing message in the OpenAI shape. Tool messages carry
/// `tool_call_id`; assistant messages with tool use carry `tool_calls`.
/// Fields are skipped when None/empty so providers don't reject empty
/// arrays / null fields.
#[derive(Serialize)]
pub struct OpenAiToolsMessage {
    pub role: &'static str,
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "tool_call_id")]
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<OpenAiOutgoingToolCall>,
}

#[derive(Serialize)]
pub struct OpenAiOutgoingToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: &'static str, // always "function"
    pub function: OpenAiOutgoingFunction,
}

#[derive(Serialize)]
pub struct OpenAiOutgoingFunction {
    pub name: String,
    /// **String-encoded JSON**, NOT a raw object. The OpenAI tool-call
    /// schema requires this — we serialize the args object first.
    pub arguments: String,
}

// ── Streaming response types (incoming wire shape) ──

/// Chunk shape with `delta.tool_calls` support. `delta.content` may
/// still arrive (prose preamble + chain-of-thought from Anthropic
/// models routed via OpenRouter) — emitted as TextChunk events.
#[derive(Deserialize, Debug)]
pub struct ToolsStreamChunk {
    #[serde(default)]
    pub choices: Vec<ToolsStreamChoice>,
    #[serde(default)]
    pub error: Option<ApiError>,
}

#[derive(Deserialize, Debug)]
pub struct ToolsStreamChoice {
    #[serde(default)]
    pub delta: Option<ToolsDelta>,
    #[serde(default, rename = "finish_reason")]
    pub finish_reason: Option<String>,
}

#[derive(Deserialize, Debug)]
pub struct ToolsDelta {
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default, rename = "tool_calls")]
    pub tool_calls: Vec<ToolsDeltaCall>,
}

#[derive(Deserialize, Debug)]
pub struct ToolsDeltaCall {
    /// Index into the parallel-tool-calls array. Required by the
    /// streaming protocol so accumulator state can be keyed correctly
    /// when the model emits N calls interleaved.
    pub index: usize,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub function: Option<ToolsDeltaFunction>,
}

#[derive(Deserialize, Debug)]
pub struct ToolsDeltaFunction {
    #[serde(default)]
    pub name: Option<String>,
    /// Partial JSON-string fragment. Concatenate across all deltas
    /// for the same `index`; parse only after finish_reason arrives.
    #[serde(default)]
    pub arguments: Option<String>,
}

/// Vendor error envelope (`{ "error": { "message": "..." } }`). All
/// OpenAI-compat providers return this shape on quota / rate-limit /
/// validation failures, sometimes mid-stream.
#[derive(Deserialize, Debug)]
pub struct ApiError {
    pub message: String,
}

/// Accumulator state for one parallel tool call as deltas stream in.
/// The provider's streaming loop keeps a `BTreeMap<usize, PartialCall>`
/// keyed by the per-call `index` from `ToolsDeltaCall`.
#[derive(Default)]
pub struct PartialCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

// ── Builders ──

/// Convert our `ToolDef` list into OpenAI's wrapped `tools` shape.
pub fn build_openai_tools(defs: &[ToolDef]) -> Vec<OpenAiToolDef> {
    defs.iter()
        .map(|t| OpenAiToolDef {
            kind: "function",
            function: OpenAiFunctionDef {
                name: t.name.clone(),
                description: t.description.clone(),
                parameters: t.input_schema.clone(),
            },
        })
        .collect()
}

/// Convert our `ChatMessage` history into OpenAI's `messages` array.
/// - `User` → role:"user", text content
/// - `Model` plain → role:"assistant", text content
/// - `Model` with tool_calls → role:"assistant", text content (may be
///   empty) plus `tool_calls` array with **stringified args**
/// - `Tool` → role:"tool", `tool_call_id` set, text content (whatever
///   the agent loop wrapped the result as)
pub fn build_openai_messages(
    system: Option<String>,
    messages: &[ChatMessage],
) -> Vec<OpenAiToolsMessage> {
    let mut out: Vec<OpenAiToolsMessage> = Vec::with_capacity(messages.len() + 1);
    if let Some(ctx) = system {
        if !ctx.is_empty() {
            out.push(OpenAiToolsMessage {
                role: "system",
                content: ctx,
                name: None,
                tool_call_id: None,
                tool_calls: Vec::new(),
            });
        }
    }
    for m in messages {
        match m.role {
            ChatRole::User => out.push(OpenAiToolsMessage {
                role: "user",
                content: m.content.clone(),
                name: None,
                tool_call_id: None,
                tool_calls: Vec::new(),
            }),
            ChatRole::Model => {
                let calls: Vec<OpenAiOutgoingToolCall> = m
                    .tool_calls
                    .iter()
                    .map(|c| OpenAiOutgoingToolCall {
                        id: c.id.clone(),
                        kind: "function",
                        function: OpenAiOutgoingFunction {
                            name: c.name.clone(),
                            // Stringify args — OpenAI's contract requires
                            // `arguments` to be a JSON string, not an object.
                            arguments: serde_json::to_string(&c.args)
                                .unwrap_or_else(|_| "{}".to_string()),
                        },
                    })
                    .collect();
                out.push(OpenAiToolsMessage {
                    role: "assistant",
                    content: m.content.clone(),
                    name: None,
                    tool_call_id: None,
                    tool_calls: calls,
                });
            }
            ChatRole::Tool => out.push(OpenAiToolsMessage {
                role: "tool",
                content: m.content.clone(),
                name: m.tool_name.clone(),
                tool_call_id: m.tool_use_id.clone(),
                tool_calls: Vec::new(),
            }),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::tool_protocol::ToolCall as ProtoToolCall;
    use serde_json::json;

    fn def(name: &str) -> ToolDef {
        ToolDef {
            name: name.into(),
            description: format!("{name} desc"),
            input_schema: json!({"type": "object", "properties": {}}),
        }
    }

    #[test]
    fn build_tools_wraps_each_def_in_function_envelope() {
        let defs = vec![def("a"), def("b")];
        let out = build_openai_tools(&defs);
        assert_eq!(out.len(), 2);
        for (got, want) in out.iter().zip(defs.iter()) {
            assert_eq!(got.kind, "function");
            assert_eq!(got.function.name, want.name);
            assert_eq!(got.function.description, want.description);
            assert_eq!(got.function.parameters, want.input_schema);
        }
    }

    #[test]
    fn build_tools_empty_list_returns_empty() {
        assert!(build_openai_tools(&[]).is_empty());
    }

    #[test]
    fn build_messages_prepends_system_when_nonempty() {
        let msgs = build_openai_messages(Some("be helpful".into()), &[]);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].role, "system");
        assert_eq!(msgs[0].content, "be helpful");
    }

    #[test]
    fn build_messages_skips_system_when_empty_or_none() {
        assert!(build_openai_messages(None, &[]).is_empty());
        assert!(build_openai_messages(Some(String::new()), &[]).is_empty());
    }

    #[test]
    fn build_messages_maps_user_role() {
        let history = vec![ChatMessage {
            role: ChatRole::User,
            content: "hi".into(),
            tool_calls: vec![],
            tool_use_id: None,
            tool_name: None,
        }];
        let out = build_openai_messages(None, &history);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].role, "user");
        assert_eq!(out[0].content, "hi");
    }

    #[test]
    fn build_messages_stringifies_tool_call_args() {
        let history = vec![ChatMessage {
            role: ChatRole::Model,
            content: "calling tool".into(),
            tool_calls: vec![ProtoToolCall {
                id: "call_1".into(),
                name: "read_file".into(),
                args: json!({"path": "notes/today.md"}),
                provider_metadata: None,
            }],
            tool_use_id: None,
            tool_name: None,
        }];
        let out = build_openai_messages(None, &history);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].role, "assistant");
        assert_eq!(out[0].tool_calls.len(), 1);
        let call = &out[0].tool_calls[0];
        assert_eq!(call.id, "call_1");
        assert_eq!(call.function.name, "read_file");
        // Args MUST be a JSON string, not an object.
        assert_eq!(call.function.arguments, r#"{"path":"notes/today.md"}"#);
    }

    #[test]
    fn build_messages_tool_role_sets_call_id_and_name() {
        let history = vec![ChatMessage {
            role: ChatRole::Tool,
            content: "ok".into(),
            tool_calls: vec![],
            tool_use_id: Some("call_1".into()),
            tool_name: Some("read_file".into()),
        }];
        let out = build_openai_messages(None, &history);
        assert_eq!(out[0].role, "tool");
        assert_eq!(out[0].tool_call_id.as_deref(), Some("call_1"));
        assert_eq!(out[0].name.as_deref(), Some("read_file"));
    }

    #[test]
    fn request_serialization_omits_optional_fields_when_unset() {
        // Ollama-style request: no max_tokens, no tools.
        let req = ChatToolsRequest {
            model: "llama3",
            messages: build_openai_messages(None, &[]),
            stream: true,
            temperature: Some(0.7),
            max_tokens: None,
            max_completion_tokens: None,
            tools: Vec::new(),
        };
        let json = serde_json::to_string(&req).expect("serialize");
        assert!(
            !json.contains("max_tokens"),
            "max_tokens must be omitted: {json}"
        );
        assert!(
            !json.contains("max_completion_tokens"),
            "max_completion_tokens must be omitted when None: {json}"
        );
        assert!(
            !json.contains("tools"),
            "empty tools must be omitted: {json}"
        );
    }

    #[test]
    fn request_serialization_includes_max_tokens_when_set() {
        // OpenRouter-style request: max_tokens required.
        let req = ChatToolsRequest {
            model: "claude-sonnet-4.6",
            messages: build_openai_messages(None, &[]),
            stream: true,
            temperature: Some(0.7),
            max_tokens: Some(4096),
            max_completion_tokens: None,
            tools: Vec::new(),
        };
        let json = serde_json::to_string(&req).expect("serialize");
        assert!(json.contains(r#""max_tokens":4096"#), "{json}");
    }

    #[test]
    fn openai_shape_uses_max_completion_tokens_and_omits_temperature() {
        // OpenAI direct: GPT-5.x rejects `max_tokens` + `temperature`, so
        // the request must carry `max_completion_tokens` and neither of the
        // other two. Pins that wire shape.
        let req = ChatToolsRequest {
            model: "gpt-5.5",
            messages: build_openai_messages(None, &[]),
            stream: true,
            temperature: None,
            max_tokens: None,
            max_completion_tokens: Some(8192),
            tools: Vec::new(),
        };
        let json = serde_json::to_string(&req).expect("serialize");
        assert!(json.contains(r#""max_completion_tokens":8192"#), "{json}");
        assert!(
            !json.contains("\"max_tokens\""),
            "no legacy max_tokens: {json}"
        );
        assert!(
            !json.contains("temperature"),
            "no temperature for GPT-5.x: {json}"
        );
    }
}
