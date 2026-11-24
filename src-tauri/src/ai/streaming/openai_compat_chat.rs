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
use super::{
    limit_error, too_many_tool_calls_error, ByteCounter, MAX_STREAMED_TEXT_BYTES,
    MAX_STREAMED_TOOL_ARG_BYTES, MAX_STREAMED_TOOL_CALLS,
};
use crate::ai::tool_protocol::{StopReason, StreamEvent, ToolCall as ProtoToolCall};
use crate::ai::tool_translator::openai_compat::{PartialCall, ToolsDeltaCall, ToolsStreamChunk};

/// Channel capacity for `StreamEvent` delivery. Small enough to
/// backpressure a runaway stream, large enough that normal token-rate
/// doesn't block.
const CHANNEL_CAPACITY: usize = 16;

/// Per-`index` accumulator for OpenAI-style tool-call deltas, with a hard bound
/// (review AT-6). The arguments of every call are held until the stream ends, so
/// without a cap a stream that never stops sending `arguments` fragments, or
/// sends a fresh `index` with every delta, grows this without limit.
#[derive(Default)]
struct ToolCallAccumulator {
    /// index → accumulated call. BTreeMap so a flush iterates in index order,
    /// matching the model's intent.
    partials: std::collections::BTreeMap<usize, PartialCall>,
    /// Bytes of id + name + argument fragments received so far, all calls.
    bytes: usize,
}

impl ToolCallAccumulator {
    /// Fold one delta in. `Err` carries the user-visible reason the reply must
    /// be stopped (too many calls, or too much argument data).
    fn apply(&mut self, delta: ToolsDeltaCall) -> Result<(), String> {
        if !self.partials.contains_key(&delta.index)
            && self.partials.len() >= MAX_STREAMED_TOOL_CALLS
        {
            return Err(too_many_tool_calls_error());
        }
        let incoming = delta.id.as_ref().map_or(0, String::len)
            + delta.function.as_ref().map_or(0, |f| {
                f.name.as_ref().map_or(0, String::len) + f.arguments.as_ref().map_or(0, String::len)
            });
        if self.bytes.saturating_add(incoming) > MAX_STREAMED_TOOL_ARG_BYTES {
            return Err(limit_error("tool-call data", MAX_STREAMED_TOOL_ARG_BYTES));
        }
        self.bytes += incoming;

        // Names and ids arrive on the FIRST delta for that index; later deltas
        // only carry argument fragments to concatenate.
        let entry = self.partials.entry(delta.index).or_default();
        if let Some(id) = delta.id {
            if entry.id.is_empty() {
                entry.id = id;
            }
        }
        if let Some(func) = delta.function {
            if let Some(name) = func.name {
                if entry.name.is_empty() {
                    entry.name = name;
                }
            }
            if let Some(args_chunk) = func.arguments {
                entry.arguments.push_str(&args_chunk);
            }
        }
        Ok(())
    }

    fn is_empty(&self) -> bool {
        self.partials.is_empty()
    }

    fn into_partials(self) -> std::collections::BTreeMap<usize, PartialCall> {
        self.partials
    }
}

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
        // Tool-call deltas accumulate per index and are parsed only on flush.
        let mut accumulator = ToolCallAccumulator::default();
        // The text forwarded so far — counted here because this task only
        // forwards it, so nothing else would notice a runaway.
        let mut text_bytes = ByteCounter::default();
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
                        if let Err(msg) =
                            text_bytes.add(content.len(), MAX_STREAMED_TEXT_BYTES, "text")
                        {
                            let _ = tx.send(StreamEvent::Stop(StopReason::Other(msg))).await;
                            return;
                        }
                        let _ = events::AiChunk(content.clone()).emit(&app);
                        if tx.send(StreamEvent::TextChunk(content)).await.is_err() {
                            break 'outer;
                        }
                    }
                }

                // Tool-call deltas: accumulate per-index (bounded — a reply
                // that runs past the caps is stopped, never held).
                for call_delta in delta.tool_calls {
                    if let Err(msg) = accumulator.apply(call_delta) {
                        let _ = tx.send(StreamEvent::Stop(StopReason::Other(msg))).await;
                        return;
                    }
                }
            }
        }

        // Flush: at end-of-stream, parse each accumulated PartialCall
        // and emit ToolCall events. We parse here (not mid-stream)
        // because Anthropic-via-OpenRouter sometimes interleaves
        // metadata fragments that would break a partial parse.
        let emit_tools =
            matches!(finish_reason.as_deref(), Some("tool_calls")) || !accumulator.is_empty();
        if emit_tools {
            for (idx, partial) in accumulator.into_partials() {
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn delta(
        index: usize,
        id: Option<&str>,
        name: Option<&str>,
        args: Option<&str>,
    ) -> ToolsDeltaCall {
        serde_json::from_value(json!({
            "index": index,
            "id": id,
            "function": { "name": name, "arguments": args },
        }))
        .unwrap()
    }

    #[test]
    fn fragments_concatenate_per_index_and_the_first_id_and_name_win() {
        let mut acc = ToolCallAccumulator::default();
        acc.apply(delta(0, Some("c1"), Some("roost_read"), Some("{\"pa")))
            .unwrap();
        acc.apply(delta(1, Some("c2"), Some("roost_search"), Some("{}")))
            .unwrap();
        acc.apply(delta(0, Some("IGNORED"), Some("IGNORED"), Some("th\":1}")))
            .unwrap();
        assert!(!acc.is_empty());
        let partials = acc.into_partials();
        assert_eq!(partials[&0].id, "c1");
        assert_eq!(partials[&0].name, "roost_read");
        assert_eq!(partials[&0].arguments, "{\"path\":1}");
        assert_eq!(partials[&1].arguments, "{}");
    }

    /// AT-6: a stream that opens a new tool call with every delta is stopped at
    /// the call cap instead of growing the map forever.
    #[test]
    fn a_stream_of_ever_new_indexes_is_stopped_at_the_call_cap() {
        let mut acc = ToolCallAccumulator::default();
        for i in 0..MAX_STREAMED_TOOL_CALLS {
            acc.apply(delta(i, None, Some("t"), None)).unwrap();
        }
        let err = acc
            .apply(delta(MAX_STREAMED_TOOL_CALLS, None, Some("t"), None))
            .unwrap_err();
        assert!(err.contains("tool calls"), "{err}");
        // A further fragment for a call that already exists is still fine.
        acc.apply(delta(0, None, None, Some("{}"))).unwrap();
    }

    /// AT-6: one call that never stops sending `arguments` is stopped at the
    /// argument-data cap.
    #[test]
    fn endless_argument_fragments_are_stopped_at_the_data_cap() {
        let mut acc = ToolCallAccumulator::default();
        let mib = "x".repeat(1024 * 1024);
        let calls_that_fit = MAX_STREAMED_TOOL_ARG_BYTES / mib.len();
        for _ in 0..calls_that_fit {
            acc.apply(delta(0, None, None, Some(&mib))).unwrap();
        }
        let err = acc.apply(delta(0, None, None, Some("y"))).unwrap_err();
        assert!(err.contains("tool-call data"), "{err}");
        // The refused fragment was not appended.
        let partials = acc.into_partials();
        assert_eq!(partials[&0].arguments.len(), MAX_STREAMED_TOOL_ARG_BYTES);
        assert!(!partials[&0].arguments.ends_with('y'));
    }
}
