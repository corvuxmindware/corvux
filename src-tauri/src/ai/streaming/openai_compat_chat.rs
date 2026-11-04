//! Shared streaming engine for OpenAI-compat chat-with-tools providers.
//!
//! Before this module, every OpenAI-compat provider (`openrouter`, `ollama`,
//! and soon `openai_direct` / `anthropic_compat`) shipped its own copy of:
//!
//! 1. Spawn a tokio task that owns the response stream.
//! 2. Drain SSE events via `SseEvents`.
//! 3. Per `data:` line: parse `ToolsStreamChunk`, surface errors, route
//!    text fragments to both the user-facing `ai:chunk` channel and the
//!    agent loop's `StreamEvent::TextChunk`.
//! 4. Accumulate tool-call deltas per `index` into a `BTreeMap<usize,
//!    PartialCall>`. Args parse only after `finish_reason` arrives
//!    (Anthropic-via-OpenRouter interleaves metadata mid-stream).
//! 5. Flush: synthesize call ids for providers that omit them, emit
//!    `StreamEvent::ToolCall` per accumulated call, then `StreamEvent::Stop`
//!    with the right `StopReason`.
//!
//! That whole block was ~130 LoC, byte-identical across openrouter + ollama
//! after Steps 1–4 of AUDIT-057. Each provider now just builds the request
//! (auth, URL, attribution headers, max_tokens) and hands the response to
//! `spawn_openai_compat_chat`. Adding a fourth provider is now ~30 LoC of
//! auth + URL plumbing instead of another ~200 LoC of copy-paste.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use tauri::AppHandle;
use tauri_specta::Event;

use crate::events;

use super::consumer::SseEvents;
use crate::ai::tool_protocol::{StopReason, StreamEvent, ToolCall as ProtoToolCall};
use crate::ai::tool_translator::openai_compat::{PartialCall, ToolsStreamChunk};

/// Channel capacity for `StreamEvent` delivery. Small enough to
/// backpressure a runaway stream, large enough that normal token-rate
/// doesn't block.
const CHANNEL_CAPACITY: usize = 16;

/// Drain an OpenAI-compat streaming chat response in a spawned task and
/// return the receiver end of a `StreamEvent` channel.
///
/// The caller is responsible for:
/// - Building the request (URL, auth headers, body via `ChatToolsRequest`).
/// - Sending it and validating `response.status().is_success()`.
/// - Mapping HTTP-level errors with a provider-specific label *before*
///   calling this — once we own the response, every downstream error
///   path is uniform (`StreamEvent::Stop(StopReason::Other(...))`).
///
/// Spawned task always emits a terminal `StreamEvent::Stop` before the
/// channel closes, so consumers can rely on receiving exactly one stop
/// event per stream (cancel, EOF, error — all paths covered).
pub fn spawn_openai_compat_chat<R: tauri::Runtime + 'static>(
    response: reqwest::Response,
    app: AppHandle<R>,
    cancel: Arc<AtomicBool>,
) -> tokio::sync::mpsc::Receiver<StreamEvent> {
    let (tx, rx) = tokio::sync::mpsc::channel::<StreamEvent>(CHANNEL_CAPACITY);

    tokio::spawn(async move {
        let mut events = SseEvents::new(response.bytes_stream(), cancel.clone());
        // index → accumulated PartialCall. BTreeMap so we iterate
        // calls in index order on flush, matching the model's intent.
        let mut partials: std::collections::BTreeMap<usize, PartialCall> =
            std::collections::BTreeMap::new();
        let mut finish_reason: Option<String> = None;

        'outer: loop {
            let event = match events.next_event().await {
                Ok(Some(e)) => e,
                Ok(None) => break, // cancel OR stream end — both fall through to flush
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
                if data.is_empty() || data == "[DONE]" {
                    continue;
                }

                let parsed: ToolsStreamChunk = match serde_json::from_str(data) {
                    Ok(p) => p,
                    Err(_) => continue, // keep-alive / partial / unknown shape
                };

                if let Some(err) = parsed.error {
                    let _ = events::AiError(err.message.clone()).emit(&app);
                    let _ = tx
                        .send(StreamEvent::Stop(StopReason::Other(err.message)))
                        .await;
                    return;
                }

                let Some(choice) = parsed.choices.into_iter().next() else {
                    continue;
                };
                if let Some(reason) = choice.finish_reason {
                    finish_reason = Some(reason);
                }
                let Some(delta) = choice.delta else {
                    continue;
                };

                // Text fragment — forward to the user-facing
                // panel AND the agent loop's history accumulator.
                if let Some(content) = delta.content {
                    if !content.is_empty() {
                        let _ = events::AiChunk(content.clone()).emit(&app);
                        if tx.send(StreamEvent::TextChunk(content)).await.is_err() {
                            break 'outer;
                        }
                    }
                }

                // Tool-call deltas: accumulate per-index. Names and
                // ids arrive on the FIRST delta for that index; later
                // deltas only carry argument fragments to concatenate.
                for call_delta in delta.tool_calls {
                    let entry = partials.entry(call_delta.index).or_default();
                    if let Some(id) = call_delta.id {
                        if entry.id.is_empty() {
                            entry.id = id;
                        }
                    }
                    if let Some(func) = call_delta.function {
                        if let Some(name) = func.name {
                            if entry.name.is_empty() {
                                entry.name = name;
                            }
                        }
                        if let Some(args_chunk) = func.arguments {
                            entry.arguments.push_str(&args_chunk);
                        }
                    }
                }
            }
        }

        // Flush: at end-of-stream, parse each accumulated PartialCall
        // and emit ToolCall events. We parse here (not mid-stream)
        // because Anthropic-via-OpenRouter sometimes interleaves
        // metadata fragments that would break a partial parse.
        let emit_tools =
            matches!(finish_reason.as_deref(), Some("tool_calls")) || !partials.is_empty();
        if emit_tools {
            for (idx, partial) in partials.into_iter() {
                // Empty-string arguments → empty object. Some models
                // emit `tool_calls` with no arguments field for
                // no-arg tools (e.g. `skill_list` with `{}`).
                let args_str = if partial.arguments.is_empty() {
                    "{}".to_string()
                } else {
                    partial.arguments
                };
                let args: serde_json::Value =
                    serde_json::from_str(&args_str).unwrap_or_else(|_| {
                        // Args didn't parse. Surface a marker arg so the
                        // agent loop's schema validator fails with a clear
                        // error instead of dispatching a partial call.
                        // Includes the raw string for debugging.
                        serde_json::json!({ "_invalid_args_raw": args_str })
                    });
                let id = if partial.id.is_empty() {
                    // Some Llama-3 variants on OpenRouter (and assorted
                    // Ollama-shim models) omit the call id. Synthesize
                    // one so the round-trip works.
                    format!("call_{idx}")
                } else {
                    partial.id
                };
                let call = ProtoToolCall {
                    id,
                    name: partial.name,
                    args,
                    // OpenAI-shape tool_calls have no per-call
                    // round-trip blob — the `id` carries everything
                    // the next request needs.
                    provider_metadata: None,
                };
                if tx.send(StreamEvent::ToolCall(call)).await.is_err() {
                    return;
                }
            }
        }

        let stop = match finish_reason.as_deref() {
            Some("tool_calls") => StopReason::ToolUse,
            Some("stop") | None => {
                if emit_tools {
                    StopReason::ToolUse
                } else {
                    StopReason::EndTurn
                }
            }
            Some(other) => StopReason::Other(other.to_string()),
        };
        let _ = tx.send(StreamEvent::Stop(stop)).await;
    });

    rx
}
