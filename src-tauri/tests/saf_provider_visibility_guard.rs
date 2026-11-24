//! Guard for Android package visibility of documents providers. Pure file
//! reads — runs on any host, no Android toolchain needed.
//!
//! A roost picked from a third-party documents provider (cloud or file-manager
//! app) is reached through that app's ContentProvider. Since Android 11 an app
//! only "sees" other apps it declares in `<queries>` or got implicit access to.
//! The system picker gives Corvux implicit access only, and Android drops it
//! when Corvux is updated and on every reboot. The persisted URI grant
//! survives, but the provider lookup then returns null ("Failed to find
//! provider info") and the roost silently fails to open. Found on a OnePlus 7
//! (Android 12) on 2026-11-24 after an in-place update: the SAF Fault Lab
//! roost would not open although `dumpsys activity permissions` still listed
//! the grant, and `dumpsys package queries` showed Corvux could see no
//! provider app.

use std::fs;
use std::path::PathBuf;

const SAF_PLUGIN_MANIFEST: &str = "plugins/tauri-plugin-saf/android/src/main/AndroidManifest.xml";

fn read(rel: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// Text of the manifest with XML comments removed, so a commented-out
/// declaration does not satisfy the guard.
fn without_comments(xml: &str) -> String {
    let mut out = String::with_capacity(xml.len());
    let mut rest = xml;
    while let Some(start) = rest.find("<!--") {
        out.push_str(&rest[..start]);
        match rest[start..].find("-->") {
            Some(end) => rest = &rest[start + end + 3..],
            None => return out,
        }
    }
    out.push_str(rest);
    out
}

#[test]
fn saf_plugin_declares_documents_provider_query() {
    let xml = without_comments(&read(SAF_PLUGIN_MANIFEST));
    let start = xml
        .find("<queries>")
        .expect("the SAF plugin manifest must declare a <queries> element");
    let end = xml[start..]
        .find("</queries>")
        .map(|e| start + e)
        .expect("unterminated <queries> element");
    let queries = &xml[start..end];
    assert!(
        queries.contains("android.content.action.DOCUMENTS_PROVIDER"),
        "<queries> must include an <intent> for android.content.action.DOCUMENTS_PROVIDER, \
         or roosts in third-party documents providers stop opening after an app update or reboot"
    );
}

#[test]
fn saf_plugin_does_not_use_query_all_packages() {
    // The narrow intent query is enough; QUERY_ALL_PACKAGES needs a Play
    // policy declaration and is not justified for Corvux.
    let xml = without_comments(&read(SAF_PLUGIN_MANIFEST));
    assert!(!xml.contains("QUERY_ALL_PACKAGES"));
    let app = without_comments(&read("gen/android/app/src/main/AndroidManifest.xml"));
    assert!(!app.contains("QUERY_ALL_PACKAGES"));
}

#[test]
fn comment_stripper_ignores_commented_declarations() {
    let xml = "<a><!-- <queries>DOCUMENTS_PROVIDER</queries> --></a>";
    assert!(!without_comments(xml).contains("<queries>"));
}
