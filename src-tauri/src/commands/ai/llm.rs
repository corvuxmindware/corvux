//! Local LLM lifecycle: download, load, unload, delete the cached
//! Gemma 4 E2B model. Status surface is mirrored in the UI's settings panel.

use std::sync::Arc;

use tauri::State;

use crate::ai::llamacpp::LlamaCppModel;
use crate::ai::models::{self, ModelStatus};

use super::types::{HttpClient, LlamaCppState};

/// Check if the local LLM model is downloaded and cached on disk.
#[tauri::command]
#[specta::specta]
pub fn get_llm_model_status() -> ModelStatus {
    models::gemma4_e2b_status()
}

/// Download the Gemma 4 E2B model from HuggingFace (~3.46 GB).
/// Skips if already cached. Emits progress events for the UI.
#[tauri::command]
#[specta::specta]
pub async fn download_llm_model(
    app: tauri::AppHandle,
    http_state: State<'_, HttpClient>,
) -> Result<(), String> {
    models::download_gemma4_e2b(&http_state.0, &app).await
}

/// Load the cached LLM model into GPU VRAM / system RAM.
/// Takes 2-5 seconds on first call, then cached in memory.
#[tauri::command]
#[specta::specta]
pub async fn load_llm_model(llm_state: State<'_, LlamaCppState>) -> Result<String, String> {
    {
        let guard = llm_state.0.lock().map_err(|e| format!("Lock error: {e}"))?;
        if guard.is_some() {
            return Ok("Model already loaded".to_string());
        }
    }

    // AUDIT-078b: hash-gate the on-disk GGUF before handing the bytes to
    // llama.cpp's native mmap+tensor pipeline. Replaces the previous
    // existence-only check, which would happily load a tampered or
    // pre-fix unverified file. Both the integrity check and the load run
    // in the same blocking task — SHA-256 over 3.46 GB takes ~5-15s on
    // SSD, and we want both costs off the tokio runtime thread.
    let model = tokio::task::spawn_blocking(|| -> Result<Arc<LlamaCppModel>, String> {
        let model_path = models::verify_gemma4_e2b_or_err()?;
        LlamaCppModel::load(&model_path)
    })
    .await
    .map_err(|e| format!("spawn_blocking failed: {e}"))??;

    let mut guard = llm_state.0.lock().map_err(|e| format!("Lock error: {e}"))?;
    *guard = Some(model);

    Ok("Model loaded successfully".to_string())
}

/// Unload the LLM model to free GPU VRAM / system RAM.
#[tauri::command]
#[specta::specta]
pub fn unload_llm_model(llm_state: State<'_, LlamaCppState>) -> Result<(), String> {
    let mut guard = llm_state.0.lock().map_err(|e| format!("Lock error: {e}"))?;
    *guard = None;
    Ok(())
}

/// Delete the cached LLM model from disk to free space.
#[tauri::command]
#[specta::specta]
pub fn delete_llm_model(llm_state: State<'_, LlamaCppState>) -> Result<(), String> {
    let mut guard = llm_state.0.lock().map_err(|e| format!("Lock error: {e}"))?;
    *guard = None;
    drop(guard);

    models::delete_gemma4_e2b()
}
