use tauri::{AppHandle, Runtime};

#[cfg(target_os = "android")]
use crate::MulticastLockExt;
use crate::Result;

/// Acquire a `WifiManager.MulticastLock` on Android. Idempotent:
/// re-acquiring without releasing first is a no-op on the Java side
/// (`MulticastLock.acquire()` is reference-counted).
///
/// On non-Android targets this is a no-op (returns `Ok(())`) so the
/// caller doesn't have to cfg-gate every call site.
#[tauri::command]
pub async fn acquire<R: Runtime>(app: AppHandle<R>) -> Result<()> {
    #[cfg(target_os = "android")]
    {
        app.multicast_lock().acquire()
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = app;
        Ok(())
    }
}

/// Release the `WifiManager.MulticastLock` previously acquired. Safe to
/// call without a prior acquire — the Java side guards against
/// double-release with `isHeld()`.
///
/// On non-Android targets this is a no-op.
#[tauri::command]
pub async fn release<R: Runtime>(app: AppHandle<R>) -> Result<()> {
    #[cfg(target_os = "android")]
    {
        app.multicast_lock().release()
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = app;
        Ok(())
    }
}
