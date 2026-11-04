//! Mobile stub for llamacpp. The real implementation pulls in the
//! `llama-cpp-2` crate, which bundles llama.cpp's C++ and doesn't cross-compile
//! through the Android NDK without a lot of ABI routing glue. On mobile, local
//! LLM is disabled — callers fall back to cloud providers via `LLMProvider`.
//!
//! This module mirrors the desktop public surface (`LlamaCppModel`,
//! `LlamaCppChat`) so `commands::ai` compiles unchanged.

use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use async_trait::async_trait;

use super::provider::{ChatMessage, LLMProvider};

const MOBILE_UNSUPPORTED: &str =
    "Local LLM is not supported on mobile yet. Use a cloud provider (Gemini/Claude) in Settings.";

pub struct LlamaCppModel;

impl LlamaCppModel {
    pub fn load(_model_path: &Path) -> Result<Arc<Self>, String> {
        Err(MOBILE_UNSUPPORTED.to_string())
    }
}

pub struct LlamaCppChat;

impl LlamaCppChat {
    pub fn new(_model: Arc<LlamaCppModel>) -> Self {
        Self
    }
}

#[async_trait]
impl<R: tauri::Runtime + 'static> LLMProvider<R> for LlamaCppChat {
    async fn stream_chat(
        &self,
        _app: &tauri::AppHandle<R>,
        _messages: Vec<ChatMessage>,
        _system_context: Option<String>,
        _cancel: Arc<AtomicBool>,
    ) -> Result<String, String> {
        Err(MOBILE_UNSUPPORTED.to_string())
    }

    fn model_name(&self) -> &str {
        "mobile-stub"
    }

    fn is_local(&self) -> bool {
        false
    }
}
