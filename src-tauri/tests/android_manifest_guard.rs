//! Guard for Google Play device reach. Pure file reads — runs on any host.
//!
//! Android implies a REQUIRED hardware feature from some permissions:
//! ACCESS_WIFI_STATE / CHANGE_WIFI_MULTICAST_STATE imply
//! `android.hardware.wifi`, CAMERA implies `android.hardware.camera`
//! (+ autofocus), RECORD_AUDIO implies `android.hardware.microphone`.
//! Unless the manifest declares the feature with `required="false"`, Play
//! hides Corvux from every device without that hardware, and the Play
//! pre-review check blocks the release ("Required hardware feature
//! declarations are restricting your app's availability", 2026-11-24 for
//! `android.hardware.wifi`). None of these is essential: Wi-Fi only helps LAN
//! sync discovery, camera only scans pairing QR codes, the mic only records
//! voice memos.

use std::fs;
use std::path::PathBuf;

const APP_MANIFEST: &str = "gen/android/app/src/main/AndroidManifest.xml";
const MULTICAST_PLUGIN: &str =
    "plugins/tauri-plugin-multicast-lock/android/src/main/java/MulticastLockPlugin.kt";

fn read(rel: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// Manifest text with XML comments removed and whitespace collapsed, so
/// commented-out or multi-line declarations are judged on what Android sees.
fn manifest() -> String {
    let xml = read(APP_MANIFEST);
    let mut out = String::with_capacity(xml.len());
    let mut rest = xml.as_str();
    while let Some(start) = rest.find("<!--") {
        out.push_str(&rest[..start]);
        match rest[start..].find("-->") {
            Some(end) => rest = &rest[start + end + 3..],
            None => {
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn declares_optional(xml: &str, feature: &str) -> bool {
    xml.match_indices("<uses-feature").any(|(i, _)| {
        let tag = &xml[i..i + xml[i..].find('>').unwrap_or(xml.len() - i)];
        tag.contains(&format!("android:name=\"{feature}\""))
            && tag.contains("android:required=\"false\"")
    })
}

#[test]
fn permission_implied_features_are_optional() {
    let xml = manifest();
    let implied = [
        (
            "android.permission.ACCESS_WIFI_STATE",
            "android.hardware.wifi",
        ),
        (
            "android.permission.CHANGE_WIFI_MULTICAST_STATE",
            "android.hardware.wifi",
        ),
        ("android.permission.CAMERA", "android.hardware.camera"),
        (
            "android.permission.CAMERA",
            "android.hardware.camera.autofocus",
        ),
        (
            "android.permission.RECORD_AUDIO",
            "android.hardware.microphone",
        ),
    ];
    for (permission, feature) in implied {
        if xml.contains(&format!("android:name=\"{permission}\"")) {
            assert!(
                declares_optional(&xml, feature),
                "{permission} implies {feature} as REQUIRED; declare \
                 <uses-feature android:name=\"{feature}\" android:required=\"false\" /> \
                 or Google Play hides Corvux from devices without it"
            );
        }
    }
}

#[test]
fn multicast_lock_tolerates_missing_wifi() {
    // With Wi-Fi optional the app installs on devices without a WifiManager;
    // a hard `as WifiManager` cast there throws on every sync start.
    let src = read(MULTICAST_PLUGIN);
    assert!(
        !src.contains("as WifiManager\n") && src.contains("as? WifiManager"),
        "MulticastLockPlugin must use a safe cast (`as? WifiManager`) and reject \
         cleanly when the device has no Wi-Fi"
    );
}

#[test]
fn declares_optional_reads_tags_not_neighbours() {
    let xml = r#"<uses-feature android:name="android.hardware.wifi" /> <uses-feature android:name="x" android:required="false" />"#;
    assert!(!declares_optional(xml, "android.hardware.wifi"));
    assert!(declares_optional(xml, "x"));
}
