//! Anthropic (Claude) native Messages-API wire-shape translator.
//!
//! Direct Anthropic is NOT OpenAI-compatible: it uses `POST /v1/messages`
//! with a top-level `system` string, `messages[].content` as an array of
//! typed blocks, tool definitions shaped as `{ name, description,
//! input_schema }` (which maps 1:1 to our `ToolDef` — no wrapper), and a
//! tool-use round-trip that has NO `"tool"` role: tool results ride back
//! as `tool_result` blocks inside a `user` turn.
//!
//! Streaming is an SSE event stream (`message_start`, `content_block_start`,
//! `content_block_delta` with `text_delta` / `input_json_delta`,
//! `content_block_stop`, `message_delta` carrying `stop_reason`,
//! `message_stop`) — a different envelope from the OpenAI `choices[].delta`
//! shape, so this provider drains it itself (see `ai/anthropic.rs`) rather
//! than reusing `streaming::openai_compat_chat`.
//!
//! Thinking is deliberately DISABLED on the request (`thinking:
//! {type:"disabled"}`). We don't yet capture-and-replay Claude's thinking
//! blocks across a tool loop (the Anthropic analog of Gemini's
//! `thoughtSignature` round-trip), and a tool loop that drops thinking
//! blocks can 400 on the follow-up turn for thinking-on models. Disabling
//! keeps every realistic model (Sonnet 5, Opus 4.6/4.7/4.8, Sonnet 4.6,
//! Haiku 4.5) robust. Caveat: an explicit `disabled` 400s on Claude Fable 5
//! / Mythos 5 — those require omitting the field. Adaptive-thinking support
//! (with block replay) is deferred future work.

use serde::{Deserialize, Serialize};

use crate::ai::provider::{ChatMessage, ChatRole};
use crate::ai::tool_protocol::ToolDef;

/// Anthropic API version header value. Sent on every request.
pub const ANTHROPIC_VERSION: &str = "2023-06-01";

// ── Request types (outgoing wire shape) ──

#[derive(Serialize)]
pub struct AnthropicRequest<'a> {
    pub model: &'a str,
    pub max_tokens: u32,
    pub stream: bool,
    /// Claude's system prompt is a top-level string, not a message with
    /// role "system" (that's the OpenAI shape).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    pub messages: Vec<AnthropicMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<AnthropicToolDef>,
    /// Always `{"type":"disabled"}` today — see module docs.
    pub thinking: ThinkingConfig,
}

/// `{"type":"disabled"}`. A dedicated type (not a bare literal) so the
/// serialized shape is pinned by a test and future variants (adaptive)
/// slot in without touching call sites.
#[derive(Serialize)]
pub struct ThinkingConfig {
    #[serde(rename = "type")]
    pub kind: &'static str,
}

impl ThinkingConfig {
    pub fn disabled() -> Self {
        Self { kind: "disabled" }
    }
}

#[derive(Serialize)]
pub struct AnthropicToolDef {
    pub name: String,
    pub description: String,
    /// JSON Schema for the tool args. Anthropic's field name is exactly
    /// `input_schema`, matching our internal `ToolDef` — no rename needed.
    pub input_schema: serde_json::Value,
}

#[derive(Serialize)]
pub struct AnthropicMessage {
    pub role: &'static str, // "user" | "assistant"
    pub content: Vec<ReqContentBlock>,
}

/// Outgoing content blocks. `rename_all = "snake_case"` maps the variant
/// names to Claude's `"text"` / `"tool_use"` / `"tool_result"` type tags.
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ReqContentBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
    },
}

// ── Builders ──

/// Convert our `ToolDef` list into Claude's tools array. Direct field
/// mapping — `input_schema` is already the name Anthropic expects.
pub fn build_anthropic_tools(defs: &[ToolDef]) -> Vec<AnthropicToolDef> {
    defs.iter()
        .map(|t| AnthropicToolDef {
            name: t.name.clone(),
            description: t.description.clone(),
            input_schema: t.input_schema.clone(),
        })
        .collect()
}

