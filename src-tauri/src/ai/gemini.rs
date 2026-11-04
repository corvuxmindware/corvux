//! Gemini cloud provider — implements `LLMProvider` (chat streaming).
//! Holds a shared `reqwest::Client` from Tauri state so we don't spin up
//! a new HTTP stack per call.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tauri_specta::Event;

use crate::events;

use super::provider::{ChatMessage, ChatRole, LLMProvider};
use super::streaming::consumer::SseEvents;
use super::tool_protocol::{StopReason, StreamEvent, ToolCall as ProtoToolCall, ToolDef};
use super::tool_translator::gemini::{
    build_gemini_contents, build_gemini_tools, next_gemini_call_id, GeminiError, GeminiPart,
    GeminiSystemInstruction, GeminiToolsGenerationConfig, GeminiToolsRequest,
    GeminiToolsStreamResponse,
};
// Test-only — the migration-dummy is only referenced by the round-trip
// fixtures in this file's `tests` module (it's the placeholder that
// `build_gemini_contents` substitutes in `tool_translator::gemini`, not
// something the runtime path imports).
#[cfg(test)]
use super::tool_translator::gemini::MIGRATION_DUMMY_SIGNATURE;

// ── Chat ────────────────────────────────────────────────────────

pub struct GeminiChat {
    client: Arc<Client>,
    api_key: String,
    model: String,
}

impl GeminiChat {
    pub fn new(client: Arc<Client>, api_key: String, model: String) -> Self {
        Self {
            client,
            api_key,
            model,
        }
    }
}

#[async_trait]
impl<R: tauri::Runtime + 'static> LLMProvider<R> for GeminiChat {
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

// ── Chat request/response types ────────────────────────────────

#[derive(Serialize)]
struct GeminiRequest {
    contents: Vec<GeminiContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    system_instruction: Option<GeminiSystemInstruction>,
    generation_config: GeminiGenerationConfig,
}

#[derive(Serialize)]
struct GeminiContent {
    role: String,
    parts: Vec<GeminiPart>,
}

// AUDIT-057: `GeminiPart` and `GeminiSystemInstruction` moved to
// `crate::ai::tool_translator::gemini` (both shapes are reused by the
// tools-path request body, which lives in the translator module).

#[derive(Serialize)]
struct GeminiGenerationConfig {
    temperature: f32,
    #[serde(rename = "maxOutputTokens")]
    max_output_tokens: u32,
    /// Halt generation at any of these literal strings. We pass
    /// `</tool_call>` so the model stops *before* it can hallucinate
    /// a fake `tool_result` block + answer in the same turn — without
    /// this, Gemini keeps generating past its own tool-call boundary
    /// and synthesizes invented tool output. The agent loop re-appends
    /// the closing tag before parsing so the regex still matches.
    #[serde(rename = "stopSequences", skip_serializing_if = "Vec::is_empty")]
    stop_sequences: Vec<String>,
}

#[derive(Deserialize)]
struct GeminiStreamResponse {
    candidates: Option<Vec<GeminiCandidate>>,
    error: Option<GeminiError>,
}

#[derive(Deserialize)]
struct GeminiCandidate {
    content: Option<GeminiResponseContent>,
}

#[derive(Deserialize)]
struct GeminiResponseContent {
    parts: Option<Vec<GeminiPart>>,
}

// AUDIT-057: `GeminiError` moved to `crate::ai::tool_translator::gemini`
// (shared with the tools-path response shape).

