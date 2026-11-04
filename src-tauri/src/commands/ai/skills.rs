//! Tauri-managed state + commands for the skill registry.
//!
//! The registry itself lives in `ai::skills`; this module is the glue
//! that keeps it alive for the app's lifetime, refreshes it when the
//! active roost changes, and exposes a `skill_list` IPC command so the
//! settings UI can render the loaded skills without going through the
//! agent loop.

use std::path::Path;
use std::sync::{Arc, Mutex};

use serde::Serialize;
use tauri::State;

use crate::ai::skills::{spawn_default_watcher, SkillRegistry, SkillWatcher};
use crate::commands::files::RoostFsState;

async fn active_roost_backend(
    roost_fs: &RoostFsState,
    requested: Option<&str>,
) -> Result<Arc<dyn crate::roostfs::RoostFs>, String> {
    let backend = roost_fs
        .current()
        .await
        .ok_or_else(|| "Open a roost first.".to_string())?;
    let active = backend.display_root();
    if let Some(requested) = requested {
        if !crate::commands::roost::same_roost_root(active, std::path::Path::new(requested)) {
            return Err("Skill request does not match the active roost".to_string());
        }
    }
    Ok(backend)
}

async fn roost_skill_manifests(
    backend: &dyn crate::roostfs::RoostFs,
) -> Result<Vec<(String, String)>, String> {
    let entries = match backend.list_recursive(".corvux/skills").await {
        Ok(entries) => entries,
        Err(crate::roostfs::RoostError::NotFound(_)) => Vec::new(),
        Err(e) => return Err(format!("scan roost skills: {e}")),
    };
    let mut manifests = Vec::new();
    for entry in entries {
        if entry.is_dir || !entry.rel_path.ends_with("/SKILL.md") {
            continue;
        }
        match backend.read(&entry.rel_path).await {
            Ok(bytes) => match String::from_utf8(bytes) {
                Ok(text) => manifests.push((entry.rel_path, text)),
                Err(e) => {
                    tracing::warn!(path = %entry.rel_path, error = %e, "skill: non-UTF8 manifest skipped")
                }
            },
            Err(e) => {
                tracing::warn!(path = %entry.rel_path, error = %e, "skill: manifest read failed")
            }
        }
    }
    Ok(manifests)
}

pub async fn refresh_registry_from_backend(
    app: &tauri::AppHandle,
    backend: &dyn crate::roostfs::RoostFs,
) -> Result<u32, String> {
    use tauri::Manager;
    let Some(skills) = app.try_state::<SkillRegistryState>() else {
        return Ok(0);
    };
    let manifests = roost_skill_manifests(backend).await?;
    let global_dir = crate::ai::skills::global_skills_dir();
    let outcome = skills
        .0
        .refresh_with_roost_contents(manifests, global_dir.as_deref());
    Ok(outcome.loaded as u32)
}

/// Wrapper struct so we can attach this in `tauri::Builder::manage`
/// without colliding with other `Arc<SkillRegistry>` registrations. The
/// `.0` is the registry itself; `chat.rs` and tools clone this Arc.
pub struct SkillRegistryState(pub Arc<SkillRegistry>);

/// Holds the live watcher so it's not dropped at the end of setup.
/// The watcher uses a notify thread that terminates as soon as the
/// `Debouncer` drops; keeping it in Tauri state pins it for the app
/// lifetime. `Mutex` because the watcher gets rebuilt when the active
/// roost changes.
pub struct SkillWatcherState(pub Mutex<Option<SkillWatcher>>);

#[derive(Serialize, specta::Type)]
pub struct SkillListView {
    pub name: String,
    pub description: String,
    #[specta(type = String)]
    pub source: &'static str,
}

/// Settings UI / inspector endpoint — returns the same list the agent
/// would see via the `skill_list` tool, but without going through the
/// agent loop. Used by the AI panel to show "you have N skills".
#[tauri::command]
#[specta::specta]
pub fn skill_list(skills: State<'_, SkillRegistryState>) -> Vec<SkillListView> {
    let entries = skills.0.entries();
    let mut out = Vec::with_capacity(entries.len());
    for (name, source) in entries {
        let manifest = match skills.0.get(&name) {
            Some(e) => e.manifest,
            None => continue,
        };
        out.push(SkillListView {
            name: manifest.name,
            description: manifest.description,
            source: match source {
                crate::ai::skills::SkillSource::Roost => "roost",
                crate::ai::skills::SkillSource::Global => "global",
            },
        });
    }
    out
}

