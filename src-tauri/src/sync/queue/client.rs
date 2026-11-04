// forget_token + IdentitySign trait for future server-rotation support
#![allow(dead_code)]

//! HTTP client for the store-and-forward queue.
//!
//! Built on `reqwest` (already in the deps tree). Holds a long-lived
//! `Identity` reference for signing the auth challenge; keeps a cached
//! bearer token until expiry and re-authenticates lazily.
//!
//! # Threading model
//!
//! Cheap to clone (`Arc` internally). Spawn one per peer? No — spawn one
//! per *queue endpoint*. The auth scope is the local NodeId, not the
//! per-pair session, so a single client can push to many recipients on
//! behalf of this device.

use std::sync::Arc;
use std::time::Duration;

use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use reqwest::Client as HttpClient;
use thiserror::Error;
use tokio::sync::Mutex;

use crate::sync::identity::Identity;

use super::protocol::*;

/// Default per-request timeout. Pushed to the conservative end because
/// every queue call already has a TTL gate behind it; if the network is
/// flaky we'd rather give up and retry on the next event than block the
/// engine for a minute.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// SYN-M3/M6 (audit follow-up) / SYN-I6: per-ENDPOINT response ceilings, not one
/// blanket cap. A single 64 MiB limit for every response closed "unbounded" but
/// let a compromised/buggy queue server make the client buffer up to 64 MiB for
/// what should be a sub-KB auth ack — the invariant calls for endpoint-specific,
/// mobile-sized bounds. Three tiers, each matching the protocol's OWN
/// server-side limit for that response shape:
///
/// - Tiny typed acks (auth challenge/token, push/delete/revoke acks — all a
///   handful of fixed-size fields): generous 4 KiB margin over an actual
///   payload of well under 100 bytes.
/// - Manifest (`ManifestResponse`, bounded server-side by `MAX_MANIFEST_ENTRIES`
///   = 512 entries × ~90 bytes/entry ≈ 46 KiB worst case): 128 KiB gives ~2.5x
///   margin without reopening the "why is this 64 MiB" question.
/// - Pull (raw ciphertext, bounded server-side by `MAX_MESSAGE_BYTES` = 16 MiB):
///   the one endpoint that legitimately needs a large cap. Mirrors the
///   queue service's own push-request headroom pattern rather than
///   inventing a new margin.
const TINY_RESPONSE_MAX_BYTES: usize = 4 * 1024;
const MANIFEST_RESPONSE_MAX_BYTES: usize = 128 * 1024;
const MESSAGE_RESPONSE_MAX_BYTES: usize = MAX_MESSAGE_BYTES + 4096;

#[derive(Debug, Error)]
pub enum QueueClientError {
    #[error("queue endpoint URL invalid: {0}")]
    InvalidUrl(String),

    #[error("http: {0}")]
    Http(#[from] reqwest::Error),

    #[error("server returned {0:?}: {1}")]
    Server(QueueErrorCode, String),

    #[error("server returned http status {0} without a typed error body: {1}")]
    UnknownServerError(u16, String),

    #[error("response body decode: {0}")]
    Decode(String),

    #[error("response body encode: {0}")]
    Encode(String),

    #[error("queue protocol version mismatch — server speaks v{server}, we speak v{client}")]
    UnsupportedVersion { server: u16, client: u16 },

    #[error("response body exceeded the {max}-byte ceiling (got at least {got})")]
    ResponseTooLarge { got: usize, max: usize },
}

pub type QueueResult<T> = Result<T, QueueClientError>;

#[derive(Clone)]
pub struct QueueClient {
    inner: Arc<QueueClientInner>,
}

struct QueueClientInner {
    /// Base URL like `https://queue.meetcorvux.com` (no trailing slash).
    base_url: String,
    http: HttpClient,
    /// The local device's identity. Used to sign the auth challenge.
    identity: Arc<Identity>,
    /// Cached bearer token; cleared on expiry or auth failure.
    token: Mutex<Option<CachedToken>>,
}

struct CachedToken {
    bearer_hex: String,
    /// Monotonic expiry deadline. Derived at issue from the token's remaining
    /// lifetime (server expiry minus client-now — both use the client clock so
    /// a constant skew cancels) and then tracked via `Instant`, which is immune
    /// to wall-clock changes. Fixes the L-9 skew that let a slow client clock
    /// keep serving a server-expired token and 401.
    expires_at: std::time::Instant,
}

/// True if the URL's host is loopback (or `localhost`). Plain http is only
/// permitted for such hosts, where there is no network path to sniff. See L-7.
fn is_loopback_host(url: &url::Url) -> bool {
    match url.host() {
        Some(url::Host::Domain(d)) => d.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    }
}

impl QueueClient {
    pub fn new(base_url: impl Into<String>, identity: Arc<Identity>) -> QueueResult<Self> {
        let mut base_url = base_url.into();
        while base_url.ends_with('/') {
            base_url.pop();
        }
        // S-02 defense-in-depth: the IPC-layer validator
        // (`commands::sync::validate_queue_endpoint`) is the primary
        // gate, but anything passing a URL into the queue client at
        // runtime (tests, future code paths) should also refuse the
        // userinfo bypass. Parse with `url::Url` and reject any URL
        // that carries credentials.
        let parsed = url::Url::parse(&base_url)
            .map_err(|_| QueueClientError::InvalidUrl(base_url.clone()))?;
        // L-7: require https for any real endpoint. Cleartext http would leak
        // the `Authorization: Bearer` token to a network sniffer (a ~15-minute
        // impersonation window). Plain http is allowed ONLY for a loopback host
        // (local dev / tests), where there is no network path to sniff.
        match parsed.scheme() {
            "https" => {}
            "http" if is_loopback_host(&parsed) => {}
            _ => return Err(QueueClientError::InvalidUrl(base_url)),
        }
        if !parsed.username().is_empty() || parsed.password().is_some() {
            return Err(QueueClientError::InvalidUrl(base_url));
        }
        let http = HttpClient::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(QueueClientError::Http)?;
        Ok(Self {
            inner: Arc::new(QueueClientInner {
                base_url,
                http,
                identity,
                token: Mutex::new(None),
            }),
        })
    }

