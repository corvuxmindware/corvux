//! Crypto primitives for the sync layer.
//!
//! Two responsibilities:
//!
//! - `pairing` — take a 6-digit PIN + server nonce and derive a 32-byte
//!   session secret via HMAC-SHA256 challenge-response → HKDF-SHA256.
//! - `fingerprint` — compute SPKI fingerprints of TLS certs and render them
//!   as human-verifiable diceware phrases.
//!
//! This module does not define transport primitives. Keys derived here are
//! consumed by the protocol layer, which binds them to the TLS session's
//! cert fingerprint to prevent downgrade attacks.

pub mod envelope;
pub mod fingerprint;
pub mod pairing;
pub mod wordlist;
