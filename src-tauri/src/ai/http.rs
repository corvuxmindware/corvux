//! Bounded reads for untrusted provider error bodies.

const MAX_ERROR_BYTES: usize = 64 * 1024;

pub(crate) async fn error_body(mut response: reqwest::Response) -> String {
    if response
        .content_length()
        .is_some_and(|len| len > MAX_ERROR_BYTES as u64)
    {
        return "[provider error body exceeded the 64 KiB limit]".into();
    }
    let mut body = Vec::new();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                if body.len().saturating_add(chunk.len()) > MAX_ERROR_BYTES {
                    return "[provider error body exceeded the 64 KiB limit]".into();
                }
                body.extend_from_slice(&chunk);
            }
            Ok(None) => return String::from_utf8_lossy(&body).into_owned(),
            Err(_) => return "[could not read provider error body]".into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn response(body: Vec<u8>, chunked: bool) -> reqwest::Response {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0; 4096];
            let _ = socket.read(&mut request).await.unwrap();
            let wire = if chunked {
                let mut wire = format!(
                    "HTTP/1.1 500 Error\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n",
                    body.len()
                )
                .into_bytes();
                wire.extend_from_slice(&body);
                wire.extend_from_slice(b"\r\n0\r\n\r\n");
                wire
            } else {
                let mut wire = format!(
                    "HTTP/1.1 500 Error\r\nContent-Length: {}\r\n\r\n",
                    body.len()
                )
                .into_bytes();
                wire.extend_from_slice(&body);
                wire
            };
            // Oversized responses may be dropped before the server finishes.
            let _ = socket.write_all(&wire).await;
        });
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
}