/// Force a one-shot refresh of the skill registry. The hot-reload
/// watcher already covers this on filesystem events; the command is a
/// belt-and-suspenders entry point for the settings UI's "refresh
/// skills" button and for callers that just opened a roost.
#[tauri::command]
#[specta::specta]
pub async fn skill_refresh(
    roost_path: Option<String>,
    app: tauri::AppHandle,
    roost_fs: State<'_, RoostFsState>,
) -> Result<u32, String> {
    let backend = active_roost_backend(roost_fs.inner(), roost_path.as_deref()).await?;
    refresh_registry_from_backend(&app, backend.as_ref()).await
}

/// Scaffold a new roost-local skill. Creates
/// `<roost>/.corvux/skills/<name>/SKILL.md` with a minimal template
/// (valid frontmatter + a placeholder body). Returns the absolute
/// path so the frontend can immediately open the file in a tab for
/// editing.
///
/// Fails when:
/// - `name` doesn't match the kebab-case rule enforced by
///   `is_valid_skill_name` (lowercase a-z, 0-9, `-`; 1-64 chars).
/// - A skill of that name already exists in the roost (refuse to
///   clobber — the user can edit the existing file directly).
/// - The roost path is not provided (we don't scaffold into the
///   global dir from Settings; that's a deliberate guard so the new
///   skill ships with the roost, not the user's machine).
#[tauri::command]
#[specta::specta]
pub async fn skill_create(
    roost_path: String,
    name: String,
    roost_fs: State<'_, RoostFsState>,
    skills: State<'_, SkillRegistryState>,
) -> Result<String, String> {
    let backend = active_roost_backend(roost_fs.inner(), Some(&roost_path)).await?;
    let trimmed = name.trim();
    if !crate::ai::skills::is_valid_skill_name(trimmed) {
        return Err(format!(
            "invalid name `{trimmed}` — use lowercase a-z, 0-9, and `-` only (1-64 chars)"
        ));
    }
    let skill_dir = format!(".corvux/skills/{trimmed}");
    if backend
        .exists(&skill_dir)
        .await
        .map_err(|e| e.to_string())?
    {
        return Err(format!("skill `{trimmed}` already exists"));
    }
    for dir in [".corvux", ".corvux/skills", skill_dir.as_str()] {
        match backend.mkdir(dir).await {
            Ok(()) | Err(crate::roostfs::RoostError::AlreadyExists(_)) => {}
            Err(e) => return Err(format!("create {dir}: {e}")),
        }
    }
    let skill_md = format!("{skill_dir}/SKILL.md");
    // Template: minimal valid frontmatter + a placeholder body. The
    // `description` uses the block-scalar form because the user almost
    // always wants to expand it to 2-3 lines (`|` survives newlines).
    let body = format!(
        "---\n\
         name: {trimmed}\n\
         description: |\n  \
           Describe when the agent should use this skill. Be specific\n  \
           — the agent reads this line to decide whether to call\n  \
           skill_load. Multiple sentences are fine.\n\
         ---\n\
         \n\
         # {trimmed}\n\
         \n\
         Write the instructions for the agent here. The full body of\n\
         this file is returned by `skill_load` (with the frontmatter\n\
         stripped). Treat it as the agent's playbook for this task.\n"
    );
    backend
        .create(&skill_md, body.as_bytes())
        .await
        .map_err(|e| format!("write SKILL.md failed: {e}"))?;
    let manifests = roost_skill_manifests(backend.as_ref()).await?;
    skills
        .0
        .refresh_with_roost_contents(manifests, crate::ai::skills::global_skills_dir().as_deref());
    Ok(format!(
        "{}/{}",
        backend.display_root().trim_end_matches('/'),
        skill_md
    ))
}

/// Called from roost-open / roost-change hooks to rebuild the watcher
/// over the new roost's skills dir. Safe to call repeatedly — the
/// previous watcher drops cleanly when the `Mutex` is overwritten.
pub fn rebind_watcher_for_roost(
    registry: Arc<SkillRegistry>,
    watcher_state: &SkillWatcherState,
    roost_root: Option<&Path>,
) {
    match spawn_default_watcher(registry, roost_root) {
        Ok(w) => {
            if let Ok(mut guard) = watcher_state.0.lock() {
                *guard = Some(w);
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "skill: watcher rebind failed");
        }
    }
}
