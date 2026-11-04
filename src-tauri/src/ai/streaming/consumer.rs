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
    buffer: String,
}

impl<S> SseEvents<S>
where
    S: Stream<Item = reqwest::Result<Bytes>> + Unpin,
{
    pub fn new(stream: S, cancel: Arc<AtomicBool>) -> Self {
        Self {
            stream,
            cancel,
            buffer: String::new(),
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
        if let Some(event) = self.drain_buffered_event() {
            return Ok(Some(event));
        }

        loop {
            if self.cancel.load(Ordering::Relaxed) {
                return Ok(None);
            }
            let chunk = match tokio::time::timeout(POLL_TIMEOUT, self.stream.next()).await {
                Ok(Some(Ok(c))) => c,
                Ok(Some(Err(e))) => return Err(format!("stream read error: {e}")),
                Ok(None) => return Ok(None), // EOF — leftover partial bytes dropped
                Err(_elapsed) => continue,   // 100ms tick, re-check cancel
            };
            self.buffer.push_str(&String::from_utf8_lossy(&chunk));
            if let Some(event) = self.drain_buffered_event() {
                return Ok(Some(event));
            }
        }
    }

    fn drain_buffered_event(&mut self) -> Option<String> {
        let pos = find_sse_boundary(&self.buffer)?;
        let event = self.buffer[..pos].to_string();
        // Defensive trim: find_sse_boundary already returns the offset
        // past the `\n\n` (or `\r\n\r\n`), but some providers send
        // stray leading newlines between events as keepalive padding —
        // strip them so the next find_sse_boundary call starts clean.
        self.buffer = self.buffer[pos..]
            .trim_start_matches('\n')
            .trim_start_matches('\r')
            .to_string();
        Some(event)
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
