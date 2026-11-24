//! Hot-reload watcher for subagent directories. Mirrors
//! `crate::ai::skills::watcher` — same notify-debouncer-full setup,
//! same 300 ms window, same best-effort error logging.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use notify::RecursiveMode;
use notify_debouncer_full::{new_debouncer, DebounceEventResult, Debouncer, RecommendedCache};
use tracing::warn;

use super::registry::{self, SubagentRegistry};
use crate::ai::skills::roost_scan;

const DEBOUNCE: Duration = Duration::from_millis(300);

pub struct SubagentWatcher {
    _debouncer: Debouncer<notify::RecommendedWatcher, RecommendedCache>,
}

pub fn spawn_watcher(
    registry: Arc<SubagentRegistry>,
    roost_dir: Option<PathBuf>,
    global_dir: Option<PathBuf>,
) -> Result<SubagentWatcher, String> {
    let reg_for_cb = registry.clone();
    let roost_for_cb = roost_dir.clone();
    let global_for_cb = global_dir.clone();

    let mut debouncer = new_debouncer(DEBOUNCE, None, move |res: DebounceEventResult| match res {
        Ok(_events) => {
            let outcome = reg_for_cb
                .refresh(roost_for_cb.as_deref(), global_for_cb.as_deref());
            if !outcome.failed.is_empty() {
                for (path, err) in &outcome.failed {
                    warn!(agent_path = %path.display(), error = %err, "subagent: parse failure during hot-reload");
                }
            }
        }
        Err(errors) => {
            for e in errors {
                warn!(error = %e, "subagent: watcher error");
            }
        }
    })
    .map_err(|e| format!("notify debouncer failed: {e}"))?;

    if let Some(dir) = roost_dir.as_deref() {
        // AT-5: only attach to a folder that stays inside the roost.
        if roost_scan::definitions_dir_exists(dir, "agents") {
            debouncer
                .watch(dir, RecursiveMode::Recursive)
                .map_err(|e| format!("watch roost agents: {e}"))?;
        }
    }
    if let Some(dir) = global_dir.as_deref() {
        if dir.exists() {
            debouncer
                .watch(dir, RecursiveMode::Recursive)
                .map_err(|e| format!("watch global agents: {e}"))?;
        }
    }

    let outcome = registry.refresh(roost_dir.as_deref(), global_dir.as_deref());
    if !outcome.failed.is_empty() {
        for (path, err) in &outcome.failed {
            warn!(agent_path = %path.display(), error = %err, "subagent: parse failure on initial scan");
        }
    }

    Ok(SubagentWatcher {
        _debouncer: debouncer,
    })
}

pub fn spawn_default_watcher(
    registry: Arc<SubagentRegistry>,
    roost_root: Option<&std::path::Path>,
) -> Result<SubagentWatcher, String> {
    let roost_dir = roost_root.map(registry::roost_agents_dir);
    let global_dir = registry::global_agents_dir();
    if let Some(ref dir) = roost_dir {
        // Through the roost filesystem (AT-5): a raw `create_dir_all` followed a
        // `.corvux` link out of the roost. A read-only roost is fine.
        if let Err(e) = roost_scan::ensure_definitions_dir(dir, "agents") {
            tracing::debug!(error = %e, "subagent: not creating the roost agents folder");
        }
    }
    spawn_watcher(registry, roost_dir, global_dir)
}
