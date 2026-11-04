//! App-layer message envelope: XChaCha20-Poly1305 over every post-handshake
//! `Message`.
//!
//! # Why this layer exists
//!
//! iroh QUIC already encrypts the wire (TLS 1.3). Adding our own envelope on
//! top is forward defence:
//!
//! 1. **Store-and-forward queue (planned, post-beta).** Encrypted CRDT
//!    deltas sit on a Corvux-operated mailbox until the recipient comes
//!    online. The queue must never see plaintext — even with full server
//!    access. Without this envelope, deltas would be plaintext at rest in
//!    the queue.
//! 2. **iroh-relay forwarding mode.** Today the relay forwards opaque
//!    QUIC packets, but any future shift toward an application-aware relay
//!    (rate-limit by note, prioritise particular peers, etc.) must not
//!    inadvertently widen what the relay can read.
//! 3. **Defence in depth.** TLS-stack compromises happen. A second sealed
//!    layer with a key the transport never sees means a TLS break alone
//!    doesn't reveal note content.
//!
//! # Threat model & design
//!
//! - Cipher: XChaCha20-Poly1305. 24-byte random nonce per frame → birthday
//!   collision after ~2^96 frames, comfortably beyond any deployment.
//! - Key: HKDF-SHA256 subkey of the pairing `SessionSecret`, label
//!   `b"corvux-app-envelope-v1"`. Domain-separated from any other subkey.
//! - Authenticated: Poly1305 MAC; tampered ciphertext returns `Err`.
//! - No AAD on v1 — the key is already per-pair-per-handshake, so there's
//!   no cross-context to bind to. If we add multi-channel concurrency
//!   later (e.g. one envelope-cipher across two iroh streams), AAD bound
//!   to the channel ID becomes load-bearing; bumping the label suffix
//!   covers the rotation.
//!
//! # Wire shape
//!
//! Plaintext is a postcard-encoded `Message`. The envelope frame on the wire
//! (carried as `Message::EncryptedEnvelope`) is:
//!
//! ```text
//!   nonce: [u8; 24]    // random per-frame
//!   ciphertext: Vec<u8> // XChaCha20-Poly1305 sealed plaintext (incl. tag)
//! ```
//!
//! `seal()` produces the pair; `open()` consumes it. Nonce-reuse is
//! prevented by sourcing every nonce from `OsRng`, never by counter — the
//! latter would race across reconnects with the same session secret.

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use rand::rngs::OsRng;
use rand::RngCore;
use zeroize::Zeroize;

use super::pairing::{derive_subkey, DomainSeparatedKey, SessionSecret};
use crate::sync::error::{CryptoError, SyncResult};

/// HKDF info-string for the envelope subkey. Bumping the suffix forces a
/// fresh derivation if we ever need to rotate without re-pairing.
const ENVELOPE_KEY_LABEL: &[u8] = b"corvux-app-envelope-v1";

/// A sealed app-layer frame. The pair `(nonce, ciphertext)` is what crosses
/// the wire as the body of `Message::EncryptedEnvelope`.
pub struct SealedFrame {
    pub nonce: [u8; 24],
    pub ciphertext: Vec<u8>,
}

/// XChaCha20-Poly1305 cipher keyed off the pairing session secret.
///
/// Holds the raw 32-byte subkey internally (zeroized on drop). Cloning is
/// disallowed — there's exactly one cipher per peer connection, owned by
/// the peer FSM. Reconstruct from the same `SessionSecret` if needed.
pub struct EnvelopeCipher {
    key: ZeroizingKey,
}

/// Newtype around a fixed-size key array that wipes on drop. Avoids us
/// accidentally `Clone`ing or `Copy`ing the raw key bytes.
struct ZeroizingKey([u8; 32]);

