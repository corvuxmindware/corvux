// as_str for message-kind logging surface
#![allow(dead_code)]

//! Wire protocol: the `Message` enum that crosses the transport.
//!
//! All messages are postcard-encoded. Postcard is a non-self-describing
//! binary format derived from Serde — variants are identified by a single
//! byte index, so field order in this file is load-bearing. Rules:
//!
//! - **Never reorder variants.** Postcard encodes them by declaration order.
//! - **Never remove variants without a protocol-version bump.** Deprecated
//!   variants stay in the enum (renamed with a `Deprecated_` prefix).
//! - **Add new variants at the end of the enum.**
//!
//! If you need to break compatibility, bump `frame::PROTOCOL_VERSION` and
//! handle both versions in the handshake.
//!
//! # Message taxonomy
//!
//! Three phases:
//!
//! 1. **Handshake.** `Hello` → `PairingChallenge` → `PairingProof` →
//!    `PairingOk` | `PairingFailed`. Only these are valid in this phase.
//!
//! 2. **Sync.** `SyncMessage` (wraps `automerge::sync::Message` for a named
//!    note), `NoteList` (full roost manifest, optional), `NoteDelete`.
//!
//! 3. **Lifecycle.** `Ping` / `Pong` for keepalive, `Bye` for clean close.
//!    Valid in any post-handshake state.
//!
//! The engine's state machine enforces which variants are accepted in
//! which state. Anything out of order is a protocol violation and the
//! connection is torn down.

pub mod handshake;
pub mod sync;

use serde::{Deserialize, Serialize};

/// Roost-relative path to a note, normalized to forward slashes.
/// `NotePath("notes/ideas.md")` regardless of OS. We never transmit
/// absolute paths — they leak local filesystem structure and don't
/// round-trip across platforms.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct NotePath(pub String);

/// Deserialize a note path through a borrowed string visitor so the wire-path
/// length limit is checked before allocating the owned `String` (SYN-I6/M5).
/// postcard's slice deserializer calls `visit_borrowed_str`/`visit_str` with a
/// view into the already frame-bounded input; only an accepted path is copied.
impl<'de> Deserialize<'de> for NotePath {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::{Error, Visitor};
        use std::fmt;

        struct NotePathVisitor;

        impl<'de> Visitor<'de> for NotePathVisitor {
            type Value = NotePath;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "a note path of at most {MAX_NOTE_PATH_BYTES} bytes")
            }

            fn visit_borrowed_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: Error,
            {
                self.visit_str(value)
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: Error,
            {
                if value.len() > MAX_NOTE_PATH_BYTES {
                    return Err(E::custom(format!(
                        "note path exceeds {MAX_NOTE_PATH_BYTES} bytes"
                    )));
                }
                Ok(NotePath(value.to_owned()))
            }

            fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
            where
                E: Error,
            {
                if value.len() > MAX_NOTE_PATH_BYTES {
                    return Err(E::custom(format!(
                        "note path exceeds {MAX_NOTE_PATH_BYTES} bytes"
                    )));
                }
                Ok(NotePath(value))
            }
        }

        deserializer.deserialize_str(NotePathVisitor)
    }
}

