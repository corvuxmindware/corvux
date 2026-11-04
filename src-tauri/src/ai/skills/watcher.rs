//! Hot-reload watcher for skill directories. Wraps `notify-debouncer-full`
//! so editor save storms (vim's atomic write, VS Code's tmp + rename)
//! coalesce into one refresh per debounce window.
//!
//! Tests live next to [`super::registry`] — this module is mostly the
//! thread-spawning glue and is exercised via the integration path in
//! [`crate::commands::ai::skills`].

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use notify::RecursiveMode;
use notify_debouncer_full::{new_debouncer, DebounceEventResult, Debouncer, RecommendedCache};
use tracing::warn;

use super::registry::{self, SkillRegistry};

/// Debounce window — long enough to coalesce a save burst, short enough
/// the agent picks up a new skill mid-session if the user is editing
/// SKILL.md in the editor next to the chat.
const DEBOUNCE: Duration = Duration::from_millis(300);

/// Handle to the running watcher. Drop terminates the watch thread.
/// Kept alive in Tauri's managed state so the watcher lives for the
/// lifetime of the open roost.
pub struct SkillWatcher {
    _debouncer: Debouncer<notify::RecommendedWatcher, RecommendedCache>,
}

/// Spawn a watcher that refreshes `registry` whenever a SKILL.md
/// changes under either provided directory. Refresh is run on the
/// caller's thread (the debouncer's callback), not async — refresh is
/// fast (single fs read pass), and avoiding `tokio::spawn` keeps the
/// watcher useful from sync test contexts.
pub fn spawn_watcher(
    registry: Arc<SkillRegistry>,
    roost_skills_dir: Option<PathBuf>,
    global_skills_dir: Option<PathBuf>,
) -> Result<SkillWatcher, String> {
    let reg_for_cb = registry.clone();
    let roost_for_cb = roost_skills_dir.clone();
    let global_for_cb = global_skills_dir.clone();

    let mut debouncer = new_debouncer(DEBOUNCE, None, move |res: DebounceEventResult| match res {
        Ok(_events) => {
            // We don't filter event kinds; any change under the watched
            // dirs warrants a full rescan. Refresh is cheap.
            let outcome = reg_for_cb.refresh(
                roost_for_cb.as_deref(),
                global_for_cb.as_deref(),
            );
            if !outcome.failed.is_empty() {
                for (path, err) in &outcome.failed {
                    warn!(skill_path = %path.display(), error = %err, "skill: parse failure during hot-reload");
                }
            }
        }
        Err(errors) => {
            for e in errors {
                warn!(error = %e, "skill: watcher error");
            }
        }
    })
    .map_err(|e| format!("notify debouncer failed: {e}"))?;

    if let Some(dir) = roost_skills_dir.as_deref() {
        if dir.exists() {
            debouncer
                .watch(dir, RecursiveMode::Recursive)
                .map_err(|e| format!("watch roost skills: {e}"))?;
        }
    }
    if let Some(dir) = global_skills_dir.as_deref() {
        if dir.exists() {
            debouncer
                .watch(dir, RecursiveMode::Recursive)
                .map_err(|e| format!("watch global skills: {e}"))?;
        }
    }

    // One initial refresh so callers don't need to call refresh()
    // separately at startup.
    let outcome = registry.refresh(roost_skills_dir.as_deref(), global_skills_dir.as_deref());
    if !outcome.failed.is_empty() {
        for (path, err) in &outcome.failed {
            warn!(skill_path = %path.display(), error = %err, "skill: parse failure on initial scan");
        }
    }

    Ok(SkillWatcher {
        _debouncer: debouncer,
    })
}

/// Convenience wrapper: derive the standard dirs (`<roost>/.corvux/skills`,
/// `~/.corvux/skills`) and spawn. Used by the Tauri setup hook so the
/// caller doesn't need to know the directory convention.
pub fn spawn_default_watcher(
    registry: Arc<SkillRegistry>,
    roost_root: Option<&std::path::Path>,
) -> Result<SkillWatcher, String> {
    let roost_dir = roost_root.map(registry::roost_skills_dir);
    let global_dir = registry::global_skills_dir();
    if let Some(ref dir) = roost_dir {
        // Best-effort create so the watcher has something to attach to;
        // ignore EEXIST and permission errors silently.
        let _ = std::fs::create_dir_all(dir);
    }
    spawn_watcher(registry, roost_dir, global_dir)
}
