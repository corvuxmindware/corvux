use tauri::{AppHandle, Runtime};

#[cfg(not(target_os = "android"))]
use crate::Error;
#[cfg(target_os = "android")]
use crate::SafExt;
use crate::{PickedFolder, Result};

#[tauri::command]
pub async fn pick_folder<R: Runtime>(app: AppHandle<R>) -> Result<PickedFolder> {
    #[cfg(target_os = "android")]
    {
        app.saf().pick_folder()
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = app;
        Err(Error::NotAvailable)
    }
}