impl Drop for ZeroizingKey {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl EnvelopeCipher {
    /// Derive the envelope subkey from the pairing session secret. Both
    /// peers run this independently with the same `SessionSecret` and
    /// arrive at the same key.
    pub fn from_session(session: &SessionSecret) -> SyncResult<Self> {
        let key = derive_subkey(session, ENVELOPE_KEY_LABEL)?;
        Ok(Self {
            key: ZeroizingKey(*key.expose()),
        })
    }

    /// Build a cipher from a [`DomainSeparatedKey`] — the only way to
    /// construct one is via `derive_subkey(session, label)`, so the
    /// type system enforces that the caller did NOT pass the raw
    /// session secret (or any other random 32 bytes) directly into a
    /// downstream cipher.
    ///
    /// AUDIT-048: tightened from `[u8; 32]` to `DomainSeparatedKey`.
    /// Used by the store-and-forward queue, whose per-pair key is an
    /// HKDF subkey of the pairing `SessionSecret` with the
    /// `b"corvux-queue-pair-v1"` label.
    pub fn from_raw_key(key: DomainSeparatedKey) -> Self {
        Self {
            key: ZeroizingKey(*key.expose()),
        }
    }

    /// Seal `plaintext` into a fresh frame with a random nonce, binding `aad`
    /// (associated data) into the authentication tag (M-2). `aad` is NOT
    /// encrypted and NOT carried in the frame — the opener must supply the
    /// identical bytes or authentication fails. Callers bind context here
    /// (envelope version, direction, per-session sequence, or the intended
    /// recipient) so a captured frame can't be reflected, reordered, replayed,
    /// or redelivered to a different recipient. Pass `&[]` for no binding.
    pub fn seal(&self, plaintext: &[u8], aad: &[u8]) -> SyncResult<SealedFrame> {
        let cipher = XChaCha20Poly1305::new(self.key.0.as_slice().into());
        let mut nonce = [0u8; 24];
        OsRng.fill_bytes(&mut nonce);
        let ciphertext = cipher
            .encrypt(
                XNonce::from_slice(&nonce),
                Payload {
                    msg: plaintext,
                    aad,
                },
            )
            .map_err(|_| CryptoError::Decrypt)?;
        Ok(SealedFrame { nonce, ciphertext })
    }

