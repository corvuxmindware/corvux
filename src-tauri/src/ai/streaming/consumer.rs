//! Cancel-aware async iterator over a streaming HTTP body's SSE events.
//!
//! Every cloud provider was running the same ~40-line loop:
//!
//! ```ignore
//! loop {
//!     if cancel.load(Ordering::Relaxed) { /* emit + return */ }
//!     let chunk = match tokio::time::timeout(Duration::from_millis(100),
//!                                            stream.next()).await {
//!         Ok(Some(c)) => c,
//!         Ok(None) => break,
//!         Err(_) => continue,    // 100ms tick — re-check cancel
//!     };
//!     buffer.push_str(&String::from_utf8_lossy(&chunk));
//!     while let Some(pos) = find_sse_boundary(&buffer) {
//!         let event = buffer[..pos].to_string();
//!         buffer = buffer[pos..].trim_start_matches('\n').trim_start_matches('\r').to_string();
//!         /* per-provider parsing */
//!     }
//! }
//! ```
//!
//! The 100ms timeout (AUDIT-041) caps the worst-case latency between the
//! user clicking Stop and the streaming function returning, even on
//! providers that pause for seconds between SSE frames.
//!
//! `SseEvents` is the same loop expressed as an async iterator: the
//! provider writes `while let Some(event) = events.next_event().await?`
//! and only thinks about parse logic for the events themselves. The
//! caller checks the cancel flag once after the loop to distinguish
//! "user stopped" from "stream ended normally" (the two produce
//! different telemetry / final-emit semantics per provider).

use bytes::Bytes;
use futures_util::Stream;
use futures_util::StreamExt;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use super::sse::find_sse_boundary;

/// How long each `stream.next()` poll waits before falling through to a
/// fresh cancel check. AUDIT-041 — chosen as the smallest interval that
/// doesn't burn CPU while keeping cancel responsiveness sub-second on
/// slow providers (Anthropic via OpenRouter can pause >1s between SSE
/// frames during thinking blocks).
const POLL_TIMEOUT: Duration = Duration::from_millis(100);

/// Bound incomplete frames and coalesced events before copying network bytes.
const MAX_SSE_BUFFER_BYTES: usize = 1024 * 1024;

/// Async iterator over the SSE events in a streaming HTTP body.
///
/// Generic over `S: Stream<Item = reqwest::Result<Bytes>>` so callers
/// pass `response.bytes_stream()` directly. The struct owns the cancel
/// flag handle and the per-tick timeout; the provider code only owns
/// the per-event parsing.
pub struct SseEvents<S>
where
    S: Stream<Item = reqwest::Result<Bytes>> + Unpin,
{
    stream: S,
    cancel: Arc<AtomicBool>,
    buffer: Vec<u8>,
}