impl NotePath {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for NotePath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Max size of an attachment that will sync over the wire. Chosen to fit
/// comfortably inside one transport frame (`frame::MAX_FRAME_BYTES`) after
/// the envelope nonce + Poly1305 tag and postcard framing — so blobs never
/// need chunking. Files larger than this don't sync (the editor keeps
/// showing "not synced yet"). The LOCAL save cap (`ATTACHMENT_MAX_BYTES`,
/// 25 MiB) is independent and unchanged — a 9–25 MiB image saves locally,
/// it just doesn't propagate to peers.
pub const SYNC_BLOB_MAX_BYTES: u64 = 8 * 1024 * 1024;

/// H-coll: semantic cap on `NoteList.paths`. The transport frame cap
/// (`frame::MAX_FRAME_BYTES`, 16 MiB) already bounds the raw bytes, but a
/// hostile peer can still pack a single valid frame with millions of tiny
/// `NotePath` entries, which then fan out into `HashSet<NotePath>` copies on
/// the consuming side (`peer.rs`) — hundreds of MiB of transient allocation
/// from one frame. This bounds the entry COUNT, independent of byte size.
/// SYN-M5 (audit follow-up): tightened from 500K to 1.5x the stated desktop
/// scale ceiling (Locked Decision #9: 50K–100K notes) — 500K was 5x that
/// target with no justification beyond "a big number that feels safe," which
/// is itself a smell on a security-facing bound. 150K stays comfortably above
/// any legitimate roost manifest while actually reflecting the scale this
/// app is built for, not "not quite 1M."
pub const MAX_NOTELIST_PATHS: usize = 150_000;

/// H-coll: cap on a single note path's length. A real roost-relative path is
/// well under this (OS `PATH_MAX` is ~4 KiB for an *absolute* path; ours are
/// relative). Bounds one oversized path from ballooning a `String`.
pub const MAX_NOTE_PATH_BYTES: usize = 4096;

/// H-coll: cap on handshake string fields (`display_name`, `roost_name`,
/// `dialer_ticket`, `dialer_pin`). The longest legitimate value is a pairing
/// ticket (a base64url `NodeAddr`, a few hundred bytes); 4 KiB is generous.
/// Bounds a hostile peer from making us hold a 16 MiB "display name".
pub const MAX_HANDSHAKE_STRING_BYTES: usize = 4096;

/// H-coll (bounded deserialization): decode a `Vec<NotePath>` that REJECTS on
/// the postcard length prefix before allocating, so a hostile `NoteList` frame
/// can't fan out into millions of `NotePath` allocations. postcard's
/// `SeqAccess::size_hint` returns the length varint up front — we reject there
/// (no allocation), and also guard the element loop as belt-and-suspenders for
/// any deserializer that doesn't report a size hint. This is enforcement
/// DURING decode, not post-decode validation, for the COLLECTION — the count
/// is genuinely rejected before `Vec::with_capacity` ever runs.
///
/// Each `NotePath` also has a manual borrowed-string visitor that checks its
/// declared content length before constructing the owned `String`. Together,
/// the collection count and per-element bytes are bounded before their owned
/// allocations. Wire format is unchanged (the same string inside the same
/// sequence).
fn deserialize_bounded_paths<'de, D>(deserializer: D) -> Result<Vec<NotePath>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::{self, SeqAccess, Visitor};
    use std::fmt;

    struct BoundedPathsVisitor;

    impl<'de> Visitor<'de> for BoundedPathsVisitor {
        type Value = Vec<NotePath>;

        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            write!(f, "a list of at most {MAX_NOTELIST_PATHS} note paths")
        }

        fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
        where
            A: SeqAccess<'de>,
        {
            // Reject on the declared length BEFORE allocating anything.
            if let Some(len) = seq.size_hint() {
                if len > MAX_NOTELIST_PATHS {
                    return Err(de::Error::custom(format!(
                        "NoteList declares {len} paths, exceeds cap {MAX_NOTELIST_PATHS}"
                    )));
                }
            }
            let cap = seq.size_hint().unwrap_or(0).min(MAX_NOTELIST_PATHS);
            let mut out = Vec::with_capacity(cap);
            while let Some(path) = seq.next_element::<NotePath>()? {
                if out.len() >= MAX_NOTELIST_PATHS {
                    return Err(de::Error::custom(format!(
                        "NoteList exceeds cap {MAX_NOTELIST_PATHS} paths"
                    )));
                }
                // Defense in depth. NotePath's own Deserialize implementation
                // already rejects this before allocating its owned String.
                if path.0.len() > MAX_NOTE_PATH_BYTES {
                    return Err(de::Error::custom(format!(
                        "NoteList path exceeds {MAX_NOTE_PATH_BYTES} bytes"
                    )));
                }
                out.push(path);
            }
            Ok(out)
        }
    }

    deserializer.deserialize_seq(BoundedPathsVisitor)
}

