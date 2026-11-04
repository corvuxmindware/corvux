//! AI subsystem: chat (FTS-tool retrieval + tool-calling), local LLM
//! lifecycle, and shared settings/state.
//!
//! Split into siblings:
//!  - `types`     — Tauri-managed state structs (Settings, ChatCancel, ...)
//!  - `settings`  — get/save settings + device-profile
//!  - `chat`      — chat_send / chat_stop + system-prompt helpers
//!  - `llm`       — download / load / unload / delete the local LLM
//!
//! All public items are re-exported here so callers can use the unchanged
//! `commands::ai::*` import paths.

// Submodules are `pub` so `tauri::generate_handler!` can resolve the
// macro-generated `__cmd__<name>` companions alongside each handler. A plain
// `pub use` would re-export the function but not the hidden companion items.
pub mod chat;
pub mod connection_test;
pub mod llm;
pub mod ollama_models;
pub mod settings;
pub mod skills;
pub mod subagents;
pub mod types;

// Types are re-exported flat so callers keep using `commands::ai::Settings`
// etc. without the extra `::types::` segment.
pub use types::{ChatCancel, HttpClient, LlamaCppState, Settings, SettingsState};
