//! App-wide background tokio runtime for code reached from sync paths.
//!
//! Tauri V2 routes synchronous `#[tauri::command]` functions through a
//! `spawn_blocking` worker pool. On Windows release builds those workers
//! do NOT carry a tokio runtime context — calling
//! `tauri::async_runtime::spawn` (or `tokio::spawn`, `tokio::time::*`,
//! `tokio::fs::*`, etc.) from inside a sync command panics with
//! "there is no reactor running, must be called from the context of a
//! Tokio 1.x runtime", and the panic surfaces inside an unwind-forbidden
//! boundary, aborting the process.
//!
//! Background helpers that need to spawn tokio work from sync code paths
//! call [`handle`] to obtain a `tokio::runtime::Handle` bound to a
//! long-lived multi-thread runtime owned by this module. That runtime is
//! independent of whatever runtime Tauri exposes (or fails to expose) to
//! the calling thread.
//!
//! Used today by `task_notify::enroll_timer` which is reached from
//! `init_graph` / `index_note` / `complete_task` — all sync commands.

use std::sync::OnceLock;
use tokio::runtime::{Builder, Handle, Runtime};

static RUNTIME: OnceLock<Runtime> = OnceLock::new();

/// Handle to the app-wide background runtime. Lazily initialized on the
/// first call from any thread. Cheap to call repeatedly — clones a
/// reference-counted handle.
pub fn handle() -> Handle {
    RUNTIME
        .get_or_init(|| {
            Builder::new_multi_thread()
                .enable_all()
                .worker_threads(2)
                .thread_name("corvux-bg")
                .build()
                .expect("failed to build corvux background runtime")
        })
        .handle()
        .clone()
}
