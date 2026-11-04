//! Encrypted store-and-forward queue.
//!
//! This module is the client side of an optional store-and-forward relay:
//! it lets a device hand an encrypted update to a queue service when the
//! other device is offline. The queue service itself is not part of this
//! repository.
//!
//! # Topology
//!
//! ```text
//!   Sender device                    Queue service             Recipient device
//!   ─────────────                    ─────────────             ────────────────
//!   peer dial fails                  durable store, keyed
//!   → encrypt FullDoc with           by recipient NodeId,
//!     per-pair queue key             30-day TTL, ACK+delete
//!   → POST /v1/push  ──────────────▶ buffer ciphertext
//!                                    ◀────────────────────────── GET /v1/manifest
//!                                    list of pending msg_ids
//!                                    ◀────────────────────────── GET /v1/pull/:id
//!                                    ciphertext stream
//!                                    ◀────────────────────────── DELETE /v1/pull/:id
//!                                    drop entry
//! ```
//!
//! # What the queue can / cannot see
//!
//! - **Can:** sender NodeId, recipient NodeId, ciphertext size, timestamps.
//! - **Cannot:** plaintext note content. The XChaCha20-Poly1305 envelope
//!   used here is keyed off a per-pair secret derived during pairing
//!   (`derive_subkey(SessionSecret, b"corvux-queue-pair-v1")`) and
//!   persisted on both ends in the peer store. The queue server never
//!   sees those bytes.
//!
//! # Module layout
//!
//! - [`protocol`] — wire types shared between client and server.
//!   No tokio / axum / reqwest deps; pure serde structs.
//! - [`client`] — `QueueClient` for the Tauri-side sync engine.
//!   Uses `reqwest`, integrates with the engine's `Identity` for
//!   challenge-response authentication.

pub mod client;
pub mod protocol;

use hmac::{Hmac, Mac};
use sha2::Sha256;

/// Per-pair, per-path opaque dedup tag for queue pushes.
///
/// `HMAC-SHA256(pair_key, "queue-dedup-v1" || path)`, truncated to 16
/// bytes. Deterministic for a given (pair_key, path) so the server can
/// still collapse rapid edits of the same note into a single pending
/// entry — but the server never learns the plaintext path. See
/// AUDIT-001 (2026-05-22 audit) for why we don't send the raw path.
///
/// The label `"queue-dedup-v1"` is a domain separator: if we ever need
/// to compute another tag under the same pair_key (e.g. an integrity
/// tag on the recipient side), it doesn't accidentally collide with
/// this one.
pub fn dedup_tag(pair_key: &[u8; 32], path: &str) -> Vec<u8> {
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(pair_key)
        .expect("HMAC-SHA256 accepts any key length");
    mac.update(b"queue-dedup-v1");
    mac.update(path.as_bytes());
    mac.finalize().into_bytes()[..16].to_vec()
}

#[cfg(test)]
mod tests {
    use super::dedup_tag;

    #[test]
    fn tag_is_deterministic_per_pair_and_path() {
        let key = [7u8; 32];
        let a = dedup_tag(&key, "notes/foo.md");
        let b = dedup_tag(&key, "notes/foo.md");
        assert_eq!(a, b);
        assert_eq!(a.len(), 16);
    }

    #[test]
    fn tag_differs_per_path_under_same_key() {
        let key = [7u8; 32];
        let a = dedup_tag(&key, "notes/foo.md");
        let b = dedup_tag(&key, "notes/bar.md");
        assert_ne!(a, b);
    }

    #[test]
    fn tag_differs_per_key_under_same_path() {
        let path = "notes/foo.md";
        let a = dedup_tag(&[1u8; 32], path);
        let b = dedup_tag(&[2u8; 32], path);
        assert_ne!(a, b);
    }

    #[test]
    fn tag_contains_no_path_substring() {
        // The whole point: server cannot recover the path from the tag.
        // Sanity: the ASCII path bytes don't appear verbatim in the tag.
        let path = "notes/secret-divorce.md";
        let tag = dedup_tag(&[0xAB; 32], path);
        let path_bytes = path.as_bytes();
        for window in tag.windows(path_bytes.len().min(tag.len())) {
            assert_ne!(window, path_bytes);
        }
        // And no recognizable ASCII run from the path.
        let needle = b"divorce";
        for window in tag.windows(needle.len()) {
            assert_ne!(window, needle);
        }
    }
}
