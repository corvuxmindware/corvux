//! Guard tests for the Windows bundle config the auto-updating release depends
//! on. Pure file reads — run on any host, no Windows toolchain needed.
//!
//! Windows ships as a direct NSIS download plus in-app auto-update — there is
//! no store-review gate in front of users, so the security-critical config is
//! the **updater itself**. Tauri applies a downloaded installer only if its
//! minisign signature verifies against the embedded `pubkey`, and it fetches
//! the manifest over the configured endpoint. Drop the pubkey, or flip the
//! endpoint to http, and a network attacker who can answer for the host can
//! ship a malicious "update" — silent remote code execution on every client.
//!
//! These tests pin that config so a future edit can't regress it unnoticed.
//! They mirror the iOS/macOS bundle-config guards (plist/entitlements); here
//! the artifact is `tauri.conf.json`.

use std::fs;
use std::path::PathBuf;

fn tauri_conf() -> serde_json::Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tauri.conf.json");
    let text = fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("tauri.conf.json missing at {}: {e}", path.display()));
    serde_json::from_str(&text).expect("tauri.conf.json must be valid JSON")
}

#[test]
fn updater_endpoints_are_https() {
    let conf = tauri_conf();
    let endpoints = conf["plugins"]["updater"]["endpoints"]
        .as_array()
        .expect("plugins.updater.endpoints must be an array");
    assert!(
        !endpoints.is_empty(),
        "updater must declare at least one endpoint"
    );
    for ep in endpoints {
        let url = ep.as_str().expect("each updater endpoint must be a string");
        assert!(
            url.starts_with("https://"),
            "updater endpoint must be HTTPS — an http manifest lets a network \
             attacker serve a malicious update: {url}"
        );
    }
}

#[test]
fn updater_signature_pubkey_is_present() {
    let conf = tauri_conf();
    let pubkey = conf["plugins"]["updater"]["pubkey"].as_str().expect(
        "plugins.updater.pubkey must be set — without it the updater would \
         apply UNSIGNED downloads",
    );
    // A real minisign public key (base64 of the untrusted-comment + key) is
    // well over 100 chars; this catches an empty string or a `""` placeholder.
    assert!(
        pubkey.len() > 40,
        "updater pubkey looks empty/placeholder ({} chars) — the minisign key \
         is what verifies every downloaded installer",
        pubkey.len()
    );
}

#[test]
fn updater_artifacts_are_emitted() {
    let conf = tauri_conf();
    assert_eq!(
        conf["bundle"]["createUpdaterArtifacts"].as_bool(),
        Some(true),
        "bundle.createUpdaterArtifacts must be true or `tauri build` won't emit \
         the signed `.sig` the updater verifies against"
    );
}

#[test]
fn nsis_is_a_bundle_target() {
    let conf = tauri_conf();
    let targets = &conf["bundle"]["targets"];
    let has_nsis = match targets {
        // `"all"` builds every installer incl. NSIS; an explicit list must name it.
        serde_json::Value::String(s) => s == "all" || s == "nsis",
        serde_json::Value::Array(a) => a.iter().any(|t| t.as_str() == Some("nsis")),
        _ => false,
    };
    assert!(
        has_nsis,
        "bundle.targets must include `nsis` — the Windows installer the updater \
         downloads and replaces in place"
    );
}
