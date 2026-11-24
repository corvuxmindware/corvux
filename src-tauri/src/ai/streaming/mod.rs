//! AUDIT-057: shared streaming helpers for the cloud-provider SSE paths.
//!
//! Every cloud provider (Gemini, OpenRouter, Ollama) was previously carrying
//! its own copies of the same SSE-line boundary detector, cancel-poll loop,
//! and finish-reason classification. The duplication was small per file but
//! grew with every new provider — by the time Anthropic Claude lands as a
//! direct provider, four copies is one too many.
//!
//! What lives here is **provider-agnostic**: the shape of an SSE stream
//! (`event:` line, `data:` payload, blank-line delimiter) is HTTP/RFC and
//! identical across every vendor that ships streaming chat.
//!
//! What does NOT live here: anything that knows about JSON shapes (delta
//! arrays, tool-call envelopes, finish-reason strings). Those vary per
//! provider and stay in the provider files.

pub mod consumer;
pub mod openai_compat_chat;
pub mod sse;

// ── Bounds on ONE streamed reply (review AT-6, 2026-11-12) ──────────────
//
// A provider (or anything that can answer in its place) controls how long a
// stream runs and how much it sends. Before AT-6 only a single incomplete SSE
// frame was bounded (1 MiB); the text and tool-call data a reply ADDS UP to were
// not. The limits below are deliberately far above anything a real answer
// reaches (the largest `max_output_tokens` we send is 10 000 tokens, about
// 40 KB) so they never trip in normal use; they exist so a runaway or hostile
// stream ends with a clear message instead of growing memory without bound.
// Each is enforced where the data is HELD: the wire bytes in `consumer.rs`
// (covers every provider at once), the legacy text buffer in each provider's
// `stream_chat`, the tool-call accumulators, and the agent loop's own buffers.

const MIB: usize = 1024 * 1024;

/// Total assistant text in one reply.
pub const MAX_STREAMED_TEXT_BYTES: usize = 8 * MIB;

/// Total tool-call argument data in one reply (all calls together).
pub const MAX_STREAMED_TOOL_ARG_BYTES: usize = 4 * MIB;

/// Distinct tool calls in one reply. The loop runs at most
/// `max_tool_calls_per_iter` (8) of them; more than this is a runaway.
pub const MAX_STREAMED_TOOL_CALLS: usize = 64;

/// User-visible reason a stream was stopped for being too big. Plain words, no
/// provider names, safe to show in the chat panel.
pub fn limit_error(what: &str, cap: usize) -> String {
    format!(
        "The AI provider's reply was stopped: it sent more than {} MB of {what}, \
         far more than a normal answer.",
        cap / MIB
    )
}

/// Append `piece` to `buf`, refusing (and leaving `buf` untouched) when the
/// result would pass `cap` bytes.
pub fn push_capped(buf: &mut String, piece: &str, cap: usize, what: &str) -> Result<(), String> {
    if buf.len().saturating_add(piece.len()) > cap {
        return Err(limit_error(what, cap));
    }
    buf.push_str(piece);
    Ok(())
}

/// Running byte total for data that is forwarded rather than stored (so there is
/// no buffer for `push_capped` to measure).
#[derive(Debug, Default)]
pub struct ByteCounter {
    used: usize,
}

impl ByteCounter {
    /// Count `n` more bytes against `cap`. Past the cap this returns the
    /// user-visible reason and leaves the total unchanged.
    pub fn add(&mut self, n: usize, cap: usize, what: &str) -> Result<(), String> {
        let next = self.used.saturating_add(n);
        if next > cap {
            return Err(limit_error(what, cap));
        }
        self.used = next;
        Ok(())
    }
}

/// User-visible reason a reply was stopped for asking for too many tool calls.
pub fn too_many_tool_calls_error() -> String {
    format!(
        "The AI provider's reply was stopped: it asked for more than {MAX_STREAMED_TOOL_CALLS} \
         tool calls at once, far more than a normal answer."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_capped_appends_up_to_the_cap_and_refuses_past_it() {
        let mut buf = String::new();
        push_capped(&mut buf, "abcd", 6, "text").unwrap();
        push_capped(&mut buf, "ef", 6, "text").unwrap();
        assert_eq!(buf, "abcdef");
        let err = push_capped(&mut buf, "g", 6, "text").unwrap_err();
        assert_eq!(buf, "abcdef", "a refused piece must not be appended");
        assert!(err.contains("text") && err.contains("stopped"), "{err}");
    }

    #[test]
    fn byte_counter_refuses_the_piece_that_crosses_the_cap_and_keeps_its_total() {
        let mut c = ByteCounter::default();
        c.add(5, 8, "text").unwrap();
        c.add(3, 8, "text").unwrap();
        assert!(c.add(1, 8, "text").is_err());
        // A refused add changes nothing, so a smaller one that fits still fails
        // only because the total is already at the cap.
        assert!(c.add(0, 8, "text").is_ok());
        assert_eq!(c.used, 8);
    }

    #[test]
    fn the_tool_call_message_names_the_limit() {
        assert!(too_many_tool_calls_error().contains("64"));
    }

    #[test]
    fn the_message_names_the_limit_in_megabytes_and_no_internals() {
        let msg = limit_error("tool-call data", MAX_STREAMED_TOOL_ARG_BYTES);
        assert!(msg.contains("4 MB"), "{msg}");
        assert!(msg.contains("tool-call data"), "{msg}");
        assert!(!msg.contains("::") && !msg.contains(".rs"), "{msg}");
    }

    // 10 000 output tokens is about 40 KB; the caps must leave headroom of two
    // orders of magnitude so they cannot trip on honest use, and stay finite.
    // Compile-time checks, so editing a cap past either bound fails the build.
    const _: () = assert!(MAX_STREAMED_TEXT_BYTES >= 100 * 40 * 1024);
    const _: () = assert!(MAX_STREAMED_TOOL_ARG_BYTES >= 100 * 40 * 1024);
    const _: () = assert!(MAX_STREAMED_TOOL_CALLS > 8);
    const _: () = assert!(MAX_STREAMED_TEXT_BYTES < 1024 * MIB);
}
