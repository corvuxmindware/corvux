//! HTTP helpers for AI provider traffic: bounded reads of untrusted response
//! bodies, and the no-redirect client used for every request whose address the
//! user (or a script in the webview) can choose.
//!
//! Review AK-1 / AK-6 (2026-11-12): the shared `HttpClient` follows up to ten
//! redirects and only the first hop is ever validated, so a user-supplied
//! address that answers `302 Location: http://169.254.169.254/…` would walk the
//! Rust client somewhere `validate_provider_base_url` never looked. The shared
//! client is left as it is on purpose — the model download
//! (`ai/models.rs::download_gemma4_e2b`) fetches from HuggingFace, which
//! redirects to its CDN — and user-supplied addresses use
//! [`no_redirect_client`] instead.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use serde::de::DeserializeOwned;

const MAX_ERROR_BYTES: usize = 64 * 1024;

/// Cap for a successful model-list body (`/api/tags`, a vendor `/models`
/// catalogue). OpenRouter's full catalogue is the largest at roughly 1–2 MB;
/// 8 MiB leaves headroom without letting a hostile server fill memory.
pub(crate) const MAX_MODEL_LIST_BYTES: usize = 8 * 1024 * 1024;

/// Cap for small JSON answers such as Ollama's `/api/version`.
pub(crate) const MAX_SMALL_JSON_BYTES: usize = MAX_ERROR_BYTES;

/// How long the no-redirect client waits to open a connection.
pub(crate) const USER_URL_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// How long the no-redirect client waits for the NEXT bytes on an open
/// connection. Deliberately a per-read idle limit, not a total timeout: an
/// Ollama chat streams for as long as the model talks, and a large model can sit
/// silent while it loads. Five minutes matches the agent loop's wall-clock cap
/// (`AgentBudget::max_wall_clock_secs`).
pub(crate) const USER_URL_READ_TIMEOUT: Duration = Duration::from_secs(300);

/// Build a client that never follows redirects. A 3xx comes back to the caller
/// as an ordinary response; see [`redirect_refused`].
pub(crate) fn build_no_redirect_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .user_agent(concat!("Corvux/", env!("CARGO_PKG_VERSION")))
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(USER_URL_CONNECT_TIMEOUT)
        .read_timeout(USER_URL_READ_TIMEOUT)
        .build()
        .map_err(|e| format!("Failed to build HTTP client: {e}"))
}

/// The process-wide no-redirect client (one connection pool). Used by the
/// Ollama chat provider and every connection / model-list probe.
pub(crate) fn no_redirect_client() -> Result<Arc<reqwest::Client>, String> {
    static CLIENT: OnceLock<Result<Arc<reqwest::Client>, String>> = OnceLock::new();
    CLIENT
        .get_or_init(|| build_no_redirect_client().map(Arc::new))
        .clone()
}

/// `Some(message)` when `status` is a redirect the no-redirect client refused
/// to follow. The message names no destination: the `Location` header is chosen
/// by the server and is not echoed back.
pub(crate) fn redirect_refused(status: reqwest::StatusCode) -> Option<String> {
    status.is_redirection().then(|| {
        format!(
            "The server answered with a redirect (HTTP {}). Corvux does not follow \
             redirects for this request — enter the server's final address instead.",
            status.as_u16()
        )
    })
}

#[derive(Debug, PartialEq, Eq)]
enum ReadError {
    TooLarge,
    Network,
}

/// Read at most `max` bytes of `response`. Refuses early on a declared
/// `Content-Length` over the cap and stops streaming as soon as the running
/// total would pass it (a chunked body has no declared length).
async fn read_capped(mut response: reqwest::Response, max: usize) -> Result<Vec<u8>, ReadError> {
    if response
        .content_length()
        .is_some_and(|len| len > max as u64)
    {
        return Err(ReadError::TooLarge);
    }
    let mut body = Vec::new();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                if body.len().saturating_add(chunk.len()) > max {
                    return Err(ReadError::TooLarge);
                }
                body.extend_from_slice(&chunk);
            }
            Ok(None) => return Ok(body),
            Err(_) => return Err(ReadError::Network),
        }
    }
}

pub(crate) async fn error_body(response: reqwest::Response) -> String {
    match read_capped(response, MAX_ERROR_BYTES).await {
        Ok(body) => String::from_utf8_lossy(&body).into_owned(),
        Err(ReadError::TooLarge) => "[provider error body exceeded the 64 KiB limit]".into(),
        Err(ReadError::Network) => "[could not read provider error body]".into(),
    }
}

/// Read a successful JSON body with a size cap AND a deadline, then parse it.
/// The deadline is independent of any per-request timeout so a server that
/// trickles bytes slowly cannot hold the caller open (AK-6).
pub(crate) async fn bounded_json<T: DeserializeOwned>(
    response: reqwest::Response,
    max_bytes: usize,
    deadline: Duration,
) -> Result<T, String> {
    let body = match tokio::time::timeout(deadline, read_capped(response, max_bytes)).await {
        Err(_) => return Err("the response took too long to arrive".into()),
        Ok(Err(ReadError::TooLarge)) => {
            return Err(format!(
                "the response was larger than the {} KiB limit",
                max_bytes / 1024
            ))
        }
        Ok(Err(ReadError::Network)) => return Err("the response could not be read".into()),
        Ok(Ok(body)) => body,
    };
    serde_json::from_slice(&body).map_err(|e| format!("the response was not valid JSON ({e})"))
}

