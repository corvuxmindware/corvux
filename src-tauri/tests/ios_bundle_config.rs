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
        // `sysinfo::Disks` (ai::capability::detect, free space before a model
        // download) links `statfs`/`fstatfs` — seen in `nm -u` of build 24.
        "NSPrivacyAccessedAPICategoryDiskSpace",
        "E174.1",
        "NSPrivacyTracking",
    ] {
        assert!(
            xml.contains(needle),
            "PrivacyInfo.xcprivacy is missing `{needle}` — App Store Connect will reject the build"
        );
    }
}

/// The manifest only counts if it is INSIDE the .app. Through build 24 the
/// file sat in `gen/apple/` but neither project.yml nor the committed
/// project.pbxproj referenced it, so every TestFlight build shipped without
/// it (verified: no `*.xcprivacy` in `build/corvux_iOS.xcarchive`). Both files
/// are checked because builds use the hand-maintained pbxproj (manual App
/// Store signing lives there) while a regen reads project.yml.
#[test]
fn privacy_manifest_is_wired_into_the_app_bundle() {
    let yml = fs::read_to_string(apple_dir().join("project.yml")).expect("project.yml");
    let entry = yml
        .split("- path: ")
        .find(|chunk| chunk.starts_with("PrivacyInfo.xcprivacy"))
        .expect("project.yml sources must list `- path: PrivacyInfo.xcprivacy`");
    let entry_head: String = entry.lines().take(3).collect::<Vec<_>>().join("\n");
    assert!(
        entry_head.contains("buildPhase: resources"),
        "project.yml must put PrivacyInfo.xcprivacy in `buildPhase: resources`, got:\n{entry_head}"
    );

    let pbx = fs::read_to_string(apple_dir().join("corvux.xcodeproj/project.pbxproj"))
        .expect("project.pbxproj");
    let phase_start = pbx
        .find("isa = PBXResourcesBuildPhase;")
        .expect("pbxproj has no PBXResourcesBuildPhase");
    let phase = &pbx[phase_start..];
    let phase = &phase[..phase.find("};").expect("unterminated resources phase")];
    assert!(
        phase.contains("PrivacyInfo.xcprivacy in Resources"),
        "project.pbxproj's Resources build phase does not copy PrivacyInfo.xcprivacy into the app"
    );
    assert!(
        pbx.contains("path = PrivacyInfo.xcprivacy;"),
        "project.pbxproj has no PBXFileReference for PrivacyInfo.xcprivacy"
    );
}

/// v1 ships iPhone-only (decision 2026-11-06). iPad support can be ADDED in a
/// later update, but once an App Store version supports iPad it can never be
/// removed — so it must not slip in via a regen (xcodegen defaults to "1,2").
/// Change this test deliberately when iPad layout + 13" screenshots are ready.
#[test]
fn ios_targets_iphone_only_until_ipad_is_a_deliberate_decision() {
    let pbx = fs::read_to_string(apple_dir().join("corvux.xcodeproj/project.pbxproj"))
        .expect("project.pbxproj");
    let families: Vec<&str> = pbx
        .lines()
        .filter(|l| l.contains("TARGETED_DEVICE_FAMILY"))
        .map(str::trim)
        .collect();
    assert!(
        !families.is_empty(),
        "pbxproj sets no TARGETED_DEVICE_FAMILY"
    );
    for line in &families {
        assert_eq!(
            *line, "TARGETED_DEVICE_FAMILY = 1;",
            "every build configuration must target iPhone only"
        );
    }
    let yml = fs::read_to_string(apple_dir().join("project.yml")).expect("project.yml");
    assert!(
        yml.contains("TARGETED_DEVICE_FAMILY: \"1\""),
        "project.yml must pin TARGETED_DEVICE_FAMILY: \"1\" or a regen re-adds iPad"
    );
}

/// Export compliance (decision 2026-11-06): Corvux ships its own standard
/// crypto (XChaCha20-Poly1305 + X25519 for E2E sync, rustls TLS), so it is
/// NOT exempt. Declared `true`; distributed outside France until the French
/// encryption declaration is filed. `false` would be a false declaration.
#[test]
fn ios_declares_non_exempt_encryption() {
    for rel in ["../../Info.plist", "corvux_iOS/Info.plist"] {
        let xml = fs::read_to_string(apple_dir().join(rel)).expect(rel);
        let after = xml
            .split("<key>ITSAppUsesNonExemptEncryption</key>")
            .nth(1)
            .unwrap_or_else(|| panic!("{rel} lacks ITSAppUsesNonExemptEncryption"));
        assert!(
            after.trim_start().starts_with("<true/>"),
            "{rel}: ITSAppUsesNonExemptEncryption must be <true/>"
        );
    }
}

/// Every permission prompt needs its usage string or iOS kills the app on the
/// first request (and App Review rejects under 5.1.1). xcodegen rewrites
/// corvux_iOS/Info.plist from project.yml's `info.properties` on any regen —
/// that wiped these keys once (2026-11-06) — so both files must carry them.
#[test]
fn usage_strings_survive_in_plist_and_regen_source() {
    let plist = fs::read_to_string(apple_dir().join("corvux_iOS/Info.plist")).expect("Info.plist");
    let yml = fs::read_to_string(apple_dir().join("project.yml")).expect("project.yml");
    for key in [
        "NSCameraUsageDescription",
        "NSMicrophoneUsageDescription",
        "NSPhotoLibraryUsageDescription",
        "NSLocalNetworkUsageDescription",
        "NSBonjourServices",
        "ITSAppUsesNonExemptEncryption",
    ] {
        assert!(
            plist.contains(&format!("<key>{key}</key>")),
            "Info.plist lacks {key}"
        );
        assert!(
            yml.contains(&format!("{key}:")),
            "project.yml info.properties lacks {key}"
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
