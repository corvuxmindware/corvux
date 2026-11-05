//! Transport abstraction and concrete implementations.
//!
//! A `Transport` is a *message-oriented* channel: it carries whole
//! `Message`s, not byte streams. Under the hood everything is TLS+TCP, but
//! the engine layer only ever calls `send(msg)` / `recv() -> msg`, which
//! keeps protocol logic independent of transport details and makes testing
//! trivial — `MockTransport` is an in-memory pair of channels that behaves
//! identically from the engine's point of view.

pub mod dns_fallback;
pub mod encrypted;
pub mod frame;
pub mod iroh;
pub mod mock;
pub mod tcp_tls;

use async_trait::async_trait;

use super::error::TransportError;
use super::protocol::Message;

/// One end of a bidirectional, framed, encrypted message channel.
///
/// Both `send` and `recv` are cancellation-safe in the `tokio::select!`
/// sense: if the future is dropped, the channel is left in a consistent
/// state (either the full frame was delivered or none of it was — the
/// underlying write is atomic at the frame level because `write_frame`
/// flushes before returning).
#[async_trait]
pub trait Transport: Send + Sync {
    async fn send(&mut self, msg: &Message) -> Result<(), TransportError>;
    async fn recv(&mut self) -> Result<Message, TransportError>;

    /// Close the underlying channel. Idempotent; subsequent ops return
    /// `TransportError::Closed`.
    async fn close(&mut self) -> Result<(), TransportError>;
}
