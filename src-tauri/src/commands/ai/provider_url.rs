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
//! Two entry points, because a value that is being CHOSEN and a value that was
//! ALREADY SAVED are different cases (review AK-9, 2026-11-12):
//!
//! * [`validate_new_provider_base_url`] — a value arriving from the webview
//!   (`save_settings` when the address changes, the two probes). Credentials in
//!   the URL (`user:pass@`) are rejected: the host a reader sees is not the host
//!   reqwest dials (the S-02 shape).
//! * [`validate_provider_base_url`] — a value that is already persisted
//!   (`build_chat_provider`, every chat). Same checks, but `user:pass@` is
//!   ACCEPTED and flagged ([`ValidatedBase::has_userinfo`]): installs from before
//!   the validator existed may hold `http://user:pass@proxy`, and refusing it
//!   would break every chat with no way back except editing settings.json. The
//!   host is the REAL host (what reqwest dials), so the consent prompt and the
//!   host class are never fooled by it; the credentials stay out of every
//!   `Debug` / dialog string ([`ValidatedBase::display_url`]).
//!
//! Every path that dials a user-configured provider URL goes through one of
//! them: `save_settings` (on change), `build_chat_provider` (every chat),
//! `test_ollama_connection`, `list_ollama_models`.

use std::net::{Ipv4Addr, Ipv6Addr};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::ai::ollama::OLLAMA_DEFAULT_BASE;

/// How far from this machine a host is. Drives both the scheme policy and
/// whether a changed host needs a native confirmation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HostClass {
    /// This computer (`localhost`, `127.0.0.0/8`, `::1`, `0.0.0.0`).
    Loopback,
    /// The user's own network: RFC 1918 / CGNAT (Tailscale) / unique-local IPv6,
    /// single-label names, `.local`, `.lan`, …
    PrivateNetwork,
    /// `169.254.0.0/16` and `fe80::/10`. NOT "the user's own network": this is
    /// where cloud instances keep their metadata service (and credentials), and
    /// where a device with no router parks itself. It needs the user's
    /// confirmation like an internet host does (review AK-8); the well-known
    /// metadata addresses are refused outright.
    LinkLocal,
    /// Anything else — the open internet.
    Public,
}

/// A base URL that passed validation, in normalized form (no trailing slash).
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct ValidatedBase {
    /// The address to dial. For a legacy saved value this still carries its
    /// `user:pass@` (see [`Self::has_userinfo`]) so existing setups keep working
    /// — never log or display it; use [`Self::display_url`].
    pub url: String,
    /// Lowercased host without brackets, for dialogs and consent keys. Always the
    /// host reqwest will dial, never a userinfo look-alike.
    pub host: String,
    pub class: HostClass,
    /// `true` for `http://`.
    pub plaintext: bool,
    /// The URL carries `user:pass@`. Only [`validate_provider_base_url`] returns
    /// such a value (a legacy saved one); [`validate_new_provider_base_url`]
    /// rejects it.
    pub has_userinfo: bool,
}

impl std::fmt::Debug for ValidatedBase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ValidatedBase")
            .field("url", &self.display_url())
            .field("host", &self.host)
            .field("class", &self.class)
            .field("plaintext", &self.plaintext)
            .field("has_userinfo", &self.has_userinfo)
            .finish()
    }
}

impl ValidatedBase {
    /// Chat traffic and any bearer token would cross the open internet
    /// unencrypted. Allowed (split-DNS home labs resolve a public-looking name to
    /// a LAN address) but only after the user confirms it in a native dialog.
    pub(crate) fn is_plaintext_to_internet(&self) -> bool {
        self.plaintext && self.class == HostClass::Public
    }

    /// [`Self::url`] without credentials: safe for dialogs, logs and `Debug`.
    pub(crate) fn display_url(&self) -> String {
        redact_userinfo(&self.url)
    }
}

/// `url` with any `user:pass@` removed. Text that does not parse as a URL is
/// returned unchanged (there is nothing to dial, so nothing to protect).
pub(crate) fn redact_userinfo(url: &str) -> String {
    match url::Url::parse(url) {
        Ok(mut u) if !u.username().is_empty() || u.password().is_some() => {
            let _ = u.set_username("");
            let _ = u.set_password(None);
            u.as_str().trim_end_matches('/').to_string()
        }
        _ => url.to_string(),
    }
}

