//! Tool-call approval gate.
//!
//! Sits between [`super::dispatch_one`] and the tool's `execute`. When a
//! tool's `SafetyTier` and the user's [`AgentAutonomy`] settings agree
//! that approval is needed, this module:
//!
//! 1. Mints a request id, registers a `oneshot::Sender` in the shared
//!    [`PendingApprovals`] state.
//! 2. Emits an `ai:tool-pending` event with `{ request_id, tool, args,
//!    tier, summary }` so the frontend can render a tool card.
//! 3. Awaits the matching [`decide_tool`] IPC call. No timeout — the
//!    agent blocks here until the user decides (matches Claude Code /
//!    Codex behavior). Escape hatch is Stop-Generation, which cancels
//!    the agent task and drops the receiver.
//! 4. Returns the resolved [`ToolDecision`] to the caller.
//!
//! "Always approve this tool" updates the persisted settings so future
//! sessions inherit the user's per-tool trust list.
//!
//! Approval is the only place where the agent loop blocks waiting on
//! user input. Everywhere else the loop is fire-and-forget on the
//! transport — keeps the UX responsive while the trust gate stays
//! synchronous-feeling for the user.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};
use tauri_specta::Event;

use crate::events;
use tokio::sync::oneshot;
use tracing::{debug, warn};
use uuid::Uuid;

use crate::commands::ai::types::{AgentAutonomy, SettingsState};
use crate::commands::graph::GraphDb;
use crate::db::queries::approval_log;

use super::registry::{SafetyTier, ToolError};
use super::secret_scanner::{self, SecretWarning};

/// Decision the user makes on a pending tool call. Mirrored on the
/// frontend; the IPC `decide_tool` accepts these as serde-tagged JSON.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "snake_case")]
pub enum ToolDecision {
    /// Run this one tool call.
    Approve,
    /// Run this call AND add the tool name to `auto_approve_tools` so
    /// future calls of this tool skip the gate. Persisted via the
    /// existing settings file.
    AlwaysApprove,
    /// Skip this call. Agent receives a retryable `ToolError` and can
    /// adjust args or move on.
    Reject,
}

/// State held in `tauri::State` keyed by request id. Map entries are
/// removed by `decide_tool` (or by `forget` on the rare emit-failed path).
#[derive(Default)]
pub struct PendingApprovals {
    inner: Mutex<HashMap<String, oneshot::Sender<ToolDecision>>>,
}

impl PendingApprovals {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a fresh request and return the `(id, receiver)` pair.
    fn register(&self) -> (String, oneshot::Receiver<ToolDecision>) {
        let id = Uuid::new_v4().to_string();
        let (tx, rx) = oneshot::channel();
        let mut guard = self.inner.lock().expect("PendingApprovals lock poisoned");
        guard.insert(id.clone(), tx);
        (id, rx)
    }

    /// Remove and resolve a pending request. Returns `true` if the id
    /// was present (and the decision was sent), `false` if the request
    /// already timed out / was decided by another caller.
    pub fn resolve(&self, id: &str, decision: ToolDecision) -> bool {
        let mut guard = self.inner.lock().expect("PendingApprovals lock poisoned");
        match guard.remove(id) {
            Some(tx) => tx.send(decision).is_ok(),
            None => false,
        }
    }

    /// Forget a pending request without resolving it. Dropping the
    /// `oneshot::Sender` wakes the matching `receiver.await` in
    /// [`request`] with `Err`, which falls through to the existing
    /// reject branch — a retryable `ToolError` the agent loop already
    /// knows how to surface to the LLM.
    ///
    /// Two callers today:
    /// 1. The rare emit-failed path inside [`request`] so a late
    ///    `decide_tool` call doesn't try to deliver to a dropped receiver.
    /// 2. The `forget_pending_approval` IPC, fired by the frontend
    ///    `ChatPanel` on unmount so an open card doesn't leave a sender
    ///    sitting in the HashMap until app restart.
    ///
    /// Idempotent: a no-op when the id is unknown.
    pub fn forget(&self, id: &str) {
        let mut guard = self.inner.lock().expect("PendingApprovals lock poisoned");
        guard.remove(id);
    }
}

