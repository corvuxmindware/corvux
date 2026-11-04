const COMMANDS: &[&str] = &["acquire", "release"];

fn main() {
    let result = tauri_plugin::Builder::new(COMMANDS)
        .android_path("android")
        .try_build();

    if !(cfg!(docsrs)
        && std::env::var("TARGET")
            .unwrap_or_default()
            .contains("android"))
    {
        result.unwrap();
    }
}
