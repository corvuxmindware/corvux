//! Corvux-internal Tauri plugin exposing iOS's `UIDocumentPickerViewController`
//! folder picker, with security-scoped bookmark persistence so the picked
//! folder remains accessible across cold starts.
//!
//! Desktop / Android stub: every call returns [`Error::NotAvailable`].

use tauri::{
    plugin::{Builder, TauriPlugin},
    Manager, Runtime,
};

pub use error::{Error, Result};
pub use models::*;

mod commands;
mod error;
mod models;

#[cfg(target_os = "ios")]
tauri::ios_plugin_binding!(init_plugin_ios_picker);

/// Handle to the iOS picker plugin's mobile backend. Present in state
/// only on iOS — other platforms don't register it.
#[cfg(target_os = "ios")]
pub struct IosPicker<R: Runtime>(tauri::plugin::PluginHandle<R>);

#[cfg(target_os = "ios")]
impl<R: Runtime> IosPicker<R> {
    pub fn pick_folder(&self) -> Result<PickedFolder> {
        self.0
            .run_mobile_plugin::<PickedFolder>("pickFolder", ())
            .map_err(Into::into)
    }

    pub fn resume_access(&self, bookmark: &str) -> Result<ResumedAccess> {
        self.0
            .run_mobile_plugin::<ResumedAccess>("resumeAccess", ResumeArgs { bookmark })
            .map_err(Into::into)
    }

    pub fn release_access(&self, path: &str) -> Result<()> {
        self.0
            .run_mobile_plugin::<PickerEmpty>("releaseAccess", ReleaseArgs { path })
            .map(|_| ())
            .map_err(Into::into)
    }
}

#[cfg(target_os = "ios")]
pub trait IosPickerExt<R: Runtime> {
    fn ios_picker(&self) -> &IosPicker<R>;
}

#[cfg(target_os = "ios")]
impl<R: Runtime, T: Manager<R>> IosPickerExt<R> for T {
    fn ios_picker(&self) -> &IosPicker<R> {
        self.state::<IosPicker<R>>().inner()
    }
}

// Empty trait stub on non-iOS so the `commands` module compiles uniformly
// across targets.
#[cfg(not(target_os = "ios"))]
pub trait IosPickerExt<R: Runtime> {}
#[cfg(not(target_os = "ios"))]
impl<R: Runtime, T: Manager<R>> IosPickerExt<R> for T {}

pub fn init<R: Runtime>() -> TauriPlugin<R> {
    Builder::new("ios-picker")
        .invoke_handler(tauri::generate_handler![
            commands::pick_folder,
            commands::resume_access,
            commands::release_access,
        ])
        .setup(|_app, _api| {
            #[cfg(target_os = "ios")]
            {
                let handle = _api.register_ios_plugin(init_plugin_ios_picker)?;
                _app.manage(IosPicker(handle));
            }
            Ok(())
        })
        .build()
}
