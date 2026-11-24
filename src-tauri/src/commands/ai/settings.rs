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
use crate::sensitive::SecretString;

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

/// Secret storage as the settings writer sees it. A trait so the policy below —
/// "a key leaves settings.json only once the keychain provably holds it" — is
/// testable with a keychain that fails, without touching the real one.
pub(crate) trait KeyStore {
    /// False on a platform with no secret storage: keys then stay in
    /// settings.json (intentional fallback).
    fn supported(&self) -> bool;
    fn get(&self, slot: &str) -> Result<Option<SecretString>, String>;
    fn set(&self, slot: &str, value: &str) -> Result<(), String>;
    fn delete(&self, slot: &str) -> Result<(), String>;
}

/// The real OS keychain / mobile secret store.
pub(crate) struct SystemKeyStore;

impl KeyStore for SystemKeyStore {
    fn supported(&self) -> bool {
        secret_store::is_supported()
    }
    fn get(&self, slot: &str) -> Result<Option<SecretString>, String> {
        secret_store::get(slot)
    }
    fn set(&self, slot: &str, value: &str) -> Result<(), String> {
        secret_store::set(slot, value)
    }
    fn delete(&self, slot: &str) -> Result<(), String> {
        secret_store::delete(slot)
    }
}

/// The five provider-key fields with their secret-store slot names. One list, so
/// a sixth provider cannot be wired into the planner and forgotten here.
fn key_slots(s: &mut Settings) -> [(&'static str, &mut Option<SecretString>); 5] {
    [
        (secret_store::keys::GEMINI_API_KEY, &mut s.gemini_api_key),
        (
            secret_store::keys::OPENROUTER_API_KEY,
            &mut s.openrouter_api_key,
        ),
        (secret_store::keys::OLLAMA_API_KEY, &mut s.ollama_api_key),
        (secret_store::keys::OPENAI_API_KEY, &mut s.openai_api_key),
        (
            secret_store::keys::ANTHROPIC_API_KEY,
            &mut s.anthropic_api_key,
        ),
    ]
}

/// True only when the store demonstrably holds exactly `key` in `slot`. A read
/// error (locked keychain, no Secret Service) is "not confirmed".
fn store_holds(keys: &(impl KeyStore + ?Sized), slot: &str, key: &SecretString) -> bool {
    keys.get(slot)
        .ok()
        .flatten()
        .is_some_and(|stored| stored == *key)
}

/// The provider keys settings.json holds, in [`key_slots`] order.
type FileKeys = [Option<SecretString>; 5];

/// The keys `settings` carries, in [`key_slots`] order (derived from that one
/// list so the two cannot drift).
fn keys_of(settings: &Settings) -> FileKeys {
    let mut copy = settings.clone();
    let mut out = FileKeys::default();
    for (i, (_, field)) in key_slots(&mut copy).into_iter().enumerate() {
        out[i] = field.take();
    }
    out
}

/// The provider keys the settings file at `path` holds RIGHT NOW, read from the
/// file itself at write time (F2/N3) — never from a cached idea of it, so a file
/// the user edited by hand (say, removing a legacy plaintext key) is judged as it
/// is. Parsed exactly as startup parses it. A missing, unreadable or corrupt file
/// holds nothing: that only ever means a key is left out of the file (fail safe).
/// The contents are wiped after parsing and no value is logged.
fn read_file_keys(path: &Path) -> FileKeys {
    let Ok(text) = std::fs::read_to_string(path) else {
        return FileKeys::default();
    };
    let text = zeroize::Zeroizing::new(text);
    serde_json::from_str::<Settings>(&text)
        .map(|s| keys_of(&s))
        .unwrap_or_default()
}

/// The settings as they may be WRITTEN to settings.json.
///
/// Provider keys live in the keychain; the file must not carry them. For each
/// key in memory:
/// 1. the keychain provably holds that exact value → left out of the file;
/// 2. `heal` and a keychain write succeeds and reads back → left out;
/// 3. otherwise the keychain cannot be confirmed (locked, D-Bus timeout, ACL
///    prompt refused…). The key may then stay in the file ONLY if `on_file` —
///    what the file holds RIGHT NOW — already holds that exact value. That is
///    the legacy migration-pending case (AK-4): a plaintext key whose startup
///    `set` failed lives only in memory + the old file, and an unrelated save
///    must not destroy the only durable copy. Any other key (hydrated from the
///    keychain, or just typed) is left out — a failed keychain READ must never
///    turn into a plaintext write, which is what "strip only when confirmed"
///    alone would do.
///
/// `heal`: step 2. Callers that persist on the user's behalf (`save_settings`)
/// pass `true`; the plain persist path used by "Always approve" passes `false`
/// so it never writes the keychain as a side effect.
fn disk_form(
    settings: &Settings,
    keys: &(impl KeyStore + ?Sized),
    heal: bool,
    on_file: &FileKeys,
) -> Settings {
    let mut snapshot = settings.clone();
    if !keys.supported() {
        // No secret storage on this platform: keys live in the file by design.
        return snapshot;
    }
    for (i, (slot, field)) in key_slots(&mut snapshot).into_iter().enumerate() {
        let Some(key) = field.clone() else { continue };
        if store_holds(keys, slot, &key) {
            *field = None;
            continue;
        }
        if heal {
            match keys.set(slot, &key) {
                Ok(()) if store_holds(keys, slot, &key) => {
                    *field = None;
                    continue;
                }
                Ok(()) => tracing::warn!(slot, "keychain write did not read back"),
                Err(error) => tracing::warn!(slot, %error, "keychain write failed"),
            }
        }
        if on_file[i].as_ref() == Some(&key) {
            // Already in the file: no new exposure, and it is the only durable copy.
            tracing::debug!(
                slot,
                "provider key not yet in the keychain; kept in settings.json"
            );
        } else {
            // Not in the file today: never write it there (value never logged).
            tracing::warn!(
                slot,
                "keychain unavailable; provider key kept in memory only and left out of settings.json"
            );
            *field = None;
        }
    }
    snapshot
}

fn to_json(settings: &Settings) -> Result<String, String> {
    serde_json::to_string_pretty(settings).map_err(|e| format!("Serialize error: {e}"))
}

/// Persist a complete settings snapshot without exposing a partially-written
/// JSON file after a crash. Callers must hold `SettingsState.settings` while
/// invoking this so disk and memory advance as one serialized mutation lane.
///
/// R-02 (2026-05-24 re-audit): this is the canonical on-disk shape — every code
/// path that persists `Settings` MUST go through here ([`disk_form`] decides
/// which keys may be in the file). The previous shape had `save_settings` strip
/// keys but `persist_always_approve` (in `approval.rs`) serialize the live
/// `SettingsState` snapshot directly, which carries hydrated keychain values in
/// memory, so clicking "Always Approve" wrote the keys back to plaintext.
pub(crate) fn persist_settings_file(path: &Path, settings: &Settings) -> Result<(), String> {
    // Never writes the keychain (no `heal`): "Always approve" must not have that
    // side effect.
    persist_settings_file_with(path, settings, &SystemKeyStore, false)
}

fn persist_settings_file_with(
    path: &Path,
    settings: &Settings,
    keys: &(impl KeyStore + ?Sized),
    heal: bool,
) -> Result<(), String> {
    // What the file holds NOW decides which keys may stay in it (see `disk_form`).
    let disk = disk_form(settings, keys, heal, &read_file_keys(path));
    // May still hold a legacy plaintext key: wipe the buffer.
    let json = zeroize::Zeroizing::new(to_json(&disk)?);
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

/// A save made up ONLY of "save / replace / remove my API key" changes is the
/// everyday Settings → AI and first-run flow, and is worded as such (review
/// AK-5). Anything else keeps the "loosen a safety setting" wording. The dialog
/// still appears and still lists every change; only the framing differs.
fn is_key_only(escalations: &[Escalation]) -> bool {
    !escalations.is_empty() && escalations.iter().all(Escalation::is_key_change)
}

fn confirmation_title(escalations: &[Escalation]) -> &'static str {
    if is_key_only(escalations) {
        "Confirm API key change"
    } else {
        "Confirm security change"
    }
}

fn confirmation_body(escalations: &[Escalation]) -> String {
    let mut body = String::from(if is_key_only(escalations) {
        "Corvux is about to change a saved AI provider key:\n\n"
    } else {
        "Corvux is about to loosen a safety setting:\n\n"
    });
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
/// persist and install. `confirm`, the keychain (`keys`) and `audit` are
/// injected so the whole policy is testable without a window, an OS keychain
/// or a database.
///
/// The settings lock is never held across the confirmation `await`: the dialog
/// can stay open for minutes and chat reads settings on every turn.
pub(crate) async fn save_settings_core<C: NativeConfirm + ?Sized, K: KeyStore + ?Sized>(
    state: &SettingsState,
    incoming: Settings,
    confirm: &C,
    keys: &K,
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
                confirmation_title(&first.escalations),
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
    apply_key_updates(
        &key_updates,
        |slot, k| keys.set(slot, k),
        |slot| keys.delete(slot),
    )?;
    // Serialize disk + memory behind the same mutex used by internal settings
    // mutations (for example "Always approve"). The provider keys leave the JSON
    // (R-02) only once the keychain is confirmed to hold them (AK-4): a key whose
    // startup migration failed is retried here (`heal`) and otherwise stays in
    // the file, so an unrelated save can never destroy the only durable copy.
    // Memory keeps them for chat.
    persist_settings_file_with(&state.path, &merged, keys, true)?;
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
        &SystemKeyStore,
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
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Mutex;

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

    /// An in-memory keychain that can be told to fail, and records every write.
    /// Never touches the real OS keychain.
    #[derive(Default)]
    pub(super) struct FakeKeys {
        slots: Mutex<HashMap<String, String>>,
        pub(super) fail_set: AtomicBool,
        pub(super) fail_get: AtomicBool,
        sets: Mutex<Vec<(String, String)>>,
        dels: Mutex<Vec<String>>,
    }

    impl FakeKeys {
        /// A keychain that already holds every key `s` has — the normal state
        /// after startup hydration read them from it.
        pub(super) fn holding(s: &Settings) -> Self {
            let k = Self::default();
            let mut s = s.clone();
            for (slot, field) in key_slots(&mut s) {
                if let Some(v) = field.as_ref() {
                    k.slots.lock().unwrap().insert(slot.into(), v.to_string());
                }
            }
            k
        }
        pub(super) fn failing_writes(self) -> Self {
            self.fail_set.store(true, Ordering::SeqCst);
            self
        }
        pub(super) fn stored(&self, slot: &str) -> Option<String> {
            self.slots.lock().unwrap().get(slot).cloned()
        }
        pub(super) fn sets(&self) -> Vec<(String, String)> {
            self.sets.lock().unwrap().clone()
        }
        pub(super) fn dels(&self) -> Vec<String> {
            self.dels.lock().unwrap().clone()
        }
    }

    impl KeyStore for FakeKeys {
        fn supported(&self) -> bool {
            true
        }
        fn get(&self, slot: &str) -> Result<Option<SecretString>, String> {
            if self.fail_get.load(Ordering::SeqCst) {
                return Err("keyring get: locked".into());
            }
            Ok(self.stored(slot).map(Into::into))
        }
        fn set(&self, slot: &str, value: &str) -> Result<(), String> {
            if self.fail_set.load(Ordering::SeqCst) {
                return Err("keyring set: locked".into());
            }
            self.sets.lock().unwrap().push((slot.into(), value.into()));
            self.slots.lock().unwrap().insert(slot.into(), value.into());
            Ok(())
        }
        fn delete(&self, slot: &str) -> Result<(), String> {
            self.dels.lock().unwrap().push(slot.into());
            self.slots.lock().unwrap().remove(slot);
            Ok(())
        }
    }

    // ── AK-4 / F2: when may a provider key be in settings.json at all? ─────
    //
    // Only (a) never when the keychain holds it, and (b) when the keychain
    // CANNOT be confirmed, only if the file ALREADY holds that exact value (the
    // legacy migration-pending case). A keychain-only key must never reach the
    // file because a keychain read failed.

    /// The JSON that would be written, given what the file holds right now.
    fn written(live: &Settings, keys: &FakeKeys, heal: bool, on_file: &FileKeys) -> String {
        to_json(&disk_form(live, keys, heal, on_file)).unwrap()
    }

    fn nothing_on_file() -> FileKeys {
        FileKeys::default()
    }

    #[test]
    fn a_legacy_key_already_in_the_file_stays_when_the_keychain_cannot_confirm_it() {
        // The state after a failed startup migration: the key is in memory and
        // in the old settings.json, the keychain is empty (or refuses writes).
        let live = hydrated();
        let on_file = keys_of(&live);
        let json = written(&live, &FakeKeys::default(), false, &on_file);
        for s in SENTINELS {
            assert!(json.contains(s), "{s} was dropped without a keychain copy");
        }
    }

    #[test]
    fn a_key_the_file_does_not_hold_is_never_written_when_the_keychain_cannot_confirm_it() {
        // F2: keys hydrated from the keychain at startup were never in the file.
        // A later keychain read error (locked KWallet/Secret Service, D-Bus
        // timeout, refused macOS prompt) must not put them there.
        let live = hydrated();
        let cases: [(&str, FakeKeys); 3] = [
            ("empty keychain", FakeKeys::default()),
            ("read error", {
                let k = FakeKeys::holding(&live);
                k.fail_get.store(true, Ordering::SeqCst);
                k
            }),
            ("read AND write errors", {
                let k = FakeKeys::holding(&live).failing_writes();
                k.fail_get.store(true, Ordering::SeqCst);
                k
            }),
        ];
        for (name, keys) in &cases {
            for heal in [false, true] {
                let json = written(&live, keys, heal, &nothing_on_file());
                for s in SENTINELS {
                    assert!(
                        !json.contains(s),
                        "{s} reached settings.json ({name}, heal={heal}): {json}"
                    );
                }
            }
        }
    }

    #[test]
    fn only_the_slots_the_file_really_holds_may_stay() {
        // Gemini is legacy (in the file); the other four were keychain-only.
        let live = hydrated();
        let mut on_file = nothing_on_file();
        on_file[0] = live.gemini_api_key.clone();
        let json = written(&live, &FakeKeys::default().failing_writes(), true, &on_file);
        assert!(json.contains("GEMINI-SENTINEL"));
        for s in &SENTINELS[1..] {
            assert!(!json.contains(s), "{s}: {json}");
        }
    }

    #[test]
    fn an_old_value_in_the_file_does_not_license_writing_a_newer_unconfirmed_one() {
        // The file holds the OLD key; memory holds a NEWER one the keychain
        // cannot confirm. Writing the newer one would be a new plaintext write.
        let live = Settings {
            gemini_api_key: Some("NEWER-KEY".into()),
            ..Settings::default()
        };
        let file_side = Settings {
            gemini_api_key: Some("OLDER-KEY".into()),
            ..Settings::default()
        };
        let json = written(
            &live,
            &FakeKeys::default().failing_writes(),
            true,
            &keys_of(&file_side),
        );
        assert!(
            !json.contains("NEWER-KEY") && !json.contains("OLDER-KEY"),
            "{json}"
        );
    }

    #[test]
    fn a_key_is_stripped_once_the_keychain_provably_holds_it() {
        let live = hydrated();
        let keys = FakeKeys::holding(&live);
        // Whether or not the file used to hold it.
        for on_file in [nothing_on_file(), keys_of(&live)] {
            let json = written(&live, &keys, false, &on_file);
            for s in SENTINELS {
                assert!(!json.contains(s), "{s} leaked into settings.json: {json}");
            }
            assert!(
                json.contains("\"agent_autonomy\""),
                "the rest is still written"
            );
        }
        assert!(
            keys.sets().is_empty(),
            "the non-healing path never writes the keychain"
        );
    }

    #[test]
    fn a_keychain_holding_a_different_value_or_failing_to_read_is_not_a_confirmation() {
        let live = hydrated();
        let legacy = keys_of(&live);
        // Stale value for one slot: not a confirmation. Legacy file copy stays…
        let keys = FakeKeys::holding(&live);
        keys.slots.lock().unwrap().insert(
            secret_store::keys::GEMINI_API_KEY.into(),
            "an-older-key".into(),
        );
        let json = written(&live, &keys, false, &legacy);
        assert!(json.contains("GEMINI-SENTINEL"));
        assert!(!json.contains("OPENAI-SENTINEL"));
        // …but a key the file never held is not written because of it.
        let json = written(&live, &keys, false, &nothing_on_file());
        assert!(!json.contains("GEMINI-SENTINEL"), "{json}");
        // A read error (locked keychain) confirms nothing: legacy copies stay.
        let keys = FakeKeys::holding(&live);
        keys.fail_get.store(true, Ordering::SeqCst);
        let json = written(&live, &keys, false, &legacy);
        for s in SENTINELS {
            assert!(json.contains(s), "{s}");
        }
    }

    #[test]
    fn healing_retries_the_keychain_write_and_strips_only_on_success() {
        let live = hydrated();
        let legacy = keys_of(&live);
        // Keychain recovered: heal writes every missing key and strips them all.
        let keys = FakeKeys::default();
        let json = written(&live, &keys, true, &legacy);
        assert_eq!(keys.sets().len(), 5);
        for s in SENTINELS {
            assert!(!json.contains(s), "{s}");
        }
        // Still refusing writes: a legacy copy is kept (nothing lost)…
        let keys = FakeKeys::default().failing_writes();
        let json = written(&live, &keys, true, &legacy);
        for s in SENTINELS {
            assert!(json.contains(s), "{s}");
        }
        // …and a key the file never held is not written.
        let json = written(&live, &keys, true, &nothing_on_file());
        for s in SENTINELS {
            assert!(!json.contains(s), "{s}");
        }
        // Already stored keys are not rewritten by heal.
        let keys = FakeKeys::holding(&live);
        written(&live, &keys, true, &legacy);
        assert!(keys.sets().is_empty());
    }

    #[test]
    fn keys_of_follows_the_slot_order() {
        let live = hydrated();
        let mut copy = live.clone();
        let from_slots: Vec<(String, Option<SecretString>)> = key_slots(&mut copy)
            .into_iter()
            .map(|(slot, f)| (slot.to_string(), f.clone()))
            .collect();
        let keys = keys_of(&live);
        for (i, (_, value)) in from_slots.iter().enumerate() {
            assert_eq!(&keys[i], value, "slot {i}");
        }
        assert!(keys.iter().all(Option::is_some));
    }

    /// F2/N3: what the file holds is read from the FILE, so it is always current.
    #[test]
    fn read_file_keys_reports_what_the_file_holds_right_now() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let holds_nothing = |p: &Path| read_file_keys(p).iter().all(Option::is_none);

        // Missing file: holds nothing.
        assert!(holds_nothing(&path));

        // A file with keys: reads them, in slot order.
        let live = hydrated();
        std::fs::write(&path, serde_json::to_string_pretty(&live).unwrap()).unwrap();
        assert_eq!(read_file_keys(&path), keys_of(&live));

        // Hand edit: the user removes the gemini key (and only that) from the
        // file. The very next read sees it — nothing is cached.
        let mut edited = live.clone();
        edited.gemini_api_key = None;
        std::fs::write(&path, serde_json::to_string_pretty(&edited).unwrap()).unwrap();
        let now = read_file_keys(&path);
        assert!(now[0].is_none(), "the removed key is no longer 'held'");
        assert!(now[1..].iter().all(Option::is_some));

        // Hand edit: a different value is judged by what it says now.
        edited.openai_api_key = Some("HAND-EDITED".into());
        std::fs::write(&path, serde_json::to_string_pretty(&edited).unwrap()).unwrap();
        assert_eq!(now_key(&path, 3).as_deref(), Some("HAND-EDITED"));

        // Corrupt, truncated, empty or wrong-shaped files hold nothing.
        for garbage in ["garbage, not json", "[1, 2", "", "[]", "42", "null"] {
            std::fs::write(&path, garbage).unwrap();
            assert!(holds_nothing(&path), "{garbage:?}");
        }
        // A directory where the file should be is unreadable: holds nothing.
        let as_dir = dir.path().join("a-directory");
        std::fs::create_dir(&as_dir).unwrap();
        assert!(holds_nothing(&as_dir));
    }

    fn now_key(path: &Path, slot_index: usize) -> Option<String> {
        read_file_keys(path)[slot_index]
            .as_ref()
            .map(|k| k.to_string())
    }

    pub(super) const SENTINELS: [&str; 5] = [
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

    /// R-02 (2026-05-24 re-audit): `disk_form` (behind `persist_settings_file`)
    /// is the single chokepoint between an in-memory Settings (which may carry
    /// hydrated keychain values) and on-disk settings.json. Drift = a regression
    /// on Locked Decision #10 (API keys in OS keychain). This test pins the
    /// contract for a keychain that holds the keys — provider keys must be
    /// stripped from the serialized form even when the in-memory struct has them
    /// hydrated. (The keychain is a fake: this test must never write the
    /// developer's real one. The other half of the contract — when a key may
    /// stay in the file — is pinned by the `a_key_…` / `only_the_slots_…` tests.)
    #[test]
    fn redacted_settings_json_strips_provider_keys_when_keychain_supported() {
        let hydrated = Settings {
            gemini_api_key: Some("GEMINI-LEAK-CHECK".into()),
            openrouter_api_key: Some("OPENROUTER-LEAK-CHECK".into()),
            ollama_api_key: Some("OLLAMA-LEAK-CHECK".into()),
            openai_api_key: Some("OPENAI-LEAK-CHECK".into()),
            anthropic_api_key: Some("ANTHROPIC-LEAK-CHECK".into()),
            ..Settings::default()
        };
        let keys = FakeKeys::holding(&hydrated);
        let json = written(&hydrated, &keys, false, &nothing_on_file());
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
    use super::tests::{hydrated, FakeKeys};
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

    /// The keychain a running app has: it already holds whatever the in-memory
    /// settings hold (startup hydration read it from there).
    fn keychain_for(current: &Settings) -> FakeKeys {
        FakeKeys::holding(current)
    }

    async fn save(
        state: &SettingsState,
        incoming: Settings,
        confirm: &(impl NativeConfirm + ?Sized),
        kc: &FakeKeys,
        audit: &mut Vec<(usize, bool)>,
    ) -> Result<SaveOutcome, String> {
        save_settings_core(state, incoming, confirm, kc, |esc, allowed| {
            audit.push((esc.len(), allowed))
        })
        .await
    }

    #[tokio::test]
    async fn an_ordinary_save_never_prompts_and_persists() {
        // Ordinary = no security-bearing change: re-saving the SAME key (R3
        // only escalates a set/replace) plus a model tweak. A first-ever or
        // replaced key now prompts — see the R3 tests in settings_guard.
        let mut current = Settings::default();
        current.gemini_api_key = Some("GEMINI-SENTINEL".into());
        let (_d, state) = state_with(current);
        let mut inc = Settings::default();
        inc.model = "gemini-x".into();
        inc.gemini_api_key = Some("GEMINI-SENTINEL".into());
        let dialog = Scripted::deny(); // would fail the save if asked
        let (kc, mut audit) = (FakeKeys::default(), Vec::new());

        save(&state, inc, &dialog, &kc, &mut audit).await.unwrap();

        assert_eq!(dialog.times_asked(), 0);
        assert!(audit.is_empty());
        let mem = state.settings.lock().unwrap();
        assert_eq!(mem.model, "gemini-x");
        assert_eq!(mem.gemini_api_key.as_deref(), Some("GEMINI-SENTINEL"));
        assert_eq!(kc.sets().len(), 1);
        // The secret reached the keychain but never the file.
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
        let (kc, mut audit) = (FakeKeys::default(), Vec::new());

        let err = save(&state, inc, &dialog, &kc, &mut audit)
            .await
            .err()
            .expect("must be refused");

        assert_eq!(err, SAVE_DECLINED);
        assert_eq!(dialog.times_asked(), 1);
        // R3: the new key is its own escalation line in the same dialog —
        // one prompt, two listed changes, both declined together.
        assert_eq!(audit, vec![(2, false)]);
        let mem = state.settings.lock().unwrap();
        assert!(!mem.agent_autonomy.auto_approve_writes);
        assert_eq!(mem.model, Settings::default().model);
        assert!(mem.openai_api_key.is_none());
        assert!(
            kc.sets().is_empty() && kc.dels().is_empty(),
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
        let (kc, mut audit) = (FakeKeys::default(), Vec::new());

        save(&state, inc, &dialog, &kc, &mut audit).await.unwrap();

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
        let (kc, mut audit) = (FakeKeys::default(), Vec::new());

        assert!(save(&state, inc, &dialog, &kc, &mut audit).await.is_err());
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
        let (kc, mut audit) = (keychain_for(&hydrated()), Vec::new());

        save(&state, inc, &dialog, &kc, &mut audit).await.unwrap();

        assert!(
            kc.sets().is_empty() && kc.dels().is_empty(),
            "keychain untouched"
        );
        let mem = state.settings.lock().unwrap();
        assert_eq!(mem.model, "tweak");
        assert_eq!(mem.gemini_api_key.as_deref(), Some("GEMINI-SENTINEL"));
        assert_eq!(mem.anthropic_api_key.as_deref(), Some("ANTHROPIC-SENTINEL"));
        // And the keys are not in the file (they are in the keychain).
        let on_disk = std::fs::read_to_string(&state.path).unwrap();
        for s in super::tests::SENTINELS {
            assert!(!on_disk.contains(s), "{s} leaked: {on_disk}");
        }
    }

    #[tokio::test]
    async fn an_empty_key_removes_exactly_that_key_after_a_native_yes() {
        let (_d, state) = state_with(hydrated());
        let mut inc = redacted_for_renderer(&hydrated());
        inc.openrouter_api_key = Some("".into());
        // AK-8: removing a saved key is confirmed like saving one.
        let dialog = Scripted::allow();
        let (kc, mut audit) = (keychain_for(&hydrated()), Vec::new());

        save(&state, inc, &dialog, &kc, &mut audit).await.unwrap();

        assert_eq!(dialog.times_asked(), 1);
        assert_eq!(audit, vec![(1, true)]);
        assert_eq!(
            kc.dels(),
            vec![secret_store::keys::OPENROUTER_API_KEY.to_string()]
        );
        let mem = state.settings.lock().unwrap();
        assert!(mem.openrouter_api_key.is_none());
        assert!(mem.gemini_api_key.is_some());
    }

    #[tokio::test]
    async fn a_declined_key_removal_deletes_nothing() {
        let (_d, state) = state_with(hydrated());
        let mut inc = redacted_for_renderer(&hydrated());
        // A script wiping every key at once.
        inc.gemini_api_key = Some("".into());
        inc.openrouter_api_key = Some("".into());
        inc.ollama_api_key = Some("".into());
        inc.openai_api_key = Some("".into());
        inc.anthropic_api_key = Some("".into());
        let dialog = Scripted::deny();
        let (kc, mut audit) = (keychain_for(&hydrated()), Vec::new());

        let err = save(&state, inc, &dialog, &kc, &mut audit)
            .await
            .err()
            .expect("declined");

        assert_eq!(err, SAVE_DECLINED);
        assert_eq!(dialog.times_asked(), 1, "one dialog for the whole wipe");
        assert_eq!(audit, vec![(5, false)]);
        assert!(kc.dels().is_empty(), "keychain untouched");
        let mem = state.settings.lock().unwrap();
        assert!(mem.gemini_api_key.is_some() && mem.anthropic_api_key.is_some());
    }

    #[tokio::test]
    async fn a_new_support_address_needs_a_native_yes_and_a_decline_changes_nothing() {
        let default = Settings::default().support_endpoint;
        let (_d, state) = state_with(Settings::default());
        let mut inc = Settings::default();
        inc.support_endpoint = Some("https://phish.example/report".into());
        let (kc, mut audit) = (FakeKeys::default(), Vec::new());

        let deny = Scripted::deny();
        let err = save(&state, inc.clone(), &deny, &kc, &mut audit)
            .await
            .err()
            .expect("declined");
        assert_eq!(err, SAVE_DECLINED);
        assert_eq!(deny.times_asked(), 1);
        assert_eq!(state.settings.lock().unwrap().support_endpoint, default);
        assert!(!state.path.exists());

        let allow = Scripted::allow();
        save(&state, inc, &allow, &kc, &mut audit).await.unwrap();
        assert_eq!(
            state.settings.lock().unwrap().support_endpoint.as_deref(),
            Some("https://phish.example/report")
        );
        assert!(std::fs::read_to_string(&state.path)
            .unwrap()
            .contains("https://phish.example/report"));
    }

    #[tokio::test]
    async fn clearing_a_key_that_was_never_saved_asks_nothing() {
        let (_d, state) = state_with(Settings::default());
        let mut inc = Settings::default();
        inc.gemini_api_key = Some("".into());
        let dialog = Scripted::deny();
        let (kc, mut audit) = (FakeKeys::default(), Vec::new());

        save(&state, inc, &dialog, &kc, &mut audit).await.unwrap();
        assert_eq!(dialog.times_asked(), 0);
    }

    // ── AK-5: key dialogs are worded as an account change ───────────────

    #[test]
    fn a_key_only_save_is_worded_as_an_account_change_not_a_safety_loosening() {
        let key = vec![Escalation::ProviderKey("Gemini")];
        assert_eq!(confirmation_title(&key), "Confirm API key change");
        let body = confirmation_body(&key);
        assert!(body.contains("change a saved AI provider key"), "{body}");
        assert!(!body.contains("loosen"), "{body}");
        assert!(
            body.contains("Save an API key for Gemini"),
            "still lists it: {body}"
        );
        assert!(
            body.contains("choose Cancel"),
            "the Cancel guidance stays: {body}"
        );
        let removal = vec![Escalation::ProviderKeyRemoval("OpenAI")];
        assert_eq!(confirmation_title(&removal), "Confirm API key change");
        assert!(!confirmation_body(&removal).contains("loosen"));
    }

    #[test]
    fn anything_besides_a_key_keeps_the_safety_wording() {
        // Mixed: a key save riding with a real escalation must not be softened.
        let mixed = vec![
            Escalation::AutoApproveWrites,
            Escalation::ProviderKey("Gemini"),
        ];
        assert_eq!(confirmation_title(&mixed), "Confirm security change");
        let body = confirmation_body(&mixed);
        assert!(body.contains("loosen a safety setting"), "{body}");
        assert!(body.contains("Save an API key for Gemini"), "{body}");
        let alone = vec![Escalation::AutoApproveReads];
        assert_eq!(confirmation_title(&alone), "Confirm security change");
        assert!(confirmation_body(&alone).contains("loosen a safety setting"));
        // An empty list is never "key only" (nothing to confirm anyway).
        assert_eq!(confirmation_title(&[]), "Confirm security change");
    }

    // ── AK-4: no later save may destroy the only durable copy of a key ──

    /// The state after a failed startup migration: the legacy key is in memory
    /// and in settings.json; the keychain refused the write.
    fn legacy_key_state() -> (tempfile::TempDir, SettingsState, Settings) {
        let mut cur = Settings::default();
        cur.gemini_api_key = Some("LEGACY-PLAINTEXT-KEY".into());
        let (d, state) = state_with(cur.clone());
        // What startup left on disk (cleanup only strips confirmed keys).
        std::fs::write(&state.path, serde_json::to_string_pretty(&cur).unwrap()).unwrap();
        (d, state, cur)
    }

    // ── F2/N3: the file is judged as it is NOW, including hand edits ────

    #[tokio::test]
    async fn a_key_the_user_removed_from_the_file_by_hand_is_not_written_back() {
        let (_d, state, cur) = legacy_key_state();
        // While the app runs, the user deletes the legacy plaintext key from
        // settings.json (the keychain still refuses everything).
        let mut edited = cur.clone();
        edited.gemini_api_key = None;
        std::fs::write(&state.path, serde_json::to_string_pretty(&edited).unwrap()).unwrap();
        let mut inc = redacted_for_renderer(&cur);
        inc.model = "tweak".into();
        let kc = FakeKeys::default().failing_writes();
        kc.fail_get.store(true, std::sync::atomic::Ordering::SeqCst);
        let mut audit = Vec::new();

        save(&state, inc, &Scripted::deny(), &kc, &mut audit)
            .await
            .unwrap();

        let on_disk = std::fs::read_to_string(&state.path).unwrap();
        assert!(on_disk.contains("tweak"));
        assert!(
            !on_disk.contains("LEGACY-PLAINTEXT-KEY"),
            "the hand-removed key was written back: {on_disk}"
        );
        assert_eq!(
            state.settings.lock().unwrap().gemini_api_key.as_deref(),
            Some("LEGACY-PLAINTEXT-KEY"),
            "still usable for chat this session"
        );
    }

    #[tokio::test]
    async fn a_missing_or_corrupt_file_holds_nothing_so_no_key_is_written_into_it() {
        for contents in [None, Some("garbage, not json"), Some("[1, 2")] {
            let (_d, state) = state_with(hydrated());
            if let Some(text) = contents {
                std::fs::write(&state.path, text).unwrap();
            }
            let mut inc = redacted_for_renderer(&hydrated());
            inc.model = "tweak".into();
            let kc = FakeKeys::holding(&hydrated()).failing_writes();
            kc.fail_get.store(true, std::sync::atomic::Ordering::SeqCst);
            let mut audit = Vec::new();

            save(&state, inc, &Scripted::deny(), &kc, &mut audit)
                .await
                .unwrap();

            let on_disk = std::fs::read_to_string(&state.path).unwrap();
            assert!(on_disk.contains("tweak"), "{contents:?}");
            for s in super::tests::SENTINELS {
                assert!(
                    !on_disk.contains(s),
                    "{s} written ({contents:?}): {on_disk}"
                );
            }
        }
    }

    // ── F2: a keychain read error must never put a key in the file ──────

    #[tokio::test]
    async fn a_keychain_only_key_is_never_written_when_the_keychain_cannot_be_read() {
        // Hydrated from the keychain at startup: in memory, never in the file.
        let (_d, state) = state_with(hydrated());
        let mut inc = redacted_for_renderer(&hydrated());
        inc.model = "tweak".into();
        let dialog = Scripted::deny();
        for (label, kc) in [
            ("read error", {
                let k = FakeKeys::holding(&hydrated());
                k.fail_get.store(true, std::sync::atomic::Ordering::SeqCst);
                k
            }),
            ("read + write errors", {
                let k = FakeKeys::holding(&hydrated()).failing_writes();
                k.fail_get.store(true, std::sync::atomic::Ordering::SeqCst);
                k
            }),
        ] {
            let mut audit = Vec::new();
            save(&state, inc.clone(), &dialog, &kc, &mut audit)
                .await
                .unwrap_or_else(|e| panic!("{label}: {e}"));
            let on_disk = std::fs::read_to_string(&state.path).unwrap();
            assert!(on_disk.contains("tweak"), "{label}");
            for s in super::tests::SENTINELS {
                assert!(
                    !on_disk.contains(s),
                    "{s} reached settings.json ({label}): {on_disk}"
                );
            }
            // Memory (what chat uses this session) is untouched.
            let mem = state.settings.lock().unwrap();
            assert_eq!(
                mem.gemini_api_key.as_deref(),
                Some("GEMINI-SENTINEL"),
                "{label}"
            );
            assert_eq!(dialog.times_asked(), 0);
        }
    }

    #[test]
    fn the_always_approve_write_never_puts_a_keychain_only_key_in_the_file() {
        // `persist_always_approve` calls `persist_settings_file` (heal = false).
        // Same rule, through the same writer, with a keychain that cannot be read.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let mut live = hydrated();
        live.agent_autonomy.auto_approve_tools = vec!["roost_edit".into()];
        let kc = FakeKeys::holding(&live);
        kc.fail_get.store(true, std::sync::atomic::Ordering::SeqCst);

        persist_settings_file_with(&path, &live, &kc, false).unwrap();

        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert!(on_disk.contains("roost_edit"), "the pin itself is written");
        for s in super::tests::SENTINELS {
            assert!(!on_disk.contains(s), "{s}: {on_disk}");
        }
        assert!(
            kc.sets().is_empty(),
            "heal = false never writes the keychain"
        );
        // And it reads back as holding no keys.
        assert!(read_file_keys(&path).iter().all(Option::is_none));
    }

    #[tokio::test]
    async fn a_new_typed_key_with_a_failing_keychain_is_a_visible_error_and_writes_nothing() {
        let (_d, state) = state_with(Settings::default());
        let mut inc = Settings::default();
        inc.gemini_api_key = Some("TYPED-KEY".into());
        let dialog = Scripted::allow(); // a new key asks; the user says yes
        let (kc, mut audit) = (FakeKeys::default().failing_writes(), Vec::new());

        let err = save(&state, inc, &dialog, &kc, &mut audit)
            .await
            .err()
            .expect("the key cannot be stored, so the save must say so");

        assert!(err.contains("keyring set"), "{err}");
        assert!(!state.path.exists(), "nothing was written to disk");
        assert!(state.settings.lock().unwrap().gemini_api_key.is_none());
        assert!(kc.stored(secret_store::keys::GEMINI_API_KEY).is_none());
    }

    #[tokio::test]
    async fn once_a_working_keychain_has_the_key_a_later_outage_cannot_put_it_back_in_the_file() {
        let (_d, state, cur) = legacy_key_state();
        let mut inc = redacted_for_renderer(&cur);
        inc.model = "one".into();
        let mut audit = Vec::new();

        // Save 1: keychain works — the legacy key moves over and the file is stripped.
        let working = FakeKeys::default();
        save(&state, inc.clone(), &Scripted::deny(), &working, &mut audit)
            .await
            .unwrap();
        let after_one = std::fs::read_to_string(&state.path).unwrap();
        assert!(!after_one.contains("LEGACY-PLAINTEXT-KEY"), "{after_one}");

        // Save 2: the keychain is now unreadable AND unwritable. The key came
        // from the keychain, so the file must NOT get it back.
        inc.model = "two".into();
        let broken = FakeKeys::holding(&cur).failing_writes();
        broken
            .fail_get
            .store(true, std::sync::atomic::Ordering::SeqCst);
        save(&state, inc, &Scripted::deny(), &broken, &mut audit)
            .await
            .unwrap();
        let after_two = std::fs::read_to_string(&state.path).unwrap();
        assert!(after_two.contains("two"));
        assert!(!after_two.contains("LEGACY-PLAINTEXT-KEY"), "{after_two}");
        assert_eq!(
            state.settings.lock().unwrap().gemini_api_key.as_deref(),
            Some("LEGACY-PLAINTEXT-KEY"),
            "still usable for chat this session"
        );
    }

    #[tokio::test]
    async fn an_unrelated_save_keeps_a_key_whose_keychain_migration_failed() {
        let (_d, state, cur) = legacy_key_state();
        // The webview round-trips the redacted settings (key = None = unchanged)
        // with one tweak; the keychain still refuses writes.
        let mut inc = redacted_for_renderer(&cur);
        inc.model = "tweak".into();
        let dialog = Scripted::deny();
        let (kc, mut audit) = (FakeKeys::default().failing_writes(), Vec::new());

        save(&state, inc, &dialog, &kc, &mut audit).await.unwrap();

        assert_eq!(dialog.times_asked(), 0);
        let on_disk = std::fs::read_to_string(&state.path).unwrap();
        assert!(on_disk.contains("tweak"));
        assert!(
            on_disk.contains("LEGACY-PLAINTEXT-KEY"),
            "the only durable copy was dropped: {on_disk}"
        );
        let mem = state.settings.lock().unwrap();
        assert_eq!(mem.gemini_api_key.as_deref(), Some("LEGACY-PLAINTEXT-KEY"));
        // And it is recoverable through the same path the next launch uses.
        let reloaded: Settings = serde_json::from_str(&on_disk).unwrap();
        assert_eq!(
            reloaded.gemini_api_key.as_deref(),
            Some("LEGACY-PLAINTEXT-KEY")
        );
    }

    #[tokio::test]
    async fn a_later_save_retries_the_keychain_and_only_then_drops_the_file_copy() {
        let (_d, state, cur) = legacy_key_state();
        let mut inc = redacted_for_renderer(&cur);
        inc.model = "tweak".into();
        let dialog = Scripted::deny();
        // The keychain works this time.
        let (kc, mut audit) = (FakeKeys::default(), Vec::new());

        save(&state, inc, &dialog, &kc, &mut audit).await.unwrap();

        assert_eq!(
            kc.stored(secret_store::keys::GEMINI_API_KEY).as_deref(),
            Some("LEGACY-PLAINTEXT-KEY"),
            "the retry moved the key into the keychain"
        );
        let on_disk = std::fs::read_to_string(&state.path).unwrap();
        assert!(!on_disk.contains("LEGACY-PLAINTEXT-KEY"), "{on_disk}");
        assert_eq!(
            state.settings.lock().unwrap().gemini_api_key.as_deref(),
            Some("LEGACY-PLAINTEXT-KEY")
        );
    }

    #[tokio::test]
    async fn a_failed_keychain_write_for_a_new_key_refuses_the_save_and_keeps_the_old_state() {
        let (_d, state, cur) = legacy_key_state();
        let mut inc = redacted_for_renderer(&cur);
        inc.gemini_api_key = Some("BRAND-NEW-KEY".into());
        let dialog = Scripted::allow(); // replacing a key asks
        let (kc, mut audit) = (FakeKeys::default().failing_writes(), Vec::new());

        let err = save(&state, inc, &dialog, &kc, &mut audit)
            .await
            .err()
            .expect("the keychain write failed");
        assert!(err.contains("keyring set"), "{err}");
        let on_disk = std::fs::read_to_string(&state.path).unwrap();
        assert!(on_disk.contains("LEGACY-PLAINTEXT-KEY"), "{on_disk}");
        assert!(!on_disk.contains("BRAND-NEW-KEY"), "{on_disk}");
        assert_eq!(
            state.settings.lock().unwrap().gemini_api_key.as_deref(),
            Some("LEGACY-PLAINTEXT-KEY")
        );
    }

    #[tokio::test]
    async fn an_unreadable_keychain_never_costs_a_key_either() {
        let (_d, state, cur) = legacy_key_state();
        let mut inc = redacted_for_renderer(&cur);
        inc.model = "tweak".into();
        let kc = FakeKeys::holding(&cur);
        kc.fail_get.store(true, std::sync::atomic::Ordering::SeqCst); // locked
        let mut audit = Vec::new();

        save(&state, inc, &Scripted::deny(), &kc, &mut audit)
            .await
            .unwrap();

        assert!(std::fs::read_to_string(&state.path)
            .unwrap()
            .contains("LEGACY-PLAINTEXT-KEY"));
    }

    #[tokio::test]
    async fn an_invalid_ollama_url_is_rejected_before_anything_is_written() {
        let (_d, state) = state_with(Settings::default());
        let mut inc = Settings::default();
        inc.ollama_base_url = "http://localhost:11434@attacker.example".into();
        let dialog = Scripted::allow();
        let (kc, mut audit) = (FakeKeys::default(), Vec::new());

        let err = save(&state, inc, &dialog, &kc, &mut audit)
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
        let (kc, mut audit) = (FakeKeys::default(), Vec::new());

        let err = save(&state, inc, &dialog, &kc, &mut audit)
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
        let (kc, mut audit) = (keychain_for(&cur), Vec::new());

        save(&state, inc, &dialog, &kc, &mut audit).await.unwrap();

        // One dialog for the address change; the dropped token is not a second
        // "remove your key" prompt (the user did not ask for that).
        assert_eq!(audit, vec![(1, true)]);
        assert_eq!(
            kc.dels(),
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
        let (kc, mut audit) = (FakeKeys::default(), Vec::new());

        let err = save(&state, inc, &dialog, &kc, &mut audit)
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
