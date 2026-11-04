// Typed-error catalog: several variants are part of the public contract /
// state-machine signalling but not yet constructed by current code paths
// (e.g. `PeerNotFound`, `NonceReuse`, `Generate`). Keep file-level rather
// than scattering per-variant allows across every enum.
#![allow(dead_code)]

//! Typed error hierarchy for the sync engine.
//!
//! Every fallible operation returns `Result<T, SyncError>`. Errors are grouped
//! by the layer they originate from so UI code can show layer-appropriate
//! diagnostics (e.g. "pairing failed: wrong PIN" vs "transport closed").
//!
//! We deliberately avoid `anyhow` here — sync correctness matters, and typed
//! errors let the engine's state machine branch on specific failure kinds
//! (e.g. retry on `Transport::Closed`, give up on `Pairing::WrongPin`).

use std::io;
use thiserror::Error;

/// Top-level error returned from every public sync API.
#[derive(Debug, Error)]
pub enum SyncError {
    #[error("identity: {0}")]
    Identity(#[from] IdentityError),

    #[error("crypto: {0}")]
    Crypto(#[from] CryptoError),

    #[error("transport: {0}")]
    Transport(#[from] TransportError),

    #[error("protocol: {0}")]
    Protocol(#[from] ProtocolError),

    #[error("crdt: {0}")]
    Crdt(#[from] CrdtError),

    #[error("store: {0}")]
    Store(#[from] StoreError),

    #[error("engine: {0}")]
    Engine(#[from] EngineError),

    #[error("io: {0}")]
    Io(#[from] io::Error),
}

/// Identity layer — keypair generation, loading, signing.
#[derive(Debug, Error)]
pub enum IdentityError {
    #[error("failed to generate keypair")]
    Generate,

    #[error("identity file corrupt or unreadable")]
    Corrupt,

    #[error("identity version {found} unsupported (expected {expected})")]
    UnsupportedVersion { expected: u16, found: u16 },
}

/// Crypto layer — pairing, session keys, fingerprints.
#[derive(Debug, Error)]
pub enum CryptoError {
    #[error("pin verification failed")]
    WrongPin,

    #[error("pairing is rate-limited; wait before retrying")]
    RateLimited,

    #[error("nonce reuse detected — refusing to continue")]
    NonceReuse,

    #[error("hmac length mismatch")]
    HmacLength,

    #[error("key derivation failed")]
    Kdf,

    #[error("ecdh shared secret is non-contributory (peer sent a low-order point)")]
    NonContributoryEcdh,

    #[error("certificate fingerprint does not match pinned value")]
    FingerprintMismatch { expected: String, found: String },

    #[error("envelope decryption failed (wrong key, tampered ciphertext, or wrong nonce)")]
    Decrypt,
}

/// Transport layer — framing, TLS, connection lifecycle.
#[derive(Debug, Error)]
pub enum TransportError {
    #[error("peer closed connection")]
    Closed,

    #[error("frame exceeds max size ({0} bytes)")]
    FrameTooLarge(u32),

    #[error("frame header malformed")]
    MalformedFrame,

    #[error("protocol version {found} unsupported (this build speaks {expected})")]
    UnsupportedProtocolVersion { expected: u16, found: u16 },

    #[error("tls handshake failed: {0}")]
    TlsHandshake(String),

    #[error("iroh transport: {0}")]
    Iroh(String),

    #[error("connect timeout")]
    ConnectTimeout,

    #[error("io: {0}")]
    Io(#[from] io::Error),
}

/// Protocol layer — message parsing, handshake state machine.
#[derive(Debug, Error)]
pub enum ProtocolError {
    #[error("decoding postcard payload: {0}")]
    Decode(String),

    #[error("encoding postcard payload: {0}")]
    Encode(String),

    #[error("unexpected message {actual} in state {state}")]
    UnexpectedMessage {
        state: &'static str,
        actual: &'static str,
    },

    #[error("handshake already complete")]
    HandshakeComplete,

    #[error("handshake aborted by peer: {0}")]
    Aborted(String),
}

/// CRDT layer — Automerge doc lifecycle, markdown round-trip.
#[derive(Debug, Error)]
pub enum CrdtError {
    #[error("automerge: {0}")]
    Automerge(String),

    #[error("doc schema version {found} unsupported (this build speaks {expected})")]
    UnsupportedSchema { expected: u16, found: u16 },

    #[error("doc is missing required field `{0}`")]
    MissingField(&'static str),

    #[error("markdown round-trip lost data (text field type mismatch)")]
    RoundTripLoss,
}

impl From<automerge::AutomergeError> for CrdtError {
    fn from(e: automerge::AutomergeError) -> Self {
        CrdtError::Automerge(e.to_string())
    }
}

/// Store layer — on-disk persistence.
#[derive(Debug, Error)]
pub enum StoreError {
    #[error("path escapes roost root: {0}")]
    PathTraversal(String),

    #[error("path contains invalid component: {0}")]
    InvalidPath(String),

    #[error("path collides with an existing note on this filesystem: {0}")]
    PathConflict(String),

    #[error("io: {0}")]
    Io(#[from] io::Error),

    #[error("encode: {0}")]
    Encode(String),

    #[error("decode: {0}")]
    Decode(String),
}

/// Engine layer — orchestration, peer lifecycle, command dispatch.
#[derive(Debug, Error)]
pub enum EngineError {
    #[error("sync is disabled in settings")]
    Disabled,

    #[error("already running")]
    AlreadyRunning,

    #[error("not connected to any peer")]
    NotConnected,

    #[error("peer control channel full (peer wedged or slow)")]
    ChannelFull,

    #[error("peer not found: {0}")]
    PeerNotFound(String),

    #[error("unsupported on mobile: {0}")]
    UnsupportedOnMobile(&'static str),

    #[error("discovery: {0}")]
    Discovery(String),
}

/// Convenience alias.
pub type SyncResult<T> = Result<T, SyncError>;

// ── AUDIT-034: structured sync-IPC error ───────────────────────────────
//
// `SyncError` flattens to opaque strings at the IPC boundary. The frontend
// can't tell "wrong PIN" from "connect timeout" from "fingerprint mismatch"
// — three states with completely different UX outcomes. `SyncCommandError`
// fixes that: serde-tagged kind enum plus a `retryable` flag the frontend
// can switch on, with a `message` for the toast text.

/// Frontend-visible kind. Variants are exhaustive across every `SyncError`
/// path that surfaces through an IPC command. Adding a variant requires
/// updating the `From<SyncError>` impl below AND surfacing the new arm
/// downstream (`syncStore.ts` should switch on `kind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncErrorKind {
    WrongPin,
    RateLimited,
    FingerprintMismatch,
    NonceReuse,
    Decrypt,
    ConnectTimeout,
    PeerClosed,
    UnsupportedProtocol,
    TlsHandshake,
    Iroh,
    PathTraversal,
    Disabled,
    AlreadyRunning,
    NotConnected,
    PeerNotFound,
    UnsupportedOnMobile,
    Discovery,
    HandshakeAborted,
    Identity,
    Crdt,
    Store,
    Io,
    /// Catch-all for failure paths that haven't earned a dedicated kind yet.
    /// Use sparingly — the whole point of the structured error is that the
    /// frontend can branch on `kind` without inspecting `message`.
    Other,
}

impl SyncErrorKind {
    /// Hint to the frontend on whether retrying the same call could
    /// plausibly succeed without user intervention. "WrongPin" is not
    /// retryable (the user has to re-enter the PIN); "ConnectTimeout" is.
    pub fn retryable(self) -> bool {
        matches!(
            self,
            SyncErrorKind::ConnectTimeout
                | SyncErrorKind::PeerClosed
                | SyncErrorKind::RateLimited
                | SyncErrorKind::NotConnected
                | SyncErrorKind::Iroh
                | SyncErrorKind::Discovery
                | SyncErrorKind::Io
        )
    }
}

/// The shape every sync IPC command should return on error. Serialises to
/// `{ "kind": "wrong_pin", "message": "...", "retryable": false }`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SyncCommandError {
    pub kind: SyncErrorKind,
    pub message: String,
    pub retryable: bool,
}

impl SyncCommandError {
    pub fn new(kind: SyncErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            retryable: kind.retryable(),
        }
    }
}

impl std::fmt::Display for SyncCommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.kind_str(), self.message)
    }
}

impl SyncCommandError {
    fn kind_str(&self) -> &'static str {
        // Static label used in the Display form. Mirrors the snake-case
        // wire form so logs and the frontend talk the same language.
        match self.kind {
            SyncErrorKind::WrongPin => "wrong_pin",
            SyncErrorKind::RateLimited => "rate_limited",
            SyncErrorKind::FingerprintMismatch => "fingerprint_mismatch",
            SyncErrorKind::NonceReuse => "nonce_reuse",
            SyncErrorKind::Decrypt => "decrypt",
            SyncErrorKind::ConnectTimeout => "connect_timeout",
            SyncErrorKind::PeerClosed => "peer_closed",
            SyncErrorKind::UnsupportedProtocol => "unsupported_protocol",
            SyncErrorKind::TlsHandshake => "tls_handshake",
            SyncErrorKind::Iroh => "iroh",
            SyncErrorKind::PathTraversal => "path_traversal",
            SyncErrorKind::Disabled => "disabled",
            SyncErrorKind::AlreadyRunning => "already_running",
            SyncErrorKind::NotConnected => "not_connected",
            SyncErrorKind::PeerNotFound => "peer_not_found",
            SyncErrorKind::UnsupportedOnMobile => "unsupported_on_mobile",
            SyncErrorKind::Discovery => "discovery",
            SyncErrorKind::HandshakeAborted => "handshake_aborted",
            SyncErrorKind::Identity => "identity",
            SyncErrorKind::Crdt => "crdt",
            SyncErrorKind::Store => "store",
            SyncErrorKind::Io => "io",
            SyncErrorKind::Other => "other",
        }
    }
}

impl From<SyncError> for SyncCommandError {
    fn from(e: SyncError) -> Self {
        let message = e.to_string();
        let kind = match &e {
            SyncError::Identity(_) => SyncErrorKind::Identity,
            SyncError::Crypto(c) => match c {
                CryptoError::WrongPin => SyncErrorKind::WrongPin,
                CryptoError::RateLimited => SyncErrorKind::RateLimited,
                CryptoError::FingerprintMismatch { .. } => SyncErrorKind::FingerprintMismatch,
                CryptoError::NonceReuse => SyncErrorKind::NonceReuse,
                CryptoError::Decrypt => SyncErrorKind::Decrypt,
                CryptoError::HmacLength | CryptoError::Kdf | CryptoError::NonContributoryEcdh => {
                    SyncErrorKind::Other
                }
            },
            SyncError::Transport(t) => match t {
                TransportError::Closed => SyncErrorKind::PeerClosed,
                TransportError::ConnectTimeout => SyncErrorKind::ConnectTimeout,
                TransportError::UnsupportedProtocolVersion { .. } => {
                    SyncErrorKind::UnsupportedProtocol
                }
                TransportError::TlsHandshake(_) => SyncErrorKind::TlsHandshake,
                TransportError::Iroh(_) => SyncErrorKind::Iroh,
                TransportError::FrameTooLarge(_) | TransportError::MalformedFrame => {
                    SyncErrorKind::Other
                }
                TransportError::Io(_) => SyncErrorKind::Io,
            },
            SyncError::Protocol(p) => match p {
                ProtocolError::Aborted(_) => SyncErrorKind::HandshakeAborted,
                _ => SyncErrorKind::Other,
            },
            SyncError::Crdt(_) => SyncErrorKind::Crdt,
            SyncError::Store(s) => match s {
                StoreError::PathTraversal(_) | StoreError::InvalidPath(_) => {
                    SyncErrorKind::PathTraversal
                }
                _ => SyncErrorKind::Store,
            },
            SyncError::Engine(en) => match en {
                EngineError::Disabled => SyncErrorKind::Disabled,
                EngineError::AlreadyRunning => SyncErrorKind::AlreadyRunning,
                EngineError::NotConnected => SyncErrorKind::NotConnected,
                // Transient peer backpressure. Handled internally (fan-out
                // records a durable outbox intent), never surfaced as a
                // command error; map to the nearest connectivity kind.
                EngineError::ChannelFull => SyncErrorKind::NotConnected,
                EngineError::PeerNotFound(_) => SyncErrorKind::PeerNotFound,
                EngineError::UnsupportedOnMobile(_) => SyncErrorKind::UnsupportedOnMobile,
                EngineError::Discovery(_) => SyncErrorKind::Discovery,
            },
            SyncError::Io(_) => SyncErrorKind::Io,
        };
        Self::new(kind, message)
    }
}

#[cfg(test)]
mod sync_command_error_tests {
    use super::*;

    fn assert_maps(e: SyncError, expected: SyncErrorKind) {
        let mapped: SyncCommandError = e.into();
        assert_eq!(mapped.kind, expected, "mapping: {mapped:?}");
        // serialize round-trip — frontend deserialises this.
        let json = serde_json::to_string(&mapped).expect("serialize");
        assert!(
            json.contains(&format!("\"kind\":\"{}\"", mapped.kind_str())),
            "wire form must use snake_case kind tag: {json}"
        );
    }

    #[test]
    fn maps_wrong_pin() {
        assert_maps(CryptoError::WrongPin.into(), SyncErrorKind::WrongPin);
    }

    #[test]
    fn maps_rate_limited() {
        assert_maps(CryptoError::RateLimited.into(), SyncErrorKind::RateLimited);
    }

    #[test]
    fn maps_fingerprint_mismatch() {
        assert_maps(
            CryptoError::FingerprintMismatch {
                expected: "a".into(),
                found: "b".into(),
            }
            .into(),
            SyncErrorKind::FingerprintMismatch,
        );
    }

    #[test]
    fn maps_decrypt() {
        assert_maps(CryptoError::Decrypt.into(), SyncErrorKind::Decrypt);
    }

    #[test]
    fn maps_connect_timeout() {
        assert_maps(
            TransportError::ConnectTimeout.into(),
            SyncErrorKind::ConnectTimeout,
        );
    }

    #[test]
    fn maps_peer_closed() {
        assert_maps(TransportError::Closed.into(), SyncErrorKind::PeerClosed);
    }

    #[test]
    fn maps_unsupported_protocol() {
        assert_maps(
            TransportError::UnsupportedProtocolVersion {
                expected: 3,
                found: 2,
            }
            .into(),
            SyncErrorKind::UnsupportedProtocol,
        );
    }

    #[test]
    fn maps_handshake_aborted() {
        assert_maps(
            ProtocolError::Aborted("roost id mismatch".into()).into(),
            SyncErrorKind::HandshakeAborted,
        );
    }

    #[test]
    fn maps_iroh_transport_error() {
        assert_maps(
            TransportError::Iroh("connect failed".into()).into(),
            SyncErrorKind::Iroh,
        );
    }

    #[test]
    fn maps_not_connected() {
        assert_maps(
            EngineError::NotConnected.into(),
            SyncErrorKind::NotConnected,
        );
    }

    #[test]
    fn maps_path_traversal() {
        assert_maps(
            StoreError::PathTraversal("/etc/shadow".into()).into(),
            SyncErrorKind::PathTraversal,
        );
    }

    #[test]
    fn retryable_flag_makes_sense() {
        // Wrong PIN: never retryable without user re-entry.
        assert!(!SyncErrorKind::WrongPin.retryable());
        // Connect timeout: classic retry case.
        assert!(SyncErrorKind::ConnectTimeout.retryable());
        // Fingerprint mismatch: never retry silently — could be MitM.
        assert!(!SyncErrorKind::FingerprintMismatch.retryable());
    }

    #[test]
    fn message_preserved_in_wire_form() {
        let err: SyncError = ProtocolError::Aborted("roost id mismatch".into()).into();
        let mapped: SyncCommandError = err.into();
        let json = serde_json::to_string(&mapped).unwrap();
        assert!(json.contains("roost id mismatch"), "{json}");
    }
}
