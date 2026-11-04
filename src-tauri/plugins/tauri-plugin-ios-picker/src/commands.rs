use tauri::{AppHandle, Runtime};

#[cfg(not(target_os = "ios"))]
use crate::Error;
#[cfg(target_os = "ios")]
use crate::IosPickerExt;
use crate::{PickedFolder, Result, ResumedAccess};

#[tauri::command]
pub async fn pick_folder<R: Runtime>(app: AppHandle<R>) -> Result<PickedFolder> {
    #[cfg(target_os = "ios")]
    {
        app.ios_picker().pick_folder()
    }
    #[cfg(not(target_os = "ios"))]
    {
        let _ = app;
        Err(Error::NotAvailable)
    }
}

#[tauri::command]
pub async fn resume_access<R: Runtime>(
    app: AppHandle<R>,
    bookmark: String,
) -> Result<ResumedAccess> {
    #[cfg(target_os = "ios")]
    {
        app.ios_picker().resume_access(&bookmark)
    }
    #[cfg(not(target_os = "ios"))]
    {
        let _ = (app, bookmark);
        Err(Error::NotAvailable)
    }
}

#[tauri::command]
pub async fn release_access<R: Runtime>(app: AppHandle<R>, path: String) -> Result<()> {
    #[cfg(target_os = "ios")]
    {
        app.ios_picker().release_access(&path)
    }
    #[cfg(not(target_os = "ios"))]
    {
        let _ = (app, path);
        Err(Error::NotAvailable)
    }
}
