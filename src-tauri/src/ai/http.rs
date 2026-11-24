//! HTTP helpers for AI provider traffic: bounded reads of untrusted response
//! bodies, and the no-redirect client used for every request whose address the
//! user (or a script in the webview) can choose.
//!
//! Review AK-1 / AK-6 (2026-11-12): the shared `HttpClient` follows up to ten
//! redirects and only the first hop is ever validated, so a user-supplied
//! address that answers `302 Location: http://169.254.169.254/…` would walk the
//! Rust client somewhere `validate_provider_base_url` never looked. The shared
//! client keeps following redirects on purpose — the model download
//! (`ai/models.rs::download_gemma4_e2b`) fetches from HuggingFace, which
//! redirects to its CDN — so there are THREE clients, by what they carry:
//!
//! * [`build_shared_client`] (Tauri state `HttpClient`): follows redirects, for
//!   the unauthenticated model download only;
//! * [`cloud_chat_client`]: no redirects, for the fixed-host cloud chat
//!   providers (a redirect would replay `x-goog-api-key` / `x-api-key`);
//! * [`no_redirect_client`]: no redirects, for any address the user chose
//!   (Ollama chat and every probe).
//!
//! All three set a connect timeout and a per-read idle timeout and none sets a
//! total timeout (AT-6): a stream or a download runs as long as bytes arrive.

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

/// How long the cloud-chat client and the shared client wait to open a
/// connection. Longer than the user-URL client's: these go over the public
/// internet from phones too.
pub(crate) const CLOUD_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// Per-read idle limit for the cloud-chat client and the shared client (AT-6).
/// Like [`USER_URL_READ_TIMEOUT`] this is NOT a total timeout — a reply, or the
/// multi-gigabyte model download, may run as long as bytes keep arriving — and it
/// is the same five minutes: a reasoning model can legitimately sit silent
/// before its first token, and no stream that the agent loop's own wall-clock
/// budget would let live can be cut by this first.
pub(crate) const CLOUD_READ_TIMEOUT: Duration = USER_URL_READ_TIMEOUT;

const USER_AGENT: &str = concat!("Corvux/", env!("CARGO_PKG_VERSION"));

/// A builder with this module's connection hygiene already applied: the Corvux
/// user agent, a connect timeout and a per-read idle timeout.
fn base_builder(connect_timeout: Duration) -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .connect_timeout(connect_timeout)
        .read_timeout(CLOUD_READ_TIMEOUT)
}

/// Build a client that never follows redirects. A 3xx comes back to the caller
/// as an ordinary response; see [`redirect_refused`].
pub(crate) fn build_no_redirect_client() -> Result<reqwest::Client, String> {
    base_builder(USER_URL_CONNECT_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| format!("Failed to build HTTP client: {e}"))
}

/// The process-wide shared client (`HttpClient` in Tauri state). It FOLLOWS
/// redirects on purpose, because the model download
/// (`ai/models.rs::download_gemma4_e2b`) fetches from HuggingFace, which
/// redirects to its CDN. That makes it the wrong client for anything that
/// carries a credential: reqwest 0.12 strips only `Authorization`, `Cookie`,
/// `Proxy-Authorization` and `WWW-Authenticate` when a redirect crosses hosts,
/// so `x-goog-api-key` / `x-api-key` would be forwarded to wherever the
/// redirect points (review AK-6). Chat providers use [`cloud_chat_client`].
pub(crate) fn build_shared_client() -> Result<reqwest::Client, String> {
    base_builder(CLOUD_CONNECT_TIMEOUT)
        .build()
        .map_err(|e| format!("Failed to build HTTP client: {e}"))
}

/// A client for the fixed-host cloud chat providers (Gemini, OpenAI, Anthropic,
/// OpenRouter): no redirects, so an API-key header can never be replayed to a
/// second host; connect + per-read idle timeouts.
///
/// None of those four APIs redirects a chat request in normal operation, so
/// refusing redirects costs nothing; a 3xx now surfaces as the provider's HTTP
/// error instead of silently sending the key elsewhere. Providers use this
/// client themselves and ignore any client they are handed — the same
/// arrangement as `OllamaChat` — so a caller cannot pass the redirect-following
/// shared client by mistake.
pub(crate) fn build_cloud_chat_client() -> Result<reqwest::Client, String> {
    base_builder(CLOUD_CONNECT_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| format!("Failed to build HTTP client: {e}"))
}

/// The process-wide cloud-chat client (one connection pool).
pub(crate) fn cloud_chat_client() -> Result<Arc<reqwest::Client>, String> {
    static CLIENT: OnceLock<Result<Arc<reqwest::Client>, String>> = OnceLock::new();
    CLIENT
        .get_or_init(|| build_cloud_chat_client().map(Arc::new))
        .clone()
}

const BAD_SECRET_HEADER: &str = "The API key contains characters that cannot be sent in a request \
                                 header (a stray line break or control character). Open Settings \
                                 \u{2192} AI and paste the key again.";