/// Wire shape of the `ai:tool-pending` event. Mirrors what the frontend
/// `ToolCard` component expects — keep both ends synchronized.
#[derive(Debug, Clone, Serialize, Deserialize, specta::Type)]
pub struct ToolPendingPayload {
    pub request_id: String,
    pub tool: String,
    pub tier: SafetyTier,
    /// Pretty-printed args JSON for display in the card body.
    ///
    /// `#[specta(skip)]` — `serde_json::Value` recurses into
    /// `Number::i64`, which trips specta's BigInt forbid. The field
    /// still flows over the wire (the frontend's hand-written
    /// `PendingTool` interface declares `args: Record<string,
    /// unknown>`); the generated binding type just omits it. Same
    /// workaround as `ChatMessage.tool_calls` — see
    /// `docs/decisions/toolcall-specta-skip.md`.
    #[specta(skip)]
    pub args: serde_json::Value,
    /// Best-guess one-liner the card renders as the title (e.g. "Edit
    /// projects/foo.md"). Falls back to the tool name on tools without
    /// a clear single-target.
    pub summary: String,
    /// File path the tool will modify, when known. Lets the card render
    /// a diff preview by reading the current file via existing IPCs.
    pub file_path: Option<String>,
    /// Pre-computed before/after diff for write tools so the user can
    /// see exactly what the agent is about to change before approving.
    /// `None` for non-write tools where there's nothing to diff.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preview: Option<DiffPreview>,
    /// Secret-shaped substrings detected in `args` (agent-hardening §
    /// 3.4.3). T6 / T7 defense — surfaces "this approval contains an
    /// AWS-key-shape" above the diff so the user can catch
    /// injection-driven exfiltration before clicking Approve. Empty
    /// vector when nothing suspicious was found; serialized as an
    /// empty array (NOT omitted) so the frontend can rely on the
    /// field always existing.
    #[serde(default)]
    pub warnings: Vec<SecretWarning>,
}

/// Structured diff payload emitted with `ai:tool-pending`. The frontend
/// renders this as the trust surface — without it, "Approve" is a leap
/// of faith.
#[derive(Debug, Clone, Serialize, Deserialize, specta::Type)]
pub struct DiffPreview {
    /// `"edit"` (note exists, content is being replaced) or `"create"`
    /// (new note, before-content is empty by definition).
    pub kind: DiffKind,
    /// Roost-relative or absolute path the write targets.
    pub path: String,
    /// Unified-format diff text (`+`/`-`/` ` prefixed lines, with a
    /// `--- before` / `+++ after` header). Same shape stored in
    /// `:edit_event.diff`, computed via `edit_events::compute_diff`.
    pub unified_diff: String,
    #[specta(type = i32)]
    pub lines_added: i64,
    #[specta(type = i32)]
    pub lines_removed: i64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "snake_case")]
pub enum DiffKind {
    Edit,
    Create,
}

/// Decide whether `tool` requires approval given the user's current
/// `AgentAutonomy` snapshot. Pure function — no I/O.
pub fn requires_approval(tier: SafetyTier, autonomy: &AgentAutonomy, tool_name: &str) -> bool {
    match tier {
        SafetyTier::Read => !autonomy.auto_approve_reads,
        SafetyTier::Write => {
            !autonomy.auto_approve_writes
                && !autonomy
                    .auto_approve_tools
                    .iter()
                    .any(|name| name == tool_name)
        }
        // Destructive always asks. The "Always approve this tool" button
        // is hidden in the card UI for this tier so the user can't
        // disable confirmation by accident.
        SafetyTier::Destructive => true,
    }
}

