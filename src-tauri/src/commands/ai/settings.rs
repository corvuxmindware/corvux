//! Generic settings + device-profile IPC handlers. Per-feature handlers
//! (embedding, chat, LLM) live alongside their orchestration in sibling
//! modules.

use serde::Serialize;
use std::io::Write;
use std::path::Path;
use tauri::State;

use crate::ai::capability::{self, DeviceProfile};
use crate::ai::tools::approval::{PendingApprovals, ToolDecision};
use crate::ai::tools::AgentProfile;
use crate::commands::files::RoostFsState;
use crate::commands::graph::GraphDb;
use crate::commands::native_confirm::{NativeConfirm, OsDialog};
use crate::db::queries::{ai_signals, approval_log};
use crate::secret_store;

use super::settings_guard::{
    covered_by, plan_settings_update, Escalation, KeyUpdate, SettingsPlan,
};
use super::types::{Settings, SettingsState};

/// Wire shape for the agent-profile picker. Mirrors the
/// `AgentProfile::descriptor()` tuple so the frontend doesn't have to
/// hard-code labels — it just renders whatever Rust returns.
#[derive(Debug, Serialize, Clone, specta::Type)]
pub struct AgentProfileInfo {
    pub id: String,
    pub label: String,
    pub summary: String,
}

/// The settings as the webview may see them: everything EXCEPT the live
/// provider API keys, which stay in the OS keychain / backend memory.
///
/// Sharp-edges F11: `get_settings` used to return the in-memory struct whose
/// five key fields are hydrated from the keychain, so any script in the
/// webview could read every provider key in one call and, combined with the
/// (now closed) unvalidated base URL, spend or exfiltrate them. The UI only
/// ever needs to know *whether* a key is set — see [`get_provider_key_status`].
///
/// Round-trip contract with `save_settings`: a key field of `None` means
/// "unchanged", `Some("")` means "remove", `Some(k)` means "replace"
/// (`settings_guard::plan_settings_update`).
pub(crate) fn redacted_for_renderer(settings: &Settings) -> Settings {
    Settings {
        gemini_api_key: None,
        openrouter_api_key: None,
        ollama_api_key: None,
        openai_api_key: None,
        anthropic_api_key: None,
        ..settings.clone()
    }
}

#[tauri::command]
#[specta::specta]
pub fn get_settings(state: State<'_, SettingsState>) -> Result<Settings, String> {
    let guard = state
        .settings
        .lock()
        .map_err(|e| format!("Lock error: {e}"))?;
    Ok(redacted_for_renderer(&guard))
}

/// Which provider keys are configured — the only thing the Settings UI needs.
/// Booleans, never key material (not even a last-4 hint).
#[derive(Debug, Serialize, Clone, Copy, PartialEq, Eq, specta::Type)]
pub struct ProviderKeyStatus {
    pub gemini: bool,
    pub openrouter: bool,
    pub ollama: bool,
    pub openai: bool,
    pub anthropic: bool,
}

pub(crate) fn provider_key_status(settings: &Settings) -> ProviderKeyStatus {
    let set = |k: &Option<crate::sensitive::SecretString>| {
        k.as_deref().is_some_and(|k| !k.trim().is_empty())
    };
    ProviderKeyStatus {
        gemini: set(&settings.gemini_api_key),
        openrouter: set(&settings.openrouter_api_key),
        ollama: set(&settings.ollama_api_key),
        openai: set(&settings.openai_api_key),
        anthropic: set(&settings.anthropic_api_key),
    }
}

#[tauri::command]
#[specta::specta]
pub fn get_provider_key_status(
    state: State<'_, SettingsState>,
) -> Result<ProviderKeyStatus, String> {
    let guard = state
        .settings
        .lock()
        .map_err(|e| format!("Lock error: {e}"))?;
    Ok(provider_key_status(&guard))
}

/// The key a connection probe should use: what the user just typed, or — when
/// the field was left empty — the saved key. Resolved in the backend so the
/// webview never has to hold a saved key to "test the saved key".
pub(crate) fn effective_probe_key(
    typed: &str,
    saved: &Option<crate::sensitive::SecretString>,
) -> String {
    let typed = typed.trim();
    if !typed.is_empty() {
        return typed.to_string();
    }
    saved
        .as_deref()
        .map(|k| k.trim().to_string())
        .unwrap_or_default()
}

/// R-02 (2026-05-24 re-audit): canonical on-disk shape for the
/// `Settings` struct. Strips the three provider API keys when the
/// platform has a keychain — they live in `secret_store`, not the
/// JSON. Returns the serialized JSON ready for `std::fs::write`.
///
/// Every code path that persists `Settings` to disk MUST go through
/// here. The previous shape had `save_settings` strip keys but
/// `persist_always_approve` (in `approval.rs`) serialize the live
/// `SettingsState` snapshot directly — which carries hydrated
/// keychain values in memory, so clicking "Always Approve" wrote
/// the keys back to plaintext settings.json.
pub(crate) fn redacted_settings_json(settings: &Settings) -> Result<String, String> {
    if secret_store::is_supported() {
        // Clone first so we don't perturb the caller's in-memory copy.
        let snapshot = Settings {
            gemini_api_key: None,
            openrouter_api_key: None,
            ollama_api_key: None,
            openai_api_key: None,
            anthropic_api_key: None,
            ..settings.clone()
        };
        serde_json::to_string_pretty(&snapshot).map_err(|e| format!("Serialize error: {e}"))
    } else {
        // No keychain — keys stay on disk (intentional fallback for
        // platforms without `secret_store::is_supported()`).
        serde_json::to_string_pretty(settings).map_err(|e| format!("Serialize error: {e}"))
    }
}

/// Persist a complete settings snapshot without exposing a partially-written
/// JSON file after a crash. Callers must hold `SettingsState.settings` while
/// invoking this so disk and memory advance as one serialized mutation lane.
pub(crate) fn persist_settings_file(path: &Path, settings: &Settings) -> Result<(), String> {
    let json = redacted_settings_json(settings)?;
    persist_settings_json(path, &json)
}

