//! Modern macOS notifications via `UNUserNotificationCenter`.
//!
//! `tauri-plugin-notification`'s desktop path uses notify-rust →
//! mac-notification-sys, which relies on the legacy `NSUserNotification` API:
//! it never shows the "Allow notifications?" prompt and is unreliable on recent
//! macOS (26+). This module posts through the modern UserNotifications
//! framework instead, which (a) requests authorization — showing the real
//! system prompt — and (b) delivers reliably.
//!
//! IMPORTANT: `UNUserNotificationCenter.currentNotificationCenter()` requires
//! the process to run as a real, signed `.app` bundle. `tauri dev` launches the
//! bare binary, where this throws an `NSInternalInconsistencyException`. Every
//! entry point therefore bails under `tauri::is_dev()`, and callers fall back to
//! the plugin's notify-rust path (which is fine for dev / Windows / Linux).

#![cfg(target_os = "macos")]

use block2::RcBlock;
use objc2::runtime::Bool;
use objc2_foundation::{NSError, NSString};
use objc2_user_notifications::{
    UNAuthorizationOptions, UNMutableNotificationContent, UNNotificationRequest,
    UNNotificationSound, UNUserNotificationCenter,
};

/// Safe to touch `UNUserNotificationCenter`? Only in a real bundle (not dev).
fn usable() -> bool {
    !tauri::is_dev()
}

/// Ask the OS for notification permission. The first call shows the system
/// "Corvux would like to send you notifications" prompt; later calls return the
/// stored decision without re-prompting. No-op in dev. Safe to call repeatedly.
pub fn request_authorization() {
    if !usable() {
        return;
    }
    let center = UNUserNotificationCenter::currentNotificationCenter();
    let opts = UNAuthorizationOptions::Alert
        | UNAuthorizationOptions::Sound
        | UNAuthorizationOptions::Badge;
    // The system invokes this asynchronously; RcBlock keeps it alive past this
    // call. We don't act on the result here — delivery just no-ops if denied.
    let handler = RcBlock::new(|_granted: Bool, _err: *mut NSError| {});
    center.requestAuthorizationWithOptions_completionHandler(opts, &handler);
}

/// Post an immediate notification through the modern API. Returns `false` when
/// the native path isn't usable (dev), so the caller can fall back to the
/// plugin. `identifier` dedupes/replaces a prior notification with the same id.
pub fn post(identifier: &str, title: &str, body: &str, with_sound: bool) -> bool {
    if !usable() {
        return false;
    }
    let center = UNUserNotificationCenter::currentNotificationCenter();

    let content = UNMutableNotificationContent::new();
    content.setTitle(&NSString::from_str(title));
    content.setBody(&NSString::from_str(body));
    if with_sound {
        content.setSound(Some(&UNNotificationSound::defaultSound()));
    }

    let id = NSString::from_str(identifier);
    // `trigger: None` = deliver immediately. The desktop scheduler already
    // sleeps until the due instant, so we post right when it fires.
    let request = UNNotificationRequest::requestWithIdentifier_content_trigger(&id, &content, None);

    center.addNotificationRequest_withCompletionHandler(&request, None);
    true
}
