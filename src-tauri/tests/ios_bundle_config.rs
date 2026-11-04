//! Guard tests for the iOS bundle config that App Store review depends on.
//!
//! These read the committed `gen/apple` files and assert the keys that, if
//! silently dropped (e.g. by a `tauri ios init` regen), would cause an App
//! Store rejection or break LAN sync. They run on any host — pure file reads,
//! no iOS toolchain needed.

use std::fs;
use std::path::PathBuf;

fn apple_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("gen/apple")
}

#[test]
fn privacy_manifest_declares_required_reason_apis() {
    // Apple rejects builds (since 2024-05-01) that use a required-reason API
    // without declaring it. We must ship a privacy manifest covering at least
    // the file-timestamp + UserDefaults categories the framework layer hits.
    let path = apple_dir().join("PrivacyInfo.xcprivacy");
    let xml = fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("PrivacyInfo.xcprivacy missing at {}: {e}", path.display()));

    for needle in [
        "NSPrivacyAccessedAPICategoryFileTimestamp",
        "C617.1",
        "NSPrivacyAccessedAPICategoryUserDefaults",
        "CA92.1",
        "NSPrivacyTracking",
    ] {
        assert!(
            xml.contains(needle),
            "PrivacyInfo.xcprivacy is missing `{needle}` — App Store Connect will reject the build"
        );
    }
}

#[test]
fn info_plist_declares_bonjour_service_for_lan_sync() {
    // iOS 14+ blocks mDNS browsing unless the Bonjour service types are
    // declared alongside NSLocalNetworkUsageDescription. The service type must
    // match `sync::discovery::mdns::SERVICE_TYPE` (`_corvux-sync._tcp.local.`).
    let path = apple_dir().join("corvux_iOS/Info.plist");
    let xml = fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("Info.plist missing at {}: {e}", path.display()));

    assert!(
        xml.contains("NSLocalNetworkUsageDescription"),
        "Info.plist missing NSLocalNetworkUsageDescription"
    );
    assert!(
        xml.contains("NSBonjourServices"),
        "Info.plist missing NSBonjourServices — iOS will silently block mDNS peer discovery"
    );
    assert!(
        xml.contains("_corvux-sync._tcp"),
        "NSBonjourServices must list `_corvux-sync._tcp` to match the mDNS SERVICE_TYPE"
    );
}
