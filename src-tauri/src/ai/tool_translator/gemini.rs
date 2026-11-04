//! Gemini wire-shape translator.
//!
//! Gemini's chat-with-tools shape is materially different from the OpenAI-
//! compat one — both halves of the tool round-trip diverge:
//!
//! - Outgoing tool definitions sit under `tools: [{ functionDeclarations:
//!   [...] }]` (a bundle wrapper), not directly under `tools: [...]`.
//! - Outgoing tool-call echoes use `functionCall: { name, args: <object> }`
//!   with **args as a raw JSON object**, not a stringified payload.
//! - Tool *responses* come back as `role: "function"`, `parts: [{
//!   functionResponse: { name, response: <object> } }]`.
//! - 2.5-series thinking models (pro, flash, flash-lite) emit an opaque
//!   `thoughtSignature` blob alongside every `functionCall`. It MUST be
//!   round-tripped verbatim on the next request — dropping it gets a
//!   `400 INVALID_ARGUMENT: Function call is missing a thought_signature`.
//!   We store it in `ProtoToolCall::provider_metadata` under
//!   `gemini_thought_signature` and re-emit from `build_gemini_contents`.
//! - Gemini's API doesn't return a call id, so we synthesize one via the
//!   process-local `next_gemini_call_id()` counter.
//!
//! Provider auth, the URL, and the SSE loop stay in `gemini.rs`. This
//! module is only JSON-shape translation + the round-trip helpers.

use serde::{Deserialize, Serialize};
use std::sync::atomic::AtomicU64;

use crate::ai::provider::{ChatMessage, ChatRole};
use crate::ai::tool_protocol::ToolDef;

/// Migration escape hatch: assistant turns that were persisted before
/// 2.A's native function-calling path landed (or were captured from a
/// non-thinking model) won't carry a real `thoughtSignature`. Without
/// one, Gemini's 2.5 thinking variants reject the request. Per
/// ai.google.dev/gemini-api/docs/thought-signatures, we can submit a
/// placeholder on the first call only when no real signature exists
/// anywhere on that turn — a single real signature wins over the dummy.
pub const MIGRATION_DUMMY_SIGNATURE: &str = "context_engineering_is_the_way to_go";

// ── Shared types (used by BOTH the legacy and native paths in gemini.rs) ──

/// Single text part. Used by `GeminiSystemInstruction` and by the legacy
/// path's request/response content shapes (which gemini.rs keeps locally).
#[derive(Serialize, Deserialize)]
pub struct GeminiPart {
    pub text: String,
}

/// System prompt envelope. Identical structure between legacy and tools
/// paths — Gemini wraps the system text under `parts: [{ text }]`.
#[derive(Serialize)]
pub struct GeminiSystemInstruction {
    pub parts: Vec<GeminiPart>,
}

/// Provider-emitted error envelope. Appears in both the legacy
/// `GeminiStreamResponse` and the tools-path `GeminiToolsStreamResponse`.
#[derive(Deserialize)]
pub struct GeminiError {
    pub message: String,
}

// ── Tools-path request types (outgoing wire shape) ──

/// Top-level streaming chat-with-tools request body.
#[derive(Serialize)]
pub struct GeminiToolsRequest {
    pub contents: Vec<GeminiToolsContent>,
    #[serde(rename = "systemInstruction", skip_serializing_if = "Option::is_none")]
    pub system_instruction: Option<GeminiSystemInstruction>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<GeminiToolDeclarationsBundle>,
    #[serde(rename = "generationConfig")]
    pub generation_config: GeminiToolsGenerationConfig,
}

#[derive(Serialize)]
pub struct GeminiToolsGenerationConfig {
    pub temperature: f32,
    #[serde(rename = "maxOutputTokens")]
    pub max_output_tokens: u32,
}

/// One row of the request body's `tools` array. Gemini wraps function
/// declarations under this object; multiple bundles can be sent but we
/// always pack everything into one for simplicity.
#[derive(Serialize)]
pub struct GeminiToolDeclarationsBundle {
    #[serde(rename = "functionDeclarations")]
    pub function_declarations: Vec<GeminiFunctionDeclaration>,
}

