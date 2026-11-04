//! Queue endpoint URL validation. Pulled out of `commands::sync` per
//! Q-03 so the trust boundary between user-supplied endpoints and the
//! network layer is in one auditable file. The validator is the only
//! gate between an attacker-controlled URL and `QueueClient::new` —
//! every userinfo / scheme / allowlist case lives here.
//!
//! S-02: see
//! `docs/audits/engineering-code-security-audit-2026-05-24.md` for the
//! bypass that motivated the rewrite (manual `://` + `:` splitting
//! read `meetcorvux.com` as the host of
//! `https://meetcorvux.com:443@evil.example/q`, while reqwest dialled
//! `evil.example`). Now routes through `url::Url`.

/// Hosts always accepted for queue endpoints, even without a user pin.
pub(super) const DEFAULT_QUEUE_HOST_ALLOWLIST: &[&str] = &["meetcorvux.com"];

/// Validate a queue endpoint URL. Rejects `http://`, malformed URLs,
/// URLs that carry userinfo (the `userinfo@host` form RFC 3986 allows),
/// and any host outside the allowlist or the user-pinned host. Returns
/// the trimmed URL on success (no other rewriting — preserves the
/// user's path/query exactly).
pub(super) fn validate_queue_endpoint(url: &str, pinned_host: &str) -> Result<String, String> {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        return Ok(String::new()); // clearing the endpoint
    }

    let parsed =
        url::Url::parse(trimmed).map_err(|e| format!("queue endpoint is not a valid URL: {e}"))?;

    // Plaintext leaks the social graph, message sizes, and Ed25519
    // auth-challenge signatures even with encrypted payloads.
    match parsed.scheme() {
        "https" => {}
        "http" => return Err("queue endpoint must use https://".into()),
        s => return Err(format!("queue endpoint must use https:// (got '{s}')")),
    }

    // RFC 3986 userinfo (`user[:pass]@host`) is the bypass vector — a
    // friendly-looking allowlisted host can be moved into the userinfo
    // while the actual `host_str()` reqwest dials is something else.
    // Forbid userinfo outright; there's no legitimate reason for a
    // queue endpoint to carry credentials in the URL.
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err("queue endpoint must not contain user:password@ — strip credentials".into());
    }

    let host = parsed
        .host_str()
        .ok_or_else(|| "queue endpoint missing host".to_string())?
        .to_ascii_lowercase();
    if host.is_empty() {
        return Err("queue endpoint missing host".into());
    }

    // Allowlist comparison. Default allowlist accepts `meetcorvux.com`
    // and any subdomain; pinned hosts match exactly. If the pinned host
    // contains a `:port`, the URL's port must match. If the pinned host
    // has no explicit port, any port (or none) is fine.
    let in_default = DEFAULT_QUEUE_HOST_ALLOWLIST
        .iter()
        .any(|allowed| host == *allowed || host.ends_with(&format!(".{allowed}")));

    let in_pinned = if pinned_host.is_empty() {
        false
    } else {
        let pinned = pinned_host.to_ascii_lowercase();
        match pinned.split_once(':') {
            Some((pin_host, pin_port_str)) => {
                let pin_port: u16 = pin_port_str
                    .parse()
                    .map_err(|_| format!("pinned host has invalid port: '{pinned_host}'"))?;
                host == pin_host && parsed.port() == Some(pin_port)
            }
            None => host == pinned,
        }
    };

    if !in_default && !in_pinned {
        return Err(format!(
            "queue host '{host}' is not in the allowlist; pin it first via Settings"
        ));
    }
    Ok(trimmed.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_http() {
        assert!(validate_queue_endpoint("http://meetcorvux.com/q", "").is_err());
    }

    #[test]
    fn rejects_unknown_host() {
        let err = validate_queue_endpoint("https://evil.example.com/q", "").unwrap_err();
        assert!(
            err.contains("allowlist"),
            "expected allowlist error, got: {err}"
        );
    }

    #[test]
    fn accepts_default_host_and_subdomains() {
        assert!(validate_queue_endpoint("https://meetcorvux.com/q", "").is_ok());
        assert!(validate_queue_endpoint("https://queue.meetcorvux.com/q", "").is_ok());
    }

    #[test]
    fn accepts_pinned_host() {
        assert!(validate_queue_endpoint("https://q.example.net/q", "q.example.net").is_ok());
    }

    #[test]
    fn pinned_host_case_insensitive() {
        assert!(validate_queue_endpoint("https://Q.Example.Net/q", "q.example.net").is_ok());
    }

    #[test]
    fn empty_clears() {
        assert_eq!(validate_queue_endpoint("", "").unwrap(), "");
        assert_eq!(validate_queue_endpoint("   ", "").unwrap(), "");
    }

    #[test]
    fn rejects_garbage() {
        assert!(validate_queue_endpoint("not-a-url", "").is_err());
        assert!(validate_queue_endpoint("https://", "").is_err());
    }

    /// S-02: the userinfo bypass that started this fix. Manual host
    /// parsing read `meetcorvux.com` as the host (the part before the
    /// first `:`); the real `url::Url` parser correctly sees
    /// `meetcorvux.com:443` as userinfo and `evil.example` as the host.
    #[test]
    fn rejects_userinfo_port_bypass() {
        let err =
            validate_queue_endpoint("https://meetcorvux.com:443@evil.example/q", "").unwrap_err();
        // Must reject — either via userinfo strip or via allowlist
        // (the real host is `evil.example`). Both are correct outcomes.
        assert!(
            err.contains("user:password") || err.contains("allowlist"),
            "expected userinfo / allowlist rejection, got: {err}"
        );
    }

    #[test]
    fn rejects_userinfo_at_only() {
        assert!(validate_queue_endpoint("https://meetcorvux.com@evil.example/q", "").is_err());
    }

    #[test]
    fn rejects_password_userinfo() {
        assert!(validate_queue_endpoint("https://user:pass@meetcorvux.com/q", "").is_err());
    }

    /// Lookalike subdomain shape — the trailing-dot check would let a
    /// host that merely *contains* the allowed name slip through if
    /// the comparison were string-contains rather than the strict
    /// `ends_with(".{allowed}")` we use. Lock it down.
    #[test]
    fn rejects_lookalike_subdomain() {
        assert!(
            validate_queue_endpoint("https://meetcorvux.com.evil.example/q", "").is_err(),
            "lookalike subdomain must not satisfy the allowlist"
        );
    }

    /// Hosts must compare case-insensitively — both `url::Url`'s
    /// internal normalization and our explicit `to_ascii_lowercase`
    /// guarantee this.
    #[test]
    fn accepts_uppercase_host() {
        assert!(validate_queue_endpoint("https://MeetCorvux.com/q", "").is_ok());
    }

    /// Pinned host with an explicit port — endpoint URL must use that
    /// same port. The previous validator stripped the port off both
    /// sides and compared just the hostname, which silently approved
    /// `q.example.net:8080` against a pin of `q.example.net:8443`.
    #[test]
    fn accepts_pinned_host_with_port() {
        assert!(
            validate_queue_endpoint("https://q.example.net:8443/q", "q.example.net:8443").is_ok()
        );
    }

    #[test]
    fn rejects_pinned_host_wrong_port() {
        let err = validate_queue_endpoint("https://q.example.net:8080/q", "q.example.net:8443")
            .unwrap_err();
        assert!(
            err.contains("allowlist"),
            "expected allowlist error, got: {err}"
        );
    }
}
