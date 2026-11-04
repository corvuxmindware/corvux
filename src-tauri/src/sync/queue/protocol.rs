// Default queue limits (MAX_MESSAGE_BYTES, DEFAULT_RECIPIENT_PENDING_CAP_BYTES,
// DEFAULT_SENDER_DAILY_UPLOAD_CAP_BYTES, DEFAULT_MESSAGE_TTL_SECS) are part of
// the wire protocol's public contract — consumed by the queue service, which
// is not part of this repository, so some constants look dead here. They're not.
#![allow(dead_code)]

//! Wire protocol for the store-and-forward queue.
//!
//! All request / response bodies cross the HTTP boundary as
//! `application/postcard` (compact binary, same encoding family the sync
//! protocol already uses). JSON would have been a defensible alternative
//! for HTTP introspectability, but consistent encoding across both pipes
//! minimises the number of moving parts a future deploy has to harden.
//!
//! # Authentication
//!
//! Stateless challenge-response, NodeId-bound:
//!
//! 1. Client → `POST /v1/auth/challenge` with their NodeId hex. Server
//!    returns a [`AuthChallenge`] containing a 32-byte nonce and a short
//!    expiry timestamp.
//! 2. Client signs `nonce || expiry_unix_secs` with their Ed25519 secret
//!    key. Sends the signature in `POST /v1/auth/verify`.
//! 3. Server verifies the signature against the NodeId's pubkey and
//!    returns an [`AuthToken`] (32-byte opaque bearer token) valid for a
//!    short window (default 1h).
//! 4. Subsequent requests carry `Authorization: Bearer <token-hex>`.
//!
//! All operations after `/auth/verify` are scoped to the authenticated
//! NodeId — push uses it as the *sender*, pull/manifest use it as the
//! *recipient*. The server never trusts a NodeId in the request body
//! over the one bound to the bearer.

use serde::{Deserialize, Serialize};
use serde_big_array::BigArray;

/// Wire-format version. Bumped on incompatible changes; both sides reject
/// anything they don't speak.
pub const QUEUE_PROTOCOL_VERSION: u16 = 1;

/// 32-byte Ed25519 NodeId — same wire form as iroh's `NodeId`. Kept as a
/// raw byte array so this crate doesn't need to depend on iroh.
pub type NodeIdBytes = [u8; 32];

/// 16-byte opaque message ID assigned by the server on push. Hex-encoded
/// in URL path components (`GET /v1/pull/:id` → 32 hex chars).
pub type MessageIdBytes = [u8; 16];

/// Single message size limit. Matches the existing transport frame cap so
/// a CRDT save that would have fit on the wire also fits in the queue.
pub const MAX_MESSAGE_BYTES: usize = 16 * 1024 * 1024;

/// Per-recipient pending storage cap. When exceeded, the server drops the
/// **oldest** pending message rather than refusing the new one — the
/// freshest data is what the recipient actually needs.
pub const DEFAULT_RECIPIENT_PENDING_CAP_BYTES: u64 = 50 * 1024 * 1024;

/// Fixed per-entry storage overhead charged against the byte cap on top of the
/// ciphertext length. Each pending message also persists a 48-byte redb key
/// plus a `MessageMeta` row (~90 B) plus B-tree overhead; if the cap counted
/// ciphertext only, an attacker pushing 1-byte ciphertexts with distinct dedup
/// keys could create tens of millions of rows and balloon the DB to multiple
/// GB while staying "under cap." Charging a flat overhead bounds that. See C-3.
pub const QUEUE_ENTRY_OVERHEAD_BYTES: u64 = 256;

/// Per-recipient pending **count** cap, enforced alongside the byte cap.
/// Defence-in-depth against the storage-amplification vector: even if every
/// ciphertext is tiny, no recipient can accumulate more than this many pending
/// entries (oldest evicted first). See C-3.
pub const DEFAULT_RECIPIENT_PENDING_CAP_COUNT: usize = 20_000;

/// Maximum entries returned by a single `/v1/manifest` response. The server
/// returns the oldest N pending entries (FIFO drain); the client pulls +
/// deletes them and the next manifest returns the next batch. Bounds the CPU /
/// serialization cost of a manifest under a full queue and the client's
/// head-of-line pull loop. Truncation is logged, never silent. See M-8.
pub const MAX_MANIFEST_ENTRIES: usize = 512;

/// Per-sender daily upload cap. Soft abuse cap; resets at UTC midnight.
pub const DEFAULT_SENDER_DAILY_UPLOAD_CAP_BYTES: u64 = 100 * 1024 * 1024;

/// Default TTL on every queued message — 30 days per the strategy doc.
/// Server enforces by walking the store on a GC tick and dropping
/// entries whose `expires_at_unix_secs` is in the past.
pub const DEFAULT_MESSAGE_TTL_SECS: u64 = 30 * 24 * 60 * 60;