pub(crate) fn persist_settings_json(path: &Path, json: &str) -> Result<(), String> {
    let tmp = path.with_extension("json.tmp");
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&tmp)
        .map_err(|e| format!("open settings temp file: {e}"))?;
    file.write_all(json.as_bytes())
        .map_err(|e| format!("write settings temp file: {e}"))?;
    file.sync_all()
        .map_err(|e| format!("sync settings temp file: {e}"))?;
    drop(file);
    atomic_replace(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("replace settings file: {e}")
    })
}

/// Strip only keys confirmed in secret storage, starting from the original
/// disk snapshot so failed migrations retain their existing fallback. This is
/// retried on every launch even when key hydration took the keychain-hit arm.
pub(crate) fn cleanup_migrated_keys(path: &Path, original: &Settings) -> Result<(), String> {
    cleanup_migrated_keys_with(original, secret_store::get, |disk| {
        let json =
            zeroize::Zeroizing::new(serde_json::to_string_pretty(disk).map_err(|e| e.to_string())?);
        persist_settings_json(path, &json)
    })
}

fn cleanup_migrated_keys_with(
    original: &Settings,
    mut get: impl FnMut(&str) -> Result<Option<crate::sensitive::SecretString>, String>,
    persist: impl FnOnce(&Settings) -> Result<(), String>,
) -> Result<(), String> {
    let mut disk = original.clone();
    let mut changed = false;
    for (key, slot) in [
        (&mut disk.gemini_api_key, secret_store::keys::GEMINI_API_KEY),
        (
            &mut disk.openrouter_api_key,
            secret_store::keys::OPENROUTER_API_KEY,
        ),
        (&mut disk.ollama_api_key, secret_store::keys::OLLAMA_API_KEY),
        (&mut disk.openai_api_key, secret_store::keys::OPENAI_API_KEY),
        (
            &mut disk.anthropic_api_key,
            secret_store::keys::ANTHROPIC_API_KEY,
        ),
    ] {
        if key.is_some() && get(slot)?.is_some() {
            *key = None;
            changed = true;
        }
    }
    if changed {
        persist(&disk)?;
    }
    Ok(())
}

#[cfg(not(windows))]
pub(crate) fn atomic_replace(from: &Path, to: &Path) -> Result<(), String> {
    std::fs::rename(from, to).map_err(|e| e.to_string())
}

#[cfg(windows)]
pub(crate) fn atomic_replace(from: &Path, to: &Path) -> Result<(), String> {
    use windows::core::HSTRING;
    use windows::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };
    let from = HSTRING::from(from.as_os_str());
    let to = HSTRING::from(to.as_os_str());
    // SAFETY: both HSTRINGs own NUL-terminated UTF-16 paths that remain alive
    // throughout the call. MoveFileExW does not retain these pointers.
    unsafe {
        MoveFileExW(
            &from,
            &to,
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
        .map_err(|e| e.to_string())
    }
}

/// Apply the planned per-provider key changes to secret storage. Only `Set`
/// and `Clear` touch the keychain; `Keep` (the renderer sent no key) never
/// does — previously every save rewrote or DELETED all five slots from
/// whatever the renderer held.
fn apply_key_updates(
    updates: &[KeyUpdate; 5],
    mut set: impl FnMut(&str, &str) -> Result<(), String>,
    mut delete: impl FnMut(&str) -> Result<(), String>,
) -> Result<(), String> {
    // Same order as `SettingsPlan::key_updates`.
    const SLOTS: [&str; 5] = [
        secret_store::keys::GEMINI_API_KEY,
        secret_store::keys::OPENROUTER_API_KEY,
        secret_store::keys::OLLAMA_API_KEY,
        secret_store::keys::OPENAI_API_KEY,
        secret_store::keys::ANTHROPIC_API_KEY,
    ];
    for (update, slot) in updates.iter().zip(SLOTS) {
        match update {
            KeyUpdate::Keep => {}
            KeyUpdate::Set(k) => set(slot, k)?,
            KeyUpdate::Clear => delete(slot)?,
        }
    }
    Ok(())
}

/// What a completed save tells its caller.
pub(crate) struct SaveOutcome {
    pub launch_at_startup: bool,
}

/// Shown to the user (via the webview's error toast) when they decline the
/// native confirmation. The save is dropped as a whole: applying the
/// non-escalating half would make the UI show state the backend does not hold.
pub(crate) const SAVE_DECLINED: &str =
    "Not saved: the security confirmation was cancelled, so nothing was changed.";

fn confirmation_body(escalations: &[Escalation]) -> String {
    let mut body = String::from("Corvux is about to loosen a safety setting:\n\n");
    // EVERY escalation is listed — the planner refuses a save with more than
    // `MAX_ESCALATIONS_PER_SAVE`, so there is never a fold to hide one behind.
    for e in escalations {
        body.push_str("\u{2022} ");
        body.push_str(&e.describe());
        body.push('\n');
    }
    body.push_str("\nIf you did not just change this in Settings, choose Cancel.");
    body
}

fn lock_settings(state: &SettingsState) -> Result<std::sync::MutexGuard<'_, Settings>, String> {
    state
        .settings
        .lock()
        .map_err(|e| format!("Lock error: {e}"))
}

