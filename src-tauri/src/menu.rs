//! Native application menu (desktop only).
//!
//! Tauri V2's window-on-Linux/Windows and the native macOS menubar both
//! consume the same `Menu` tree. We wire predefined items where possible
//! (Undo/Redo/Cut/Copy/Paste/Quit/etc. — the OS handles their behavior
//! correctly on every platform) and emit `menu:<id>` events to the
//! frontend for custom actions like "New Note" or "Open Roost" so the
//! existing JS handlers can do the work without us reimplementing them
//! in Rust.
//!
//! Mobile (Android/iOS) doesn't render a native menu, and Tauri V2's
//! `tauri::menu` module is `#[cfg(desktop)]`-only — so this whole file
//! is gated off on mobile via `mod menu;` in `lib.rs`. The call site in
//! `setup` is gated separately for clarity.

use tauri::menu::{Menu, MenuBuilder, MenuItemBuilder, PredefinedMenuItem, SubmenuBuilder};
use tauri::{AppHandle, Manager, Runtime, Wry};
use tauri_specta::Event;

use crate::events;

/// Build the application menu. Called from `lib::run` during `setup`.
///
/// Returns a `Menu<Wry>` ready to install via `app.set_menu(menu)`.
pub fn build<R: Runtime>(app: &AppHandle<R>) -> tauri::Result<Menu<R>> {
    let file = SubmenuBuilder::new(app, "File")
        .item(
            &MenuItemBuilder::with_id("new_note", "New Note")
                .accelerator("CmdOrCtrl+N")
                .build(app)?,
        )
        .item(
            &MenuItemBuilder::with_id("open_roost", "Open Roost…")
                .accelerator("CmdOrCtrl+O")
                .build(app)?,
        )
        .separator()
        .item(&PredefinedMenuItem::quit(app, None)?)
        .build()?;

    // Predefined edit items map to native OS commands on every platform —
    // they're what makes Cmd+Z behave correctly inside the WebView's
    // CodeMirror editor on macOS. We don't add custom edit items because
    // CodeMirror already has its own keybindings; this menu is about
    // discoverability ("the menu exists, the shortcuts are listed there")
    // rather than redirecting events.
    let edit = SubmenuBuilder::new(app, "Edit")
        .item(&PredefinedMenuItem::undo(app, None)?)
        .item(&PredefinedMenuItem::redo(app, None)?)
        .separator()
        .item(&PredefinedMenuItem::cut(app, None)?)
        .item(&PredefinedMenuItem::copy(app, None)?)
        .item(&PredefinedMenuItem::paste(app, None)?)
        .item(&PredefinedMenuItem::select_all(app, None)?)
        .build()?;

    let view = SubmenuBuilder::new(app, "View")
        .item(
            &MenuItemBuilder::with_id("toggle_sidebar", "Toggle Sidebar")
                .accelerator("CmdOrCtrl+B")
                .build(app)?,
        )
        .item(
            &MenuItemBuilder::with_id("toggle_chat", "Toggle Chat")
                .accelerator("CmdOrCtrl+Shift+L")
                .build(app)?,
        )
        .item(
            &MenuItemBuilder::with_id("toggle_graph", "Toggle Graph")
                .accelerator("CmdOrCtrl+Shift+G")
                .build(app)?,
        )
        .separator()
        .item(&PredefinedMenuItem::fullscreen(app, None)?)
        .build()?;

    let help = SubmenuBuilder::new(app, "Help")
        .item(&MenuItemBuilder::with_id("open_github", "GitHub").build(app)?)
        .item(
            &MenuItemBuilder::with_id("about", concat!("About Corvux ", env!("CARGO_PKG_VERSION")))
                .build(app)?,
        )
        .build()?;

    MenuBuilder::new(app)
        .item(&file)
        .item(&edit)
        .item(&view)
        .item(&help)
        .build()
}

/// Register the menu-event handler. Forwards every custom menu click to
/// the frontend as `menu:<id>` (e.g. `menu:new_note`). The frontend's
/// existing keyboard handlers can listen for these and trigger the same
/// flow as the keyboard shortcut.
///
/// Must be called *after* `app.set_menu(...)` so the Tauri runtime knows
/// which menu the events come from.
pub fn install_handler(app: &AppHandle<Wry>) {
    let handle = app.clone();
    app.on_menu_event(move |_app, event| {
        // PredefinedMenuItems (Undo, Cut, …) handle themselves at the OS
        // level and don't surface in this callback on macOS; the events
        // here are exclusively the custom ids we registered above. Forward
        // every one of them to the frontend so the JS layer decides what
        // to do — keeps menu wiring out of Rust.
        if let Some(window) = handle.get_webview_window("main") {
            let id = event.id().0.as_str();
            let _ = events::MenuEvent(id.to_string()).emit(&window);
        }
    });
}