/// GLOBAL durable-storage ceiling across ALL senders and recipients (B3).
/// Every other cap is keyed on an attacker-controlled principal — the
/// per-sender daily cap on the authenticated NodeId, the pending cap on the
/// recipient — so a Sybil attacker who mints unlimited free NodeIds and
/// rotates recipient ids can force `N × per-principal-cap` of durable storage
/// with no code-level bound. This is the only cap that bounds the SUM. When a
/// push would carry total buffered storage past this ceiling it is REJECTED
/// (429) rather than triggering global eviction — dropping a legitimate
/// recipient's messages to make room for a flood would let the attacker
/// weaponise eviction. The 30-day TTL + pulls free space, so the condition is
/// self-healing. Operator-tunable via `QueueLimits`; size the service disk to
/// comfortably exceed it. NOTE: this bounds the SERVER (disk exhaustion /
/// crash) but does not by itself defeat Sybil availability abuse — a
/// determined attacker can still occupy the ceiling and starve legitimate
/// pushes until TTL. Closing THAT requires a front-of-server per-source rate
/// limit or an entitlement/allowlist layer, which is an ops/product decision
/// (and must not become an account-login flow — Locked Decision #6).
pub const DEFAULT_MAX_TOTAL_STORAGE_BYTES: u64 = 5 * 1024 * 1024 * 1024;

// ── /v1/auth/challenge ───────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthChallengeRequest {
    pub protocol_version: u16,
    pub node_id: NodeIdBytes,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthChallenge {
    /// Random 32-byte challenge. Client must sign `nonce || expiry_le`.
    pub nonce: [u8; 32],
    /// Wall-clock UNIX seconds after which the server will refuse to
    /// honour a verify against this challenge. Bound into the signed
    /// payload so a stolen challenge can't be replayed indefinitely.
    pub expires_at_unix_secs: u64,
}

// ── /v1/auth/verify ──────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthVerifyRequest {
    pub protocol_version: u16,
    pub node_id: NodeIdBytes,
    /// Echo of the challenge nonce — server pairs it back to the
    /// stored expiry without keeping per-NodeId state.
    pub nonce: [u8; 32],
    /// Echoed expiry (same value the server returned).
    pub expires_at_unix_secs: u64,
    /// Ed25519 signature over `nonce || expires_at_unix_secs.to_le_bytes()`.
    #[serde(with = "BigArray")]
    pub signature: [u8; 64],
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthToken {
    /// Opaque 32-byte bearer. Hex-encoded in the `Authorization: Bearer`
    /// header. Random — not derivable from NodeId so logs don't leak.
    pub token: [u8; 32],
    /// Token expiry as the **server's** absolute wall clock. Retained for
    /// server-side introspection/logging; clients must NOT convert this to
    /// their own clock (see `expires_in_secs`).
    pub expires_at_unix_secs: u64,
    /// Token lifetime as a **duration** from the moment the server issued it.
    /// Clients anchor this to a local monotonic `Instant` (`Instant::now() +
    /// expires_in_secs`) so token refresh is immune to client/server wall-clock
    /// skew — deriving a deadline from `expires_at_unix_secs − client_now`
    /// mixes two clocks and a slow client over-holds the token → spurious 401s.
    /// See L-9.
    pub expires_in_secs: u64,
}

// ── /v1/push ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PushRequest {
    pub protocol_version: u16,
    /// Recipient's NodeId. Server stores under this key.
    pub recipient: NodeIdBytes,
    /// Opaque message-class tag — used for de-duplication on the
    /// server side. Same `(recipient, dedup_key)` from the same sender
    /// replaces any prior pending message. Empty = no de-dup; every
    /// push appends.
    ///
    /// The live client computes this as a 16-byte HMAC over the note
    /// path under the per-pair queue key (see `sync::queue::dedup_tag`).
    /// The server only sees the opaque tag — it never learns the
    /// plaintext path, even though it can still collapse rapid edits
    /// to the same note into a single pending entry (because the same
    /// path under the same pair key produces the same tag). See
    /// AUDIT-001 in `docs/audit-2026-05-22-tasks.md`.
    pub dedup_key: Vec<u8>,
    /// Encrypted CRDT payload. The queue server never decrypts —
    /// content key lives only on the two endpoints.
    pub ciphertext: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PushResponse {
    pub message_id: MessageIdBytes,
    pub expires_at_unix_secs: u64,
}

// ── /v1/revoke ───────────────────────────────────────────────────────

/// Purge every message the *authenticated caller* (as sender) has pending for
/// `recipient`. Sent when a device unpairs a peer (H-3): the caller can no
/// longer talk to that peer, so any ciphertext it already pushed for them
/// should not sit on the server for up to the 30-day TTL waiting to be pulled
/// by a device the caller has revoked.
///
/// Authorization is by the bearer token, which is bound to the *sender*
/// NodeId — so a caller can only ever purge messages **it itself sent**, never
/// another sender's. `recipient` is the only body field; the sender is never
/// trusted from the body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RevokeRequest {
    pub protocol_version: u16,
    /// The peer being unpaired — whose inbox we clear of our own messages.
    pub recipient: NodeIdBytes,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RevokeResponse {
    pub protocol_version: u16,
    /// How many pending messages were purged. Advisory (for logs); the client
    /// treats revoke as best-effort.
    pub purged: u64,
}