/// Everything `save_settings` does except talk to Tauri: plan against the
/// current settings, ask for an out-of-band confirmation when the plan
/// escalates, then — under the settings lock — re-plan, apply keychain changes,
/// persist and install. `confirm`, the keychain closures and `audit` are
/// injected so the whole policy is testable without a window, an OS keychain
/// or a database.
///
/// The settings lock is never held across the confirmation `await`: the dialog
/// can stay open for minutes and chat reads settings on every turn.
pub(crate) async fn save_settings_core<C: NativeConfirm + ?Sized>(
    state: &SettingsState,
    incoming: Settings,
    confirm: &C,
    set_key: impl FnMut(&str, &str) -> Result<(), String>,
    del_key: impl FnMut(&str) -> Result<(), String>,
    mut audit: impl FnMut(&[Escalation], bool),
) -> Result<SaveOutcome, String> {
    let launch_at_startup = incoming.launch_at_startup;

    // 1. Plan against a snapshot (lock released before any await).
    let snapshot = lock_settings(state)?.clone();
    let first = plan_settings_update(&snapshot, incoming.clone())?;

    // 2. Out-of-band confirmation, only when something escalates.
    let mut shown: Vec<Escalation> = Vec::new();
    if !first.escalations.is_empty() {
        let allowed = confirm
            .confirm(
                "Confirm security change",
                &confirmation_body(&first.escalations),
                "Allow change",
            )
            .await;
        audit(&first.escalations, allowed);
        if !allowed {
            return Err(SAVE_DECLINED.to_string());
        }
        shown = first.escalations;
    }

    // 3. Apply under the lock. Re-plan: another save may have landed while the
    //    dialog was open, and a confirmation only covers what it showed.
    let mut guard = lock_settings(state)?;
    let plan = plan_settings_update(&guard, incoming)?;
    if !covered_by(&shown, &plan.escalations) {
        return Err(
            "Settings changed while the confirmation was open. Review them and save again."
                .to_string(),
        );
    }
    let SettingsPlan {
        merged,
        key_updates,
        escalations: _,
    } = plan;
    apply_key_updates(&key_updates, set_key, del_key)?;
    // Serialize disk + memory behind the same mutex used by internal settings
    // mutations (for example "Always approve"). `persist_settings_file` strips
    // the provider keys from the JSON (R-02); memory keeps them for chat.
    persist_settings_file(&state.path, &merged)?;
    *guard = merged;
    Ok(SaveOutcome { launch_at_startup })
}

/// Persist settings.
///
/// Provider API keys follow `settings_guard`'s contract: a key field of `None`
/// leaves the stored key alone, `Some("")` removes it, `Some(k)` replaces it.
/// Keys live in the OS keychain; settings.json keeps them as `null`; the
/// in-memory copy holds the live values for chat. `get_settings` never returns
/// them (F11).
///
/// Changes that loosen a safety setting — enabling agent auto-approve, pinning
/// a tool, un-blocking a path, pointing Ollama at an internet host — need the
/// user to confirm in a native OS dialog the webview cannot click (F1/F2). The
/// decision and the answer are written to the approval log.
#[tauri::command]
#[specta::specta]
pub async fn save_settings(
    app: tauri::AppHandle,
    settings: Settings,
    state: State<'_, SettingsState>,
    graph: State<'_, GraphDb>,
) -> Result<(), String> {
    let dialog = OsDialog(app.clone());
    let outcome = save_settings_core(
        &state,
        settings,
        &dialog,
        secret_store::set,
        secret_store::delete,
        |escalations, allowed| record_escalations(&graph, escalations, allowed),
    )
    .await?;

    // Sync the OS autostart registration with the saved choice. Failures
    // are logged inside the helper, not surfaced — the setting is already
    // persisted, and a flaky registry/LaunchAgent write shouldn't fail the
    // whole save. The next launch's startup reconcile will retry anyway.
    crate::reconcile_autostart(&app, outcome.launch_at_startup);
    Ok(())
}

