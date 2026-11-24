//! Validation for LLM provider base URLs (today: the user's Ollama server).
//!
//! The queue endpoint got a real validator after the S-02 userinfo bypass
//! (`commands::sync_endpoint`). The Ollama base URL got none: it was stored and
//! dialled as opaque text, so any script in the webview could point chat
//! traffic — which carries the roost note index, up to three injected note
//! bodies and, when set, a bearer token — at a host of its choosing
//! (sharp-edges F2, 2026-11-11).
//!
//! This validator fixes the SHAPE (scheme, userinfo, host, no query/fragment)
//! and CLASSIFIES the host. It deliberately cannot decide whether an internet
//! host is the user's own, or whether plain `http://` to a public-looking name
//! is a split-DNS home lab or an exfil channel — those are human decisions, made
//! through [`super::settings_guard`]'s native confirmation when the host
//! changes. (An earlier draft hard-rejected `http://` to a public name; that
//! broke existing split-DNS setups with no way to re-save them, so it is now a
//! flagged, dialog-confirmed choice — see [`ValidatedBase::is_plaintext_to_internet`].)
//!
//! Every path that dials a user-configured provider URL goes through
//! [`validate_provider_base_url`]: `save_settings` (on change),
//! `build_chat_provider` (every chat), `test_ollama_connection`,
//! `list_ollama_models`.

use std::net::{Ipv4Addr, Ipv6Addr};

use crate::ai::ollama::OLLAMA_DEFAULT_BASE;

/// How far from this machine a host is. Drives both the scheme policy and
/// whether a changed host needs a native confirmation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HostClass {
    /// This computer (`localhost`, `127.0.0.0/8`, `::1`, `0.0.0.0`).
    Loopback,
    /// The user's own network: RFC 1918 / link-local / CGNAT (Tailscale) /
    /// unique-local IPv6, single-label names, `.local`, `.lan`, `.ts.net`, …
    PrivateNetwork,
    /// Anything else — the open internet.
    Public,
}

/// A base URL that passed validation, in normalized form (no trailing slash).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ValidatedBase {
    pub url: String,
    /// Lowercased host without brackets, for dialogs and consent keys.
    pub host: String,
    pub class: HostClass,
    /// `true` for `http://`.
    pub plaintext: bool,
}

impl ValidatedBase {
    /// Chat traffic and any bearer token would cross the open internet
    /// unencrypted. Allowed (split-DNS home labs resolve a public-looking name to
    /// a LAN address) but only after the user confirms it in a native dialog.
    pub(crate) fn is_plaintext_to_internet(&self) -> bool {
        self.plaintext && self.class == HostClass::Public
    }
}

/// Name suffixes that only ever resolve inside a private network.
const PRIVATE_SUFFIXES: &[&str] = &[
    ".local",
    ".lan",
    ".localdomain",
    ".home.arpa",
    ".internal",
    // NOT `.ts.net`: Tailscale Funnel publishes `https://x.tailNNNN.ts.net`
    // to the whole internet with public DNS and a valid certificate, so a
    // MagicDNS name proves nothing. Tailscale users use the 100.x address
    // (classified private) or the short MagicDNS name (single-label).
];

fn classify_v4(ip: Ipv4Addr) -> Result<HostClass, String> {
    if ip.is_multicast() || ip.is_broadcast() {
        return Err("that address is not a server address".into());
    }
    if ip.is_loopback() || ip.is_unspecified() {
        return Ok(HostClass::Loopback);
    }
    let o = ip.octets();
    let cgnat = o[0] == 100 && (o[1] & 0xC0) == 64; // 100.64.0.0/10
    if ip.is_private() || ip.is_link_local() || cgnat {
        return Ok(HostClass::PrivateNetwork);
    }
    Ok(HostClass::Public)
}

fn classify_v6(ip: Ipv6Addr) -> Result<HostClass, String> {
    if let Some(v4) = ip.to_ipv4_mapped() {
        return classify_v4(v4);
    }
    if ip.is_multicast() {
        return Err("that address is not a server address".into());
    }
    if ip.is_loopback() || ip.is_unspecified() {
        return Ok(HostClass::Loopback);
    }
    let seg0 = ip.segments()[0];
    let unique_local = (seg0 & 0xfe00) == 0xfc00; // fc00::/7
    let link_local = (seg0 & 0xffc0) == 0xfe80; // fe80::/10
    if unique_local || link_local {
        return Ok(HostClass::PrivateNetwork);
    }
    Ok(HostClass::Public)
}