#[derive(Serialize)]
pub struct GeminiFunctionDeclaration {
    pub name: String,
    pub description: String,
    /// JSON Schema for `args`. Gemini calls it `parameters`.
    pub parameters: serde_json::Value,
}

/// Content message in the tools-enabled request. Same shape as the
/// legacy `GeminiContent` but the `parts` allow `text`, `functionCall`,
/// or `functionResponse` — not just text.
#[derive(Serialize)]
pub struct GeminiToolsContent {
    pub role: String,
    pub parts: Vec<GeminiToolsPart>,
}

/// Parts in the tools-enabled request can carry any of three payloads.
/// Modeled as separate optional fields rather than an enum so the JSON
/// shape matches Gemini's "one of" expectation directly (Gemini accepts
/// a part with one populated field; others are omitted).
///
/// `thought_signature` is REQUIRED to sit alongside `function_call` when
/// replaying assistant history for 2.5-series thinking models. The
/// `build_gemini_contents` builder pulls it back out of the captured
/// `provider_metadata` blob and (for migration-history turns with no
/// captured signature anywhere) substitutes the `MIGRATION_DUMMY_SIGNATURE`.
#[derive(Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct GeminiToolsPart {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub function_call: Option<GeminiOutgoingFunctionCall>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub function_response: Option<GeminiOutgoingFunctionResponse>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thought_signature: Option<String>,
}

/// Echo of a prior tool-call when replaying assistant history. Gemini
/// expects the same call shape going back in as comes out.
#[derive(Serialize)]
pub struct GeminiOutgoingFunctionCall {
    pub name: String,
    pub args: serde_json::Value,
}

#[derive(Serialize)]
pub struct GeminiOutgoingFunctionResponse {
    pub name: String,
    /// Arbitrary JSON object the tool produced. We try to parse the
    /// agent loop's stringified result content as JSON; if that fails
    /// (e.g. the tool returned plain text), we wrap it under `{text}`.
    pub response: serde_json::Value,
}

// ── Tools-path response types (incoming wire shape) ──

/// SSE chunk shape with `functionCall` support. Anything not parsed
/// here is treated as "ignore this chunk" by the streaming loop.
#[derive(Deserialize)]
pub struct GeminiToolsStreamResponse {
    pub candidates: Option<Vec<GeminiToolsCandidate>>,
    pub error: Option<GeminiError>,
}

#[derive(Deserialize)]
pub struct GeminiToolsCandidate {
    pub content: Option<GeminiToolsResponseContent>,
    #[serde(rename = "finishReason")]
    pub finish_reason: Option<String>,
}

#[derive(Deserialize)]
pub struct GeminiToolsResponseContent {
    pub parts: Option<Vec<GeminiToolsResponsePart>>,
}

#[derive(Deserialize)]
pub struct GeminiToolsResponsePart {
    pub text: Option<String>,
    #[serde(rename = "functionCall")]
    pub function_call: Option<GeminiIncomingFunctionCall>,
    /// Base64 blob the model emits alongside `functionCall` for 2.5-
    /// series thinking models. Opaque to us; must round-trip verbatim
    /// on the next request or Gemini rejects with a 400.
    #[serde(rename = "thoughtSignature")]
    pub thought_signature: Option<String>,
}

#[derive(Deserialize)]
pub struct GeminiIncomingFunctionCall {
    pub name: String,
    /// Gemini sends `args` as a JSON object, not a string.
    pub args: serde_json::Value,
}

// ── Builders ──

/// Convert our generic `ToolDef` list into Gemini's request shape.
pub fn build_gemini_tools(defs: &[ToolDef]) -> Vec<GeminiToolDeclarationsBundle> {
    if defs.is_empty() {
        return Vec::new();
    }
    let declarations = defs
        .iter()
        .map(|t| GeminiFunctionDeclaration {
            name: t.name.clone(),
            description: t.description.clone(),
            parameters: t.input_schema.clone(),
        })
        .collect();
    vec![GeminiToolDeclarationsBundle {
        function_declarations: declarations,
    }]
}