/// Write one approval-log row per escalation the user was asked about, whatever
/// they answered, so a disarmed approval gate is never silent. Best-effort like
/// every approval-log write (no roost open ⇒ no DB ⇒ the tracing line below is
/// the record).
fn record_escalations(graph: &GraphDb, escalations: &[Escalation], allowed: bool) {
    let decision = if allowed {
        "escalation_confirmed"
    } else {
        "escalation_declined"
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    for (i, e) in escalations.iter().enumerate() {
        let summary = e.describe();
        tracing::warn!(decision, change = %summary, "settings: security-relevant change");
        let Ok(guard) = graph.db.lock() else { continue };
        let Some(db) = guard.as_ref() else { continue };
        let entry = approval_log::ApprovalLogEntry {
            request_id: format!("settings-{now:.3}-{i}"),
            timestamp: now,
            tool: e.audit_tool().to_string(),
            tier: "settings".to_string(),
            decision: decision.to_string(),
            args_hash: approval_log::hash_args(&summary),
            args_summary: summary,
        };
        if let Err(err) = approval_log::record_decision(db, &entry) {
            tracing::warn!(error = %err, "settings: approval-log write failed");
        }
    }
}

/// List the available agent profiles for the settings UI. Driven by
/// `AgentProfile::all()` so the frontend stays in sync with whatever
/// profiles are defined in Rust — adding a new profile = a new variant
/// + a new `match` arm, the UI picks it up for free.
#[tauri::command]
#[specta::specta]
pub fn list_agent_profiles() -> Result<Vec<AgentProfileInfo>, String> {
    Ok(AgentProfile::all()
        .iter()
        .map(|p| {
            let (id, label, summary) = p.descriptor();
            AgentProfileInfo {
                id: id.to_string(),
                label: label.to_string(),
                summary: summary.to_string(),
            }
        })
        .collect())
}

/// Snapshot of detected device capability + ML tier.
///
/// Cheap to call (sysinfo refresh is sub-millisecond on modern systems).
/// Used by the settings panel to show "what Corvux thinks your hardware
/// can handle" (and historically informed embedding-backend routing,
/// since removed).
#[tauri::command]
#[specta::specta]
pub fn get_device_profile() -> Result<DeviceProfile, String> {
    Ok(capability::detect())
}

/// Resolve a pending tool-approval request. The frontend calls this from
/// the `ToolCard` Approve / Reject / Always-Approve buttons. Unknown
/// `request_id` (already decided / timed out) returns an error so the
/// UI can collapse the card.
#[tauri::command]
#[specta::specta]
pub fn decide_tool(
    request_id: String,
    decision: ToolDecision,
    state: State<'_, PendingApprovals>,
) -> Result<(), String> {
    if state.resolve(&request_id, decision) {
        Ok(())
    } else {
        Err("Pending tool approval not found (already decided or timed out)".into())
    }
}

/// Drop a pending tool-approval request without resolving it. Called by
/// the frontend on `ChatPanel` unmount so the in-memory `PendingApprovals`
/// HashMap doesn't accumulate orphan senders until app restart. The
/// agent loop's `receiver.await` resolves with `Err`, which the approval
/// gate already maps to `ToolDecision::Reject` — the retryable path the
/// LLM knows how to recover from.
///
/// Idempotent: forgetting an unknown id is a no-op (silent success).
#[tauri::command]
#[specta::specta]
pub fn forget_pending_approval(
    request_id: String,
    state: State<'_, PendingApprovals>,
) -> Result<(), String> {
    state.forget(&request_id);
    Ok(())
}

/// Wire shape for the Approval History UI. Mirrors
/// `db::queries::approval_log::ApprovalLogEntry` 1:1 — keeping a
/// separate type so the Rust struct can change shape (e.g. add
/// rationale) without breaking the frontend immediately.
#[derive(Debug, Serialize, specta::Type)]
pub struct ApprovalHistoryRow {
    pub request_id: String,
    pub timestamp: f64,
    pub tool: String,
    pub tier: String,
    pub decision: String,
    pub args_hash: String,
    pub args_summary: String,
}

impl From<approval_log::ApprovalLogEntry> for ApprovalHistoryRow {
    fn from(e: approval_log::ApprovalLogEntry) -> Self {
        Self {
            request_id: e.request_id,
            timestamp: e.timestamp,
            tool: e.tool,
            tier: e.tier,
            decision: e.decision,
            args_hash: e.args_hash,
            args_summary: e.args_summary,
        }
    }
}

/// Return the most recent `limit` approval decisions. Backs the
/// Settings → AI → Approval History page. `limit` is clamped to 1..500
/// to keep the response payload bounded — UI shows a "load more" cue
/// when the cap is hit.
#[tauri::command]
#[specta::specta]
pub fn list_approval_log(
    limit: Option<u32>,
    graph: State<'_, GraphDb>,
) -> Result<Vec<ApprovalHistoryRow>, String> {
    let limit = limit.map(|n| n as usize).unwrap_or(100).clamp(1, 500);
    let db_guard = graph.db.lock().map_err(|e| format!("Lock error: {e}"))?;
    let db = match db_guard.as_ref() {
        Some(d) => d,
        None => return Ok(Vec::new()),
    };
    let rows = approval_log::list_recent(db, limit)?;
    Ok(rows.into_iter().map(ApprovalHistoryRow::from).collect())
}

/// Per-tool reliability row returned by [`tool_reliability_stats`].
/// Mirrors `approval_log::ToolStat` 1:1 — separated so the wire shape
/// can evolve independently of the storage representation.
#[derive(Debug, Serialize, specta::Type)]
pub struct ToolReliabilityRow {
    pub tool: String,
    #[specta(type = i32)]
    pub total: i64,
    #[specta(type = i32)]
    pub approvals: i64,
    #[specta(type = i32)]
    pub always_approvals: i64,
    #[specta(type = i32)]
    pub rejections: i64,
    #[specta(type = i32)]
    pub forgotten: i64,
    pub rejection_rate: f64,
}

impl From<approval_log::ToolStat> for ToolReliabilityRow {
    fn from(s: approval_log::ToolStat) -> Self {
        Self {
            tool: s.tool,
            total: s.total,
            approvals: s.approvals,
            always_approvals: s.always_approvals,
            rejections: s.rejections,
            forgotten: s.forgotten,
            rejection_rate: s.rejection_rate,
        }
    }
}

/// Return per-tool reliability stats over the last `window_days` days
/// (default 30). Most-rejected tools first. Backs the Settings → AI →
/// Tool Stats panel. § 4.4 of agent-hardening.
#[tauri::command]
#[specta::specta]
pub fn tool_reliability_stats(
    window_days: Option<f64>,
    graph: State<'_, GraphDb>,
) -> Result<Vec<ToolReliabilityRow>, String> {
    let window_secs = window_days.unwrap_or(30.0).max(0.0) * 86_400.0;
    let db_guard = graph.db.lock().map_err(|e| format!("Lock error: {e}"))?;
    let db = match db_guard.as_ref() {
        Some(d) => d,
        None => return Ok(Vec::new()),
    };
    let stats = approval_log::tool_stats(db, Some(window_secs))?;
    Ok(stats.into_iter().map(ToolReliabilityRow::from).collect())
}

/// Return the aggregate per-note signal summary backing the
/// Settings → AI → AI Learning panel. Counts every Approve / Reject
/// row written by the approval gate, plus the top-N most-boosted and
/// most-penalized notes. Self-evolution loop (Phase 2.4).
#[tauri::command]
#[specta::specta]
pub fn get_ai_learning_summary(
    top_n: Option<u32>,
    graph: State<'_, GraphDb>,
) -> Result<ai_signals::SignalSummary, String> {
    let top_n = top_n.map(|n| n as usize).unwrap_or(5).clamp(0, 25);
    let db_guard = graph.db.lock().map_err(|e| format!("Lock error: {e}"))?;
    let db = match db_guard.as_ref() {
        Some(d) => d,
        None => {
            return Ok(ai_signals::SignalSummary {
                total_signals: 0,
                approve_signals: 0,
                reject_signals: 0,
                top_boosted: Vec::new(),
                top_penalized: Vec::new(),
            });
        }
    };
    ai_signals::summarize_signals(db, top_n)
}

/// Wipe every row from `:ai_signals` — back behind the
/// Settings → AI → AI Learning → "Reset learning" button. Destructive
/// but recoverable: the user just stops biasing retrievals; the
/// approval log audit trail (`:approval_log`) is NOT affected.
#[tauri::command]
#[specta::specta]
pub fn reset_ai_learning(graph: State<'_, GraphDb>) -> Result<(), String> {
    let db_guard = graph.db.lock().map_err(|e| format!("Lock error: {e}"))?;
    let db = match db_guard.as_ref() {
        Some(d) => d,
        None => return Ok(()),
    };
    ai_signals::clear_all_signals(db)
}

/// Wire shape for the AGENTS.md status check returned by
/// [`agents_md_status`]. Distinguishes "no roost open" / "no file" /
/// "file present" so the UI can render the right empty state.
#[derive(Debug, Serialize, Default, specta::Type)]
pub struct AgentsMdStatus {
    /// True when the file exists at `{roost}/AGENTS.md` AND passed
    /// path-safety + regular-file checks.
    pub exists: bool,
    /// Roost-relative path of the file when present, for the
    /// "Open in editor" button.
    pub path: Option<String>,
    /// Byte size of the loaded body (post-sanitize, post-truncate).
    /// `None` when no file.
    #[specta(type = Option<u32>)]
    pub body_bytes: Option<usize>,
    /// True when the on-disk file exceeded the 4 KB cap and was cut.
    pub truncated: bool,
    /// Tag families the sanitizer stripped before injection. Empty
    /// when the file was clean; non-empty values surface as a
    /// security warning in the panel.
    pub stripped_tags: Vec<String>,
}

/// Read-only status for `{roost}/AGENTS.md`. Tells the Settings panel
/// whether the file exists, how big it is, whether it was truncated,
/// and whether tag sanitization fired. Never writes — purely
/// observational. Returns `Default` (all-empty, exists=false) when no
/// roost is open so the panel can render the virtual empty state
/// without conditional branches.
#[tauri::command]
#[specta::specta]
pub async fn agents_md_status(roost_fs: State<'_, RoostFsState>) -> Result<AgentsMdStatus, String> {
    let Some(backend) = roost_fs.current().await else {
        return Ok(AgentsMdStatus::default());
    };
    let loaded = match backend.read("AGENTS.md").await {
        Ok(bytes) => String::from_utf8(bytes).ok().and_then(|raw| {
            crate::ai::roost_instructions::from_text(
                &raw,
                crate::ai::roost_instructions::RoostInstructionSource::LocalDefault,
            )
        }),
        Err(crate::roostfs::RoostError::NotFound(_)) => None,
        Err(e) => return Err(format!("read AGENTS.md: {e}")),
    };
    match loaded {
        Some(v) => Ok(AgentsMdStatus {
            exists: true,
            path: Some(format!(
                "{}/AGENTS.md",
                backend.display_root().trim_end_matches('/')
            )),
            body_bytes: Some(v.body.len()),
            truncated: v.truncated,
            stripped_tags: v.stripped_tags.into_iter().map(|s| s.to_string()).collect(),
        }),
        None => Ok(AgentsMdStatus {
            exists: false,
            path: None,
            body_bytes: None,
            truncated: false,
            stripped_tags: Vec::new(),
        }),
    }
}

/// Create `{roost}/AGENTS.md` with the commented-out onboarding
/// skeleton from `agent-hardening-and-self-improvement.md` § 5.13.
/// Idempotent: if the file already exists, this is a no-op (returns
/// Ok). The frontend should refresh status after calling.
///
/// Never auto-invoked — must be triggered by an explicit user click in
/// Settings → AI → Roost Instructions. This is intentional (§ 5.0
/// review correction): writing a placeholder file into the user's
/// roost without explicit consent is roost pollution, even if the
/// content is commented out.
#[tauri::command]
#[specta::specta]
pub async fn create_agents_md(roost_fs: State<'_, RoostFsState>) -> Result<String, String> {
    let Some(backend) = roost_fs.current().await else {
        return Err("No roost is currently open. Open a roost first.".to_string());
    };
    let target = format!("{}/AGENTS.md", backend.display_root().trim_end_matches('/'));
    if backend
        .exists("AGENTS.md")
        .await
        .map_err(|e| format!("check AGENTS.md: {e}"))?
    {
        // Idempotent — still return the path so the UI can show
        // "Already exists" gracefully without a separate error path.
        return Ok(target);
    }

    const SKELETON: &str = "\
# Roost Instructions for Corvux

<!-- Anything you write here is loaded into the agent's context every turn.
     Keep it under 4 KB. Plain markdown - no special syntax required.
     Examples below are commented out - uncomment and edit, or delete this whole file. -->

<!--
## Folder conventions
- Daily notes go in: `Journal/YYYY-MM-DD.md`
- Inbox: `00 Inbox/`
- Reference (read-only): `Reference/`

## Tags
- Use singular tag forms (#idea not #ideas)

## Tone
- Bullets, not prose, for lists
- Cite by [[Title]] not file path

## Don't
- Don't auto-create notes in `Reference/`
-->
";
    match backend.create("AGENTS.md", SKELETON.as_bytes()).await {
        Ok(()) | Err(crate::roostfs::RoostError::AlreadyExists(_)) => {}
        Err(e) => return Err(format!("write AGENTS.md failed: {e}")),
    }
    tracing::info!(path = %target, "roost_instructions: created skeleton AGENTS.md");
    Ok(target)
}

#[cfg(test)]
mod tests {

    #[test]
    fn retries_plaintext_cleanup_when_keychain_already_contains_key() {
        let original = Settings {
            gemini_api_key: Some("old-disk-key".into()),
            ..Settings::default()
        };
        for fail in [true, false] {
            let result = cleanup_migrated_keys_with(
                &original,
                |_| Ok(Some("stored-key".into())),
                |disk| {
                    assert!(disk.gemini_api_key.is_none());
                    if fail {
                        Err("locked".into())
                    } else {
                        Ok(())
                    }
                },
            );
            assert_eq!(result.is_err(), fail);
            assert_eq!(original.gemini_api_key.as_deref(), Some("old-disk-key"));
        }
    }

    #[test]
    fn plaintext_cleanup_retains_unmigrated_fallbacks() {
        let original = Settings {
            gemini_api_key: Some("gemini".into()),
            openai_api_key: Some("openai".into()),
            ..Settings::default()
        };
        cleanup_migrated_keys_with(
            &original,
            |slot| Ok((slot == secret_store::keys::GEMINI_API_KEY).then(|| "stored".into())),
            |disk| {
                assert!(disk.gemini_api_key.is_none());
                assert_eq!(disk.openai_api_key.as_deref(), Some("openai"));
                Ok(())
            },
        )
        .unwrap();
    }
    use super::*;

    pub(super) fn hydrated() -> Settings {
        Settings {
            gemini_api_key: Some("GEMINI-SENTINEL".into()),
            openrouter_api_key: Some("OPENROUTER-SENTINEL".into()),
            ollama_api_key: Some("OLLAMA-SENTINEL".into()),
            openai_api_key: Some("OPENAI-SENTINEL".into()),
            anthropic_api_key: Some("ANTHROPIC-SENTINEL".into()),
            ..Settings::default()
        }
    }

    const SENTINELS: [&str; 5] = [
        "GEMINI-SENTINEL",
        "OPENROUTER-SENTINEL",
        "OLLAMA-SENTINEL",
        "OPENAI-SENTINEL",
        "ANTHROPIC-SENTINEL",
    ];

    /// F11 (sharp-edges 2026-11-11): the wire form handed to the webview must
    /// not carry ANY provider key. A "fourth provider key someone forgot to
    /// wire through" shows up here as a leaked sentinel.
    #[test]
    fn the_renderer_view_of_settings_carries_no_provider_key() {
        let view = redacted_for_renderer(&hydrated());
        let json = serde_json::to_string(&view).expect("serialize");
        for s in SENTINELS {
            assert!(!json.contains(s), "{s} leaked to the webview: {json}");
        }
        // Everything else survives, so the UI still has its settings.
        assert_eq!(view.model, Settings::default().model);
        assert!(view.agent_autonomy.auto_approve_reads);
    }

    #[test]
    fn key_status_reports_booleans_only() {
        let s = provider_key_status(&hydrated());
        assert!(s.gemini && s.openrouter && s.ollama && s.openai && s.anthropic);
        let none = provider_key_status(&Settings::default());
        assert!(
            !none.gemini && !none.openrouter && !none.ollama && !none.openai && !none.anthropic
        );
        // A whitespace-only key is not "set".
        let blank = Settings {
            gemini_api_key: Some("  ".into()),
            ..Settings::default()
        };
        assert!(!provider_key_status(&blank).gemini);
        let json = serde_json::to_string(&s).unwrap();
        for sentinel in SENTINELS {
            assert!(!json.contains(sentinel));
        }
    }

    #[test]
    fn probe_key_prefers_typed_then_falls_back_to_saved() {
        let saved = Some(crate::sensitive::SecretString::from("saved-key"));
        assert_eq!(effective_probe_key("  typed ", &saved), "typed");
        assert_eq!(effective_probe_key("", &saved), "saved-key");
        assert_eq!(effective_probe_key("   ", &saved), "saved-key");
        assert_eq!(effective_probe_key("", &None), "");
    }

    /// R-02 (2026-05-24 re-audit): the shared `redacted_settings_json`
    /// helper is the single chokepoint between an in-memory Settings
    /// (which may carry hydrated keychain values) and on-disk
    /// settings.json. Drift = a regression on Locked Decision #10
    /// (API keys in OS keychain). This test pins the contract for the
    /// `is_supported() == true` branch — keychain platforms must
    /// strip provider keys from the serialized form even when the
    /// in-memory struct has them hydrated.
    #[test]
    fn redacted_settings_json_strips_provider_keys_when_keychain_supported() {
        // Skip if the test platform has no keychain — on those the
        // helper intentionally leaves keys on disk.
        if !secret_store::is_supported() {
            return;
        }
        let hydrated = Settings {
            gemini_api_key: Some("GEMINI-LEAK-CHECK".into()),
            openrouter_api_key: Some("OPENROUTER-LEAK-CHECK".into()),
            ollama_api_key: Some("OLLAMA-LEAK-CHECK".into()),
            openai_api_key: Some("OPENAI-LEAK-CHECK".into()),
            anthropic_api_key: Some("ANTHROPIC-LEAK-CHECK".into()),
            ..Settings::default()
        };
        let json = redacted_settings_json(&hydrated).expect("serialize");
        for sentinel in [
            "GEMINI-LEAK-CHECK",
            "OPENROUTER-LEAK-CHECK",
            "OLLAMA-LEAK-CHECK",
            "OPENAI-LEAK-CHECK",
            "ANTHROPIC-LEAK-CHECK",
        ] {
            assert!(
                !json.contains(sentinel),
                "{sentinel} leaked through redacted_settings_json: {json}"
            );
        }
        // `auto_approve_tools` is the always-approve write path — make
        // sure the helper still serializes the rest of the struct so
        // approval state actually persists.
        assert!(
            json.contains("\"agent_autonomy\""),
            "redacted JSON must still include non-secret fields"
        );
    }
}

#[cfg(test)]
mod credential_deletion_tests {
    use super::*;

    fn all_keep() -> [KeyUpdate; 5] {
        [
            KeyUpdate::Keep,
            KeyUpdate::Keep,
            KeyUpdate::Keep,
            KeyUpdate::Keep,
            KeyUpdate::Keep,
        ]
    }

    #[test]
    fn clearing_a_provider_key_surfaces_deletion_failure() {
        let mut updates = all_keep();
        updates[0] = KeyUpdate::Clear;
        let err = apply_key_updates(&updates, |_, _| Ok(()), |_| Err("keychain locked".into()))
            .unwrap_err();
        assert_eq!(err, "keychain locked");
    }

    #[test]
    fn keep_never_touches_the_keychain() {
        // Before F11 every save rewrote or deleted all five slots from whatever
        // the webview held; a redacted round-trip would have wiped every key.
        apply_key_updates(
            &all_keep(),
            |slot, _| panic!("set({slot}) must not run for Keep"),
            |slot| panic!("delete({slot}) must not run for Keep"),
        )
        .unwrap();
    }

    #[test]
    fn set_and_clear_hit_the_right_slots() {
        let mut updates = all_keep();
        updates[1] = KeyUpdate::Set("or".into());
        updates[3] = KeyUpdate::Clear;
        let mut sets = Vec::new();
        let mut dels = Vec::new();
        apply_key_updates(
            &updates,
            |slot, v| {
                sets.push((slot.to_string(), v.to_string()));
                Ok(())
            },
            |slot| {
                dels.push(slot.to_string());
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(
            sets,
            vec![(
                secret_store::keys::OPENROUTER_API_KEY.to_string(),
                "or".to_string()
            )]
        );
        assert_eq!(dels, vec![secret_store::keys::OPENAI_API_KEY.to_string()]);
    }
}

#[cfg(test)]
mod save_core_tests {
    //! The whole `save_settings` policy — plan → native confirm → apply — run
    //! against a real settings file with a scripted dialog and recording
    //! keychain closures. No window, no OS keychain, no database.
    #![allow(clippy::field_reassign_with_default)]
    use super::tests::hydrated;
    use super::*;
    use crate::commands::native_confirm::test_support::Scripted;
    use std::sync::Mutex;

    fn state_with(settings: Settings) -> (tempfile::TempDir, SettingsState) {
        let dir = tempfile::tempdir().unwrap();
        let state = SettingsState {
            path: dir.path().join("settings.json"),
            settings: Mutex::new(settings),
        };
        (dir, state)
    }

    #[derive(Default)]
    struct Keychain {
        sets: Vec<(String, String)>,
        dels: Vec<String>,
    }

    async fn save(
        state: &SettingsState,
        incoming: Settings,
        confirm: &(impl NativeConfirm + ?Sized),
        kc: &mut Keychain,
        audit: &mut Vec<(usize, bool)>,
    ) -> Result<SaveOutcome, String> {
        let kc = Mutex::new(kc);
        save_settings_core(
            state,
            incoming,
            confirm,
            |slot, v| {
                kc.lock().unwrap().sets.push((slot.into(), v.into()));
                Ok(())
            },
            |slot| {
                kc.lock().unwrap().dels.push(slot.into());
                Ok(())
            },
            |esc, allowed| audit.push((esc.len(), allowed)),
        )
        .await
    }

    #[tokio::test]
    async fn an_ordinary_save_never_prompts_and_persists() {
        let (_d, state) = state_with(Settings::default());
        let mut inc = Settings::default();
        inc.model = "gemini-x".into();
        inc.gemini_api_key = Some("GEMINI-SENTINEL".into());
        let dialog = Scripted::deny(); // would fail the save if asked
        let (mut kc, mut audit) = (Keychain::default(), Vec::new());

        save(&state, inc, &dialog, &mut kc, &mut audit)
            .await
            .unwrap();

        assert_eq!(dialog.times_asked(), 0);
        assert!(audit.is_empty());
        let mem = state.settings.lock().unwrap();
        assert_eq!(mem.model, "gemini-x");
        assert_eq!(mem.gemini_api_key.as_deref(), Some("GEMINI-SENTINEL"));
        assert_eq!(kc.sets.len(), 1);
        // The secret reached the keychain closure but never the file.
        let on_disk = std::fs::read_to_string(&state.path).unwrap();
        assert!(on_disk.contains("gemini-x"));
        assert!(!on_disk.contains("GEMINI-SENTINEL"), "{on_disk}");
    }

    #[tokio::test]
    async fn declining_the_dialog_changes_nothing() {
        let (_d, state) = state_with(Settings::default());
        let mut inc = Settings::default();
        inc.model = "would-be-saved".into(); // rides along with the escalation
        inc.agent_autonomy.auto_approve_writes = true;
        inc.openai_api_key = Some("OPENAI-SENTINEL".into());
        let dialog = Scripted::deny();
        let (mut kc, mut audit) = (Keychain::default(), Vec::new());

        let err = save(&state, inc, &dialog, &mut kc, &mut audit)
            .await
            .err()
            .expect("must be refused");

        assert_eq!(err, SAVE_DECLINED);
        assert_eq!(dialog.times_asked(), 1);
        assert_eq!(audit, vec![(1, false)]);
        let mem = state.settings.lock().unwrap();
        assert!(!mem.agent_autonomy.auto_approve_writes);
        assert_eq!(mem.model, Settings::default().model);
        assert!(mem.openai_api_key.is_none());
        assert!(
            kc.sets.is_empty() && kc.dels.is_empty(),
            "no keychain writes"
        );
        assert!(!state.path.exists(), "nothing was persisted");
    }

    #[tokio::test]
    async fn allowing_the_dialog_applies_the_escalation_and_logs_it() {
        let (_d, state) = state_with(Settings::default());
        let mut inc = Settings::default();
        inc.agent_autonomy.auto_approve_writes = true;
        let dialog = Scripted::allow();
        let (mut kc, mut audit) = (Keychain::default(), Vec::new());

        save(&state, inc, &dialog, &mut kc, &mut audit)
            .await
            .unwrap();

        assert_eq!(dialog.times_asked(), 1);
        assert_eq!(audit, vec![(1, true)]);
        assert!(
            state
                .settings
                .lock()
                .unwrap()
                .agent_autonomy
                .auto_approve_writes
        );
        assert!(std::fs::read_to_string(&state.path)
            .unwrap()
            .contains("\"auto_approve_writes\": true"));
    }

    #[tokio::test]
    async fn the_audit_misuse_example_is_stopped_by_a_declined_dialog() {
        // get → spread → save, exactly as the sharp-edges report shows.
        let mut cur = Settings::default();
        cur.agent_autonomy.agent_blocked_paths = vec!["Journal/".into(), "Finance/".into()];
        let (_d, state) = state_with(cur.clone());
        let mut inc = redacted_for_renderer(&cur);
        inc.agent_autonomy.auto_approve_writes = true;
        inc.agent_autonomy.auto_approve_tools = vec!["roost_write".into()];
        inc.agent_autonomy.agent_blocked_paths.clear();
        let dialog = Scripted::deny();
        let (mut kc, mut audit) = (Keychain::default(), Vec::new());

        assert!(save(&state, inc, &dialog, &mut kc, &mut audit)
            .await
            .is_err());
        let mem = state.settings.lock().unwrap();
        assert!(!mem.agent_autonomy.auto_approve_writes);
        assert_eq!(mem.agent_autonomy.agent_blocked_paths.len(), 2);
    }

    #[tokio::test]
    async fn a_redacted_round_trip_does_not_wipe_stored_keys() {
        let (_d, state) = state_with(hydrated());
        // The webview did get_settings (keys = None) and saves one tweak back.
        let mut inc = redacted_for_renderer(&hydrated());
        inc.model = "tweak".into();
        let dialog = Scripted::deny();
        let (mut kc, mut audit) = (Keychain::default(), Vec::new());

        save(&state, inc, &dialog, &mut kc, &mut audit)
            .await
            .unwrap();

        assert!(
            kc.sets.is_empty() && kc.dels.is_empty(),
            "keychain untouched"
        );
        let mem = state.settings.lock().unwrap();
        assert_eq!(mem.model, "tweak");
        assert_eq!(mem.gemini_api_key.as_deref(), Some("GEMINI-SENTINEL"));
        assert_eq!(mem.anthropic_api_key.as_deref(), Some("ANTHROPIC-SENTINEL"));
    }

    #[tokio::test]
    async fn an_empty_key_removes_exactly_that_key() {
        let (_d, state) = state_with(hydrated());
        let mut inc = redacted_for_renderer(&hydrated());
        inc.openrouter_api_key = Some("".into());
        let dialog = Scripted::deny();
        let (mut kc, mut audit) = (Keychain::default(), Vec::new());

        save(&state, inc, &dialog, &mut kc, &mut audit)
            .await
            .unwrap();

        assert_eq!(
            kc.dels,
            vec![secret_store::keys::OPENROUTER_API_KEY.to_string()]
        );
        let mem = state.settings.lock().unwrap();
        assert!(mem.openrouter_api_key.is_none());
        assert!(mem.gemini_api_key.is_some());
    }

    #[tokio::test]
    async fn an_invalid_ollama_url_is_rejected_before_anything_is_written() {
        let (_d, state) = state_with(Settings::default());
        let mut inc = Settings::default();
        inc.ollama_base_url = "http://localhost:11434@attacker.example".into();
        let dialog = Scripted::allow();
        let (mut kc, mut audit) = (Keychain::default(), Vec::new());

        let err = save(&state, inc, &dialog, &mut kc, &mut audit)
            .await
            .err()
            .expect("userinfo is the S-02 bypass shape");
        assert!(err.contains("user:password@"), "{err}");
        assert_eq!(
            dialog.times_asked(),
            0,
            "no point asking about an invalid value"
        );
        assert!(!state.path.exists());
    }

    #[tokio::test]
    async fn plain_http_to_the_internet_asks_and_a_decline_changes_nothing() {
        let (_d, state) = state_with(Settings::default());
        let mut inc = Settings::default();
        inc.ollama_base_url = "http://attacker.example:11434".into();
        let dialog = Scripted::deny();
        let (mut kc, mut audit) = (Keychain::default(), Vec::new());

        let err = save(&state, inc, &dialog, &mut kc, &mut audit)
            .await
            .err()
            .expect("declined");
        assert_eq!(err, SAVE_DECLINED);
        assert_eq!(dialog.times_asked(), 1);
        assert_eq!(
            state.settings.lock().unwrap().ollama_base_url,
            "http://localhost:11434"
        );
    }

    #[tokio::test]
    async fn changing_the_ollama_server_removes_its_saved_token_from_the_keychain() {
        let mut cur = Settings::default();
        cur.ollama_base_url = "https://ollama.mine.example".into();
        cur.ollama_api_key = Some("TOKEN-FOR-OLD-SERVER".into());
        let (_d, state) = state_with(cur.clone());
        let mut inc = redacted_for_renderer(&cur); // token field None = "unchanged"
        inc.ollama_base_url = "https://attacker.example".into();
        let dialog = Scripted::allow();
        let (mut kc, mut audit) = (Keychain::default(), Vec::new());

        save(&state, inc, &dialog, &mut kc, &mut audit)
            .await
            .unwrap();

        assert_eq!(
            kc.dels,
            vec![secret_store::keys::OLLAMA_API_KEY.to_string()]
        );
        assert!(state.settings.lock().unwrap().ollama_api_key.is_none());
    }

    /// Answers "allow" but, while the dialog is "open", lets another save land.
    struct RacingDialog<'a> {
        state: &'a SettingsState,
    }

    #[async_trait::async_trait]
    impl NativeConfirm for RacingDialog<'_> {
        async fn confirm(&self, _t: &str, _b: &str, _a: &str) -> bool {
            self.state
                .settings
                .lock()
                .unwrap()
                .agent_autonomy
                .agent_blocked_paths
                .push("B/".into());
            true
        }
    }

    #[tokio::test]
    async fn a_confirmation_does_not_cover_a_change_it_never_showed() {
        let mut cur = Settings::default();
        cur.agent_autonomy.agent_blocked_paths = vec!["A/".into()];
        let (_d, state) = state_with(cur);
        let mut inc = Settings::default();
        inc.agent_autonomy.agent_blocked_paths = vec![]; // un-block A/ (shown)
        let dialog = RacingDialog { state: &state };
        let (mut kc, mut audit) = (Keychain::default(), Vec::new());

        let err = save(&state, inc, &dialog, &mut kc, &mut audit)
            .await
            .err()
            .expect("B/ was never shown");
        assert!(err.contains("changed while"), "{err}");
        // Nothing from the refused save was applied.
        assert_eq!(
            state
                .settings
                .lock()
                .unwrap()
                .agent_autonomy
                .agent_blocked_paths,
            vec!["A/".to_string(), "B/".to_string()]
        );
    }
}