// ── /v1/manifest ─────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestEntry {
    pub message_id: MessageIdBytes,
    pub sender: NodeIdBytes,
    pub size_bytes: u64,
    pub queued_at_unix_secs: u64,
    pub expires_at_unix_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestResponse {
    pub protocol_version: u16,
    pub entries: Vec<ManifestEntry>,
}

// ── Plaintext payload (sender → recipient, never seen by server) ─────

/// What's inside the encrypted blob each push carries. Sender postcard-
/// encodes this, encrypts with the per-pair queue key, and sends the
/// resulting ciphertext over to the queue. Recipient does the inverse.
///
/// `doc_save` is `automerge::AutoCommit::save()` output for the note —
/// the same wire shape `Message::FullDoc` carries on the live transport.
/// We send full saves rather than incremental sync messages because
/// computing a sync message offline requires per-peer Automerge state
/// the engine doesn't currently persist; sending the whole save is
/// O(doc-size) per push but convergence-correct.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueuePayload {
    /// Roost-relative forward-slash path. `String` rather than the
    /// `NotePath` newtype so this module doesn't depend on the sync
    /// protocol layer.
    pub path: String,
    /// Full Automerge save. Receiver merges into local doc.
    pub doc_save: Vec<u8>,
}

// ── Errors ───────────────────────────────────────────────────────────

/// Server-side failure surface. Maps onto HTTP status codes in the axum
/// handler; clients reify back into a typed error.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum QueueErrorCode {
    /// Wire-format version mismatch. Bumping the constant would force
    /// this on every prior client, so the server treats it as a hard
    /// "go home and update" signal.
    UnsupportedVersion,
    /// Body postcard-decode failed.
    MalformedRequest,
    /// `Authorization` header missing, malformed, or expired.
    Unauthorized,
    /// Auth verify failed (signature mismatch or replayed/expired nonce).
    InvalidCredentials,
    /// Push payload exceeds the per-message cap.
    PayloadTooLarge,
    /// Sender hit the daily upload cap.
    SenderQuotaExhausted,
    /// Requested message ID doesn't exist (or already deleted).
    NotFound,
    /// Internal error — server is misbehaving. Client should retry
    /// after backoff; persistent failures are an ops issue, not the
    /// client's problem.
    Internal,
    /// The server's GLOBAL storage ceiling is reached (B3). Not the client's
    /// fault; retryable after backoff — space frees as messages are pulled or
    /// hit their TTL. The client's durable outbox keeps the change and retries.
    /// Appended LAST to keep postcard variant indices of the codes above stable
    /// (a mid-enum insert would renumber `NotFound`/`Internal` on the wire).
    StorageFull,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct QueueErrorBody {
    pub code: QueueErrorCode,
    pub message: String,
}

// ── Encoding helpers ─────────────────────────────────────────────────

/// `application/postcard` MIME-style content type. Used in the HTTP
/// `Content-Type` header so middleware can branch on body shape.
pub const POSTCARD_MIME: &str = "application/postcard";

/// Encode any serializable into postcard bytes.
pub fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, String> {
    postcard::to_allocvec(value).map_err(|e| e.to_string())
}

/// Decode postcard bytes into the target type.
pub fn decode<T: for<'a> Deserialize<'a>>(bytes: &[u8]) -> Result<T, String> {
    postcard::from_bytes(bytes).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_request_roundtrip() {
        let req = PushRequest {
            protocol_version: QUEUE_PROTOCOL_VERSION,
            recipient: [9u8; 32],
            dedup_key: b"notes/foo.md".to_vec(),
            ciphertext: vec![1, 2, 3, 4],
        };
        let bytes = encode(&req).unwrap();
        let back: PushRequest = decode(&bytes).unwrap();
        assert_eq!(back.recipient, [9u8; 32]);
        assert_eq!(back.dedup_key, b"notes/foo.md");
        assert_eq!(back.ciphertext, vec![1, 2, 3, 4]);
    }

    #[test]
    fn manifest_response_roundtrip() {
        let resp = ManifestResponse {
            protocol_version: QUEUE_PROTOCOL_VERSION,
            entries: vec![ManifestEntry {
                message_id: [7u8; 16],
                sender: [3u8; 32],
                size_bytes: 1234,
                queued_at_unix_secs: 1_700_000_000,
                expires_at_unix_secs: 1_700_000_000 + DEFAULT_MESSAGE_TTL_SECS,
            }],
        };
        let bytes = encode(&resp).unwrap();
        let back: ManifestResponse = decode(&bytes).unwrap();
        assert_eq!(back.entries.len(), 1);
        assert_eq!(back.entries[0].size_bytes, 1234);
    }

    #[test]
    fn error_body_roundtrip() {
        let err = QueueErrorBody {
            code: QueueErrorCode::Unauthorized,
            message: "missing bearer".into(),
        };
        let bytes = encode(&err).unwrap();
        let back: QueueErrorBody = decode(&bytes).unwrap();
        assert_eq!(back, err);
    }
}
