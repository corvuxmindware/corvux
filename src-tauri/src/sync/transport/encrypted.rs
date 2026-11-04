// into_inner exposed for future transport unwrap path
#![allow(dead_code)]

//! `EncryptedTransport<T>` — decorator that wraps every outbound `Message` in
//! an `EncryptedEnvelope` and unwraps every inbound one.
//!
//! Installed by `peer::spawn` after the handshake derives a `SessionSecret`.
//! From the sync loop's perspective nothing changes: it still calls
//! `transport.send(&msg)` / `transport.recv()` and gets plaintext `Message`s
//! either way. The wire is opaque past this layer.
//!
//! # Invariants
//!
//! - **Outbound:** every `send` produces exactly one `EncryptedEnvelope` on
//!   the underlying transport. We never bypass — there's no "sometimes
//!   plaintext" mode in v2.
//! - **Inbound:** every `recv` expects an `EncryptedEnvelope`. Anything else
//!   is rejected as a protocol violation. A v1 peer's plaintext content
//!   message would land here only after a successful v2 handshake, which is
//!   already impossible (handshake checks protocol_version), so reaching
//!   this branch means a buggy or hostile peer.
//! - **Per-frame randomness:** each `send` draws a fresh 24-byte nonce from
//!   `OsRng`. Reconnects with the same `SessionSecret` are safe — XChaCha
//!   nonce space is large enough that random selection has no collision
//!   risk in practice.

use async_trait::async_trait;

use super::Transport;
use crate::sync::crypto::envelope::EnvelopeCipher;
use crate::sync::error::{ProtocolError, TransportError};
use crate::sync::protocol::{decode, encode, Message};

/// Envelope-AAD version tag (M-2). Bumping this forces every peer to agree on
/// the exact AAD construction; a mismatch fails authentication closed.
const ENVELOPE_AAD_V2: &[u8] = b"corvux-env-v2";

/// Build the per-frame AAD bound by the envelope: version ‖ direction ‖ seq.
/// Binding direction stops a frame being reflected back to its sender;
/// binding the monotonic seq (checked increasing on recv) stops replay/reorder.
fn frame_aad(direction: u8, seq: u64) -> Vec<u8> {
    let mut aad = Vec::with_capacity(ENVELOPE_AAD_V2.len() + 1 + 8);
    aad.extend_from_slice(ENVELOPE_AAD_V2);
    aad.push(direction);
    aad.extend_from_slice(&seq.to_le_bytes());
    aad
}

pub struct EncryptedTransport<T> {
    inner: T,
    cipher: EnvelopeCipher,
    /// Direction byte WE stamp on outbound frames (0 = client→server,
    /// 1 = server→client). The peer opens expecting `1 - send_direction`.
    send_direction: u8,
    /// Monotonic counter for outbound frames; bound into each frame's AAD.
    send_seq: u64,
    /// Highest inbound seq accepted so far. `None` until the first frame.
    /// A frame whose seq is not strictly greater is rejected as a replay.
    recv_seq: Option<u64>,
}

impl<T> EncryptedTransport<T> {
    /// `is_client` sets the outbound direction so the two peers stamp
    /// different direction bytes (reflection defence). Both sides start their
    /// sequence at 0.
    pub fn new(inner: T, cipher: EnvelopeCipher, is_client: bool) -> Self {
        Self {
            inner,
            cipher,
            send_direction: if is_client { 0 } else { 1 },
            send_seq: 0,
            recv_seq: None,
        }
    }

    /// Drop the encryption layer and recover the underlying transport. Only
    /// useful in tests / teardown — production never downgrades.
    pub fn into_inner(self) -> T {
        self.inner
    }
}

#[async_trait]
impl<T: Transport + Send + Sync> Transport for EncryptedTransport<T> {
    async fn send(&mut self, msg: &Message) -> Result<(), TransportError> {
        // Refuse to encrypt an already-encrypted variant. Nothing in the
        // engine should ever construct one of these — if it does, something
        // is double-wrapping and we'd silently corrupt the wire.
        if matches!(msg, Message::EncryptedEnvelope { .. }) {
            return Err(TransportError::Iroh(
                "EncryptedTransport refused to double-wrap an EncryptedEnvelope".into(),
            ));
        }
        let plaintext = encode(msg).map_err(protocol_to_transport)?;
        let seq = self.send_seq;
        let aad = frame_aad(self.send_direction, seq);
        let sealed = self
            .cipher
            .seal(&plaintext, &aad)
            .map_err(crypto_to_transport)?;
        // Overflow is unreachable in any real session (2^64 frames), but
        // saturate rather than wrap so we can never reissue a seq.
        self.send_seq = self.send_seq.saturating_add(1);
        let envelope = Message::EncryptedEnvelope {
            nonce: sealed.nonce,
            seq,
            ciphertext: sealed.ciphertext,
        };
        self.inner.send(&envelope).await
    }

