//! Guard for the sync engine's DNS setup. Pure file reads — runs on any host.
//!
//! Reliance Jio's resolver answers REFUSED for `dns.iroh.link` (found
//! 2026-11-05), so with iroh's default (system-only) resolver the N0 pkarr
//! publish and DNS address lookup fail on Jio and a device that changes
//! networks can't be re-found. `build_endpoint` therefore hands iroh a
//! resolver that keeps the system configuration first and adds public
//! fallbacks. These tests keep that wiring from being dropped by a refactor.

use std::fs;
use std::path::PathBuf;

fn iroh_transport_source() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/sync/transport/iroh.rs");
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// Body of `fn name` from its signature to the first line that is exactly `}`.
fn fn_body<'a>(src: &'a str, signature: &str) -> &'a str {
    let start = src
        .find(signature)
        .unwrap_or_else(|| panic!("`{signature}` not found in iroh.rs"));
    let end = src[start..]
        .find("\n}\n")
        .map(|i| start + i)
        .expect("function body must end with a top-level `}`");
    &src[start..end]
}

#[test]
fn endpoint_uses_the_fallback_dns_resolver() {
    let src = iroh_transport_source();
    let body = fn_body(&src, "pub async fn build_endpoint(");
    assert!(
        body.contains(".dns_resolver(sync_dns_resolver())"),
        "build_endpoint must pass `sync_dns_resolver()` to the iroh endpoint — \
         without it, discovery fails on ISPs that refuse dns.iroh.link (Jio)"
    );
}

#[test]
fn fallback_resolver_keeps_the_system_dns_first() {
    let src = iroh_transport_source();
    let body = fn_body(&src, "fn sync_dns_resolver()");
    assert!(
        body.contains(".with_system_defaults()"),
        "sync_dns_resolver must start from the system DNS configuration; the \
         public resolvers are only a fallback"
    );
    assert!(
        body.contains("SYNC_DNS_FALLBACKS"),
        "sync_dns_resolver must add SYNC_DNS_FALLBACKS"
    );
    let consts = &src[src
        .find("const SYNC_DNS_FALLBACKS")
        .expect("SYNC_DNS_FALLBACKS must exist")..];
    let decl = &consts[..consts
        .find("];")
        .expect("SYNC_DNS_FALLBACKS must be an array")];
    assert!(
        decl.matches("SocketAddr::new").count() >= 2,
        "keep at least two independent fallback resolvers"
    );
}
