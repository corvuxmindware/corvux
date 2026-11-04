//! Typed Tauri events — backend → frontend push channel, end-to-end
//! typed via tauri-specta.
//!
//! ## What this module is
//!
//! The companion to [`specta_bridge`]: the same end-to-end-typed
//! pattern, applied to events instead of commands. Each `pub struct`
//! below is a `tauri_specta::Event` that:
//!
//!  1. Declares its wire name via `#[tauri_specta(event_name = "...")]`
//!     — preserving the existing kebab/colon/double-slash channel names
//!     (e.g. `ai:chunk`, `sync://event`) so untyped `listen()` callers
//!     keep working unchanged during the migration.
//!  2. Carries a typed payload (`pub struct Foo(pub T);` newtype, or a
//!     re-export of an existing payload struct). serde's default for
//!     single-field tuple structs serializes as the inner value, so
//!     newtype wrappers don't change the wire shape — the new
//!     `events.aiChunk.listen(cb)` API and the legacy
//!     `listen("ai:chunk", cb)` API receive identical JSON.
//!  3. Gets registered in [`specta_bridge::register_events`] so it
//!     appears in the generated `bindings.ts` `events` object.
//!
//! Backend emit sites use the typed form:
//!
//! ```ignore
//! events::AiChunk(text.into()).emit(&app).ok();
//! ```
//!
//! Frontend listeners use the typed form:
//!
//! ```ignore
//! events.aiChunk.listen((event) => { /* event.payload is `string` */ });
//! ```
//!
//! ## Why centralize event definitions here
//!
//! Before this module, every emit site built an ad-hoc payload (often
//! `serde_json::json!({...})`) and the frontend hand-wrote a matching TS
//! interface above each `listen("name", ...)` call. Drift was invisible
//! until a user-visible bug hit. Now the Rust struct is the single
//! source of truth; the frontend imports the generated type.
//!
//! ## Wire-name preservation
//!
//! tauri-specta defaults to kebab-casing the Rust struct identifier.
//! Several of our event names (`ai:chunk`, `sync://event`) aren't valid
//! Rust idents, so the explicit `event_name = "..."` attribute is
//! mandatory. Future events SHOULD use the attribute too, so the wire
//! name is grep-able from this file and doesn't quietly shift if the
//! struct gets renamed.

use serde::{Deserialize, Serialize};
use tauri_specta::Event;

use crate::ai::models::ProgressPayload;
use crate::ai::tools::approval::ToolPendingPayload;
use crate::ai::tools::{SecretWarningEvent, ToolResult};
use crate::commands::files::FileEntry;
use crate::commands::sync::{DiscoveryEventPayload, SyncEventPayload};
use crate::diagnostics::LogEntry;

// ---------------------------------------------------------------------
// AI streaming — chat completion tokens. Payload = the text fragment.
// ---------------------------------------------------------------------

/// Streaming token fragment from the active chat completion. Fired
/// per-chunk by every provider implementation (gemini, ollama,
/// openrouter, llamacpp, openai-compat). Frontend appends to the
/// in-progress assistant message.
#[derive(Clone, Debug, Serialize, Deserialize, specta::Type, Event)]
#[tauri_specta(event_name = "ai:chunk")]
pub struct AiChunk(pub String);

/// Stream complete. Payload is the full concatenated text — the
/// frontend uses this to replace any provider-side reformatting (e.g.
/// trailing-whitespace differences) and seal the assistant message.
#[derive(Clone, Debug, Serialize, Deserialize, specta::Type, Event)]
#[tauri_specta(event_name = "ai:done")]
pub struct AiDone(pub String);

/// Stream failed mid-flight. Payload = error message rendered in the
/// chat panel; the in-progress message is rolled back.
#[derive(Clone, Debug, Serialize, Deserialize, specta::Type, Event)]
#[tauri_specta(event_name = "ai:error")]
pub struct AiError(pub String);

// ---------------------------------------------------------------------
// AI tool / agent loop — approval gate and result reporting.
// ---------------------------------------------------------------------

/// Tool call is paused awaiting user approval. Frontend renders a
/// `ToolCard` per pending entry; calling `decide_tool` with the
/// matching `request_id` resolves it.
#[derive(Clone, Debug, Serialize, Deserialize, specta::Type, Event)]
#[tauri_specta(event_name = "ai:tool-pending")]
pub struct AiToolPending(pub ToolPendingPayload);

/// Tool call completed (success OR error). Frontend updates the
/// matching `ToolCard` and may apply side effects (e.g. open the newly
/// written note via `file_path`).
#[derive(Clone, Debug, Serialize, Deserialize, specta::Type, Event)]
#[tauri_specta(event_name = "ai:tool-result")]
pub struct AiToolResult(pub ToolResult);

/// Approval decision landed for a pending tool call. Frontend uses this
/// to collapse the card on any path that resolves the gate — including
/// timeouts where the user never clicked (the backend treats those as
/// `Reject` and emits this event with `decision: "reject"`).
///
/// Payload is `{request_id, decision}`. The `decision` field is a
/// stringified `ToolDecision`; we use a hand-rolled struct here rather
/// than a tuple newtype so the frontend can pattern-match by key name.
#[derive(Clone, Debug, Serialize, Deserialize, specta::Type, Event)]
#[tauri_specta(event_name = "ai:tool-decided")]
pub struct AiToolDecided {
    pub request_id: String,
    pub decision: crate::ai::tools::approval::ToolDecision,
}

