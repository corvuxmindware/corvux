//! Corvux-internal Tauri plugin that acquires/releases an Android
//! `WifiManager.MulticastLock` for the duration of a sync session.
//!
//! Why it exists: by default Android filters out incoming WiFi multicast
//! packets to save battery. mDNS service discovery is multicast, so
//! without a MulticastLock, our `mdns-sd` browser sees zero peers on
//! Android even when desktop devices are advertising on the same LAN.
//! Acquiring the lock allows the OS to deliver multicast packets to our
//! process; releasing it on sync stop returns to default battery
//! behavior.
//!
//! Platforms:
//! - **Android**: real implementation, calls
//!   `WifiManager.createMulticastLock("corvux-sync")`.
//! - **Desktop / iOS**: stubs that return `Ok(())` so callers don't need
//!   to cfg-gate every site. Desktop OSes don't filter multicast and
//!   iOS uses a different API path that this plugin doesn't cover.
//!
//! The required `<uses-permission android:name="android.permission
//! .CHANGE_WIFI_MULTICAST_STATE" />` is already declared in the main
//! app's `AndroidManifest.xml`.

use tauri::{
    plugin::{Builder, TauriPlugin},
    Manager, Runtime,
};

pub use error::{Error, Result};

mod commands;
mod error;

#[cfg(target_os = "android")]
const PLUGIN_IDENTIFIER: &str = "app.tauri.multicastlock";

/// Handle to the plugin's mobile backend. Present in state only on
/// Android — other platforms don't register it, and the command layer
/// no-ops without touching this.
#[cfg(target_os = "android")]
pub struct MulticastLock<R: Runtime>(tauri::plugin::PluginHandle<R>);

#[cfg(target_os = "android")]
impl<R: Runtime> MulticastLock<R> {
    pub fn acquire(&self) -> Result<()> {
        self.0
            .run_mobile_plugin::<serde_json::Value>("acquire", ())
            .map(|_| ())
            .map_err(Into::into)
    }

    pub fn release(&self) -> Result<()> {
        self.0
            .run_mobile_plugin::<serde_json::Value>("release", ())
            .map(|_| ())
            .map_err(Into::into)
    }
}

#[cfg(target_os = "android")]
pub trait MulticastLockExt<R: Runtime> {
    fn multicast_lock(&self) -> &MulticastLock<R>;
}

#[cfg(target_os = "android")]
impl<R: Runtime, T: Manager<R>> MulticastLockExt<R> for T {
    fn multicast_lock(&self) -> &MulticastLock<R> {
        self.state::<MulticastLock<R>>().inner()
    }
}

// Empty trait stub on non-Android so the `commands` module compiles
// uniformly across targets. Mirrors the pattern the SAF plugin uses.
#[cfg(not(target_os = "android"))]
pub trait MulticastLockExt<R: Runtime> {}
#[cfg(not(target_os = "android"))]
impl<R: Runtime, T: Manager<R>> MulticastLockExt<R> for T {}

pub fn init<R: Runtime>() -> TauriPlugin<R> {
    Builder::new("multicast-lock")
        .invoke_handler(tauri::generate_handler![
            commands::acquire,
            commands::release
        ])
        .setup(|_app, _api| {
            #[cfg(target_os = "android")]
            {
                let handle =
                    _api.register_android_plugin(PLUGIN_IDENTIFIER, "MulticastLockPlugin")?;
                _app.manage(MulticastLock(handle));
            }
            Ok(())
        })
        .build()
}
