//! Regenerate `src/lib/bindings.ts` from the tauri-specta builder without
//! booting the Tauri runtime.
//!
//! Usage:
//!   cd Corvux/src-tauri && cargo run --example gen_bindings
//!
//! The dev loop (`pnpm tauri dev`) already regenerates bindings on every
//! boot via the same exporter. This binary is for:
//!   1. CI — regenerate then `git diff --exit-code` to assert bindings
//!      are in sync with the Rust source.
//!   2. Fresh clones — populate `bindings.ts` without launching the GUI.
//!   3. Quick iteration — preview the generated output after a struct
//!      change without waiting for the full dev-build cycle.

use corvux_lib::specta_bridge;

fn main() {
    specta_bridge::build()
        .export(specta_bridge::exporter(), specta_bridge::BINDINGS_PATH)
        .expect("failed to export tauri-specta bindings");
    eprintln!("Wrote {}", specta_bridge::BINDINGS_PATH);
}