/// Shown when a CHOSEN address carries credentials.
const USERINFO_REJECTED: &str = "must not contain user:password@ — remove the credentials";

/// Cloud instance-metadata services. A "server address" here is never an Ollama
/// install — it is a request for the machine's cloud credentials. Refused in
/// every mode, including for an already-saved value.
const METADATA_V4: [Ipv4Addr; 4] = [
    Ipv4Addr::new(169, 254, 169, 254), // AWS, GCP, Azure, OpenStack, DigitalOcean…
    Ipv4Addr::new(169, 254, 170, 2),   // AWS ECS task metadata
    Ipv4Addr::new(100, 100, 100, 200), // Alibaba Cloud (inside the CGNAT range)
    Ipv4Addr::new(192, 0, 0, 192),     // Oracle Cloud
];
const METADATA_V6: Ipv6Addr = Ipv6Addr::new(0xfd00, 0x0ec2, 0, 0, 0, 0, 0, 0x0254); // AWS IMDS over IPv6
const METADATA_NAMES: &[&str] = &["metadata.google.internal", "instance-data.ec2.internal"];

const METADATA_REFUSED: &str =
    "is a cloud metadata address, not an Ollama server — enter your Ollama server's address";

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
    if METADATA_V4.contains(&ip) {
        return Err(METADATA_REFUSED.into());
    }
    if ip.is_loopback() || ip.is_unspecified() {
        return Ok(HostClass::Loopback);
    }
    // 169.254.0.0/16 is checked BEFORE the private ranges: it is not the user's
    // own network, it is where the cloud metadata service lives.
    if ip.is_link_local() {
        return Ok(HostClass::LinkLocal);
    }
    let o = ip.octets();
    let cgnat = o[0] == 100 && (o[1] & 0xC0) == 64; // 100.64.0.0/10
    if ip.is_private() || cgnat {
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
    if ip == METADATA_V6 {
        return Err(METADATA_REFUSED.into());
    }
    if ip.is_loopback() || ip.is_unspecified() {
        return Ok(HostClass::Loopback);
    }
    let seg0 = ip.segments()[0];
    let unique_local = (seg0 & 0xfe00) == 0xfc00; // fc00::/7
    let link_local = (seg0 & 0xffc0) == 0xfe80; // fe80::/10
    if link_local {
        return Ok(HostClass::LinkLocal);
    }
    if unique_local {
        return Ok(HostClass::PrivateNetwork);
    }
    Ok(HostClass::Public)
}

