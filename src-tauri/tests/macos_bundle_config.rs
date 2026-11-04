//! Guard tests for the macOS bundle config the signed/notarized build depends
//! on. Pure file reads — run on any host, no macOS toolchain needed.
//!
//! macOS ships Developer ID direct (not the Mac App Store), so the gates here
//! are Hardened-Runtime entitlements + TCC usage strings for the in-app media
//! features (voice-memo mic, QR-pairing camera) — not App Store review. Without
//! them the OS denies mic/camera on a signed build even though dev works.

use std::fs;
use std::path::PathBuf;

fn src_tauri() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn entitlements_grant_mic_and_camera_and_nothing_dangerous() {
    let path = src_tauri().join("Entitlements.plist");
    let xml = fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("Entitlements.plist missing at {}: {e}", path.display()));

    for needle in [
        "com.apple.security.device.audio-input",
        "com.apple.security.device.camera",
    ] {
        assert!(
            xml.contains(needle),
            "Entitlements.plist must grant `{needle}` (Hardened Runtime media access)"
        );
    }
    // Least-privilege lock: none of the runtime-weakening entitlements, and no
    // sandbox (Corvux ships unsandboxed on macOS — sandbox breaks MCP + sync).
    // Match the actual `<key>…</key>` DECLARATION, not bare substrings, so the
    // file's own explanatory comment (which names these to warn against them)
    // doesn't trip the check.
    for forbidden in [
        "<key>com.apple.security.cs.allow-unsigned-executable-memory</key>",
        "<key>com.apple.security.cs.disable-library-validation</key>",
        "<key>com.apple.security.cs.allow-dyld-environment-variables</key>",
        "<key>com.apple.security.cs.allow-jit</key>",
        "<key>com.apple.security.app-sandbox</key>",
    ] {
        assert!(
            !xml.contains(forbidden),
            "Entitlements.plist must NOT declare `{forbidden}` — it weakens the Hardened Runtime"
        );
    }
}

#[test]
fn info_plist_declares_mic_and_camera_usage_strings() {
    let path = src_tauri().join("Info.plist");
    let xml = fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("Info.plist missing at {}: {e}", path.display()));
    for needle in ["NSMicrophoneUsageDescription", "NSCameraUsageDescription"] {
        assert!(
            xml.contains(needle),
            "Info.plist must declare `{needle}` (TCC requires it under Hardened Runtime)"
        );
    }
}

#[test]
fn tauri_conf_references_the_macos_entitlements() {
    let path = src_tauri().join("tauri.conf.json");
    let json = fs::read_to_string(&path).expect("tauri.conf.json");
    assert!(
        json.contains("Entitlements.plist"),
        "tauri.conf.json bundle.macOS.entitlements must reference Entitlements.plist, \
         or the entitlements never reach codesign"
    );
}