async fn stream_chat_impl<R: tauri::Runtime>(
    client: &Client,
    app: &tauri::AppHandle<R>,
    api_key: &str,
    model: &str,
    messages: Vec<ChatMessage>,
    system_context: Option<String>,
    cancel: Arc<AtomicBool>,
) -> Result<String, String> {
    let url = format!(
        "https://generativelanguage.googleapis.com/v1beta/models/{}:streamGenerateContent?alt=sse",
        model
    );

    let contents: Vec<GeminiContent> = messages
        .iter()
        .map(|m| GeminiContent {
            role: match m.role {
                ChatRole::User => "user".to_string(),
                ChatRole::Model => "model".to_string(),
                // Legacy `stream_chat` doesn't speak native tool_use —
                // it expects tool results to be inlined as user turns.
                // Map Tool messages onto "user" so the model still sees
                // them. The native `stream_chat_with_tools` (to be
                // added) will handle Tool messages properly.
                ChatRole::Tool => "user".to_string(),
            },
            parts: vec![GeminiPart {
                text: m.content.clone(),
            }],
        })
        .collect();

    let system_instruction = system_context.map(|ctx| GeminiSystemInstruction {
        parts: vec![GeminiPart { text: ctx }],
    });

    let body = GeminiRequest {
        contents,
        system_instruction,
        generation_config: GeminiGenerationConfig {
            temperature: 0.7,
            // 10K headroom — note bodies that include a multi-week table
            // (Technoo / Puli weekly summaries) routinely exceed 4K
            // tokens when wrapped as `roost_write` function-call JSON.
            // Gemini Flash 2.5/3.x all support up to 64K output; 10K is a
            // safety-vs-runaway-cost compromise. Pair this with the
            // MAX_TOKENS recovery in agent_loop_v2 so a stream that
            // still hits the cap retries with a "be concise" nudge
            // instead of dead-ending.
            max_output_tokens: 10_000,
            stop_sequences: vec!["</tool_call>".to_string()],
        },
    };

    let response = client
        .post(&url)
        .header("x-goog-api-key", api_key)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("HTTP request failed: {e}"))?;

    if !response.status().is_success() {
        let status = response.status();
        let body_text = response.text().await.unwrap_or_default();
        return Err(format!("Gemini API error ({}): {}", status, body_text));
    }

    // AUDIT-057: shared cancel-aware SSE consumer. Per-event parsing
    // (Gemini's `candidates[].content.parts[].text` shape) stays here.
    let mut events = SseEvents::new(response.bytes_stream(), cancel.clone());
    let mut full_text = String::new();

    while let Some(event) = events.next_event().await? {
        for line in event.lines() {
            if let Some(data) = line.strip_prefix("data: ") {
                let data = data.trim();
                if data.is_empty() {
                    continue;
                }

                match serde_json::from_str::<GeminiStreamResponse>(data) {
                    Ok(resp) => {
                        if let Some(err) = resp.error {
                            events::AiError(err.message.clone()).emit(app).ok();
                            return Err(err.message);
                        }

                        if let Some(text) = resp
                            .candidates
                            .as_ref()
                            .and_then(|c| c.first())
                            .and_then(|c| c.content.as_ref())
                            .and_then(|c| c.parts.as_ref())
                            .and_then(|p| p.first())
                            .map(|p| p.text.as_str())
                        {
                            if !text.is_empty() {
                                full_text.push_str(text);
                                events::AiChunk(text.to_string()).emit(app).ok();
                            }
                        }
                    }
                    Err(_) => {
                        // Skip unparseable chunks (e.g. keep-alive)
                    }
                }
            }
        }
    }

    events::AiDone(full_text.clone()).emit(app).ok();
    Ok(full_text)
}

// AUDIT-057: `find_sse_boundary` moved to `crate::ai::streaming::sse` and
// imported at the top of this file. The previous local copy was identical
// to the OpenRouter and Ollama copies.

// ── Native function-calling path (Phase 2.A) ────────────────────
//
// Gemini's tool-use protocol:
// - Request body adds a `tools` array of `{ functionDeclarations: [...] }`.
// - Model emits `parts[].functionCall: { name, args }` — args is a JSON
//   object delivered whole (not chunked), so accumulation logic is
//   simpler than OpenAI-shape providers.
// - Tool responses round-trip as a `role: "function"` message with
//   `parts[].functionResponse: { name, response: <obj> }`.
// - Finish reason `STOP` lands at end of every successful generation;
//   tool use is detected by inspecting whether any part carried a
//   `functionCall`, not by a special finish reason.

// AUDIT-057: every tools-path type + builder (`GeminiToolsRequest`,
// `GeminiToolsGenerationConfig`, `GeminiToolDeclarationsBundle`,
// `GeminiFunctionDeclaration`, `GeminiToolsContent`, `GeminiToolsPart`,
// `GeminiOutgoingFunctionCall`, `GeminiOutgoingFunctionResponse`,
// `GeminiToolsStreamResponse`, `GeminiToolsCandidate`,
// `GeminiToolsResponseContent`, `GeminiToolsResponsePart`,
// `GeminiIncomingFunctionCall`, `build_gemini_tools`,
// `build_gemini_contents`, `next_gemini_call_id`) lives in
// `crate::ai::tool_translator::gemini` and is imported above.