/// Request approval and block until the user decides.
///
/// No timeout — matches Claude Code / Codex behavior. The agent stays
/// suspended on the oneshot until the user clicks Approve / Reject, or
/// until Stop-Generation cancels the agent task (which drops the
/// receiver and resolves this await with an Err).
///
/// On `Reject` returns a retryable `ToolError` so the LLM gets a
/// structured message it can respond to (`"User rejected …"`) rather
/// than a hard failure that aborts the whole agent loop.
///
/// On `AlwaysApprove`, persists the tool name to `agent_autonomy.auto_approve_tools`
/// before returning OK so subsequent calls in this same session skip
/// the gate.
#[allow(clippy::too_many_arguments)]
pub async fn request<R: tauri::Runtime + 'static>(
    app: &AppHandle<R>,
    autonomy: &mut AgentAutonomy,
    tool_name: &str,
    tier: SafetyTier,
    args: &serde_json::Value,
    summary: String,
    file_path: Option<String>,
    preview: Option<DiffPreview>,
) -> Result<(), ToolError> {
    let pending = match app.try_state::<PendingApprovals>() {
        Some(p) => p,
        None => {
            // No state installed — defensive. Lib.rs always manages this
            // state today, so falling here means a misconfigured test.
            warn!("approval: PendingApprovals state missing — auto-approving");
            return Ok(());
        }
    };

    let (request_id, receiver) = pending.register();
    // Secret-scanner sidecar (§3.4.3). Scan BEFORE emit so the warning
    // arrives in the same payload the card renders from — no second
    // round-trip. Best-effort: scanner failures (which it doesn't have
    // — it's pure-function) would never block the approval flow.
    let warnings = secret_scanner::scan_value(args);
    if !warnings.is_empty() {
        debug!(
            tool = tool_name,
            count = warnings.len(),
            "approval: secret-scanner found suspicious patterns"
        );
    }
    let payload = ToolPendingPayload {
        request_id: request_id.clone(),
        tool: tool_name.to_string(),
        tier,
        args: args.clone(),
        summary,
        file_path,
        preview,
        warnings,
    };
    if let Err(e) = events::AiToolPending(payload.clone()).emit(app) {
        // Frontend bridge is down — clean up the pending entry and bail
        // with a retryable error so the agent loop doesn't hang.
        pending.forget(&request_id);
        return Err(ToolError::retryable(format!(
            "approval gate could not reach the UI: {e}"
        )));
    }

    let (decision, log_kind) = match receiver.await {
        Ok(d) => {
            let kind = match d {
                ToolDecision::Approve => approval_log::DecisionKind::Approve,
                ToolDecision::AlwaysApprove => approval_log::DecisionKind::AlwaysApprove,
                ToolDecision::Reject => approval_log::DecisionKind::Reject,
            };
            (d, kind)
        }
        Err(_recv_err) => {
            // Sender dropped without a decision — happens on app shutdown,
            // panel-unmount (`forget_pending_approval` IPC), or
            // Stop-Generation cancelling the agent task. The agent loop
            // still sees this as a Reject (retryable), but the audit
            // log distinguishes it as `forgotten` so the scorecard can
            // tell "user said no" apart from "user wasn't there".
            (ToolDecision::Reject, approval_log::DecisionKind::Forgotten)
        }
    };

    // Best-effort audit log row. Failures never block the agent loop —
    // a dropped audit row is preferable to a stuck approval gate.
    record_audit(
        app,
        &request_id,
        tool_name,
        tier,
        log_kind,
        args,
        &payload.summary,
    );

    // Self-evolution loop (Phase 2.4) — path-aware tool decisions also
    // land in `:ai_signals` as +1/-1 rows so the RAG retrieval can
    // reweight future top-K results. Skipped when no file_path is
    // available (e.g. `roost_search`) since path-less rows can't drive
    // per-note ranking.
    record_signal_for_decision(app, tool_name, payload.file_path.as_deref(), log_kind);

    let _ = events::AiToolDecided {
        request_id: request_id.clone(),
        decision,
    }
    .emit(app);

    match decision {
        ToolDecision::Approve => Ok(()),
        ToolDecision::AlwaysApprove => {
            // Add to the in-loop snapshot so further calls in THIS
            // agent loop skip the gate immediately.
            if !autonomy.auto_approve_tools.iter().any(|n| n == tool_name) {
                autonomy.auto_approve_tools.push(tool_name.to_string());
            }
            // Persist to settings so future sessions inherit the choice.
            // Failure here is non-fatal — the in-loop snapshot still
            // benefits the rest of this session.
            if let Err(e) = persist_always_approve(app, tool_name) {
                warn!(error = %e, tool = tool_name, "approval: could not persist always-approve");
            }
            Ok(())
        }
        ToolDecision::Reject => Err(ToolError::retryable(format!(
            "User rejected `{tool_name}`. Style rules for your next reply: \
             ONE short sentence. Do NOT restate that the action was rejected — \
             the user already knows, they just clicked Reject. Do NOT apologize. \
             Do NOT hedge with phrases like 'it looks like', 'perhaps', 'I see'. \
             Either ask ONE specific clarifying question (e.g. \"Different date?\") \
             or acknowledge with a single word (\"Got it.\"). \
             Retry policy: do NOT retry the same call within THIS turn. But if the \
             user's NEXT message says \"go ahead\", \"do it anyway\", \"sorry, do \
             it\", \"yes proceed\", or similar override language, you SHOULD retry \
             with the same args — they've changed their mind. Do not fabricate \
             reasons the action can't be done (\"it was never created\", \"there's \
             nothing to delete\") — if you're unsure of current state, call \
             `roost_read` or `query_tasks` to verify before claiming."
        ))),
    }
}