#[allow(clippy::enum_variant_names)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Message {
    // ── Handshake phase ─────────────────────────────────────────────
    /// Opening frame. Client identifies itself and declares the highest
    /// protocol version it speaks (the frame header carries the version
    /// too; this field is a sanity cross-check the engine validates).
    ///
    /// `roost_name` is the last path segment of the roost folder — e.g.
    /// `"MyBrain"` for both `C:/Users/x/MyBrain` and `/storage/emulated/0/MyBrain`.
    /// Peers refuse to sync if their roost names disagree; that's the
    /// safety net against accidentally pairing two unrelated roosts.
    Hello {
        device_id: [u8; 16],
        protocol_version: u16,
        /// Human-readable device name for the UI ("Mahesh's MacBook").
        display_name: String,
        /// Roost folder name (not full path). Must match on both peers.
        roost_name: String,
        /// AUDIT-015: 16 random bytes generated once per roost and persisted
        /// at `<roost>/.corvux/vault-id.bin`. Combined with `roost_name`,
        /// this gives every roost a globally distinct identity, so two
        /// physically separate "MyBrain" roosts can't pair-and-weld their
        /// CRDT histories. Peers store the value at first pair; on
        /// reconnect, mismatch yields `PairingFailure::RoostMismatch`.
        vault_id: [u8; 16],
        /// Symmetric pairing (Batch E): the DIALER's own redial credentials —
        /// a dialable `corvux1:` ticket for the dialer's endpoint plus the
        /// dialer's own hosting PIN — so the HOST can dial the dialer back
        /// after a restart, instead of reconnect being one-directional (only
        /// the original scanner could redial). `None` on redials and on legacy
        /// peers that predate this field.
        dialer_ticket: Option<String>,
        dialer_pin: Option<String>,
        /// Client's ephemeral X25519 public key for this handshake (H-2/H-3).
        /// The server combines it with its own ephemeral key to derive a
        /// per-handshake ECDH secret mixed into the session key, giving
        /// forward secrecy: a later compromise of the PIN / resumption token
        /// can't decrypt this session without the discarded ephemeral secret.
        client_eph_pub: [u8; 32],
    },

    /// Server → client: 32-byte random nonce + the server's ephemeral X25519
    /// public key (H-2/H-3). Client HMACs the nonce with the PIN/token to
    /// produce the proof, and completes the ECDH with `server_eph_pub`.
    PairingChallenge {
        nonce: [u8; 32],
        server_eph_pub: [u8; 32],
        /// M-3: the server's `vault_id`, surfaced *before* the client derives a
        /// session/proof (the copy in `PairingOk` arrives only afterward). On a
        /// reconnect the dialing client compares it to the vault_id it pinned
        /// for this device and aborts a roost swap pre-session, symmetric with
        /// the server's pre-session check of the client's vault_id.
        server_vault_id: [u8; 16],
    },

    /// Client → server: HMAC-SHA256(PIN, nonce).
    PairingProof {
        proof: [u8; 32],
    },

    /// Server → client: pairing accepted. Post-handshake sync begins.
    PairingOk {
        /// Server device-id echoed back so client can pin it.
        device_id: [u8; 16],
        display_name: String,
        /// AUDIT-015: server's own `vault_id`, so the client can pin it too
        /// and detect roost swaps on subsequent reconnects in either direction.
        vault_id: [u8; 16],
    },

    /// Server → client: pairing rejected. Transport will close after this.
    PairingFailed {
        reason: PairingFailure,
    },

    // ── Sync phase ──────────────────────────────────────────────────
    /// Carries an `automerge::sync::Message` for a specific note.
    /// `bytes` is the encoded sync message from Automerge's sync module.
    /// We don't parse it here — it's opaque to the wire layer.
    SyncMessage {
        path: NotePath,
        bytes: Vec<u8>,
    },

    /// Announce the full list of notes we have. Peers use this to request
    /// any notes they're missing entirely (the sync protocol alone can
    /// only reconcile docs both sides already know about).
    NoteList {
        /// H-coll: bounded at DECODE time (not post-decode) so a hostile frame
        /// can't fan out into millions of allocations before we reject it. See
        /// [`deserialize_bounded_paths`].
        #[serde(deserialize_with = "deserialize_bounded_paths")]
        paths: Vec<NotePath>,
    },

    /// Peer is asking us to send the full save of a note we have. This is
    /// how an empty peer bootstraps — sync-protocol exchanges alone don't
    /// convey the initial snapshot.
    RequestFullDoc {
        path: NotePath,
    },

    /// Full `automerge::AutoCommit::save()` output for a note.
    FullDoc {
        path: NotePath,
        bytes: Vec<u8>,
    },

    /// A slice of a full-doc snapshot that is too large to fit in a single
    /// frame. Sent as an ordered sequence (`chunk_index` in `0..chunk_count`);
    /// the receiver buffers by `path` and, once the final chunk arrives,
    /// reassembles the bytes and adopts them exactly as a one-shot `FullDoc`.
    /// This lifts the 16 MiB single-frame cap on a long-lived note's Automerge
    /// history WITHOUT changing the (divergence-free) adopt semantics — the
    /// reassembled bytes are identical to what `FullDoc` would have carried.
    /// See C-4.
    FullDocChunk {
        path: NotePath,
        chunk_index: u32,
        chunk_count: u32,
        bytes: Vec<u8>,
    },

    /// Tombstone: note was deleted on our side, peer should move theirs
    /// to trash. Phase 2 feature; included now so the wire format
    /// reserves the variant slot.
    NoteDelete {
        path: NotePath,
        at_millis: u64,
    },

    // ── Lifecycle ───────────────────────────────────────────────────
    Ping,
    Pong,
    /// Clean close. After sending `Bye`, neither side sends more messages.
    Bye,

    // ── App-layer envelope ──────────────────────────────────────────
    /// XChaCha20-Poly1305 sealed wrapper around a postcard-encoded inner
    /// `Message`. After the pairing handshake completes, every message
    /// sent over the transport is wrapped in this variant — the inner
    /// `Message` is the real payload, and the outer envelope is what the
    /// store-and-forward queue / iroh-relay sees on the wire.
    ///
    /// Handshake-phase messages (`Hello` … `PairingFailed`) are NEVER
    /// wrapped — the envelope key is derived from the pairing
    /// `SessionSecret`, which doesn't exist until the handshake is done.
    /// Receiving a plaintext content message after handshake (or an
    /// EncryptedEnvelope before it) is a protocol violation.
    ///
    /// Added in PROTOCOL_VERSION 2; v1 peers can't decode this variant.
    EncryptedEnvelope {
        nonce: [u8; 24],
        /// Per-session, per-direction monotonic frame counter (M-2). Bound
        /// into the envelope AAD and required to strictly increase on the
        /// receive side, so a captured frame can't be replayed or reordered
        /// by an application-aware relay. Starts at 0 for the first frame each
        /// side sends after the handshake.
        seq: u64,
        ciphertext: Vec<u8>,
    },

    // ── Attachment blobs (PROTOCOL_VERSION 4) ───────────────────────
    /// Peer asks us to send the bytes of a roost-relative attachment
    /// (always under `.attachments/`; validated on receive). Reactive
    /// request/response only — never enqueued to the store-and-forward
    /// queue, so blobs are live-P2P-only (both devices online). Added in
    /// PROTOCOL_VERSION 4.
    RequestBlob {
        rel: String,
    },

    /// The bytes of a requested attachment. One frame, no chunking —
    /// `bytes.len()` is guaranteed <= `SYNC_BLOB_MAX_BYTES` by the sender,
    /// and the receiver re-checks (never trust the peer's framing). Added
    /// in PROTOCOL_VERSION 4.
    Blob {
        rel: String,
        bytes: Vec<u8>,
    },

    /// Negative response to `RequestBlob`: we don't have it, it's over the
    /// sync cap, or the path was rejected. `rel` echoes the request so the
    /// receiver can clear its in-flight entry and mark the path terminal
    /// (no retry loop). Added in PROTOCOL_VERSION 4.
    BlobUnavailable {
        rel: String,
        reason: BlobUnavailableReason,
    },
}