    /// Push an encrypted payload to the queue for `recipient`.
    /// `dedup_key` lets the server collapse rapid-fire pushes from this
    /// sender into a single pending entry — typically the note path.
    pub async fn push(
        &self,
        recipient: NodeIdBytes,
        dedup_key: Vec<u8>,
        ciphertext: Vec<u8>,
    ) -> QueueResult<PushResponse> {
        let body = encode(&PushRequest {
            protocol_version: QUEUE_PROTOCOL_VERSION,
            recipient,
            dedup_key,
            ciphertext,
        })
        .map_err(QueueClientError::Encode)?;
        // PushResponse is a tiny fixed-shape ack (message_id + expiry) — the
        // large ciphertext rides the REQUEST, already bounded server-side.
        let resp_bytes = self
            .authed_post("/v1/push", body, TINY_RESPONSE_MAX_BYTES)
            .await?;
        decode::<PushResponse>(&resp_bytes).map_err(QueueClientError::Decode)
    }

    /// List messages pending delivery to *us* (this device's NodeId).
    pub async fn manifest(&self) -> QueueResult<Vec<ManifestEntry>> {
        let bytes = self
            .authed_get("/v1/manifest", MANIFEST_RESPONSE_MAX_BYTES)
            .await?;
        let resp: ManifestResponse = decode(&bytes).map_err(QueueClientError::Decode)?;
        if resp.protocol_version != QUEUE_PROTOCOL_VERSION {
            return Err(QueueClientError::UnsupportedVersion {
                server: resp.protocol_version,
                client: QUEUE_PROTOCOL_VERSION,
            });
        }
        Ok(resp.entries)
    }

    /// Stream the ciphertext for a specific message id. Caller is
    /// responsible for decrypting via the per-pair queue key — this
    /// client never touches plaintext.
    pub async fn pull(&self, message_id: MessageIdBytes) -> QueueResult<Vec<u8>> {
        let path = format!("/v1/pull/{}", hex::encode(message_id));
        // The one endpoint that legitimately returns up to MAX_MESSAGE_BYTES
        // of raw ciphertext — every other endpoint is a small typed ack.
        self.authed_get(&path, MESSAGE_RESPONSE_MAX_BYTES).await
    }

    /// Acknowledge processing of a message; server drops it from storage.
    pub async fn delete(&self, message_id: MessageIdBytes) -> QueueResult<()> {
        let url = format!(
            "{}/v1/pull/{}",
            self.inner.base_url,
            hex::encode(message_id)
        );
        // SYN-M2 twin: DELETE was the one authed verb not routed through the
        // re-auth path (authed_post/authed_get only cover POST/GET), so a 401
        // on delete wedged the outbox drain's ACK. Route it through the shared
        // helper — it gets the 401 re-auth retry AND the SYN-M3 bounded body.
        self.send_with_reauth(
            |token| {
                self.inner
                    .http
                    .delete(&url)
                    .header(AUTHORIZATION, format!("Bearer {token}"))
            },
            TINY_RESPONSE_MAX_BYTES,
        )
        .await
        .map(|_| ())
    }

