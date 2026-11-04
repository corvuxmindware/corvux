//! Tool execution result + frontend event emission.

use serde::Serialize;
use tauri::AppHandle;
use tauri_specta::Event;

use crate::events;

/// Result of executing a tool call. Emitted to the frontend so the UI can
/// render a tool-card (preview / accept / dismiss) and react to side
/// effects (e.g. opening a freshly-created note).
#[derive(Debug, Clone, Serialize, serde::Deserialize, specta::Type)]
pub struct ToolResult {
    /// Tool name (`roost_search`, `roost_read`, …).
    pub tool: String,
    /// True when the tool ran without error. Errors get reported to the
    /// LLM so it can retry with adjusted args; the user sees them via the
    /// `ai:tool-result` event.
    pub success: bool,
    /// Human-readable status — first thing rendered in the tool-card.
    pub message: String,
    /// Structured payload — the JSON the LLM gets back as the
    /// `tool_result` content for this call. May also drive richer UI
    /// rendering (e.g. result lists for `roost_search`).
    ///
    /// `#[specta(skip)]` — `serde_json::Value` recurses into
    /// `Number::i64`, which trips specta's BigInt forbid. The field
    /// still flows over the wire (frontend reads it as `unknown`);
    /// the binding type just omits it. Same workaround as
    /// `ChatMessage.tool_calls` — see
    /// `docs/decisions/toolcall-specta-skip.md`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[specta(skip)]
    pub data: Option<serde_json::Value>,
    /// Roost-relative or absolute path of a file the tool created or
    /// modified. Set by `roost_write` so the frontend can auto-open the
    /// new note.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_path: Option<String>,
}

impl ToolResult {
    pub fn ok(tool: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            tool: tool.into(),
            success: true,
            message: message.into(),
            data: None,
            file_path: None,
        }
    }

    pub fn err(tool: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            tool: tool.into(),
            success: false,
            message: message.into(),
            data: None,
            file_path: None,
        }
    }

    pub fn with_data(mut self, data: serde_json::Value) -> Self {
        self.data = Some(data);
        self
    }

    pub fn with_file_path(mut self, path: impl Into<String>) -> Self {
        self.file_path = Some(path.into());
        self
    }
}

/// Emit `ai:tool-result` so the frontend can render the tool-card.
pub fn emit_tool_result<R: tauri::Runtime>(app: &AppHandle<R>, result: &ToolResult) {
    let _ = events::AiToolResult(result.clone()).emit(app);
}

/// Wire shape of `ai:secret-warning`. Fired whenever the secret-scanner
/// detects suspicious content in either a tool's arguments OR its
/// returned result — regardless of whether the call went through the
/// approval gate. Auto-approved tools used to bypass the scanner
/// entirely (the scanner lived inside the gate); this event channel
/// makes the warning unconditional.
#[derive(Debug, Clone, Serialize, serde::Deserialize, specta::Type)]
pub struct SecretWarningEvent {
    pub tool: String,
    /// `"args"` when the secret was in the tool input, `"result"` when
    /// the tool emitted it (e.g. `roost_read` returning a note that
    /// contains an AWS key). `String`-typed so the field crosses the
    /// specta export — `&'static str` doesn't impl `specta::Type`.
    pub origin: String,
    pub warnings: Vec<super::secret_scanner::SecretWarning>,
}

pub fn emit_secret_warning<R: tauri::Runtime>(
    app: &AppHandle<R>,
    tool: &str,
    origin: &'static str,
    warnings: Vec<super::secret_scanner::SecretWarning>,
) {
    if warnings.is_empty() {
        return;
    }
    let _ = events::AiSecretWarning(SecretWarningEvent {
        tool: tool.to_string(),
        origin: origin.to_string(),
        warnings,
    })
    .emit(app);
}