/// Convert our `ChatMessage` history into Gemini's `contents` array.
/// Handles the three role variants:
/// - `User` → `role: "user"`, single text part.
/// - `Model` with tool_calls → `role: "model"`, text part (if non-empty)
///   followed by one `functionCall` part per tool call. Mirrors the way
///   the model originally emitted them so the next turn's tool-use
///   chain stays coherent.
/// - `Tool` → `role: "function"`, single `functionResponse` part. The
///   content string is parsed as JSON if possible; otherwise wrapped
///   under `{ "text": ... }` so Gemini still accepts it.
pub fn build_gemini_contents(messages: &[ChatMessage]) -> Vec<GeminiToolsContent> {
    messages
        .iter()
        .map(|m| match m.role {
            ChatRole::User => GeminiToolsContent {
                role: "user".to_string(),
                parts: vec![GeminiToolsPart {
                    text: Some(m.content.clone()),
                    ..Default::default()
                }],
            },
            ChatRole::Model => {
                let mut parts: Vec<GeminiToolsPart> = Vec::new();
                if !m.content.is_empty() {
                    parts.push(GeminiToolsPart {
                        text: Some(m.content.clone()),
                        ..Default::default()
                    });
                }
                // Migration escape hatch: a Model turn with `tool_calls`
                // but NO captured signatures anywhere comes from
                // pre-2.A persisted history (legacy XML path) or a
                // model that just didn't emit one. Inject the dummy
                // signature on the first call to bypass strict
                // validation. Only fires when nothing real is
                // available — a single real signature anywhere wins
                // over the dummy.
                let has_any_signature = m.tool_calls.iter().any(|c| {
                    c.provider_metadata
                        .as_ref()
                        .and_then(|v| v.get("gemini_thought_signature"))
                        .and_then(|v| v.as_str())
                        .is_some()
                });
                let needs_migration_dummy = !m.tool_calls.is_empty() && !has_any_signature;
                for (idx, call) in m.tool_calls.iter().enumerate() {
                    // Extract the round-trip blob. None when the call
                    // came from a non-thinking model (e.g. tests, older
                    // sessions persisted before this field existed) or
                    // a subsequent parallel call (per Gemini's spec,
                    // only the first parallel call carries the signature).
                    let real_signature = call
                        .provider_metadata
                        .as_ref()
                        .and_then(|v| v.get("gemini_thought_signature"))
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());
                    let thought_signature = real_signature.or_else(|| {
                        if needs_migration_dummy && idx == 0 {
                            Some(MIGRATION_DUMMY_SIGNATURE.to_string())
                        } else {
                            None
                        }
                    });
                    parts.push(GeminiToolsPart {
                        function_call: Some(GeminiOutgoingFunctionCall {
                            name: call.name.clone(),
                            args: call.args.clone(),
                        }),
                        thought_signature,
                        ..Default::default()
                    });
                }
                // Empty assistant turn is unusual but possible — keep
                // it as a single text part so Gemini doesn't reject
                // an empty parts array.
                if parts.is_empty() {
                    parts.push(GeminiToolsPart {
                        text: Some(String::new()),
                        ..Default::default()
                    });
                }
                GeminiToolsContent {
                    role: "model".to_string(),
                    parts,
                }
            }
            ChatRole::Tool => {
                let name = m.tool_name.clone().unwrap_or_else(|| "tool".to_string());
                // Try the content as JSON; fall back to wrapping the
                // raw string so Gemini's required-object shape holds.
                let response = serde_json::from_str::<serde_json::Value>(&m.content)
                    .unwrap_or_else(|_| serde_json::json!({ "text": m.content }));
                GeminiToolsContent {
                    role: "function".to_string(),
                    parts: vec![GeminiToolsPart {
                        function_response: Some(GeminiOutgoingFunctionResponse { name, response }),
                        ..Default::default()
                    }],
                }
            }
        })
        .collect()
}

/// Monotonic counter for synthesized tool-call ids. Gemini's API
/// doesn't assign call ids; we synthesize them so the agent loop can
/// pair calls with responses by id consistently across providers.
/// Process-local; resets on app restart.
pub fn next_gemini_call_id() -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("gemini_call_{n}")
}
