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
use crate::db::queries::{ai_signals, approval_log};
use crate::secret_store;

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

#[tauri::command]
#[specta::specta]
pub fn get_settings(state: State<'_, SettingsState>) -> Result<Settings, String> {
    let guard = state
        .settings
        .lock()
        .map_err(|e| format!("Lock error: {e}"))?;
    Ok(guard.clone())
}

/// Provider API keys lifted out of `Settings` before serialization so
/// they can be written to the OS keychain rather than disk. Each field
/// is `None` if the source struct had no key set OR if we're on a
/// platform without a keychain (in which case the caller never strips,
/// see `take_provider_keys`).
struct ProviderKeys {
    gemini: Option<String>,
    openrouter: Option<String>,
    ollama: Option<String>,
    openai: Option<String>,
    anthropic: Option<String>,
}

/// S-03: lift the three provider API keys out of `settings` so the
/// on-disk JSON never carries plaintext credentials on a platform that
/// has somewhere safer. Mutates `settings` in place, returning the
/// keys for the caller to either write to the keychain or hand back
/// to the in-memory copy. Pure (no I/O) — unit-testable.
fn take_provider_keys(settings: &mut Settings) -> ProviderKeys {
    ProviderKeys {
        gemini: settings.gemini_api_key.take(),
        openrouter: settings.openrouter_api_key.take(),
        ollama: settings.ollama_api_key.take(),
        openai: settings.openai_api_key.take(),
        anthropic: settings.anthropic_api_key.take(),
    }
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

#[cfg(not(windows))]
fn atomic_replace(from: &Path, to: &Path) -> Result<(), String> {
    std::fs::rename(from, to).map_err(|e| e.to_string())
}

#[cfg(windows)]
fn atomic_replace(from: &Path, to: &Path) -> Result<(), String> {
    use windows::core::HSTRING;
    use windows::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };
    let from = HSTRING::from(from.as_os_str());
    let to = HSTRING::from(to.as_os_str());
    unsafe {
        MoveFileExW(
            &from,
            &to,
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
        .map_err(|e| e.to_string())
    }
}

/// Persist settings. On platforms with a keychain the Gemini,
/// OpenRouter, and Ollama API keys are pulled out of the struct,
/// written to the OS keychain, and the on-disk settings.json keeps
/// the fields as `null`. The in-memory copy still holds the real keys
/// so subsequent `get_settings` calls return them.
#[tauri::command]
#[specta::specta]
pub fn save_settings(
    app: tauri::AppHandle,
    mut settings: Settings,
    state: State<'_, SettingsState>,
) -> Result<(), String> {
    // Captured before `settings` is consumed below so we can reconcile
    // the OS "launch at login" registration after the write succeeds.
    // `app: AppHandle` is a runtime-injected Tauri arg, not part of the
    // serialized command payload, so the generated bindings signature
    // (saveSettings(settings)) is unchanged.
    let launch_at_startup = settings.launch_at_startup;
    let keys = if secret_store::is_supported() {
        let keys = take_provider_keys(&mut settings);
        for (key, slot) in [
            (keys.gemini.as_deref(), secret_store::keys::GEMINI_API_KEY),
            (
                keys.openrouter.as_deref(),
                secret_store::keys::OPENROUTER_API_KEY,
            ),
            (keys.ollama.as_deref(), secret_store::keys::OLLAMA_API_KEY),
            (keys.openai.as_deref(), secret_store::keys::OPENAI_API_KEY),
            (
                keys.anthropic.as_deref(),
                secret_store::keys::ANTHROPIC_API_KEY,
            ),
        ] {
            match key {
                Some(k) if !k.is_empty() => {
                    secret_store::set(slot, k)?;
                }
                _ => {
                    let _ = secret_store::delete(slot);
                }
            }
        }
        keys
    } else {
        // No keychain — leave the keys on disk. Clone so we can still
        // restore them into the in-memory copy below.
        ProviderKeys {
            gemini: settings.gemini_api_key.clone(),
            openrouter: settings.openrouter_api_key.clone(),
            ollama: settings.ollama_api_key.clone(),
            openai: settings.openai_api_key.clone(),
            anthropic: settings.anthropic_api_key.clone(),
        }
    };

    // R-02: route through the shared redacted-JSON helper. With
    // keychain support, `settings` here already has provider keys
    // stripped (we called `take_provider_keys` above). The helper is
    // idempotent — it re-NULLs the fields if anyone calls it with a
    // hydrated struct, which is the contract `persist_always_approve`
    // relies on.
    // Re-hydrate the in-memory copy with the live keys so handlers that
    // read `state.settings` after save see the values the user just set.
    settings.gemini_api_key = keys.gemini;
    settings.openrouter_api_key = keys.openrouter;
    settings.ollama_api_key = keys.ollama;
    settings.openai_api_key = keys.openai;
    settings.anthropic_api_key = keys.anthropic;

    // Serialize disk + memory behind the same mutex used by internal settings
    // mutations (for example "Always approve"). Writing before acquiring the
    // mutex allowed two successful calls to leave disk and memory representing
    // different snapshots.
    let mut guard = state
        .settings
        .lock()
        .map_err(|e| format!("Lock error: {e}"))?;
    persist_settings_file(&state.path, &settings)?;
    *guard = settings;
    drop(guard);

    // Sync the OS autostart registration with the saved choice. Failures
    // are logged inside the helper, not surfaced — the setting is already
    // persisted, and a flaky registry/LaunchAgent write shouldn't fail the
    // whole save. The next launch's startup reconcile will retry anyway.
    crate::reconcile_autostart(&app, launch_at_startup);
    Ok(())
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
    use super::*;

    /// S-03: after lifting provider keys, the on-disk JSON shape must
    /// not contain any of the secret values. This is the regression
    /// test that catches "someone added a fourth provider key and
    /// forgot to wire it through `take_provider_keys`."
    #[test]
    fn take_provider_keys_strips_all_three_from_serialized_json() {
        let mut settings = Settings {
            gemini_api_key: Some("GEMINI-SENTINEL".into()),
            openrouter_api_key: Some("OPENROUTER-SENTINEL".into()),
            ollama_api_key: Some("OLLAMA-SENTINEL".into()),
            openai_api_key: Some("OPENAI-SENTINEL".into()),
            anthropic_api_key: Some("ANTHROPIC-SENTINEL".into()),
            ..Settings::default()
        };

        let keys = take_provider_keys(&mut settings);

        // The returned `ProviderKeys` carries the originals so the
        // caller can write them to the keychain.
        assert_eq!(keys.gemini.as_deref(), Some("GEMINI-SENTINEL"));
        assert_eq!(keys.openrouter.as_deref(), Some("OPENROUTER-SENTINEL"));
        assert_eq!(keys.ollama.as_deref(), Some("OLLAMA-SENTINEL"));
        assert_eq!(keys.openai.as_deref(), Some("OPENAI-SENTINEL"));
        assert_eq!(keys.anthropic.as_deref(), Some("ANTHROPIC-SENTINEL"));

        // The struct that gets serialized to disk now has None for all
        // three. Serialize and assert none of the sentinels appear.
        let json = serde_json::to_string(&settings).expect("serialize");
        assert!(
            !json.contains("GEMINI-SENTINEL"),
            "gemini key leaked to disk JSON: {json}"
        );
        assert!(
            !json.contains("OPENROUTER-SENTINEL"),
            "openrouter key leaked to disk JSON: {json}"
        );
        assert!(
            !json.contains("OLLAMA-SENTINEL"),
            "ollama key leaked to disk JSON: {json}"
        );
        assert!(
            !json.contains("OPENAI-SENTINEL"),
            "openai key leaked to disk JSON: {json}"
        );
        assert!(
            !json.contains("ANTHROPIC-SENTINEL"),
            "anthropic key leaked to disk JSON: {json}"
        );
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

    /// `take_provider_keys` on a struct with no keys set must produce
    /// an all-None bundle — important because the keychain delete path
    /// runs based on `Some(empty)` vs `None`.
    #[test]
    fn take_provider_keys_returns_none_for_unset_fields() {
        // Default has no provider keys configured.
        let mut settings = Settings {
            gemini_api_key: None,
            openrouter_api_key: None,
            ollama_api_key: None,
            openai_api_key: None,
            anthropic_api_key: None,
            ..Settings::default()
        };

        let keys = take_provider_keys(&mut settings);
        assert!(keys.gemini.is_none());
        assert!(keys.openrouter.is_none());
        assert!(keys.ollama.is_none());
        assert!(keys.openai.is_none());
        assert!(keys.anthropic.is_none());
    }
}