/// Secret-scanner detected a credential-shaped substring in a tool's
/// args or result. Unconditional — fires regardless of whether the
/// tool went through the approval gate.
#[derive(Clone, Debug, Serialize, Deserialize, specta::Type, Event)]
#[tauri_specta(event_name = "ai:secret-warning")]
pub struct AiSecretWarning(pub SecretWarningEvent);

/// Subagent (Task tool) execution started. Frontend renders a "Running
/// subagent" chevron under the parent assistant turn.
#[derive(Clone, Debug, Serialize, Deserialize, specta::Type, Event)]
#[tauri_specta(event_name = "ai:subagent-start")]
pub struct AiSubagentStart {
    pub subagent_type: String,
    pub description: String,
    pub depth: u8,
}

/// Subagent execution finished — success flag + result text length so
/// the frontend can collapse the chevron and (optionally) show how much
/// the subagent produced.
#[derive(Clone, Debug, Serialize, Deserialize, specta::Type, Event)]
#[tauri_specta(event_name = "ai:subagent-done")]
pub struct AiSubagentDone {
    pub subagent_type: String,
    pub success: bool,
    #[specta(type = u32)]
    pub text_chars: usize,
}

// ---------------------------------------------------------------------
// AI model downloads — the local Gemma LLM (gguf).
// ---------------------------------------------------------------------

/// Per-file download starting. Payload = the model identifier (e.g.
/// `gemma-4-e2b-it`).
#[derive(Clone, Debug, Serialize, Deserialize, specta::Type, Event)]
#[tauri_specta(event_name = "ai:model-download-start")]
pub struct AiModelDownloadStart(pub String);

/// Per-file download completed.
#[derive(Clone, Debug, Serialize, Deserialize, specta::Type, Event)]
#[tauri_specta(event_name = "ai:model-download-done")]
pub struct AiModelDownloadDone(pub String);

/// Per-chunk progress for a model download. Throttled to ~1% changes
/// at the emit site so this doesn't flood the IPC.
#[derive(Clone, Debug, Serialize, Deserialize, specta::Type, Event)]
#[tauri_specta(event_name = "ai:model-download-progress")]
pub struct AiModelDownloadProgress(pub ProgressPayload);

// ---------------------------------------------------------------------
// Graph index — long-running rebuilds the frontend wants to surface.
// ---------------------------------------------------------------------

/// Bridge-detection (graph algorithm) progress. Payload is a string
/// ("started" / "done") — not an enum because the frontend only treats
/// it as a status badge.
#[derive(Clone, Debug, Serialize, Deserialize, specta::Type, Event)]
#[tauri_specta(event_name = "graph:bridges-progress")]
pub struct GraphBridgesProgress(pub String);

/// Background graph reconciliation changed the active roost after the
/// frontend had already received the warm cached view. The renderer bumps its
/// revision and refreshes derived overlays on this event.
#[derive(Clone, Debug, Serialize, Deserialize, specta::Type, Event)]
#[tauri_specta(event_name = "graph:changed")]
pub struct GraphChanged {
    pub added: u32,
    pub modified: u32,
    pub purged: u32,
}

// ---------------------------------------------------------------------
// Diagnostics — in-app log buffer for the Settings diagnostics tab.
// ---------------------------------------------------------------------

/// One in-app log entry — flushed live to the Diagnostics panel so
/// users can see what's happening without an external log viewer.
#[derive(Clone, Debug, Serialize, Deserialize, specta::Type, Event)]
#[tauri_specta(event_name = "log:event")]
pub struct LogEvent(pub LogEntry);

// ---------------------------------------------------------------------
// Native menu — OS menu bar items routed through to the renderer.
// ---------------------------------------------------------------------

/// A native menu item was clicked. Payload = the item's id string.
#[derive(Clone, Debug, Serialize, Deserialize, specta::Type, Event)]
#[tauri_specta(event_name = "menu:event")]
pub struct MenuEvent(pub String);

// ---------------------------------------------------------------------
// Sync — iroh/TCP peer-to-peer event stream.
// ---------------------------------------------------------------------

/// Sync engine event (peer connected, scan progress, note written, ...).
#[derive(Clone, Debug, Serialize, Deserialize, specta::Type, Event)]
#[tauri_specta(event_name = "sync://event")]
pub struct SyncStreamEvent(pub SyncEventPayload);

/// mDNS discovery event (peer found / lost on the local network).
#[derive(Clone, Debug, Serialize, Deserialize, specta::Type, Event)]
#[tauri_specta(event_name = "sync://discovered")]
pub struct SyncDiscoveredEvent(pub DiscoveryEventPayload);

// ---------------------------------------------------------------------
// Roost — file-tree mutations surface to the renderer for refresh.
// ---------------------------------------------------------------------

/// Roost tree changed (note created, renamed, deleted, or moved).
/// Payload = the fresh full tree so the renderer can swap it in
/// without a second IPC roundtrip.
#[derive(Clone, Debug, Serialize, Deserialize, specta::Type, Event)]
#[tauri_specta(event_name = "roost://tree-changed")]
#[serde(rename_all = "camelCase")]
pub struct RoostTreeChanged {
    pub roost_path: String,
    pub generation: u32,
    pub entries: Vec<FileEntry>,
}