/// Stringly render the safety tier for the audit log column. Kept in
/// one place so a future tier rename only touches this function.
fn tier_as_str(tier: SafetyTier) -> &'static str {
    match tier {
        SafetyTier::Read => "read",
        SafetyTier::Write => "write",
        SafetyTier::Destructive => "destructive",
    }
}

/// Best-effort audit log writer. Pulls the live `GraphDb` state, hashes
/// the args JSON, and inserts one `:approval_log` row. Every failure
/// branch is `warn!` + drop — the audit trail is observability, never
/// a precondition for the agent to make progress.
fn record_audit<R: tauri::Runtime>(
    app: &AppHandle<R>,
    request_id: &str,
    tool_name: &str,
    tier: SafetyTier,
    decision: approval_log::DecisionKind,
    args: &serde_json::Value,
    summary: &str,
) {
    let graph = match app.try_state::<GraphDb>() {
        Some(g) => g,
        None => {
            // Test harnesses + early-startup paths can hit this. Don't
            // warn (would spam logs); silent drop is correct for these.
            return;
        }
    };
    let db_guard = match graph.db.lock() {
        Ok(g) => g,
        Err(e) => {
            warn!(error = %e, "audit log: db lock poisoned, skipping row");
            return;
        }
    };
    let db = match db_guard.as_ref() {
        Some(d) => d,
        None => {
            // Roost not opened yet — nothing to write against.
            return;
        }
    };

    let args_json = serde_json::to_string(args).unwrap_or_default();
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);

    let entry = approval_log::ApprovalLogEntry {
        request_id: request_id.to_string(),
        timestamp,
        tool: tool_name.to_string(),
        tier: tier_as_str(tier).to_string(),
        decision: decision.as_str().to_string(),
        args_hash: approval_log::hash_args(&args_json),
        args_summary: summary.to_string(),
    };

    if let Err(e) = approval_log::record_decision(db, &entry) {
        warn!(error = %e, tool = tool_name, "audit log: record failed");
    }
}

/// Self-evolution producer. Maps an approval decision on a path-aware
/// write/destructive tool to a `+1` / `-1` row in `:ai_signals` keyed
/// by the roost-relative file path. RAG retrieval consumes these to
/// reweight top-K — notes the user has consistently approved float up,
/// rejected ones sink (within a clamped envelope so a hot streak can't
/// pin retrieval forever).
///
/// Best-effort everywhere: every failure branch is `warn!` + drop. A
/// missing path or absent DB just skips the row — the gate's audit log
/// already captured the decision.
fn record_signal_for_decision<R: tauri::Runtime>(
    app: &AppHandle<R>,
    tool_name: &str,
    file_path: Option<&str>,
    decision: approval_log::DecisionKind,
) {
    // Path-less calls (roost_search etc.) carry no per-note signal.
    let Some(path) = file_path else {
        return;
    };
    if path.is_empty() {
        return;
    }
    // Forgotten = panel unmount / agent cancel. The user didn't actually
    // express preference, so the loop doesn't learn from it.
    let signal: i64 = match decision {
        approval_log::DecisionKind::Approve | approval_log::DecisionKind::AlwaysApprove => 1,
        approval_log::DecisionKind::Reject => -1,
        approval_log::DecisionKind::Forgotten => return,
    };

    let graph = match app.try_state::<GraphDb>() {
        Some(g) => g,
        None => return,
    };
    let db_guard = match graph.db.lock() {
        Ok(g) => g,
        Err(_) => return,
    };
    let db = match db_guard.as_ref() {
        Some(d) => d,
        None => return,
    };

    // Normalize the path the same way the RAG consumer's note ids look:
    // forward slashes, no leading slash, no `.md` suffix. The retrieval
    // side joins on `context_note_id`; the two must agree.
    let normalized = normalize_signal_path(path);

    if let Err(e) = crate::db::queries::record_signal(
        db,
        "", // query_hash unused on the approval path
        tool_name,
        "tool_path",
        &normalized,
        signal,
        &normalized,
        0,
        0,
        "roost://approval-gate",
    ) {
        warn!(error = %e, tool = tool_name, "ai_signals: record failed");
    }
}