/// Attach a secret as a request header, marked SENSITIVE (excluded from `Debug`
/// output and from HTTP/2 header compression tables). Use this for every
/// credential header that `reqwest`'s own `bearer_auth` does not cover
/// (`x-goog-api-key`, `x-api-key`); `bearer_auth` already marks its value, and
/// a hand-built `Authorization` header must never be written (review AK-9; the
/// `credential_headers_are_always_marked_sensitive` test enforces both).
pub(crate) fn secret_header(
    req: reqwest::RequestBuilder,
    name: &'static str,
    secret: &str,
) -> Result<reqwest::RequestBuilder, String> {
    let mut value = reqwest::header::HeaderValue::from_str(secret)
        .map_err(|_| BAD_SECRET_HEADER.to_string())?;
    value.set_sensitive(true);
    Ok(req.header(name, value))
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

    /// AK-6: the cloud-chat client must never follow a redirect, so a vendor
    /// host (or whatever answers in its place) cannot make it replay the API-key
    /// header at a second address. The second address here is a loopback
    /// listener that counts connections.
    #[tokio::test]
    async fn the_cloud_chat_client_never_follows_a_redirect_with_the_api_key() {
        let (first, _target, hits) = redirect_trap().await;
        let client = build_cloud_chat_client().unwrap();
        let req = client.post(format!("http://{first}/v1/messages"));
        let resp = secret_header(req, "x-api-key", "sk-test-secret")
            .unwrap()
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::FOUND);
        assert_never_dialed(&hits).await;
    }

    /// The flip side, pinned on purpose: the SHARED client keeps following
    /// redirects because the HuggingFace model download depends on it. If this
    /// starts failing, someone made the shared client refuse redirects — fine for
    /// security, but then `download_gemma4_e2b` needs its own client first.
    #[tokio::test]
    async fn the_shared_client_still_follows_redirects_for_the_model_download() {
        let (first, _target, hits) = redirect_trap().await;
        let client = build_shared_client().unwrap();
        // The target accepts and drops the connection, so the request itself
        // errors; what matters is that the connection was made.
        let _ = client
            .get(format!("http://{first}/model.gguf"))
            .send()
            .await;
        for _ in 0..40 {
            if hits.load(std::sync::atomic::Ordering::SeqCst) > 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(
            hits.load(std::sync::atomic::Ordering::SeqCst) >= 1,
            "the shared client no longer follows redirects"
        );
    }

    // Idle timeouts, not a total timeout (a stream or the model download runs as
    // long as bytes keep arriving); compile-time so editing them past a bound
    // fails the build.
    const _: () =
        assert!(CLOUD_CONNECT_TIMEOUT.as_secs() >= 5 && CLOUD_CONNECT_TIMEOUT.as_secs() <= 60);
    const _: () = assert!(CLOUD_READ_TIMEOUT.as_secs() >= 120);

    #[test]
    fn secret_header_marks_the_value_sensitive_and_rejects_line_breaks() {
        let req = secret_header(
            reqwest::Client::new().get("http://localhost/"),
            "x-api-key",
            "sk-test-secret",
        )
        .unwrap()
        .build()
        .unwrap();
        let value = req.headers().get("x-api-key").unwrap();
        assert!(value.is_sensitive());
        assert!(!format!("{value:?}").contains("sk-test-secret"));

        let err = secret_header(
            reqwest::Client::new().get("http://localhost/"),
            "x-api-key",
            "sk-test\nsecret",
        )
        .err()
        .unwrap();
        assert!(err.contains("API key") && !err.contains("sk-test"), "{err}");
    }

    /// `bearer_auth` is the sensitive-marking Authorization form; pin that this
    /// is what reqwest does so the AK-9 rule below ("never hand-build it") stays
    /// grounded in behaviour, not folklore.
    #[test]
    fn bearer_auth_marks_the_authorization_value_sensitive() {
        let req = reqwest::Client::new()
            .get("http://localhost/")
            .bearer_auth("sk-test-secret")
            .build()
            .unwrap();
        assert!(req
            .headers()
            .get(reqwest::header::AUTHORIZATION)
            .unwrap()
            .is_sensitive());
    }

    /// AK-9 class guard. Every credential header in `src/ai` must go through a
    /// sensitive-marking form: `bearer_auth` for `Authorization`, `secret_header`
    /// for `x-goog-api-key` / `x-api-key`. A hand-built header is an unwiped,
    /// unmarked copy that shows up in `Debug` output. Needles are assembled at
    /// run time so this file does not match itself.
    #[test]
    fn credential_headers_are_always_marked_sensitive() {
        fn collect(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            for entry in std::fs::read_dir(dir).unwrap().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    collect(&path, out);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    out.push(path);
                }
            }
        }
        let open = ".header(\"";
        let needles = [
            format!("{open}{}", "Authorization"),
            format!("{open}{}", "authorization"),
            format!("{open}{}", "x-goog-api-key"),
            format!("{open}{}", "x-api-key"),
            format!("header({}", "AUTHORIZATION"),
            format!("header(reqwest::header::{}", "AUTHORIZATION"),
        ];
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/ai");
        let mut files = Vec::new();
        collect(&root, &mut files);
        assert!(files.len() > 20, "scanned too few files: {}", files.len());
        let mut offenders = Vec::new();
        for file in files {
            let text = std::fs::read_to_string(&file).unwrap();
            for (n, line) in text.lines().enumerate() {
                if needles.iter().any(|needle| line.contains(needle.as_str())) {
                    offenders.push(format!("{}:{}: {}", file.display(), n + 1, line.trim()));
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "hand-built credential header(s); use bearer_auth / http::secret_header:\n{}",
            offenders.join("\n")
        );
    }

    #[test]
    fn only_3xx_counts_as_a_refused_redirect() {
        assert!(redirect_refused(reqwest::StatusCode::OK).is_none());
        assert!(redirect_refused(reqwest::StatusCode::NOT_FOUND).is_none());
        assert!(redirect_refused(reqwest::StatusCode::MOVED_PERMANENTLY).is_some());
        assert!(redirect_refused(reqwest::StatusCode::TEMPORARY_REDIRECT).is_some());
    }
}