/// Convert our `ChatMessage` history into Claude's `messages` array.
///
/// - `User`  → `{role:"user", content:[{text}]}`
/// - `Model` → `{role:"assistant", content:[{text}?, {tool_use}...]}`
/// - `Tool`  → a `tool_result` block. Claude has no `"tool"` role, so
///   tool results ride inside a `user` turn. **Consecutive** `Tool`
///   messages (parallel tool calls dispatched together) are coalesced
///   into ONE user turn with multiple `tool_result` blocks — Claude
///   requires all results for a given assistant tool-use turn to arrive
///   in the single immediately-following user message.
///
/// The system prompt is NOT handled here (it's a top-level request field).
pub fn build_anthropic_messages(messages: &[ChatMessage]) -> Vec<AnthropicMessage> {
    let mut out: Vec<AnthropicMessage> = Vec::with_capacity(messages.len());
    let mut last_was_tool = false;

    for m in messages {
        match m.role {
            ChatRole::User => {
                out.push(AnthropicMessage {
                    role: "user",
                    content: vec![ReqContentBlock::Text {
                        text: m.content.clone(),
                    }],
                });
                last_was_tool = false;
            }
            ChatRole::Model => {
                let mut blocks: Vec<ReqContentBlock> = Vec::new();
                if !m.content.is_empty() {
                    blocks.push(ReqContentBlock::Text {
                        text: m.content.clone(),
                    });
                }
                for c in &m.tool_calls {
                    blocks.push(ReqContentBlock::ToolUse {
                        id: c.id.clone(),
                        name: c.name.clone(),
                        input: c.args.clone(),
                    });
                }
                // Claude rejects an empty content array. An assistant turn
                // with neither prose nor tool calls shouldn't occur, but
                // guard with an empty text block just in case.
                if blocks.is_empty() {
                    blocks.push(ReqContentBlock::Text {
                        text: String::new(),
                    });
                }
                out.push(AnthropicMessage {
                    role: "assistant",
                    content: blocks,
                });
                last_was_tool = false;
            }
            ChatRole::Tool => {
                let block = ReqContentBlock::ToolResult {
                    tool_use_id: m.tool_use_id.clone().unwrap_or_default(),
                    content: m.content.clone(),
                };
                if last_was_tool {
                    // Append to the user turn we just opened for the
                    // previous tool result(s).
                    if let Some(last) = out.last_mut() {
                        last.content.push(block);
                    } else {
                        out.push(AnthropicMessage {
                            role: "user",
                            content: vec![block],
                        });
                    }
                } else {
                    out.push(AnthropicMessage {
                        role: "user",
                        content: vec![block],
                    });
                }
                last_was_tool = true;
            }
        }
    }
    out
}

// ── Streaming response types (incoming SSE wire shape) ──

/// One SSE event from the Messages streaming API, discriminated on the
/// `type` field. Unknown/ignored events (ping, message_start, thinking
/// blocks) fall into `Unknown` so the parser never hard-fails on a shape
/// we don't consume.
#[derive(Deserialize, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamEventWire {
    ContentBlockStart {
        index: usize,
        content_block: BlockStart,
    },
    ContentBlockDelta {
        index: usize,
        delta: BlockDelta,
    },
    MessageDelta {
        delta: MessageDeltaBody,
    },
    MessageStop,
    Error {
        error: AnthropicApiError,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Deserialize, Debug)]
pub struct BlockStart {
    #[serde(rename = "type")]
    pub block_type: String,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Deserialize, Debug)]
pub struct BlockDelta {
    #[serde(rename = "type")]
    pub delta_type: String,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub partial_json: Option<String>,
}

#[derive(Deserialize, Debug)]
pub struct MessageDeltaBody {
    #[serde(default)]
    pub stop_reason: Option<String>,
}

#[derive(Deserialize, Debug)]
pub struct AnthropicApiError {
    #[serde(default)]
    pub message: String,
}

/// Accumulator for one streamed `tool_use` block. `input` arrives as
/// `input_json_delta.partial_json` fragments that must be concatenated
/// and parsed only after the block completes.
#[derive(Default)]
pub struct PartialToolUse {
    pub id: String,
    pub name: String,
    pub json: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::tool_protocol::ToolCall as ProtoToolCall;
    use serde_json::json;

