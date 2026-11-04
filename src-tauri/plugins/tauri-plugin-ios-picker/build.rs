const COMMANDS: &[&str] = &["pick_folder", "resume_access", "release_access"];

fn main() {
    let result = tauri_plugin::Builder::new(COMMANDS)
        .ios_path("ios")
        .try_build();

    // On non-iOS targets the iOS path doesn't exist; the SAF plugin uses the
    // same defensive pattern. Allow the build to proceed so this crate can
    // compile on desktop / Android without the iOS bits.
    if !(cfg!(docsrs) && std::env::var("TARGET").unwrap_or_default().contains("ios")) {
        result.unwrap();
    }
}