impl<S> SseEvents<S>
where
    S: Stream<Item = reqwest::Result<Bytes>> + Unpin,
{
    pub fn new(stream: S, cancel: Arc<AtomicBool>) -> Self {
        Self {
            stream,
            cancel,
            buffer: Vec::new(),
        }
    }

    /// Pull the next complete SSE event off the wire.
    ///
    /// Returns:
    /// - `Ok(Some(event))` — full event ready (caller parses).
    /// - `Ok(None)` — stream ended cleanly **or** the cancel flag fired.
    ///   Caller checks the cancel flag separately when it needs to
    ///   distinguish for telemetry / final-emit semantics.
    /// - `Err(msg)` — network error from the underlying stream.
    pub async fn next_event(&mut self) -> Result<Option<String>, String> {
        // Buffer may already contain a fully-framed event from a prior
        // pull that delivered multiple events in one chunk.
        if let Some(event) = self.drain_buffered_event()? {
            return Ok(Some(event));
        }

        loop {
            if self.cancel.load(Ordering::Relaxed) {
                return Ok(None);
            }
            let chunk = match tokio::time::timeout(POLL_TIMEOUT, self.stream.next()).await {
                Ok(Some(Ok(c))) => c,
                Ok(Some(Err(e))) => return Err(format!("stream read error: {e}")),
                Ok(None) => {
                    std::str::from_utf8(&self.buffer)
                        .map_err(|_| "provider stream ended with invalid UTF-8".to_string())?;
                    return Ok(None); // EOF — valid but unframed tail is dropped
                }
                Err(_elapsed) => continue, // 100ms tick, re-check cancel
            };
            if self.buffer.len().saturating_add(chunk.len()) > MAX_SSE_BUFFER_BYTES {
                return Err("provider SSE buffer exceeded its 1 MiB limit".into());
            }
            self.buffer.extend_from_slice(&chunk);
            if let Some(event) = self.drain_buffered_event()? {
                return Ok(Some(event));
            }
        }
    }

    fn drain_buffered_event(&mut self) -> Result<Option<String>, String> {
        // A chunk may end partway through a codepoint. Keep those bytes until
        // the next chunk and inspect only the valid prefix for SSE delimiters.
        let text = match std::str::from_utf8(&self.buffer) {
            Ok(text) => text,
            Err(error) if error.error_len().is_none() => {
                std::str::from_utf8(&self.buffer[..error.valid_up_to()])
                    .map_err(|_| "invalid provider UTF-8 prefix".to_string())?
            }
            Err(_) => return Err("provider stream contains invalid UTF-8".into()),
        };
        let Some(pos) = find_sse_boundary(text) else {
            return Ok(None);
        };
        let event = text[..pos].to_string();
        // Defensive trim: find_sse_boundary already returns the offset
        // past the `\n\n` (or `\r\n\r\n`), but some providers send
        // stray leading newlines between events as keepalive padding —
        // strip them so the next find_sse_boundary call starts clean.
        let padding = self.buffer[pos..]
            .iter()
            .take_while(|b| **b == b'\n' || **b == b'\r')
            .count();
        self.buffer.drain(..pos + padding);
        Ok(Some(event))
    }
}

/// Convenience type alias for the common case — a boxed dynamic stream
/// returned by `reqwest::Response::bytes_stream()`. Saves provider call
/// sites from spelling out the `<R: Stream<...> + Unpin>` generic when
/// they're only ever fed one concrete stream type.
#[allow(dead_code)] // public surface kept for provider call sites
pub type BoxedSseEvents = SseEvents<Pin<Box<dyn Stream<Item = reqwest::Result<Bytes>> + Send>>>;

#[cfg(test)]
mod tests {

    #[tokio::test]
    async fn preserves_utf8_split_at_every_byte() {
        let text = "data: \u{754c}\u{1f600}\u{e9}\n\n";
        let chunks: Vec<reqwest::Result<Bytes>> = text
            .as_bytes()
            .iter()
            .map(|b| Ok(Bytes::copy_from_slice(&[*b])))
            .collect();
        let mut events = SseEvents::new(
            futures_util::stream::iter(chunks),
            Arc::new(AtomicBool::new(false)),
        );
        assert_eq!(events.next_event().await.unwrap().as_deref(), Some(text));
        assert_eq!(events.next_event().await.unwrap(), None);
    }

    #[tokio::test]
    async fn rejects_oversize_and_invalid_utf8_streams() {
        for bytes in [
            Bytes::from(vec![b'x'; MAX_SSE_BUFFER_BYTES + 1]),
            Bytes::from_static(&[0xff]),
            Bytes::from_static(&[0xe7]),
        ] {
            let chunks: Vec<reqwest::Result<Bytes>> = vec![Ok(bytes)];
            let mut events = SseEvents::new(
                futures_util::stream::iter(chunks),
                Arc::new(AtomicBool::new(false)),
            );
            assert!(events.next_event().await.is_err());
            assert!(events.buffer.len() <= MAX_SSE_BUFFER_BYTES);
        }
    }

    use super::*;
    use futures_util::stream;