    #[test]
    fn tools_map_input_schema_directly() {
        let defs = vec![ToolDef {
            name: "roost_read".into(),
            description: "Read notes".into(),
            input_schema: json!({"type":"object","required":["paths"]}),
        }];
        let out = build_anthropic_tools(&defs);
        let v = serde_json::to_value(&out).unwrap();
        assert_eq!(v[0]["name"], "roost_read");
        assert_eq!(v[0]["description"], "Read notes");
        assert_eq!(v[0]["input_schema"]["required"][0], "paths");
    }

    #[test]
    fn user_message_is_text_block_user_role() {
        let out = build_anthropic_messages(&[ChatMessage::user("hi")]);
        let v = serde_json::to_value(&out).unwrap();
        assert_eq!(v[0]["role"], "user");
        assert_eq!(v[0]["content"][0]["type"], "text");
        assert_eq!(v[0]["content"][0]["text"], "hi");
    }

    #[test]
    fn model_with_tool_call_emits_text_then_tool_use_object_input() {
        // Regression: Claude wants `input` as an OBJECT (unlike OpenAI's
        // stringified arguments). Pin that.
        let call = ProtoToolCall {
            id: "toolu_1".into(),
            name: "roost_search".into(),
            args: json!({"query": "fitness"}),
            provider_metadata: None,
        };
        let out =
            build_anthropic_messages(&[ChatMessage::model_with_tools("Looking.", vec![call])]);
        let v = serde_json::to_value(&out).unwrap();
        assert_eq!(v[0]["role"], "assistant");
        assert_eq!(v[0]["content"][0]["type"], "text");
        assert_eq!(v[0]["content"][0]["text"], "Looking.");
        assert_eq!(v[0]["content"][1]["type"], "tool_use");
        assert_eq!(v[0]["content"][1]["id"], "toolu_1");
        assert_eq!(v[0]["content"][1]["name"], "roost_search");
        // input is an object, NOT a JSON string.
        assert!(v[0]["content"][1]["input"].is_object());
        assert_eq!(v[0]["content"][1]["input"]["query"], "fitness");
    }

    #[test]
    fn model_with_only_tool_call_omits_empty_text_block() {
        let call = ProtoToolCall {
            id: "toolu_1".into(),
            name: "t".into(),
            args: json!({}),
            provider_metadata: None,
        };
        let out = build_anthropic_messages(&[ChatMessage::model_with_tools("", vec![call])]);
        let v = serde_json::to_value(&out).unwrap();
        assert_eq!(v[0]["content"].as_array().unwrap().len(), 1);
        assert_eq!(v[0]["content"][0]["type"], "tool_use");
    }