/// Bring a tool's path arg into the same shape RAG retrieval uses for
/// `note_id`: forward slashes, no leading slash, no `.md` suffix,
/// lowercase. Matches `commands::ai::embedding::roost_relative_id`
/// post-roost-strip; we don't have a roost root in this scope so we
/// just normalize the format and let the consumer join on the
/// normalized string. Paths that arrive already roost-relative pass
/// through unchanged.
fn normalize_signal_path(path: &str) -> String {
    let p = path.replace('\\', "/");
    let p = p.trim_start_matches('/');
    let p = p.trim_end_matches('/');
    let p = p.strip_suffix(".md").unwrap_or(p);
    let p = p.strip_suffix(".markdown").unwrap_or(p);
    p.to_lowercase()
}

/// Append `tool_name` to the persisted `agent_autonomy.auto_approve_tools`.
/// Idempotent: a no-op if the name is already in the list. Goes through
/// the same `SettingsState` mutex + on-disk file the existing
/// `save_settings` IPC uses, so the next `get_settings` call observes
/// the change.
fn persist_always_approve<R: tauri::Runtime>(
    app: &AppHandle<R>,
    tool_name: &str,
) -> Result<(), String> {
    let state = app
        .try_state::<SettingsState>()
        .ok_or_else(|| "SettingsState missing".to_string())?;
    let mut guard = state
        .settings
        .lock()
        .map_err(|e| format!("settings lock: {e}"))?;
    if !guard
        .agent_autonomy
        .auto_approve_tools
        .iter()
        .any(|n| n == tool_name)
    {
        guard
            .agent_autonomy
            .auto_approve_tools
            .push(tool_name.to_string());
    }
    // R-02 (2026-05-24 re-audit): route through the shared
    // `redacted_settings_json` helper rather than serializing the
    // in-memory `SettingsState` directly. The in-memory snapshot
    // carries the hydrated Gemini / OpenRouter / Ollama keys from
    // the keychain (see startup migration in `lib.rs`); a direct
    // serialize wrote them back to plaintext settings.json every
    // time the user clicked "Always Approve."
    crate::commands::ai::settings::persist_settings_file(&state.path, &guard)?;
    debug!(tool = tool_name, "approval: persisted always-approve");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn autonomy_default() -> AgentAutonomy {
        AgentAutonomy::default()
    }

    #[test]
    fn read_tools_skip_approval_when_auto_approve_reads_default_on() {
        let a = autonomy_default();
        assert!(!requires_approval(SafetyTier::Read, &a, "roost_read"));
    }

    #[test]
    fn read_tools_require_approval_when_auto_approve_reads_off() {
        let a = AgentAutonomy {
            auto_approve_reads: false,
            ..AgentAutonomy::default()
        };
        assert!(requires_approval(SafetyTier::Read, &a, "roost_read"));
    }

    #[test]
    fn write_tools_require_approval_by_default() {
        let a = autonomy_default();
        assert!(requires_approval(SafetyTier::Write, &a, "roost_edit"));
    }

    #[test]
    fn write_tools_skip_approval_when_auto_approve_writes_on() {
        let a = AgentAutonomy {
            auto_approve_writes: true,
            ..AgentAutonomy::default()
        };
        assert!(!requires_approval(SafetyTier::Write, &a, "roost_edit"));
    }

    #[test]
    fn allowlisted_write_skips_approval_even_when_writes_locked() {
        let a = AgentAutonomy {
            auto_approve_tools: vec!["complete_task".to_string()],
            ..AgentAutonomy::default()
        };
        assert!(!requires_approval(SafetyTier::Write, &a, "complete_task"));
        // Other write tools still gated.
        assert!(requires_approval(SafetyTier::Write, &a, "roost_edit"));
    }

    #[test]
    fn destructive_always_requires_approval_regardless_of_settings() {
        let a = AgentAutonomy {
            auto_approve_writes: true,
            auto_approve_tools: vec!["delete_task".to_string()],
            ..AgentAutonomy::default()
        };
        assert!(requires_approval(
            SafetyTier::Destructive,
            &a,
            "delete_task"
        ));
    }

    #[tokio::test]
    async fn pending_register_and_resolve_roundtrip() {
        let p = PendingApprovals::new();
        let (id, rx) = p.register();
        assert!(p.resolve(&id, ToolDecision::Approve));
        let got = rx.await.unwrap();
        assert_eq!(got, ToolDecision::Approve);
    }

    #[test]
    fn pending_resolve_unknown_id_returns_false() {
        let p = PendingApprovals::new();
        assert!(!p.resolve("not-a-real-id", ToolDecision::Approve));
    }

    #[tokio::test]
    async fn pending_forget_drops_sender_so_receiver_errors() {
        let p = PendingApprovals::new();
        let (id, rx) = p.register();
        p.forget(&id);
        assert!(rx.await.is_err());
    }
}