/// Loopback HTTP fixtures shared by the probe tests in `commands::ai`.
#[cfg(test)]
pub(crate) mod test_server {
    use std::net::SocketAddr;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// One-shot loopback server that answers the first request with `wire`.
    pub(crate) async fn serve_once(wire: Vec<u8>) -> SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0; 4096];
            let _ = socket.read(&mut request).await.unwrap();
            // Oversized responses may be dropped before the server finishes.
            let _ = socket.write_all(&wire).await;
        });
        addr
    }

    /// A raw HTTP/1.1 response with `body`, either length-declared or chunked.
    pub(crate) fn wire(status: &str, body: &[u8], chunked: bool) -> Vec<u8> {
        let mut wire = if chunked {
            format!(
                "HTTP/1.1 {status}\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n",
                body.len()
            )
            .into_bytes()
        } else {
            format!(
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\n\r\n",
                body.len()
            )
            .into_bytes()
        };
        wire.extend_from_slice(body);
        if chunked {
            wire.extend_from_slice(b"\r\n0\r\n\r\n");
        }
        wire
    }

    /// A server that answers `302` pointing at a second loopback listener (the
    /// stand-in for a loopback / private / cloud-metadata address). Returns the
    /// first server's address, the target's address, and a counter of
    /// connections the target received — it must stay at zero.
    pub(crate) async fn redirect_trap() -> (SocketAddr, SocketAddr, Arc<AtomicUsize>) {
        let target = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target_addr = target.local_addr().unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_srv = hits.clone();
        tokio::spawn(async move {
            while let Ok((_socket, _)) = target.accept().await {
                hits_srv.fetch_add(1, Ordering::SeqCst);
            }
        });
        let redirect = format!(
            "HTTP/1.1 302 Found\r\nLocation: http://{target_addr}/latest/meta-data\r\n\
             Content-Length: 0\r\n\r\n"
        );
        let first = serve_once(redirect.into_bytes()).await;
        (first, target_addr, hits)
    }

    /// Give a (wrongly) followed redirect time to land before asserting.
    pub(crate) async fn assert_never_dialed(hits: &AtomicUsize) {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(hits.load(Ordering::SeqCst), 0, "the redirect was followed");
    }
}

#[cfg(test)]
mod tests {
    use super::test_server::{assert_never_dialed, redirect_trap, serve_once, wire};
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn response(body: Vec<u8>, chunked: bool) -> reqwest::Response {
        let addr = serve_once(wire("500 Error", &body, chunked)).await;
        reqwest::Client::new()
            .get(format!("http://{addr}"))
            .send()
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn preserves_small_error_bodies() {
        assert_eq!(
            error_body(response("エラー".as_bytes().to_vec(), false).await).await,
            "エラー"
        );
    }

    #[tokio::test]
    async fn rejects_declared_and_chunked_oversized_error_bodies() {
        for chunked in [false, true] {
            let text = error_body(response(vec![b'x'; MAX_ERROR_BYTES + 1], chunked).await).await;
            assert!(text.contains("exceeded"));
            assert!(text.len() < 100);
        }
    }

    #[tokio::test]
    async fn bounded_json_parses_within_the_cap() {
        let addr = serve_once(wire("200 OK", br#"{"version":"0.5.1"}"#, false)).await;
        let resp = reqwest::get(format!("http://{addr}")).await.unwrap();
        let v: serde_json::Value = bounded_json(resp, 1024, Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(v["version"], "0.5.1");
    }

    #[tokio::test]
    async fn bounded_json_refuses_declared_and_chunked_oversized_bodies() {
        // Valid JSON, just too big: the cap is what refuses it, not the parser.
        let mut big = b"\"".to_vec();
        big.extend_from_slice(&[b'x'; 2048]);
        big.push(b'"');
        for chunked in [false, true] {
            let addr = serve_once(wire("200 OK", &big, chunked)).await;
            let resp = reqwest::get(format!("http://{addr}")).await.unwrap();
            let e = bounded_json::<serde_json::Value>(resp, 1024, Duration::from_secs(5))
                .await
                .unwrap_err();
            assert!(e.contains("larger than"), "{chunked}: {e}");
        }
    }

    #[tokio::test]
    async fn bounded_json_gives_up_on_a_body_that_never_finishes() {
        // Headers promise 100 bytes; the server sends 2 and then stalls.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0; 4096];
            let _ = socket.read(&mut request).await;
            let _ = socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n{\"")
                .await;
            tokio::time::sleep(Duration::from_secs(30)).await;
            drop(socket);
        });
        let resp = reqwest::get(format!("http://{addr}")).await.unwrap();
        let e = bounded_json::<serde_json::Value>(resp, 1024, Duration::from_millis(200))
            .await
            .unwrap_err();
        assert!(e.contains("too long"), "{e}");
    }

    #[tokio::test]
    async fn the_no_redirect_client_reports_a_302_and_never_dials_the_target() {
        let (first, target_addr, hits) = redirect_trap().await;
        let client = no_redirect_client().unwrap();
        let resp = client
            .get(format!("http://{first}/api/tags"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::FOUND);
        let msg = redirect_refused(resp.status()).expect("3xx is refused");
        assert!(msg.contains("redirect") && msg.contains("302"), "{msg}");
        assert!(
            !msg.contains(&target_addr.to_string()),
            "Location is not echoed"
        );
        assert_never_dialed(&hits).await;
    }

    #[test]
    fn only_3xx_counts_as_a_refused_redirect() {
        assert!(redirect_refused(reqwest::StatusCode::OK).is_none());
        assert!(redirect_refused(reqwest::StatusCode::NOT_FOUND).is_none());
        assert!(redirect_refused(reqwest::StatusCode::MOVED_PERMANENTLY).is_some());
        assert!(redirect_refused(reqwest::StatusCode::TEMPORARY_REDIRECT).is_some());
    }
}