    #[test]
    fn tool_result_rides_in_user_turn_no_tool_role() {
        let msg = ChatMessage::tool_response("toolu_1", "roost_search", r#"{"hits":[]}"#);
        let out = build_anthropic_messages(&[msg]);
        let v = serde_json::to_value(&out).unwrap();
        assert_eq!(v[0]["role"], "user");
        assert_eq!(v[0]["content"][0]["type"], "tool_result");
        assert_eq!(v[0]["content"][0]["tool_use_id"], "toolu_1");
        assert_eq!(v[0]["content"][0]["content"], r#"{"hits":[]}"#);
    }

    #[test]
    fn consecutive_tool_results_coalesce_into_one_user_turn() {
        // Parallel tool calls: two tool responses in a row must land in
        // ONE user message with two tool_result blocks — Claude rejects
        // split-across-turns tool results for the same assistant turn.
        let a = ChatMessage::tool_response("toolu_1", "a", "ra");
        let b = ChatMessage::tool_response("toolu_2", "b", "rb");
        let out = build_anthropic_messages(&[a, b]);
        assert_eq!(out.len(), 1, "two tool results → one user turn");
        let v = serde_json::to_value(&out).unwrap();
        assert_eq!(v[0]["content"].as_array().unwrap().len(), 2);
        assert_eq!(v[0]["content"][0]["tool_use_id"], "toolu_1");
        assert_eq!(v[0]["content"][1]["tool_use_id"], "toolu_2");
    }

    #[test]
    fn full_turn_sequence_alternates_correctly() {
        // user → assistant(tool_use) → tool_result → assistant(text)
        let call = ProtoToolCall {
            id: "toolu_1".into(),
            name: "roost_read".into(),
            args: json!({"paths": ["x"]}),
            provider_metadata: None,
        };
        let msgs = vec![
            ChatMessage::user("read x"),
            ChatMessage::model_with_tools("", vec![call]),
            ChatMessage::tool_response("toolu_1", "roost_read", "content of x"),
            ChatMessage::model("Here it is."),
        ];
        let out = build_anthropic_messages(&msgs);
        assert_eq!(out.len(), 4);
        assert_eq!(out[0].role, "user");
        assert_eq!(out[1].role, "assistant");
        assert_eq!(out[2].role, "user"); // tool_result turn
        assert_eq!(out[3].role, "assistant");
    }

    #[test]
    fn request_serializes_with_thinking_disabled_and_top_level_system() {
        let req = AnthropicRequest {
            model: "claude-sonnet-5",
            max_tokens: 8192,
            stream: true,
            system: Some("be helpful".into()),
            messages: build_anthropic_messages(&[ChatMessage::user("hi")]),
            tools: build_anthropic_tools(&[]),
            thinking: ThinkingConfig::disabled(),
        };
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["model"], "claude-sonnet-5");
        assert_eq!(v["max_tokens"], 8192);
        assert_eq!(v["stream"], true);
        assert_eq!(v["system"], "be helpful");
        assert_eq!(v["thinking"]["type"], "disabled");
        // Empty tools omitted from the wire.
        assert!(v.get("tools").is_none());
    }

    #[test]
    fn parse_content_block_start_tool_use() {
        let raw = r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_9","name":"roost_read","input":{}}}"#;
        let ev: StreamEventWire = serde_json::from_str(raw).unwrap();
        match ev {
            StreamEventWire::ContentBlockStart {
                index,
                content_block,
            } => {
                assert_eq!(index, 1);
                assert_eq!(content_block.block_type, "tool_use");
                assert_eq!(content_block.id.as_deref(), Some("toolu_9"));
                assert_eq!(content_block.name.as_deref(), Some("roost_read"));
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn parse_text_delta_and_input_json_delta() {
        let text =
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hi"}}"#;
        match serde_json::from_str::<StreamEventWire>(text).unwrap() {
            StreamEventWire::ContentBlockDelta { delta, .. } => {
                assert_eq!(delta.delta_type, "text_delta");
                assert_eq!(delta.text.as_deref(), Some("Hi"));
            }
            _ => panic!("expected delta"),
        }
        let json = r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"pa"}}"#;
        match serde_json::from_str::<StreamEventWire>(json).unwrap() {
            StreamEventWire::ContentBlockDelta { delta, .. } => {
                assert_eq!(delta.delta_type, "input_json_delta");
                assert_eq!(delta.partial_json.as_deref(), Some("{\"pa"));
            }
            _ => panic!("expected delta"),
        }
    }

    #[test]
    fn parse_message_delta_stop_reason() {
        let raw = r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":5}}"#;
        match serde_json::from_str::<StreamEventWire>(raw).unwrap() {
            StreamEventWire::MessageDelta { delta } => {
                assert_eq!(delta.stop_reason.as_deref(), Some("tool_use"));
            }
            _ => panic!("expected message_delta"),
        }
    }

    #[test]
    fn parse_error_event() {
        let raw = r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#;
        match serde_json::from_str::<StreamEventWire>(raw).unwrap() {
            StreamEventWire::Error { error } => assert_eq!(error.message, "Overloaded"),
            _ => panic!("expected error"),
        }
    }

    #[test]
    fn unknown_events_do_not_fail_parsing() {
        // Events we don't consume (message_start, ping, content_block_stop,
        // and any future type) must fall into `Unknown` rather than error
        // the whole parse — the `#[serde(other)]` catch-all guarantees it.
        for raw in [
            r#"{"type":"message_start","message":{"id":"msg_1","role":"assistant"}}"#,
            r#"{"type":"ping"}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"some_future_event","foo":123}"#,
        ] {
            let ev: StreamEventWire = serde_json::from_str(raw).unwrap();
            assert!(
                matches!(ev, StreamEventWire::Unknown),
                "expected Unknown for {raw}"
            );
        }
    }

    #[test]
    fn parse_message_stop() {
        let ev: StreamEventWire = serde_json::from_str(r#"{"type":"message_stop"}"#).unwrap();
        assert!(matches!(ev, StreamEventWire::MessageStop));
    }
}
