//! Corvux-internal Tauri plugin exposing Android's Storage Access Framework
//! to the Rust side. Provides a folder picker that returns a persistable
//! `content://` tree URI which the app then hands to a `SafFs` vault backend.
//!
//! Desktop / iOS stub: every call returns [`Error::NotAvailable`].

use tauri::{
    plugin::{Builder, TauriPlugin},
    Manager, Runtime,
};

pub use error::{Error, Result};
pub use models::*;

mod commands;
mod error;
mod models;

#[cfg(target_os = "android")]
const PLUGIN_IDENTIFIER: &str = "app.tauri.saf";

/// Handle to the SAF plugin's mobile backend. Present in state only on
/// Android — other platforms don't register it, and the command layer
/// returns [`Error::NotAvailable`] instead of touching this.
#[cfg(target_os = "android")]
pub struct Saf<R: Runtime>(tauri::plugin::PluginHandle<R>);

#[cfg(target_os = "android")]
impl<R: Runtime> Saf<R> {
    pub fn pick_folder(&self) -> Result<PickedFolder> {
        self.0
            .run_mobile_plugin::<PickedFolder>("pickFolder", ())
            .map_err(Into::into)
    }

    pub fn list_folder(&self, tree_uri: &str, rel_dir: &str) -> Result<SafListResponse> {
        self.0
            .run_mobile_plugin::<SafListResponse>("listFolder", ListArgs { tree_uri, rel_dir })
            .map_err(Into::into)
    }

    /// Recursive list — every descendant of `rel_dir` in one JNI call.
    /// Used by the vault walker to avoid the D-sequential-listFolder
    /// chain that dominates SAF cold-start on any non-flat vault. The
    /// response shape mirrors [`Self::list_folder`] so the walker can
    /// consume both uniformly.
    pub fn list_tree(&self, tree_uri: &str, rel_dir: &str) -> Result<SafListResponse> {
        self.0
            .run_mobile_plugin::<SafListResponse>("listTree", ListArgs { tree_uri, rel_dir })
            .map_err(Into::into)
    }

    pub fn read_document(&self, tree_uri: &str, rel: &str) -> Result<SafReadResponse> {
        self.0
            .run_mobile_plugin::<SafReadResponse>("readDocument", ReadArgs { tree_uri, rel })
            .map_err(Into::into)
    }

    pub fn write_document(
        &self,
        tree_uri: &str,
        rel: &str,
        base64: String,
        create_new: bool,
    ) -> Result<()> {
        self.0
            .run_mobile_plugin::<SafEmpty>(
                "writeDocument",
                WriteArgs {
                    tree_uri,
                    rel,
                    base64,
                    create_new,
                },
            )
            .map(|_| ())
            .map_err(Into::into)
    }

    pub fn delete_document(&self, tree_uri: &str, rel: &str) -> Result<()> {
        self.0
            .run_mobile_plugin::<SafEmpty>("deleteDocument", RelArgs { tree_uri, rel })
            .map(|_| ())
            .map_err(Into::into)
    }

    pub fn rename_document(&self, tree_uri: &str, from_rel: &str, to_rel: &str) -> Result<()> {
        self.0
            .run_mobile_plugin::<SafEmpty>(
                "renameDocument",
                RenameArgs {
                    tree_uri,
                    from_rel,
                    to_rel,
                },
            )
            .map(|_| ())
            .map_err(Into::into)
    }

    pub fn mkdir(&self, tree_uri: &str, rel: &str) -> Result<()> {
        self.0
            .run_mobile_plugin::<SafEmpty>("mkdir", RelArgs { tree_uri, rel })
            .map(|_| ())
            .map_err(Into::into)
    }

    /// Stat a document without reading its contents. The previous
    /// existence-check path called `read_document` and discarded the
    /// bytes; on a 1MB attachment that's a 1MB+ JNI round-trip per check.
    /// `stat_document` does only a `findFile` walk and returns metadata.
    pub fn stat_document(&self, tree_uri: &str, rel: &str) -> Result<SafStatResponse> {
        self.0
            .run_mobile_plugin::<SafStatResponse>("statDocument", RelArgs { tree_uri, rel })
            .map_err(Into::into)
    }
}

#[cfg(target_os = "android")]
pub trait SafExt<R: Runtime> {
    fn saf(&self) -> &Saf<R>;
}

#[cfg(target_os = "android")]
impl<R: Runtime, T: Manager<R>> SafExt<R> for T {
    fn saf(&self) -> &Saf<R> {
        self.state::<Saf<R>>().inner()
    }
}

// Empty trait stub on non-Android so the `commands` module compiles
// uniformly across targets.
#[cfg(not(target_os = "android"))]
pub trait SafExt<R: Runtime> {}
#[cfg(not(target_os = "android"))]
impl<R: Runtime, T: Manager<R>> SafExt<R> for T {}

pub fn init<R: Runtime>() -> TauriPlugin<R> {
    Builder::new("saf")
        .invoke_handler(tauri::generate_handler![commands::pick_folder])
        .setup(|_app, _api| {
            #[cfg(target_os = "android")]
            {
                let handle = _api.register_android_plugin(PLUGIN_IDENTIFIER, "SafPlugin")?;
                _app.manage(Saf(handle));
            }
            Ok(())
        })
        .build()
}