    /// Purge every message this device (as sender) has pending for `recipient`
    /// on the server — called when unpairing that peer (H-3). Best-effort from
    /// the caller's side: a removed peer can still decrypt anything it already
    /// pulled, but this stops the server delivering ciphertext we queued for a
    /// device we've revoked. Returns the server's purged count (advisory).
    pub async fn revoke(&self, recipient: NodeIdBytes) -> QueueResult<u64> {
        let body = encode(&RevokeRequest {
            protocol_version: QUEUE_PROTOCOL_VERSION,
            recipient,
        })
        .map_err(QueueClientError::Encode)?;
        let resp_bytes = self
            .authed_post("/v1/revoke", body, TINY_RESPONSE_MAX_BYTES)
            .await?;
        let resp: RevokeResponse = decode(&resp_bytes).map_err(QueueClientError::Decode)?;
        Ok(resp.purged)
    }

    // ── internals ────────────────────────────────────────────────────

    async fn authed_post(&self, path: &str, body: Vec<u8>, max: usize) -> QueueResult<Vec<u8>> {
        let url = format!("{}{path}", self.inner.base_url);
        self.send_with_reauth(
            |token| {
                self.inner
                    .http
                    .post(&url)
                    .header(AUTHORIZATION, format!("Bearer {token}"))
                    .header(CONTENT_TYPE, POSTCARD_MIME)
                    .body(body.clone())
            },
            max,
        )
        .await
    }

    async fn authed_get(&self, path: &str, max: usize) -> QueueResult<Vec<u8>> {
        let url = format!("{}{path}", self.inner.base_url);
        self.send_with_reauth(
            |token| {
                self.inner
                    .http
                    .get(&url)
                    .header(AUTHORIZATION, format!("Bearer {token}"))
            },
            max,
        )
        .await
    }

    /// Send an authenticated request, re-authenticating once on a `401`
    /// (SYN-M2). Our token lifetime is a monotonic *estimate* of the server's,
    /// so a server restart, signing-key rotation, or clock skew we
    /// under-estimated can reject a token we still believe is valid. Without a
    /// retry the whole queue path (push/pull/manifest/delete/revoke) wedges
    /// until the cached token would have expired locally. On a 401 we forget
    /// the token, re-auth from scratch, and retry EXACTLY once — a second 401
    /// is a genuine auth failure (revoked device, wrong endpoint) and is
    /// surfaced. `build` is called fresh per attempt (it re-reads the token and
    /// rebuilds the request, cloning any body), so the retry carries the new
    /// bearer.
    async fn send_with_reauth<F>(&self, build: F, max: usize) -> QueueResult<Vec<u8>>
    where
        F: Fn(String) -> reqwest::RequestBuilder,
    {
        let token = self.token().await?;
        let resp = build(token).send().await?;
        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            self.forget_token().await;
            let token = self.token().await?;
            let resp = build(token).send().await?;
            return consume(resp, max).await;
        }
        consume(resp, max).await
    }

    /// Returns a fresh bearer token, authenticating if the cache is
    /// empty or expired. Token is held under a mutex so concurrent
    /// callers serialize on a single auth round-trip.
    async fn token(&self) -> QueueResult<String> {
        let mut guard = self.inner.token.lock().await;
        if let Some(cached) = guard.as_ref() {
            // Refresh a minute early so we don't race the boundary. Measured
            // monotonically (Instant), so a wrong wall clock can't make us keep
            // an expired token (L-9).
            if cached.expires_at > std::time::Instant::now() + std::time::Duration::from_secs(60) {
                return Ok(cached.bearer_hex.clone());
            }
        }
        let fresh = self.authenticate().await?;
        let bearer = fresh.bearer_hex.clone();
        *guard = Some(fresh);
        Ok(bearer)
    }