    async fn recv(&mut self) -> Result<Message, TransportError> {
        let frame = self.inner.recv().await?;
        match frame {
            Message::EncryptedEnvelope {
                nonce,
                seq,
                ciphertext,
            } => {
                // Open expecting the PEER's direction (1 - ours), binding the
                // frame's own seq. A tampered seq/direction changes the AAD and
                // fails authentication here (M-2).
                let expected_direction = 1 - self.send_direction;
                let aad = frame_aad(expected_direction, seq);
                let plaintext = self
                    .cipher
                    .open(&nonce, &ciphertext, &aad)
                    .map_err(crypto_to_transport)?;
                // Replay/reorder defence: seq must strictly increase.
                if let Some(last) = self.recv_seq {
                    if seq <= last {
                        return Err(TransportError::Iroh(format!(
                            "EncryptedTransport rejected replayed/reordered frame (seq {seq} <= {last})"
                        )));
                    }
                }
                self.recv_seq = Some(seq);
                let inner = decode(&plaintext).map_err(protocol_to_transport)?;
                // Defence: a peer mustn't smuggle a re-wrapped envelope as
                // the inner plaintext. We always reject nested envelopes.
                if matches!(inner, Message::EncryptedEnvelope { .. }) {
                    return Err(TransportError::Iroh(
                        "EncryptedTransport saw a nested EncryptedEnvelope".into(),
                    ));
                }
                Ok(inner)
            }
            other => {
                // Plaintext post-handshake. v2 disallows this entirely.
                Err(protocol_to_transport(ProtocolError::UnexpectedMessage {
                    state: "EncryptedTransport",
                    actual: variant_name(&other),
                }))
            }
        }
    }

    async fn close(&mut self) -> Result<(), TransportError> {
        self.inner.close().await
    }
}

fn variant_name(m: &Message) -> &'static str {
    match m {
        Message::Hello { .. } => "Hello",
        Message::PairingChallenge { .. } => "PairingChallenge",
        Message::PairingProof { .. } => "PairingProof",
        Message::PairingOk { .. } => "PairingOk",
        Message::PairingFailed { .. } => "PairingFailed",
        Message::SyncMessage { .. } => "SyncMessage",
        Message::NoteList { .. } => "NoteList",
        Message::RequestFullDoc { .. } => "RequestFullDoc",
        Message::FullDoc { .. } => "FullDoc",
        Message::FullDocChunk { .. } => "FullDocChunk",
        Message::NoteDelete { .. } => "NoteDelete",
        Message::Ping => "Ping",
        Message::Pong => "Pong",
        Message::Bye => "Bye",
        Message::EncryptedEnvelope { .. } => "EncryptedEnvelope",
        Message::RequestBlob { .. } => "RequestBlob",
        Message::Blob { .. } => "Blob",
        Message::BlobUnavailable { .. } => "BlobUnavailable",
    }
}

fn protocol_to_transport(e: ProtocolError) -> TransportError {
    // The Transport trait only knows TransportError; tunnel protocol/crypto
    // failures through `Iroh(String)` so the engine still gets a useful
    // message in logs without a brand-new variant.
    TransportError::Iroh(format!("envelope: {e}"))
}

