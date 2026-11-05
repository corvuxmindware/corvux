//! Guard for the sync engine's DNS setup. Pure file reads — runs on any host.
//!
//! Reliance Jio's resolver answers REFUSED for `dns.iroh.link` (found
//! 2026-11-05), so with iroh's default (system-only) resolver the N0 pkarr
//! publish and DNS address lookup fail on Jio and a device that changes
//! networks can't be re-found. `build_endpoint` therefore hands iroh the
//! `SystemThenPublic` resolver from `src/sync/transport/dns_fallback.rs`: the
//! system DNS first, public resolvers only as an explicit second step (adding
//! them to hickory's pool stopped working in hickory-resolver 0.26.3). The
//! fallback behaviour itself is unit-tested in that module; these tests keep
//! the wiring from being dropped by a refactor.

use std::fs;
use std::path::PathBuf;

fn source(rel: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// Body of `fn name` from its signature to the first line that is exactly `}`.
fn fn_body<'a>(src: &'a str, signature: &str) -> &'a str {
    let start = src
        .find(signature)
        .unwrap_or_else(|| panic!("`{signature}` not found"));
    let end = src[start..]
        .find("\n}\n")
        .map(|i| start + i)
        .expect("function body must end with a top-level `}`");
    &src[start..end]
}

#[test]
fn endpoint_uses_the_fallback_dns_resolver() {
    let src = source("src/sync/transport/iroh.rs");
    let body = fn_body(&src, "pub async fn build_endpoint(");
    assert!(
        body.contains(".dns_resolver(super::dns_fallback::sync_dns_resolver())"),
        "build_endpoint must pass `dns_fallback::sync_dns_resolver()` to the iroh \
         endpoint — without it, discovery fails on ISPs that refuse dns.iroh.link (Jio)"
    );
}

#[test]
fn fallback_resolver_asks_the_system_dns_first() {
    let src = source("src/sync/transport/dns_fallback.rs");
    assert!(
        fn_body(&src, "pub fn sync_dns_resolver()").contains("SystemThenPublic::new()"),
        "sync_dns_resolver must build the SystemThenPublic resolver"
    );
    let new = &src[src
        .find("pub fn new() -> Self")
        .expect("SystemThenPublic::new must exist")..];
    let new = &new[..new.find("\n    }\n").expect("end of new()")];
    let system = new.find("with_system_defaults()");
    let public = new.find("SYNC_DNS_FALLBACKS");
    assert!(
        matches!((system, public), (Some(s), Some(p)) if s < p),
        "SystemThenPublic::new must build the system resolver first and the \
         SYNC_DNS_FALLBACKS one second"
    );
    let decl = &src[src
        .find("pub const SYNC_DNS_FALLBACKS")
        .expect("SYNC_DNS_FALLBACKS must exist")..];
    let decl = &decl[..decl
        .find("];")
        .expect("SYNC_DNS_FALLBACKS must be an array")];
    assert!(
        decl.matches("SocketAddr::new").count() >= 2,
        "keep at least two independent fallback resolvers"
    );
}
