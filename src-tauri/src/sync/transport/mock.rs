// Test-only transport. `MockTransport` + `pair()` are consumed by
// integration tests under `tests/` and by sync::engine tests, both of
// which the lib build doesn't see. File-level allow keeps the contract
// usable without per-symbol noise.
#![allow(dead_code)]

//! In-memory transport for tests.
//!
//! Two `MockTransport`s created with `pair()` share a pair of mpsc channels,
//! forming a bidirectional message link. No framing, no crypto — we're
//! testing engine logic, not the wire. Framing and TLS have their own unit
//! tests elsewhere.
//!
//! Useful patterns:
//!
//! - `pair()` for the common case: two engines A and B connected directly.
//! - `drop(tx)` to simulate a peer hang-up: the other side's next `recv`
//!   surfaces `TransportError::Closed`.

use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::sync::error::TransportError;
use crate::sync::protocol::Message;

use super::Transport;

pub struct MockTransport {
    tx: Option<mpsc::Sender<Message>>,
    rx: mpsc::Receiver<Message>,
}

impl MockTransport {
    /// Build a pair of connected endpoints.
    pub fn pair() -> (Self, Self) {
        let (a_tx, b_rx) = mpsc::channel(32);
        let (b_tx, a_rx) = mpsc::channel(32);
        (
            MockTransport {
                tx: Some(a_tx),
                rx: a_rx,
            },
            MockTransport {
                tx: Some(b_tx),
                rx: b_rx,
            },
        )
    }
}

#[async_trait]
impl Transport for MockTransport {
    async fn send(&mut self, msg: &Message) -> Result<(), TransportError> {
        let tx = self.tx.as_ref().ok_or(TransportError::Closed)?;
        tx.send(msg.clone())
            .await
            .map_err(|_| TransportError::Closed)?;
        Ok(())
    }

    async fn recv(&mut self) -> Result<Message, TransportError> {
        self.rx.recv().await.ok_or(TransportError::Closed)
    }

    async fn close(&mut self) -> Result<(), TransportError> {
        // Dropping the sender signals close to the peer's `recv`.
        self.tx.take();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::protocol::Message;

    #[tokio::test]
    async fn mock_roundtrip() {
        let (mut a, mut b) = MockTransport::pair();
        a.send(&Message::Ping).await.unwrap();
        let got = b.recv().await.unwrap();
        assert!(matches!(got, Message::Ping));
    }

    #[tokio::test]
    async fn close_surfaces_to_peer() {
        let (mut a, mut b) = MockTransport::pair();
        a.close().await.unwrap();
        let err = b.recv().await.unwrap_err();
        assert!(matches!(err, TransportError::Closed));
    }
}
