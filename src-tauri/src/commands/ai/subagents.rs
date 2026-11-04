//! Tauri-managed state + IPC commands for the subagent registry.
//! Same shape as `commands::ai::skills` — kept separate so each
//! registry's lifecycle (initial scan, watcher rebind, refresh
//! endpoint) can evolve on its own.

use std::path::Path;
use std::sync::{Arc, Mutex};

use serde::Serialize;
use tauri::State;

use crate::ai::subagents::{spawn_default_watcher, SubagentRegistry, SubagentWatcher};
use crate::commands::files::RoostFsState;

pub struct SubagentRegistryState(pub Arc<SubagentRegistry>);

pub struct SubagentWatcherState(pub Mutex<Option<SubagentWatcher>>);

pub async fn refresh_registry_from_backend(
    app: &tauri::AppHandle,
    backend: &dyn crate::roostfs::RoostFs,
) -> Result<u32, String> {
    use tauri::Manager;
    let Some(state) = app.try_state::<SubagentRegistryState>() else {
        return Ok(0);
    };
    let entries = match backend.list_recursive(".corvux/agents").await {
        Ok(entries) => entries,
        Err(crate::roostfs::RoostError::NotFound(_)) => Vec::new(),
        Err(e) => return Err(format!("scan roost subagents: {e}")),
    };
    let mut manifests = Vec::new();
    for entry in entries {
        if entry.is_dir || !entry.rel_path.ends_with("/AGENT.md") {
            continue;
        }
        match backend.read(&entry.rel_path).await {
            Ok(bytes) => match String::from_utf8(bytes) {
                Ok(text) => manifests.push((entry.rel_path, text)),
                Err(e) => {
                    tracing::warn!(path = %entry.rel_path, error = %e, "subagent: non-UTF8 manifest skipped")
                }
            },
            Err(e) => {
                tracing::warn!(path = %entry.rel_path, error = %e, "subagent: manifest read failed")
            }
        }
    }
    let global_dir = crate::ai::subagents::global_agents_dir();
    let outcome = state
        .0
        .refresh_with_roost_contents(manifests, global_dir.as_deref());
    Ok(outcome.loaded as u32)
}

#[derive(Serialize, specta::Type)]
pub struct SubagentListView {
    pub name: String,
    pub description: String,
    #[specta(type = String)]
    pub source: &'static str,
    pub tools: Option<Vec<String>>,
    pub model: Option<String>,
}

/// Settings UI endpoint — list every registered subagent (built-in,
/// global, roost-local) for the future "Subagents" pane.
#[tauri::command]
#[specta::specta]
pub fn subagent_list(state: State<'_, SubagentRegistryState>) -> Vec<SubagentListView> {
    let entries = state.0.entries();
    let mut out = Vec::with_capacity(entries.len());
    for (name, source) in entries {
        let def = match state.0.get(&name) {
            Some(e) => e.def,
            None => continue,
        };
        out.push(SubagentListView {
            name: def.name,
            description: def.description,
            source: match source {
                crate::ai::subagents::SubagentSource::Roost => "roost",
                crate::ai::subagents::SubagentSource::Global => "global",
                crate::ai::subagents::SubagentSource::Builtin => "builtin",
            },
            tools: def.tools,
            model: def.model,
        });
    }
    out
}

/// One-shot rescan of both subagent directories. Mirrors
/// `skill_refresh` — used by the settings UI button and by callers
/// that just opened a roost out-of-band.
#[tauri::command]
#[specta::specta]
pub async fn subagent_refresh(
    roost_path: Option<String>,
    app: tauri::AppHandle,
    roost_fs: State<'_, RoostFsState>,
) -> Result<u32, String> {
    let backend = roost_fs
        .current()
        .await
        .ok_or_else(|| "Open a roost first.".to_string())?;
    if let Some(requested) = roost_path.as_deref() {
        if !crate::commands::roost::same_roost_root(
            backend.display_root(),
            std::path::Path::new(requested),
        ) {
            return Err("Subagent request does not match the active roost".to_string());
        }
    }
    refresh_registry_from_backend(&app, backend.as_ref()).await
}

/// Re-attach the hot-reload watcher to a new roost root. Called from
/// `roost::open_roost` after the new roost is mounted.
pub fn rebind_watcher_for_roost(
    registry: Arc<SubagentRegistry>,
    watcher_state: &SubagentWatcherState,
    roost_root: Option<&Path>,
) {
    match spawn_default_watcher(registry, roost_root) {
        Ok(w) => {
            if let Ok(mut guard) = watcher_state.0.lock() {
                *guard = Some(w);
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "subagent: watcher rebind failed");
        }
    }
}