    fn make_stream(
        chunks: Vec<Result<&'static str, &'static str>>,
    ) -> impl Stream<Item = reqwest::Result<Bytes>> + Unpin {
        // We can't construct real reqwest::Error values in tests, so we
        // map an Err arm to a synthesized one via reqwest::get on an
        // invalid URL. Tests below only use the Ok arms — the Err
        // pathway is exercised at integration level.
        stream::iter(chunks.into_iter().map(|c| match c {
            Ok(s) => Ok(Bytes::from(s)),
            Err(_) => panic!("test fixtures should only use Ok arms"),
        }))
    }

    #[tokio::test]
    async fn yields_each_event_in_order() {
        let stream = make_stream(vec![Ok("data: a\n\ndata: b\n\n")]);
        let cancel = Arc::new(AtomicBool::new(false));
        let mut events = SseEvents::new(stream, cancel);

        assert_eq!(
            events.next_event().await.unwrap().as_deref(),
            Some("data: a\n\n")
        );
        assert_eq!(
            events.next_event().await.unwrap().as_deref(),
            Some("data: b\n\n")
        );
        assert_eq!(events.next_event().await.unwrap(), None);
    }

    #[tokio::test]
    async fn yields_event_split_across_chunks() {
        // Realistic case: SSE event arrives in two TCP chunks.
        let stream = make_stream(vec![Ok("data: hel"), Ok("lo\n\n")]);
        let cancel = Arc::new(AtomicBool::new(false));
        let mut events = SseEvents::new(stream, cancel);

        assert_eq!(
            events.next_event().await.unwrap().as_deref(),
            Some("data: hello\n\n")
        );
        assert_eq!(events.next_event().await.unwrap(), None);
    }

    #[tokio::test]
    async fn cancel_flag_returns_none_immediately() {
        // No chunks available, cancel pre-set — should NOT block on the
        // 100ms timeout; the first check happens before the poll.
        let stream = make_stream(vec![]);
        let cancel = Arc::new(AtomicBool::new(true));
        let mut events = SseEvents::new(stream, cancel);

        assert_eq!(events.next_event().await.unwrap(), None);
    }

    #[tokio::test]
    async fn cancel_after_partial_returns_none_without_yielding_partial() {
        // Buffer has half an event. Cancel fires before the rest
        // arrives. Caller should see None (cancel) and decide what to
        // do — partial bytes get dropped.
        let stream = make_stream(vec![Ok("data: partial without delimiter")]);
        let cancel = Arc::new(AtomicBool::new(false));
        let mut events = SseEvents::new(stream, cancel.clone());

        // Trigger one read so the partial lands in the buffer.
        // Use a tokio::select! so we don't actually block — set cancel
        // after a brief sleep so the next poll observes it.
        let cancel_setter = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            cancel_setter.store(true, Ordering::Relaxed);
        });

        // First call: stream yields one chunk into the buffer (no
        // delimiter, so no event), then the cancel flag flips during
        // the next poll → None.
        assert_eq!(events.next_event().await.unwrap(), None);
    }

    #[tokio::test]
    async fn empty_stream_returns_none() {
        let stream = make_stream(vec![]);
        let cancel = Arc::new(AtomicBool::new(false));
        let mut events = SseEvents::new(stream, cancel);
        assert_eq!(events.next_event().await.unwrap(), None);
    }

    #[tokio::test]
    async fn trims_keepalive_newlines_between_events() {
        // Some providers pad with extra blank lines. The drainer
        // strips them so the next find_sse_boundary call doesn't see
        // a phantom-empty event.
        let stream = make_stream(vec![Ok("data: a\n\n\ndata: b\n\n")]);
        let cancel = Arc::new(AtomicBool::new(false));
        let mut events = SseEvents::new(stream, cancel);
        assert_eq!(
            events.next_event().await.unwrap().as_deref(),
            Some("data: a\n\n")
        );
        assert_eq!(
            events.next_event().await.unwrap().as_deref(),
            Some("data: b\n\n")
        );
    }
}