    async fn authenticate(&self) -> QueueResult<CachedToken> {
        let node_id = self.inner.identity.verifying_key().to_bytes();

        // Step 1: challenge.
        let chal_body = encode(&AuthChallengeRequest {
            protocol_version: QUEUE_PROTOCOL_VERSION,
            node_id,
        })
        .map_err(QueueClientError::Encode)?;
        let resp = self
            .inner
            .http
            .post(format!("{}/v1/auth/challenge", self.inner.base_url))
            .header(CONTENT_TYPE, POSTCARD_MIME)
            .body(chal_body)
            .send()
            .await?;
        let chal_bytes = consume(resp, TINY_RESPONSE_MAX_BYTES).await?;
        let chal: AuthChallenge = decode(&chal_bytes).map_err(QueueClientError::Decode)?;

        // Step 2: sign nonce || expires.
        let mut signed = [0u8; 40];
        signed[..32].copy_from_slice(&chal.nonce);
        signed[32..].copy_from_slice(&chal.expires_at_unix_secs.to_le_bytes());
        // `Identity::sign` returns the raw 64-byte signature.
        let sig = self.inner.identity.sign(&signed);

        let verify_body = encode(&AuthVerifyRequest {
            protocol_version: QUEUE_PROTOCOL_VERSION,
            node_id,
            nonce: chal.nonce,
            expires_at_unix_secs: chal.expires_at_unix_secs,
            signature: sig,
        })
        .map_err(QueueClientError::Encode)?;
        let resp = self
            .inner
            .http
            .post(format!("{}/v1/auth/verify", self.inner.base_url))
            .header(CONTENT_TYPE, POSTCARD_MIME)
            .body(verify_body)
            .send()
            .await?;
        let token_bytes = consume(resp, TINY_RESPONSE_MAX_BYTES).await?;
        let token: AuthToken = decode(&token_bytes).map_err(QueueClientError::Decode)?;
        // Anchor the server-supplied lifetime *duration* to our own monotonic
        // Instant — no wall clock touches this, so client/server skew can't make
        // us over-hold (spurious 401s) or refresh too eagerly. The absolute
        // `expires_at_unix_secs` is deliberately NOT used here. See L-9.
        Ok(CachedToken {
            bearer_hex: hex::encode(token.token),
            expires_at: std::time::Instant::now()
                + std::time::Duration::from_secs(token.expires_in_secs),
        })
    }

    /// Drop the cached token. Useful when a request returns 401 and we
    /// want the next call to re-authenticate from scratch.
    pub async fn forget_token(&self) {
        *self.inner.token.lock().await = None;
    }

    /// Test-only: plant a bogus but not-yet-locally-expired token so the next
    /// authed request hits a server 401, exercising the SYN-M2 re-auth retry.
    #[cfg(test)]
    async fn poison_token_for_test(&self) {
        *self.inner.token.lock().await = Some(CachedToken {
            bearer_hex: "de".repeat(32), // 64 hex chars, not a token the server issued
            expires_at: std::time::Instant::now() + std::time::Duration::from_secs(3600),
        });
    }
}

/// Identity::sign exposes signature bytes; we wrap that for the client
/// without leaking the secret key.
trait IdentitySign {
    fn sign(&self, msg: &[u8]) -> [u8; 64];
}

impl IdentitySign for Identity {
    fn sign(&self, msg: &[u8]) -> [u8; 64] {
        Identity::sign(self, msg)
    }
}

async fn consume(resp: reqwest::Response, max: usize) -> QueueResult<Vec<u8>> {
    let status = resp.status();
    let bytes = read_body_capped(resp, max).await?;
    if status.is_success() {
        return Ok(bytes);
    }
    Err(decode_server_error(status.as_u16(), &bytes))
}

/// Read a response body into memory with a hard byte ceiling (SYN-M3). Rejects
/// a declared-oversize `Content-Length` up front, then streams chunk-by-chunk
/// so a chunked/absent-length response (or one that lies about its length)
/// can't exceed the cap either — we bail the moment the running total would
/// cross `max`, before buffering the rest.
async fn read_body_capped(mut resp: reqwest::Response, max: usize) -> QueueResult<Vec<u8>> {
    if let Some(len) = resp.content_length() {
        if len > max as u64 {
            return Err(QueueClientError::ResponseTooLarge {
                got: len.min(usize::MAX as u64) as usize,
                max,
            });
        }
    }
    let mut buf = Vec::new();
    while let Some(chunk) = resp.chunk().await? {
        if buf.len().saturating_add(chunk.len()) > max {
            return Err(QueueClientError::ResponseTooLarge {
                got: buf.len().saturating_add(chunk.len()),
                max,
            });
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}

fn decode_server_error(status: u16, body: &[u8]) -> QueueClientError {
    if let Ok(err) = decode::<QueueErrorBody>(body) {
        return QueueClientError::Server(err.code, err.message);
    }
    QueueClientError::UnknownServerError(status, String::from_utf8_lossy(body).into_owned())
}

fn now_unix_secs() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