/// Streaming implementation for the native function-calling path.
/// Spawns a background task that drains the SSE stream and pushes
/// typed events through an mpsc channel. The caller drains the
/// receiver and acts on each event; errors flow through as
/// `Stop(StopReason::Other(...))`.
///
/// `app` / `cancel` / `client` are cloned upfront so the spawned task
/// outlives this function's stack frame.
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
    let url = format!(
        "https://generativelanguage.googleapis.com/v1beta/models/{}:streamGenerateContent?alt=sse",
        model
    );

    let contents = build_gemini_contents(&messages);
    let system_instruction = system_context.map(|ctx| GeminiSystemInstruction {
        parts: vec![GeminiPart { text: ctx }],
    });
    let body = GeminiToolsRequest {
        contents,
        system_instruction,
        tools: build_gemini_tools(&tools),
        // No stop sequence — native function-calling doesn't need to
        // halt mid-stream to parse XML. Let the model finish naturally.
        generation_config: GeminiToolsGenerationConfig {
            temperature: 0.7,
            // 10K headroom — see legacy path for rationale. Native
            // function-calling path needs this most because the LLM
            // packs the whole note body into the function_call.args
            // JSON, which counts against output tokens.
            max_output_tokens: 10_000,
        },
    };

    let response = client
        .post(&url)
        .header("x-goog-api-key", &api_key)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("HTTP request failed: {e}"))?;

    if !response.status().is_success() {
        let status = response.status();
        let body_text = response.text().await.unwrap_or_default();
        return Err(format!("Gemini API error ({}): {}", status, body_text));
    }

    // Channel capacity 16: small enough to backpressure a runaway
    // stream, large enough that normal token-rate doesn't block.
    let (tx, rx) = tokio::sync::mpsc::channel::<StreamEvent>(16);

    tokio::spawn(async move {
        // AUDIT-057: shared cancel-aware SSE consumer. Per-event parsing
        // and the thoughtSignature → provider_metadata round-trip stay
        // here — Gemini-specific.
        let mut events = SseEvents::new(response.bytes_stream(), cancel.clone());
        let mut emitted_tool_call = false;
        let mut finish_reason: Option<String> = None;

        'outer: loop {
            let event = match events.next_event().await {
                Ok(Some(e)) => e,
                Ok(None) => break,
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

                let parsed: GeminiToolsStreamResponse = match serde_json::from_str(data) {
                    Ok(p) => p,
                    Err(_) => continue, // ignore keep-alives + non-JSON
                };

                if let Some(err) = parsed.error {
                    let _ = events::AiError(err.message.clone()).emit(&app);
                    let _ = tx
                        .send(StreamEvent::Stop(StopReason::Other(err.message)))
                        .await;
                    return;
                }

                let Some(candidates) = parsed.candidates else {
                    continue;
                };
                let Some(candidate) = candidates.into_iter().next() else {
                    continue;
                };
                if let Some(reason) = candidate.finish_reason {
                    finish_reason = Some(reason);
                }
                let Some(content) = candidate.content else {
                    continue;
                };
                let Some(parts) = content.parts else { continue };
                for part in parts {
                    if let Some(text) = part.text {
                        if !text.is_empty() {
                            // Mirror legacy streaming behavior:
                            // emit ai:chunk for the user-facing
                            // panel, AND push a TextChunk event
                            // for the agent loop's history.
                            let _ = events::AiChunk(text.clone()).emit(&app);
                            if tx.send(StreamEvent::TextChunk(text)).await.is_err() {
                                break 'outer;
                            }
                        }
                    }
                    if let Some(fc) = part.function_call {
                        emitted_tool_call = true;
                        // Thinking-Gemini (3.x / 2.5-series) emits a
                        // `thoughtSignature` as a SIBLING of the
                        // functionCall on the same part. We stash
                        // it on ProtoToolCall.provider_metadata so
                        // the agent loop persists it on the
                        // assistant ChatMessage; `build_gemini_contents`
                        // reads it back when replaying history. Drop
                        // it and the next request hits 400 INVALID_ARGUMENT.
                        let provider_metadata = part
                            .thought_signature
                            .map(|sig| serde_json::json!({ "gemini_thought_signature": sig }));
                        let call = ProtoToolCall {
                            id: next_gemini_call_id(),
                            name: fc.name,
                            args: fc.args,
                            provider_metadata,
                        };
                        if tx.send(StreamEvent::ToolCall(call)).await.is_err() {
                            break 'outer;
                        }
                    }
                }
            }
        }

        // Map Gemini's coarse finish reasons + tool-emission state to
        // our `StopReason`:
        // - any tool call emitted this turn → `ToolUse` (loop must
        //   dispatch and continue)
        // - finish `STOP` (or unset, default) without tool calls →
        //   `EndTurn`
        // - everything else (`MAX_TOKENS`, `SAFETY`, `RECITATION`,
        //   `OTHER`) → `Other(reason)` so the loop can surface it.
        let stop = if emitted_tool_call {
            StopReason::ToolUse
        } else {
            match finish_reason.as_deref() {
                Some("STOP") | None => StopReason::EndTurn,
                Some(other) => StopReason::Other(other.to_string()),
            }
        };
        let _ = tx.send(StreamEvent::Stop(stop)).await;
    });

    Ok(rx)
}