fn classify_domain(domain: &str) -> HostClass {
    let d = domain.trim_end_matches('.').to_ascii_lowercase();
    // Only the bare name `localhost` is loopback. `anything.localhost` is a
    // DNS name: browsers and systemd-resolved pin it to 127.0.0.1, but the
    // resolver this app uses is the OS's, and on some systems it asks DNS — so
    // an attacker-chosen `x.localhost` could resolve to a public address. It
    // falls through to the normal rules (public → confirm). Review AK-8.
    if d == "localhost" {
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

/// Validate and normalize an ALREADY-SAVED provider base URL (what every chat
/// dials). An empty value means "use the default local server" and validates as
/// [`OLLAMA_DEFAULT_BASE`].
///
/// Rejects: unparseable input; schemes other than `http`/`https`; a missing
/// host; query strings and fragments (they would corrupt the `{base}/v1/...`
/// join); multicast/broadcast addresses; and cloud metadata addresses.
/// Plain `http://` to a [`HostClass::Public`] host is NOT rejected here — it is
/// flagged ([`ValidatedBase::is_plaintext_to_internet`]) and the caller must get
/// the user's confirmation.
///
/// `user:pass@` is ACCEPTED and flagged ([`ValidatedBase::has_userinfo`]) so a
/// legacy saved `http://user:pass@proxy` keeps working (review AK-9); a warning
/// is logged once per run, without the credentials. A value that is being
/// CHOSEN must use [`validate_new_provider_base_url`] instead.
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
    let has_userinfo = !parsed.username().is_empty() || parsed.password().is_some();
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
    if METADATA_NAMES.contains(&host.as_str()) {
        return Err(METADATA_REFUSED.into());
    }
    if has_userinfo {
        warn_legacy_userinfo_once(&host);
    }

    Ok(ValidatedBase {
        url: parsed.as_str().trim_end_matches('/').to_string(),
        host,
        class,
        plaintext: parsed.scheme() == "http",
        has_userinfo,
    })
}

/// Validate a provider base URL that is being CHOSEN (it came from the webview):
/// everything [`validate_provider_base_url`] checks, and in addition no
/// `user:pass@` — the host a reader sees is not the host reqwest dials (the S-02
/// bypass shape), and credentials belong in the access-token field.
pub(crate) fn validate_new_provider_base_url(raw: &str) -> Result<ValidatedBase, String> {
    let v = validate_provider_base_url(raw)?;
    if v.has_userinfo {
        return Err(USERINFO_REJECTED.into());
    }
    Ok(v)
}

/// The visible warning for AK-9: logged once per run so a legacy credentialed
/// address is not silently left in place; the Settings panel shows the user the
/// same thing. Never includes the credentials.
fn warn_legacy_userinfo_once(host: &str) {
    static WARNED: AtomicBool = AtomicBool::new(false);
    if !WARNED.swap(true, Ordering::Relaxed) {
        tracing::warn!(
            host,
            "ollama base URL contains user:password@ (legacy value) — still used as saved; \
             move the credentials to the access-token field"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(s: &str) -> ValidatedBase {
        validate_provider_base_url(s).unwrap_or_else(|e| panic!("{s:?} should pass: {e}"))
    }
    /// A value that is being CHOSEN: the strict entry point (a superset of the
    /// saved-value checks).
    fn err(s: &str) -> String {
        validate_new_provider_base_url(s).expect_err(&format!("{s:?} should be rejected"))
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
            "http://[::ffff:127.0.0.1]:11434",
        ] {
            assert_eq!(ok(s).class, HostClass::Loopback, "{s}");
        }
    }

    #[test]
    fn only_the_bare_name_localhost_is_loopback() {
        // AK-8: `x.localhost` is a DNS name. Browsers pin it to 127.0.0.1, the OS
        // resolver may not, so it must not get the no-prompt loopback treatment.
        assert_eq!(ok("http://localhost:11434").class, HostClass::Loopback);
        assert_eq!(ok("http://LOCALHOST.:11434").class, HostClass::Loopback);
        for s in [
            "http://foo.localhost:11434",
            "https://a.b.localhost",
            "http://localhost.evil.example",
            "http://notlocalhost.example",
        ] {
            let v = ok(s);
            assert_eq!(v.class, HostClass::Public, "{s}");
        }
        // Plain http to such a name is flagged like any public-looking name.
        assert!(ok("http://foo.localhost:11434").is_plaintext_to_internet());
    }

    #[test]
    fn link_local_is_its_own_class_not_the_users_own_network() {
        // AK-8: 169.254.0.0/16 is where cloud metadata lives; fe80::/10 is the
        // IPv6 twin. Both need the user's confirmation; neither is "private".
        for s in [
            "http://169.254.10.10:11434",
            "http://169.254.0.1",
            "http://[fe80::1]:11434",
            "http://[::ffff:169.254.10.10]:11434",
        ] {
            assert_eq!(ok(s).class, HostClass::LinkLocal, "{s}");
        }
        // Neighbouring ranges are unaffected.
        assert_eq!(ok("http://169.253.1.1").class, HostClass::Public);
        assert_eq!(ok("http://169.255.1.1").class, HostClass::Public);
        assert_eq!(
            ok("http://[fd12:3456::1]:11434").class,
            HostClass::PrivateNetwork
        );
    }

    #[test]
    fn cloud_metadata_endpoints_are_refused_in_every_spelling() {
        for s in [
            "http://169.254.169.254",
            "http://169.254.169.254/latest/meta-data",
            "https://169.254.169.254:8443",
            "http://[::ffff:169.254.169.254]",
            "http://[::ffff:a9fe:a9fe]",
            "http://2852039166", // decimal form of 169.254.169.254
            "http://0xa9.0xfe.0xa9.0xfe",
            "http://169.254.170.2",
            "http://100.100.100.200",
            "http://192.0.0.192",
            "http://[fd00:ec2::254]",
            "http://metadata.google.internal",
            "http://Metadata.Google.Internal.",
            "http://instance-data.ec2.internal",
        ] {
            let e = validate_provider_base_url(s).expect_err(s);
            assert!(e.contains("metadata"), "{s}: {e}");
            // The same refusal for a value being chosen.
            assert!(validate_new_provider_base_url(s).is_err(), "{s}");
        }
        // The rest of the Tailscale / CGNAT range stays a normal private range.
        assert_eq!(
            ok("http://100.100.100.100").class,
            HostClass::PrivateNetwork
        );
    }

    #[test]
    fn private_network_forms_allow_http() {
        for s in [
            "http://192.168.1.20:11434",
            "http://10.0.0.5:11434",
            "http://172.16.4.4:11434",
            "http://172.31.255.1:11434",
            "http://100.101.102.103:11434", // Tailscale CGNAT
            "http://[fd12:3456::1]:11434",
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
    fn a_legacy_saved_value_with_credentials_is_accepted_and_flagged() {
        // AK-9: `http://user:pass@proxy` was valid before the validator existed.
        // The chat path must keep dialling it instead of failing every message.
        let v = ok("http://alice:s3cret@proxy.lan:11434/");
        assert!(v.has_userinfo);
        assert_eq!(v.host, "proxy.lan");
        assert_eq!(v.class, HostClass::PrivateNetwork);
        // The dial address keeps the credentials (reqwest turns them into Basic
        // auth); everything a human or a log sees does not.
        assert_eq!(v.url, "http://alice:s3cret@proxy.lan:11434");
        assert_eq!(v.display_url(), "http://proxy.lan:11434");
        let dbg = format!("{v:?}");
        assert!(!dbg.contains("s3cret") && !dbg.contains("alice"), "{dbg}");
        // The host is the REAL one, so the S-02 look-alike cannot fake a class.
        let sneaky = ok("http://localhost:11434@evil.example");
        assert_eq!(sneaky.host, "evil.example");
        assert_eq!(sneaky.class, HostClass::Public);
        assert!(sneaky.has_userinfo);
        // A value without credentials is not flagged.
        assert!(!ok("http://proxy.lan:11434").has_userinfo);
        // Choosing such a value is still refused.
        assert!(err("http://alice:s3cret@proxy.lan:11434").contains("user:password@"));
    }

    #[test]
    fn redact_userinfo_strips_credentials_and_leaves_other_text_alone() {
        assert_eq!(
            redact_userinfo("https://u:p@h.example:8443/x/"),
            "https://h.example:8443/x"
        );
        assert_eq!(redact_userinfo("https://h.example"), "https://h.example");
        assert_eq!(redact_userinfo("not a url"), "not a url");
    }

    /// The class guard for AK-9: only the saved-value paths may use the
    /// credential-tolerant validator. A new caller that takes a value from the
    /// webview must use `validate_new_provider_base_url`.
    #[test]
    fn only_saved_value_paths_call_the_tolerant_validator() {
        fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            for entry in std::fs::read_dir(dir).unwrap().flatten() {
                let p = entry.path();
                if p.is_dir() {
                    walk(&p, out);
                } else if p.extension().is_some_and(|e| e == "rs") {
                    out.push(p);
                }
            }
        }
        // chat.rs: every chat re-validates the SAVED value. settings_guard.rs:
        // `origin()` compares the SAVED value's origin. ollama_models.rs:
        // `resolve_ollama_probe` compares against the SAVED server.
        const ALLOWED: [&str; 4] = [
            "provider_url.rs",
            "chat.rs",
            "settings_guard.rs",
            "ollama_models.rs",
        ];
        let mut files = Vec::new();
        walk(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
            &mut files,
        );
        let needle = ["validate_provider", "_base_url("].concat();
        let mut offenders = Vec::new();
        for f in files {
            let name = f.file_name().unwrap().to_string_lossy().to_string();
            if ALLOWED.contains(&name.as_str()) {
                continue;
            }
            let text = std::fs::read_to_string(&f).unwrap();
            if text.contains(&needle) {
                offenders.push(f.display().to_string());
            }
        }
        assert!(
            offenders.is_empty(),
            "these files call the credential-tolerant validator on what may be a \
             webview-supplied value; use validate_new_provider_base_url: {offenders:?}"
        );
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
