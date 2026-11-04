//! Length-prefixed framing with an explicit protocol version.
//!
//! Wire format per frame:
//!
//! ```text
//!   offset  size  field
//!   0       4     payload_length (u32, big-endian; does NOT include these 4 bytes)
//!   4       2     protocol_version (u16, big-endian)
//!   6       N     postcard-encoded `Message`
//! ```
//!
//! The `payload_length` covers `protocol_version + postcard bytes` so the
//! reader can `read_exact(payload_length)` in one call after the header.
//!
//! # Why version per frame
//!
//! The cheaper alternative is "version once at handshake". But putting it on
//! every frame means a mid-stream protocol upgrade (e.g. after a resumed
//! TLS session) is an unambiguous decode decision, not a guess at what the
//! peer remembers sending. The 2-byte-per-frame overhead is negligible next
//! to Automerge change payloads.
//!
//! # Bounds
//!
//! `MAX_FRAME_BYTES = 16 MiB`. Any single frame larger than this is treated
//! as a malicious or buggy peer and tears down the connection. A 16 MiB cap
//! is generous for Automerge change batches (typical note: few hundred
//! bytes; worst case: dozens of KB); lifting it later is a protocol-version
//! bump, not a silent config change.

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::sync::error::TransportError;

/// Wire-protocol version.
///
/// - **v1 → v2**: introduced the app-layer `EncryptedEnvelope`.
/// - **v2 → v3** (AUDIT-015): `Hello` now carries a 16-byte `vault_id` and
///   the server may reject with `PairingFailure::RoostMismatch` if it had
///   previously paired with this peer under a different roost id. Two
///   roosts that happen to share a `roost_name` ("MyBrain") but otherwise
///   contain different content can no longer pair-and-weld their CRDT
///   histories together silently.
/// - **v3 → v4**: attachment blob sync — `RequestBlob` / `Blob` /
///   `BlobUnavailable`. Exact-version-match handshake (no negotiation) means
///   any peer that pairs is guaranteed to speak blobs; no per-peer gating.
///   Blobs are reactive request/response, live-P2P-only, never queued.
///
/// Existing paired devices must update both ends; the auto-reconnect path
/// will surface "unsupported version" until they do.
pub const PROTOCOL_VERSION: u16 = 6;

/// 16 MiB — hard cap on a single frame. See module docs.
pub const MAX_FRAME_BYTES: u32 = 16 * 1024 * 1024;

/// Write one frame to the stream. `payload` must already be postcard-encoded.
pub async fn write_frame<W: AsyncWrite + Unpin>(
    mut w: W,
    version: u16,
    payload: &[u8],
) -> Result<(), TransportError> {
    // payload_length = 2 (version) + payload.len()
    let total = 2u64 + payload.len() as u64;
    if total > MAX_FRAME_BYTES as u64 {
        return Err(TransportError::FrameTooLarge(total as u32));
    }
    w.write_u32(total as u32).await?;
    w.write_u16(version).await?;
    w.write_all(payload).await?;
    w.flush().await?;
    Ok(())
}

/// Read one frame from the stream. Returns `(version, payload_bytes)`.
/// Guarantees payload.len() <= MAX_FRAME_BYTES - 2.
pub async fn read_frame<R: AsyncRead + Unpin>(mut r: R) -> Result<(u16, Vec<u8>), TransportError> {
    let len = r.read_u32().await.map_err(|e| {
        // A clean EOF shows up as `UnexpectedEof`; surface as `Closed` so
        // the engine can distinguish "peer left" from real I/O failure.
        if e.kind() == std::io::ErrorKind::UnexpectedEof {
            TransportError::Closed
        } else {
            TransportError::Io(e)
        }
    })?;
    if len < 2 {
        // Below header minimum — can't even encode a version.
        return Err(TransportError::MalformedFrame);
    }
    if len > MAX_FRAME_BYTES {
        return Err(TransportError::FrameTooLarge(len));
    }
    let version = r.read_u16().await?;
    let payload_len = (len - 2) as usize;
    let mut buf = vec![0u8; payload_len];
    r.read_exact(&mut buf).await?;
    Ok((version, buf))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    #[tokio::test]
    async fn roundtrip_small_frame() {
        let (a, b) = duplex(1024);
        let (mut a_r, a_w) = tokio::io::split(a);
        let (_b_r, mut b_w) = tokio::io::split(b);

        tokio::spawn(async move {
            write_frame(&mut b_w, PROTOCOL_VERSION, b"hello")
                .await
                .unwrap();
        });

        let (ver, payload) = read_frame(&mut a_r).await.unwrap();
        assert_eq!(ver, PROTOCOL_VERSION);
        assert_eq!(payload, b"hello");
        drop(a_w);
    }

    #[tokio::test]
    async fn read_rejects_oversize_header() {
        let (a, b) = duplex(64);
        let (mut a_r, _a_w) = tokio::io::split(a);
        let (_b_r, mut b_w) = tokio::io::split(b);

        tokio::spawn(async move {
            // Claim a ridiculously large frame. Reader must bail before
            // allocating or reading the body.
            b_w.write_u32(MAX_FRAME_BYTES + 1).await.unwrap();
            b_w.write_u16(PROTOCOL_VERSION).await.unwrap();
            // Intentionally no body — reader should never get here.
        });

        let err = read_frame(&mut a_r).await.unwrap_err();
        assert!(matches!(err, TransportError::FrameTooLarge(_)));
    }

    #[tokio::test]
    async fn read_rejects_malformed_too_short() {
        let (a, b) = duplex(64);
        let (mut a_r, _a_w) = tokio::io::split(a);
        let (_b_r, mut b_w) = tokio::io::split(b);

        tokio::spawn(async move {
            b_w.write_u32(1).await.unwrap(); // can't fit a version
            b_w.write_u8(0).await.unwrap();
        });

        let err = read_frame(&mut a_r).await.unwrap_err();
        assert!(matches!(err, TransportError::MalformedFrame));
    }

    #[tokio::test]
    async fn read_detects_clean_close() {
        let (a, b) = duplex(64);
        let (mut a_r, _a_w) = tokio::io::split(a);
        drop(b); // peer closes before sending anything

        let err = read_frame(&mut a_r).await.unwrap_err();
        assert!(matches!(err, TransportError::Closed));
    }

    #[tokio::test]
    async fn write_rejects_oversize_payload() {
        let (_a, b) = duplex(64);
        let (_b_r, mut b_w) = tokio::io::split(b);

        let big = vec![0u8; MAX_FRAME_BYTES as usize]; // header adds 2 → over
        let err = write_frame(&mut b_w, PROTOCOL_VERSION, &big)
            .await
            .unwrap_err();
        assert!(matches!(err, TransportError::FrameTooLarge(_)));
    }
}