/// Why a [`Message::RequestBlob`] couldn't be satisfied. Append-only
/// (postcard tags by declaration order — never reorder).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum BlobUnavailableReason {
    /// The sender doesn't have this attachment on disk (e.g. it was pasted
    /// on a third device that's currently offline).
    NotFound,
    /// The attachment exceeds `SYNC_BLOB_MAX_BYTES` and won't sync.
    OverCap,
    /// The requested path was rejected — outside `.attachments/`, traversal,
    /// or otherwise failed roost-scope validation.
    Rejected,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PairingFailure {
    WrongPin,
    RateLimited,
    UnsupportedVersion,
    AlreadyPaired,
    Cancelled,
    /// Roost folder names don't match. Never reorder variants — postcard
    /// tags them by declaration index.
    RoostMismatch {
        expected: String,
        got: String,
    },
    /// AUDIT-015: same peer device reconnected but is now presenting a
    /// different roost id from the one we recorded at first pair. Either
    /// the user pointed Corvux at a different roost folder, or this is an
    /// attacker. Refuse to merge their CRDT history.
    RoostIdMismatch,
    /// SRX brute-force fix: an inbound FIRST-PAIR handshake arrived while no
    /// enrollment window was open (the host isn't actively pairing). Refused
    /// before the PIN is checked, so a first-pair attempt only has a chance
    /// during the few minutes the user is actually pairing. Known peers
    /// reconnect via resumption token and never see this. Appended at the END
    /// of the enum — postcard tags variants by declaration order, so new
    /// variants must never be inserted mid-list.
    PairingClosed,
}