#[cfg(test)]
mod tools_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn build_gemini_tools_empty_returns_no_bundle() {
        let out = build_gemini_tools(&[]);
        assert!(out.is_empty(), "empty defs should produce no tools field");
    }

    #[test]
    fn build_gemini_tools_packs_all_into_one_bundle() {
        let defs = vec![
            ToolDef {
                name: "roost_read".into(),
                description: "Read notes".into(),
                input_schema: json!({"type":"object","required":["paths"]}),
            },
            ToolDef {
                name: "roost_search".into(),
                description: "Search roost".into(),
                input_schema: json!({"type":"object","required":["query"]}),
            },
        ];
        let out = build_gemini_tools(&defs);
        assert_eq!(out.len(), 1, "all decls land in one bundle");
        assert_eq!(out[0].function_declarations.len(), 2);
        assert_eq!(out[0].function_declarations[0].name, "roost_read");
        assert_eq!(out[0].function_declarations[1].name, "roost_search");
    }

    #[test]
    fn build_gemini_tools_serializes_with_camel_case_field_name() {
        // Regression: Gemini API rejects snake_case `function_declarations`.
        // The field MUST serialize as `functionDeclarations`.
        let defs = vec![ToolDef {
            name: "x".into(),
            description: "y".into(),
            input_schema: json!({}),
        }];
        let out = build_gemini_tools(&defs);
        let ser = serde_json::to_string(&out).unwrap();
        assert!(
            ser.contains("\"functionDeclarations\""),
            "expected camelCase, got {ser}"
        );
        assert!(!ser.contains("function_declarations"));
    }

    #[test]
    fn user_message_renders_as_text_part_with_user_role() {
        let msgs = vec![ChatMessage::user("hi")];
        let out = build_gemini_contents(&msgs);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].role, "user");
        assert_eq!(out[0].parts.len(), 1);
        assert_eq!(out[0].parts[0].text.as_deref(), Some("hi"));
        assert!(out[0].parts[0].function_call.is_none());
        assert!(out[0].parts[0].function_response.is_none());
    }

    #[test]
    fn model_with_tool_calls_emits_text_then_function_call_parts() {
        let call = ProtoToolCall {
            id: "gemini_call_0".into(),
            name: "roost_search".into(),
            args: json!({"query":"fitness"}),
            provider_metadata: None,
        };
        let msgs = vec![ChatMessage::model_with_tools("Let me check.", vec![call])];
        let out = build_gemini_contents(&msgs);
        assert_eq!(out[0].role, "model");
        assert_eq!(out[0].parts.len(), 2, "text + function_call");
        assert_eq!(out[0].parts[0].text.as_deref(), Some("Let me check."));
        let fc = out[0].parts[1].function_call.as_ref().expect("call part");
        assert_eq!(fc.name, "roost_search");
        assert_eq!(fc.args, json!({"query":"fitness"}));
        // No captured signature → migration dummy gets injected on the
        // first (and only) call so Gemini 3.x's strict validation
        // accepts the replayed history. See the dedicated migration
        // tests below for full coverage.
        assert_eq!(
            out[0].parts[1].thought_signature.as_deref(),
            Some(MIGRATION_DUMMY_SIGNATURE)
        );
    }

    #[test]
    fn model_with_only_tool_calls_omits_text_part() {
        // When the assistant skips prose and jumps straight to tools,
        // we don't emit an empty text part — Gemini would accept it
        // but it's noise.
        let call = ProtoToolCall {
            id: "id".into(),
            name: "t".into(),
            args: json!({}),
            provider_metadata: None,
        };
        let msgs = vec![ChatMessage::model_with_tools("", vec![call])];
        let out = build_gemini_contents(&msgs);
        assert_eq!(out[0].parts.len(), 1);
        assert!(out[0].parts[0].function_call.is_some());
    }

    #[test]
    fn thought_signature_round_trips_through_provider_metadata() {
        // Regression: thinking-Gemini (3.x / 2.5-series) requires the
        // `thoughtSignature` blob to come back attached to the same
        // functionCall part on the next request. Drop it and the API
        // returns 400 INVALID_ARGUMENT after the first tool call. This
        // test pins the storage convention used by the streaming
        // handler at gemini.rs:~900 — `provider_metadata` is a JSON
        // object with the signature under `gemini_thought_signature`.
        let call = ProtoToolCall {
            id: "gemini_call_0".into(),
            name: "roost_search".into(),
            args: json!({"query":"fitness"}),
            provider_metadata: Some(json!({
                "gemini_thought_signature": "abc-base64-blob"
            })),
        };
        let msgs = vec![ChatMessage::model_with_tools("Looking it up.", vec![call])];
        let out = build_gemini_contents(&msgs);
        // text part + function_call part (signature lives on the part)
        assert_eq!(out[0].parts.len(), 2);
        let fc_part = &out[0].parts[1];
        assert!(fc_part.function_call.is_some());
        assert_eq!(
            fc_part.thought_signature.as_deref(),
            Some("abc-base64-blob"),
            "thoughtSignature must be re-emitted as a sibling of functionCall"
        );
    }

    #[test]
    fn thought_signature_missing_metadata_falls_back_to_migration_dummy() {
        // Migration path: a Model turn with tool_calls but no captured
        // signatures (pre-2.A persisted chat OR non-thinking model
        // history) gets the dummy signature on the FIRST call so
        // Gemini 3.x's strict validation accepts it instead of
        // 400-ing. Per ai.google.dev/gemini-api/docs/thought-signatures.
        let call = ProtoToolCall {
            id: "id".into(),
            name: "t".into(),
            args: json!({}),
            provider_metadata: Some(json!({ "unrelated_key": 42 })),
        };
        let msgs = vec![ChatMessage::model_with_tools("", vec![call])];
        let out = build_gemini_contents(&msgs);
        assert_eq!(
            out[0].parts[0].thought_signature.as_deref(),
            Some(MIGRATION_DUMMY_SIGNATURE),
            "first call without captured signature must get the dummy"
        );
    }

    #[test]
    fn migration_dummy_only_on_first_parallel_call() {
        // Per Gemini's parallel-call spec, only the first call needs a
        // signature; subsequent ones omit it. The migration fallback
        // must follow the same rule — emit the dummy ONLY on call[0],
        // leave the rest with no signature.
        let make = |id: &str| ProtoToolCall {
            id: id.into(),
            name: "t".into(),
            args: json!({}),
            provider_metadata: None,
        };
        let msgs = vec![ChatMessage::model_with_tools(
            "",
            vec![make("c0"), make("c1"), make("c2")],
        )];
        let out = build_gemini_contents(&msgs);
        assert_eq!(out[0].parts.len(), 3);
        assert_eq!(
            out[0].parts[0].thought_signature.as_deref(),
            Some(MIGRATION_DUMMY_SIGNATURE)
        );
        assert!(out[0].parts[1].thought_signature.is_none());
        assert!(out[0].parts[2].thought_signature.is_none());
    }

    #[test]
    fn real_signature_anywhere_disables_migration_dummy() {
        // Mixed case: parallel calls where call[0] HAS a real signature
        // (Gemini parallel-call convention) and call[1] doesn't. The
        // dummy must NOT clobber the real signature — has_any_signature
        // gates it off entirely.
        let call0 = ProtoToolCall {
            id: "c0".into(),
            name: "t".into(),
            args: json!({}),
            provider_metadata: Some(json!({
                "gemini_thought_signature": "REAL_SIG"
            })),
        };
        let call1 = ProtoToolCall {
            id: "c1".into(),
            name: "t".into(),
            args: json!({}),
            provider_metadata: None,
        };
        let msgs = vec![ChatMessage::model_with_tools("", vec![call0, call1])];
        let out = build_gemini_contents(&msgs);
        assert_eq!(
            out[0].parts[0].thought_signature.as_deref(),
            Some("REAL_SIG")
        );
        assert!(out[0].parts[1].thought_signature.is_none());
    }

    #[test]
    fn tool_response_uses_function_role_and_parses_json_when_possible() {
        let msg = ChatMessage::tool_response(
            "gemini_call_0",
            "roost_search",
            r#"{"hits":[{"path":"a"}]}"#,
        );
        let out = build_gemini_contents(&[msg]);
        assert_eq!(out[0].role, "function");
        let fr = out[0].parts[0]
            .function_response
            .as_ref()
            .expect("function_response");
        assert_eq!(fr.name, "roost_search");
        assert_eq!(fr.response, json!({"hits":[{"path":"a"}]}));
    }

    #[test]
    fn tool_response_wraps_plain_text_under_text_field() {
        // Some tool results are plain text, not JSON. The Gemini API
        // requires `response` to be a JSON object, so we wrap.
        let msg = ChatMessage::tool_response("id", "fn", "just a string");
        let out = build_gemini_contents(&[msg]);
        let fr = out[0].parts[0].function_response.as_ref().unwrap();
        assert_eq!(fr.response, json!({"text":"just a string"}));
    }

    #[test]
    fn parse_chunk_with_text_part() {
        let chunk = r#"{"candidates":[{"content":{"parts":[{"text":"hello"}]}}]}"#;
        let parsed: GeminiToolsStreamResponse = serde_json::from_str(chunk).unwrap();
        let candidates = parsed.candidates.unwrap();
        let part = &candidates[0]
            .content
            .as_ref()
            .unwrap()
            .parts
            .as_ref()
            .unwrap()[0];
        assert_eq!(part.text.as_deref(), Some("hello"));
        assert!(part.function_call.is_none());
    }

    #[test]
    fn parse_chunk_with_function_call_part() {
        let chunk = r#"{"candidates":[{"content":{"parts":[{"functionCall":{"name":"roost_read","args":{"paths":["x"]}}}]},"finishReason":"STOP"}]}"#;
        let parsed: GeminiToolsStreamResponse = serde_json::from_str(chunk).unwrap();
        let candidates = parsed.candidates.unwrap();
        let candidate = &candidates[0];
        assert_eq!(candidate.finish_reason.as_deref(), Some("STOP"));
        let part = &candidate.content.as_ref().unwrap().parts.as_ref().unwrap()[0];
        assert!(part.text.is_none());
        let fc = part.function_call.as_ref().unwrap();
        assert_eq!(fc.name, "roost_read");
        assert_eq!(fc.args, json!({"paths":["x"]}));
    }

    #[test]
    fn parse_keepalive_chunk_is_ignored() {
        // Gemini occasionally sends `{}` between real chunks. The
        // parser must not crash; the streaming loop's `continue` on
        // None-candidates handles it.
        let chunk = r#"{}"#;
        let parsed: GeminiToolsStreamResponse = serde_json::from_str(chunk).unwrap();
        assert!(parsed.candidates.is_none());
        assert!(parsed.error.is_none());
    }

    #[test]
    fn parse_error_chunk_surfaces_message() {
        let chunk = r#"{"error":{"message":"quota exceeded"}}"#;
        let parsed: GeminiToolsStreamResponse = serde_json::from_str(chunk).unwrap();
        assert_eq!(parsed.error.unwrap().message, "quota exceeded");
    }

    #[test]
    fn next_call_id_is_unique_and_monotonic() {
        let a = next_gemini_call_id();
        let b = next_gemini_call_id();
        assert_ne!(a, b);
        assert!(a.starts_with("gemini_call_"));
        assert!(b.starts_with("gemini_call_"));
    }
}