fn crypto_to_transport(e: crate::sync::error::SyncError) -> TransportError {
    TransportError::Iroh(format!("envelope: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::crypto::pairing::{derive_session_secret, Nonce, Pin};
    use crate::sync::protocol::NotePath;
    use crate::sync::transport::mock::MockTransport;

    fn cipher_pair() -> (EnvelopeCipher, EnvelopeCipher) {
        let pin = Pin::parse("424242").unwrap();
        let nonce = Nonce::generate();
        let session_a = derive_session_secret(&pin, &nonce, &[0u8; 32]).unwrap();
        let session_b = derive_session_secret(&pin, &nonce, &[0u8; 32]).unwrap();
        (
            EnvelopeCipher::from_session(&session_a).unwrap(),
            EnvelopeCipher::from_session(&session_b).unwrap(),
        )
    }

    #[tokio::test]
    async fn roundtrip_through_encrypted_transport() {
        let (a_inner, b_inner) = MockTransport::pair();
        let (cipher_a, cipher_b) = cipher_pair();
        let mut a = EncryptedTransport::new(a_inner, cipher_a, true);
        let mut b = EncryptedTransport::new(b_inner, cipher_b, false);

        let original = Message::SyncMessage {
            path: NotePath("notes/foo.md".into()),
            bytes: vec![1, 2, 3, 4, 5],
        };
        a.send(&original).await.unwrap();

        let recovered = b.recv().await.unwrap();
        match recovered {
            Message::SyncMessage { path, bytes } => {
                assert_eq!(path.0, "notes/foo.md");
                assert_eq!(bytes, vec![1, 2, 3, 4, 5]);
            }
            other => panic!("expected SyncMessage, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn wire_carries_envelope_not_plaintext() {
        // `a` is encrypted; `b_inner` is the raw mock. We should see an
        // `EncryptedEnvelope` on the raw side, not the inner SyncMessage.
        let (a_inner, mut b_inner) = MockTransport::pair();
        let (cipher_a, _cipher_b) = cipher_pair();
        let mut a = EncryptedTransport::new(a_inner, cipher_a, true);

        a.send(&Message::Ping).await.unwrap();
        let raw = b_inner.recv().await.unwrap();
        assert!(matches!(raw, Message::EncryptedEnvelope { .. }));
    }

    #[tokio::test]
    async fn refuses_to_double_wrap() {
        let (a_inner, _b_inner) = MockTransport::pair();
        let (cipher_a, _cipher_b) = cipher_pair();
        let mut a = EncryptedTransport::new(a_inner, cipher_a, true);

        let already = Message::EncryptedEnvelope {
            nonce: [0u8; 24],
            seq: 0,
            ciphertext: vec![],
        };
        let err = a.send(&already).await.unwrap_err();
        assert!(matches!(err, TransportError::Iroh(_)));
    }

    #[tokio::test]
    async fn rejects_plaintext_received_in_encrypted_context() {
        // `a` is encrypted, but the peer sends raw plaintext on the inner
        // mock — simulating a buggy / hostile counterpart.
        let (a_inner, mut b_inner) = MockTransport::pair();
        let (cipher_a, _cipher_b) = cipher_pair();
        let mut a = EncryptedTransport::new(a_inner, cipher_a, true);

        b_inner.send(&Message::Ping).await.unwrap();
        let err = a.recv().await.unwrap_err();
        assert!(matches!(err, TransportError::Iroh(_)));
    }

    #[tokio::test]
    async fn wrong_key_recv_fails() {
        // Two independent cipher pairs — `b` can't decrypt what `a` sent.
        let (a_inner, b_inner) = MockTransport::pair();
        let (cipher_a, _) = cipher_pair();
        let (_, cipher_b_other) = cipher_pair();
        let mut a = EncryptedTransport::new(a_inner, cipher_a, true);
        let mut b = EncryptedTransport::new(b_inner, cipher_b_other, false);

        a.send(&Message::Ping).await.unwrap();
        let err = b.recv().await.unwrap_err();
        assert!(matches!(err, TransportError::Iroh(_)));
    }

    #[tokio::test]
    async fn replayed_frame_is_rejected() {
        // M-2: a captured, valid frame re-delivered must be rejected because
        // its seq no longer strictly increases.
        let (cipher_a, cipher_b) = cipher_pair();
        let (mut feeder, b_inner) = MockTransport::pair();
        let mut b = EncryptedTransport::new(b_inner, cipher_b, false);

        // Hand-build a valid client→server frame (direction 0, seq 0).
        let plaintext = encode(&Message::Ping).unwrap();
        let sealed = cipher_a.seal(&plaintext, &frame_aad(0, 0)).unwrap();
        let envelope = Message::EncryptedEnvelope {
            nonce: sealed.nonce,
            seq: 0,
            ciphertext: sealed.ciphertext,
        };

        feeder.send(&envelope).await.unwrap();
        assert!(matches!(b.recv().await.unwrap(), Message::Ping));
        // Replay the identical frame.
        feeder.send(&envelope).await.unwrap();
        let err = b.recv().await.unwrap_err();
        assert!(
            matches!(err, TransportError::Iroh(_)),
            "replay must be rejected"
        );
    }

    #[tokio::test]
    async fn reflected_frame_is_rejected() {
        // M-2: a frame stamped with a SIDE's own direction must not open on
        // that same side's recv (which expects the peer's direction), so a
        // relay can't reflect a frame back to its sender.
        let (cipher_a, cipher_a2) = cipher_pair(); // same session
        let (mut feeder, a_inner) = MockTransport::pair();
        let mut a = EncryptedTransport::new(a_inner, cipher_a, true); // dir 0

        // Seal a frame stamped with a's OWN direction (0) — a reflection.
        let plaintext = encode(&Message::Ping).unwrap();
        let sealed = cipher_a2.seal(&plaintext, &frame_aad(0, 0)).unwrap();
        let reflected = Message::EncryptedEnvelope {
            nonce: sealed.nonce,
            seq: 0,
            ciphertext: sealed.ciphertext,
        };
        feeder.send(&reflected).await.unwrap();
        let err = a.recv().await.unwrap_err();
        assert!(
            matches!(err, TransportError::Iroh(_)),
            "reflection must be rejected"
        );
    }
}