    /// Open a `SealedFrame`, requiring `aad` to match what `seal` bound.
    /// Returns `Err(CryptoError::Decrypt)` on any authentication failure:
    /// wrong key, tampered ciphertext, truncated tag, mismatched nonce, or
    /// mismatched AAD. Callers should treat this as a protocol violation and
    /// tear the connection down — anything that fails authentication is hostile.
    pub fn open(&self, nonce: &[u8; 24], ciphertext: &[u8], aad: &[u8]) -> SyncResult<Vec<u8>> {
        let cipher = XChaCha20Poly1305::new(self.key.0.as_slice().into());
        cipher
            .decrypt(
                XNonce::from_slice(nonce),
                Payload {
                    msg: ciphertext,
                    aad,
                },
            )
            .map_err(|_| CryptoError::Decrypt.into())
    }
}

impl std::fmt::Debug for EnvelopeCipher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("EnvelopeCipher(****)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::crypto::pairing::{derive_session_secret, Nonce, Pin};

    fn make_cipher() -> EnvelopeCipher {
        let pin = Pin::parse("424242").unwrap();
        let nonce = Nonce::generate();
        let session = derive_session_secret(&pin, &nonce, &[0u8; 32]).unwrap();
        EnvelopeCipher::from_session(&session).unwrap()
    }

    #[test]
    fn seal_and_open_roundtrip() {
        let cipher = make_cipher();
        let plaintext = b"hello, sync";
        let sealed = cipher.seal(plaintext, b"aad").unwrap();
        let recovered = cipher
            .open(&sealed.nonce, &sealed.ciphertext, b"aad")
            .unwrap();
        assert_eq!(recovered, plaintext);
    }

    #[test]
    fn two_peers_with_same_session_secret_can_open_each_others_frames() {
        let pin = Pin::parse("123456").unwrap();
        let nonce = Nonce::generate();
        let session_a = derive_session_secret(&pin, &nonce, &[0u8; 32]).unwrap();
        let session_b = derive_session_secret(&pin, &nonce, &[0u8; 32]).unwrap();
        let cipher_a = EnvelopeCipher::from_session(&session_a).unwrap();
        let cipher_b = EnvelopeCipher::from_session(&session_b).unwrap();

        let plaintext = b"crossing the pair boundary";
        let sealed = cipher_a.seal(plaintext, b"").unwrap();
        let recovered = cipher_b
            .open(&sealed.nonce, &sealed.ciphertext, b"")
            .unwrap();
        assert_eq!(recovered, plaintext);
    }

    #[test]
    fn wrong_key_fails_to_open() {
        let cipher_a = make_cipher();
        let cipher_b = make_cipher(); // independent session secret → different subkey

        let sealed = cipher_a.seal(b"secret", b"").unwrap();
        let err = cipher_b
            .open(&sealed.nonce, &sealed.ciphertext, b"")
            .unwrap_err();
        assert!(matches!(
            err,
            crate::sync::error::SyncError::Crypto(CryptoError::Decrypt)
        ));
    }

    #[test]
    fn mismatched_aad_fails_to_open() {
        // M-2: the same key + nonce + ciphertext must NOT open under different
        // associated data. This is what binds direction/seq/context.
        let cipher = make_cipher();
        let sealed = cipher.seal(b"bound to aad", b"context-A").unwrap();
        let err = cipher
            .open(&sealed.nonce, &sealed.ciphertext, b"context-B")
            .unwrap_err();
        assert!(matches!(
            err,
            crate::sync::error::SyncError::Crypto(CryptoError::Decrypt)
        ));
        // …and opens cleanly under the correct AAD.
        assert_eq!(
            cipher
                .open(&sealed.nonce, &sealed.ciphertext, b"context-A")
                .unwrap(),
            b"bound to aad"
        );
    }

    #[test]
    fn tampered_ciphertext_fails_to_open() {
        let cipher = make_cipher();
        let mut sealed = cipher.seal(b"don't touch this", b"").unwrap();
        // Flip a bit somewhere in the body. Poly1305 must catch it.
        sealed.ciphertext[0] ^= 0x01;
        let err = cipher
            .open(&sealed.nonce, &sealed.ciphertext, b"")
            .unwrap_err();
        assert!(matches!(
            err,
            crate::sync::error::SyncError::Crypto(CryptoError::Decrypt)
        ));
    }

    #[test]
    fn tampered_nonce_fails_to_open() {
        let cipher = make_cipher();
        let sealed = cipher.seal(b"don't touch this either", b"").unwrap();
        let mut bad_nonce = sealed.nonce;
        bad_nonce[0] ^= 0x01;
        let err = cipher
            .open(&bad_nonce, &sealed.ciphertext, b"")
            .unwrap_err();
        assert!(matches!(
            err,
            crate::sync::error::SyncError::Crypto(CryptoError::Decrypt)
        ));
    }

    #[test]
    fn nonces_differ_between_seals() {
        // Random per-frame: vanishingly unlikely to repeat across two seals.
        let cipher = make_cipher();
        let s1 = cipher.seal(b"a", b"").unwrap();
        let s2 = cipher.seal(b"a", b"").unwrap();
        assert_ne!(s1.nonce, s2.nonce);
        // …and so does the resulting ciphertext, even with identical input.
        assert_ne!(s1.ciphertext, s2.ciphertext);
    }

    #[test]
    fn ciphertext_is_longer_than_plaintext_by_tag_size() {
        // Poly1305 appends a 16-byte authentication tag.
        let cipher = make_cipher();
        let plaintext = b"abc";
        let sealed = cipher.seal(plaintext, b"").unwrap();
        assert_eq!(sealed.ciphertext.len(), plaintext.len() + 16);
    }
}