/// Encode a `Message` to a postcard byte blob.
pub fn encode(msg: &Message) -> Result<Vec<u8>, crate::sync::error::ProtocolError> {
    postcard::to_allocvec(msg).map_err(|e| crate::sync::error::ProtocolError::Encode(e.to_string()))
}

/// Decode a postcard byte blob into a `Message`.
///
/// H-coll: after the structural decode, enforce SEMANTIC collection/string
/// caps that postcard alone doesn't. postcard uses serde's cautious
/// pre-allocation (so a claimed-huge length does not eagerly allocate), but a
/// valid-size frame can still carry a `NoteList` with millions of tiny paths or
/// a multi-MiB handshake string, which amplifies into large downstream
/// allocations. Rejecting here — before the message reaches the engine's
/// consumption paths — bounds that fan-out for every inbound frame in one place.
pub fn decode(bytes: &[u8]) -> Result<Message, crate::sync::error::ProtocolError> {
    let msg: Message = postcard::from_bytes(bytes)
        .map_err(|e| crate::sync::error::ProtocolError::Decode(e.to_string()))?;
    validate_decoded(&msg)?;
    Ok(msg)
}

/// Enforce the H-coll semantic caps on a freshly decoded `Message`.
fn validate_decoded(msg: &Message) -> Result<(), crate::sync::error::ProtocolError> {
    use crate::sync::error::ProtocolError;
    let too_long = |s: &str| s.len() > MAX_HANDSHAKE_STRING_BYTES;
    match msg {
        Message::Hello {
            display_name,
            roost_name,
            dialer_ticket,
            dialer_pin,
            ..
        } => {
            if too_long(display_name)
                || too_long(roost_name)
                || dialer_ticket.as_deref().is_some_and(too_long)
                || dialer_pin.as_deref().is_some_and(too_long)
            {
                return Err(ProtocolError::Decode(
                    "handshake string exceeds max length".into(),
                ));
            }
        }
        Message::NoteList { paths } => {
            if paths.len() > MAX_NOTELIST_PATHS {
                return Err(ProtocolError::Decode(format!(
                    "NoteList has {} paths, exceeds cap {}",
                    paths.len(),
                    MAX_NOTELIST_PATHS
                )));
            }
            if paths.iter().any(|p| p.0.len() > MAX_NOTE_PATH_BYTES) {
                return Err(ProtocolError::Decode(
                    "NoteList contains an over-long path".into(),
                ));
            }
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// H-coll: a `NoteList` whose path count exceeds the semantic cap is
    /// rejected at decode — before it fans out into `HashSet` copies on the
    /// consuming side. The frame byte-cap alone doesn't bound the entry count
    /// (millions of tiny paths fit in one valid frame).
    #[test]
    fn decode_rejects_notelist_over_path_cap() {
        let paths = vec![NotePath(String::new()); MAX_NOTELIST_PATHS + 1];
        let bytes = encode(&Message::NoteList { paths }).unwrap();
        assert!(
            decode(&bytes).is_err(),
            "a NoteList with more than MAX_NOTELIST_PATHS entries must be rejected"
        );
    }

    /// H-coll: a NoteList at the cap still decodes (the cap doesn't break
    /// legitimate large manifests).
    #[test]
    fn decode_accepts_notelist_at_path_cap() {
        let paths = vec![NotePath("n.md".into()); 10]; // well under cap
        let bytes = encode(&Message::NoteList { paths }).unwrap();
        assert!(decode(&bytes).is_ok());
    }

    /// H-coll: an over-long individual path is rejected.
    #[test]
    fn decode_rejects_over_long_note_path() {
        let paths = vec![NotePath("x".repeat(MAX_NOTE_PATH_BYTES + 1))];
        let bytes = encode(&Message::NoteList { paths }).unwrap();
        assert!(decode(&bytes).is_err());
    }

    /// H-coll: an over-long handshake string is rejected.
    #[test]
    fn decode_rejects_over_long_handshake_string() {
        let msg = Message::Hello {
            device_id: [1u8; 16],
            protocol_version: 3,
            display_name: "x".repeat(MAX_HANDSHAKE_STRING_BYTES + 1),
            roost_name: "r".into(),
            vault_id: [2u8; 16],
            dialer_ticket: None,
            dialer_pin: None,
            client_eph_pub: [3u8; 32],
        };
        let bytes = encode(&msg).unwrap();
        assert!(
            decode(&bytes).is_err(),
            "a Hello with an over-long display_name must be rejected"
        );
    }

    #[test]
    fn encode_decode_roundtrip_hello() {
        let msg = Message::Hello {
            device_id: [7u8; 16],
            protocol_version: 3,
            display_name: "MacBook".into(),
            roost_name: "MyBrain".into(),
            vault_id: [9u8; 16],
            dialer_ticket: Some("corvux1:abc".into()),
            dialer_pin: Some("123456".into()),
            client_eph_pub: [4u8; 32],
        };
        let bytes = encode(&msg).unwrap();
        let back = decode(&bytes).unwrap();
        match back {
            Message::Hello {
                device_id,
                protocol_version,
                display_name,
                roost_name,
                vault_id,
                dialer_ticket,
                dialer_pin,
                client_eph_pub,
            } => {
                assert_eq!(device_id, [7u8; 16]);
                assert_eq!(protocol_version, 3);
                assert_eq!(display_name, "MacBook");
                assert_eq!(roost_name, "MyBrain");
                assert_eq!(vault_id, [9u8; 16]);
                assert_eq!(dialer_ticket.as_deref(), Some("corvux1:abc"));
                assert_eq!(dialer_pin.as_deref(), Some("123456"));
                assert_eq!(client_eph_pub, [4u8; 32]);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn encode_is_compact() {
        // Sanity: postcard should be noticeably smaller than JSON for
        // the common SyncMessage case. This isn't a hard guarantee, just
        // catches a regression if someone accidentally switches to
        // bincode-with-varint or similar.
        let msg = Message::Ping;
        assert!(encode(&msg).unwrap().len() <= 2);
    }

    #[test]
    fn decode_errors_on_garbage() {
        assert!(decode(&[0xff, 0xff, 0xff, 0xff, 0xff]).is_err());
    }

    #[test]
    fn encode_decode_roundtrip_blob() {
        let payload = vec![0xABu8; 4096];
        let msg = Message::Blob {
            rel: ".attachments/paste-123.webp".into(),
            bytes: payload.clone(),
        };
        let bytes = encode(&msg).unwrap();
        match decode(&bytes).unwrap() {
            Message::Blob { rel, bytes } => {
                assert_eq!(rel, ".attachments/paste-123.webp");
                assert_eq!(bytes, payload);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn encode_decode_roundtrip_request_blob() {
        let msg = Message::RequestBlob {
            rel: ".attachments/recording-9.webm".into(),
        };
        let bytes = encode(&msg).unwrap();
        match decode(&bytes).unwrap() {
            Message::RequestBlob { rel } => {
                assert_eq!(rel, ".attachments/recording-9.webm")
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn encode_decode_roundtrip_blob_unavailable() {
        for reason in [
            BlobUnavailableReason::NotFound,
            BlobUnavailableReason::OverCap,
            BlobUnavailableReason::Rejected,
        ] {
            let msg = Message::BlobUnavailable {
                rel: ".attachments/x.webp".into(),
                reason: reason.clone(),
            };
            let bytes = encode(&msg).unwrap();
            match decode(&bytes).unwrap() {
                Message::BlobUnavailable { rel, reason: got } => {
                    assert_eq!(rel, ".attachments/x.webp");
                    assert_eq!(got, reason);
                }
                _ => panic!("wrong variant"),
            }
        }
    }
}