fn classify_domain(domain: &str) -> HostClass {
    let d = domain.trim_end_matches('.').to_ascii_lowercase();
    if d == "localhost" || d.ends_with(".localhost") {
        return HostClass::Loopback;
    }
    // Single-label names (`my-desktop`) can only come from the local resolver
    // or a hosts file.
    if !d.contains('.') {
        return HostClass::PrivateNetwork;
    }
    if PRIVATE_SUFFIXES.iter().any(|s| d.ends_with(s)) {
        return HostClass::PrivateNetwork;
    }
    HostClass::Public
}

/// Validate and normalize a provider base URL. An empty value means "use the
/// default local server" and validates as [`OLLAMA_DEFAULT_BASE`].
///
/// Rejects: unparseable input; schemes other than `http`/`https`;
/// `user:pass@` userinfo (the S-02 bypass shape — the host a reader sees is not
/// the host reqwest dials); a missing host; query strings and fragments (they
/// would corrupt the `{base}/v1/...` join); and multicast/broadcast addresses.
/// Plain `http://` to a [`HostClass::Public`] host is NOT rejected here — it is
/// flagged ([`ValidatedBase::is_plaintext_to_internet`]) and the caller must get
/// the user's confirmation.
pub(crate) fn validate_provider_base_url(raw: &str) -> Result<ValidatedBase, String> {
    let trimmed = raw.trim();
    let input = if trimmed.is_empty() {
        OLLAMA_DEFAULT_BASE
    } else {
        trimmed
    };

    let parsed = url::Url::parse(input).map_err(|e| format!("not a valid URL ({e})"))?;

    match parsed.scheme() {
        "http" | "https" => {}
        other => {
            return Err(format!(
                "must start with http:// or https:// (got '{other}')"
            ))
        }
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err("must not contain user:password@ — remove the credentials".into());
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err("must not contain a ? query or # fragment".into());
    }

    let (host, class) = match parsed.host() {
        None => return Err("is missing a host".into()),
        Some(url::Host::Domain(d)) => (
            d.trim_end_matches('.').to_ascii_lowercase(),
            classify_domain(d),
        ),
        Some(url::Host::Ipv4(ip)) => (ip.to_string(), classify_v4(ip)?),
        Some(url::Host::Ipv6(ip)) => (ip.to_string(), classify_v6(ip)?),
    };
    if host.is_empty() {
        return Err("is missing a host".into());
    }

    Ok(ValidatedBase {
        url: parsed.as_str().trim_end_matches('/').to_string(),
        host,
        class,
        plaintext: parsed.scheme() == "http",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(s: &str) -> ValidatedBase {
        validate_provider_base_url(s).unwrap_or_else(|e| panic!("{s:?} should pass: {e}"))
    }
    fn err(s: &str) -> String {
        validate_provider_base_url(s).expect_err(&format!("{s:?} should be rejected"))
    }

    #[test]
    fn default_and_empty_are_loopback_http() {
        let d = ok(OLLAMA_DEFAULT_BASE);
        assert_eq!(d.class, HostClass::Loopback);
        assert_eq!(d.url, "http://localhost:11434");
        assert_eq!(ok("").url, "http://localhost:11434");
        assert_eq!(ok("   ").url, "http://localhost:11434");
    }

    #[test]
    fn trailing_slashes_are_normalized_away() {
        assert_eq!(ok("http://localhost:11434/").url, "http://localhost:11434");
        assert_eq!(
            ok("https://h.example/ollama/").url,
            "https://h.example/ollama"
        );
    }

    #[test]
    fn loopback_forms_allow_http() {
        for s in [
            "http://localhost:11434",
            "http://127.0.0.1:11434",
            "http://127.5.5.5:11434",
            "http://[::1]:11434",
            "http://0.0.0.0:11434",
            "http://foo.localhost:11434",
            "http://[::ffff:127.0.0.1]:11434",
        ] {
            assert_eq!(ok(s).class, HostClass::Loopback, "{s}");
        }
    }

    #[test]
    fn private_network_forms_allow_http() {
        for s in [
            "http://192.168.1.20:11434",
            "http://10.0.0.5:11434",
            "http://172.16.4.4:11434",
            "http://172.31.255.1:11434",
            "http://169.254.10.10:11434",
            "http://100.101.102.103:11434", // Tailscale CGNAT
            "http://[fd12:3456::1]:11434",
            "http://[fe80::1]:11434",
            "http://my-desktop:11434",
            "http://desktop.local:11434",
            "http://box.lan:11434",
            "http://nas.home.arpa:11434",
        ] {
            assert_eq!(ok(s).class, HostClass::PrivateNetwork, "{s}");
        }
    }

    #[test]
    fn near_misses_of_private_ranges_are_public() {
        // 172.32/12 is outside 172.16/12; 100.128 is outside 100.64/10.
        assert_eq!(ok("https://172.32.0.1").class, HostClass::Public);
        assert_eq!(ok("https://100.128.0.1").class, HostClass::Public);
        assert_eq!(ok("https://192.169.1.1").class, HostClass::Public);
        assert_eq!(ok("https://notlocal.example").class, HostClass::Public);
        // A look-alike suffix must not pass as private.
        assert_eq!(
            ok("https://evil.local.example.com").class,
            HostClass::Public
        );
        assert_eq!(
            ok("https://evil-ts.net.example.com").class,
            HostClass::Public
        );
    }

    #[test]
    fn plain_http_to_the_internet_is_flagged_not_rejected() {
        for s in [
            "http://attacker.example:11434",
            "http://8.8.8.8",
            "http://[2001:db8::1]",
            "http://ollama.com",
            // Split-DNS home lab: public-looking name that resolves to a LAN IP.
            "http://ollama.home.example.com:11434",
        ] {
            let v = ok(s);
            assert!(v.is_plaintext_to_internet(), "{s}");
        }
        for s in [
            "https://ollama.com",
            "https://attacker.example",
            "http://localhost:11434",
            "http://192.168.1.9:11434",
        ] {
            assert!(!ok(s).is_plaintext_to_internet(), "{s}");
        }
        assert_eq!(ok("https://attacker.example").host, "attacker.example");
    }

    #[test]
    fn tailscale_funnel_names_are_public() {
        // `*.ts.net` can be published to the whole internet (Tailscale Funnel).
        let v = ok("http://desktop.tail1234.ts.net:11434");
        assert_eq!(v.class, HostClass::Public);
        assert!(v.is_plaintext_to_internet());
        // The 100.x address and the short MagicDNS name stay private.
        assert_eq!(
            ok("http://100.101.102.103:11434").class,
            HostClass::PrivateNetwork
        );
        assert_eq!(ok("http://desktop:11434").class, HostClass::PrivateNetwork);
    }

    #[test]
    fn userinfo_is_rejected_even_when_it_imitates_a_safe_host() {
        // The S-02 shape: a reader sees `localhost`, reqwest dials evil.example.
        let e = err("http://localhost:11434@evil.example/");
        assert!(e.contains("user:password@"), "{e}");
        err("https://user:pass@h.example");
        err("https://user@h.example");
    }

    #[test]
    fn other_schemes_and_garbage_are_rejected() {
        for s in [
            "ftp://h.example",
            "file:///etc/passwd",
            "javascript:alert(1)",
            "ws://h.example",
            "not a url",
            "//h.example",
            "http://",
            "localhost:11434", // parses as scheme `localhost`
        ] {
            err(s);
        }
    }

    #[test]
    fn query_and_fragment_are_rejected() {
        err("https://h.example/?x=1");
        err("https://h.example/#frag");
    }

    #[test]
    fn multicast_and_broadcast_are_rejected() {
        err("http://224.0.0.1");
        err("http://255.255.255.255");
        err("http://[ff02::1]");
    }

    #[test]
    fn host_is_lowercased_and_trailing_dot_stripped() {
        assert_eq!(ok("https://Ollama.COM.").host, "ollama.com");
    }
}
