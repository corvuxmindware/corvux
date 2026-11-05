//! Guard for the Android `ndk_context` contract. Pure file reads — runs on any
//! host, no Android toolchain needed.
//!
//! iroh's network monitor reads the process-global Android context through
//! `ndk_context::android_context()` the first time a sync endpoint is built.
//! Before Tauri 2.12 nothing set it, so the app carried its own JNI shim
//! (called from `MulticastLockPlugin.load()`). Since Tauri 2.12, tao 0.37 sets
//! it in the Activity's `onCreate`, and `ndk_context` asserts it is only set
//! once: keeping the shim made every Android launch abort with
//! `assertion failed: previous.is_none()` (found on-device 2026-11-05).
//!
//! So: tao must stay >= 0.37 (or pairing crashes with "android context was not
//! initialized" again), and nothing in this crate or its local plugins may
//! initialize the context a second time.

use std::fs;
use std::path::{Path, PathBuf};

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn collect(dir: &Path, exts: &[&str], out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            // Skip build output and generated Gradle/Tauri scaffolding.
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if matches!(name, "target" | "build" | ".gradle" | ".tauri") {
                continue;
            }
            collect(&path, exts, out);
        } else if path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| exts.contains(&e))
        {
            out.push(path);
        }
    }
}

#[test]
fn app_does_not_initialize_the_android_context_itself() {
    let root = manifest_dir();
    let mut files = Vec::new();
    collect(&root.join("src"), &["rs"], &mut files);
    collect(&root.join("plugins"), &["rs", "kt", "java"], &mut files);
    assert!(!files.is_empty(), "guard found no source files to scan");

    let needles = ["initialize_android_context(", "nativeInitNdkContext"];
    let mut hits = Vec::new();
    for f in &files {
        let text = fs::read_to_string(f).unwrap_or_default();
        for needle in needles {
            if text.contains(needle) {
                hits.push(format!("{} contains `{needle}`", f.display()));
            }
        }
    }
    assert!(
        hits.is_empty(),
        "tao (Tauri >= 2.12) already initializes ndk_context; a second init \
         aborts every Android launch (`assertion failed: previous.is_none()`):\n{}",
        hits.join("\n")
    );
}

/// Every `tao` version recorded in a Cargo.lock.
fn tao_versions(lock: &str) -> Vec<String> {
    let mut versions = Vec::new();
    let mut lines = lock.lines();
    while let Some(line) = lines.next() {
        if line == "name = \"tao\"" {
            if let Some(v) = lines
                .next()
                .and_then(|l| l.strip_prefix("version = \""))
                .and_then(|l| l.strip_suffix('"'))
            {
                versions.push(v.to_string());
            }
        }
    }
    versions
}

/// tao sets ndk_context in `onCreate` from 0.37 on.
fn initializes_android_context(version: &str) -> bool {
    let mut parts = version.split('.').map(|p| p.parse::<u64>().unwrap_or(0));
    let (major, minor) = (parts.next().unwrap_or(0), parts.next().unwrap_or(0));
    major > 0 || minor >= 37
}

#[test]
fn tao_is_new_enough_to_initialize_the_android_context() {
    let lock = fs::read_to_string(manifest_dir().join("Cargo.lock")).expect("read Cargo.lock");
    let versions = tao_versions(&lock);
    assert!(!versions.is_empty(), "tao not found in Cargo.lock");
    for v in versions {
        assert!(
            initializes_android_context(&v),
            "tao {v} predates the onCreate ndk_context init (0.37): iroh would \
             panic with \"android context was not initialized\" on pairing"
        );
    }
}

#[test]
fn tao_version_check_rejects_old_tao() {
    let lock = "[[package]]\nname = \"tao\"\nversion = \"0.35.2\"\n";
    assert_eq!(tao_versions(lock), ["0.35.2"]);
    assert!(!initializes_android_context("0.35.2"));
    assert!(initializes_android_context("0.37.0"));
    assert!(initializes_android_context("1.0.0"));
}
