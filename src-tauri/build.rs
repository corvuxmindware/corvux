fn main() {
    // Re-run the build script (and re-embed the Windows .ico / macOS .icns into
    // the compiled binary) whenever any icon file changes. Cargo otherwise has
    // no way of knowing that `icons/*` feed into the compiled exe's resources,
    // so icon swaps quietly leave the previous binary's icon in place.
    println!("cargo:rerun-if-changed=icons");
    println!("cargo:rerun-if-changed=tauri.conf.json");
    // Surface the iOS bundleVersion (TestFlight build number) to the binary so
    // diagnostic reports can print "0.1.9 (build 15)" instead of a bare
    // version. Multiple TestFlight builds share one marketing version, and
    // debugging from a report that can't say WHICH build produced it has
    // already cost a mis-attributed repro. Cheap string scan on purpose — no
    // JSON dependency in the build script for one field; absent/odd shapes
    // fall back to "unknown".
    println!(
        "cargo:rustc-env=CORVUX_BUNDLE_BUILD={}",
        read_bundle_version().unwrap_or_else(|| "unknown".into())
    );
    tauri_build::build();
}

fn read_bundle_version() -> Option<String> {
    let conf = std::fs::read_to_string("tauri.conf.json").ok()?;
    let key_at = conf.find("\"bundleVersion\"")?;
    let after_colon = conf[key_at..].find(':')? + key_at + 1;
    let rest = conf[after_colon..].trim_start();
    let value = rest.strip_prefix('"')?;
    let end = value.find('"')?;
    Some(value[..end].to_string())
}
