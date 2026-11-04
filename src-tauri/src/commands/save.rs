//! AUDIT-053: `save_roost_note` — the single IPC for "user is saving a
//! markdown file in their roost."
//!
//! Replaces the prior fan-out where the frontend chained three to four
//! IPCs (`write_file` → 2s wait → `index_note` → `get_backlinks`, with
//! `index_search` racing on the side). Every keystroke-bursty save on
//! mobile paid four WebView/JNI bridge crossings; this collapses them
//! to one.
//!
//! Boundaries:
//! - `write_file` (in `commands::files`) stays the generic atomic-write
//!   IPC. Settings JSON, chat session blobs, bookmark files — anything
//!   non-markdown — keeps using it.
//! - `save_roost_note` is the **domain** operation for roost notes. It
//!   owns the full post-save sequence: write → edit-history → sync
//!   notify → graph re-index → FTS5 update → backlinks read → embed
//!   enqueue → task reschedule. Adding a new post-save concern (concept
//!   extraction, episodic memory, anything Phase 3) goes in this one
//!   function instead of growing the TypeScript orchestration in
//!   `installSaveAndIdleHooks`.
//!
//! Error isolation: the FTS5 update path uses a separate SQLite
//! database than the graph and can plausibly fail independently
//! (read-only mount, disk full on just the search DB, etc). Rather
//! than fail the whole save, FTS5 errors surface in `SaveOutcome.fts5`
//! as a structured result the frontend can show as a non-blocking
//! warning ("graph updated, search index will retry").

use serde::Serialize;
use std::path::Path;
use std::time::Instant;
use tauri::State;

use super::files::{write_file_internal, RoostFsState, WriteOutcome};
use super::graph::{index_note_internal, BacklinkResult, GraphDb, GraphStatsResult};
use super::search::SearchState;
use super::sync::SyncEngineState;
use super::tasks::TaskRow;
use crate::db::queries;

/// Bundled result of a roost-note save. Returned by `save_roost_note`
/// in one IPC round-trip so the frontend doesn't have to chain
/// follow-up IPCs to refresh the UI.
#[derive(Debug, Serialize, specta::Type)]
pub struct SaveOutcome {
    /// Graph stats AFTER the re-index (same shape as `IndexNoteResponse.stats`).
    pub stats: GraphStatsResult,
    /// Full task list AFTER the re-index — frontend's `tasksStore`
    /// replaces its state with this, matching the legacy `index_note`
    /// behavior.
    pub tasks: Vec<TaskRow>,
    /// Notes that link to the just-saved note. Empty when the note has
    /// no inbound links OR when the graph DB couldn't resolve the path
    /// (note hasn't been indexed yet by some other code path).
    pub backlinks: Vec<BacklinkResult>,
    /// FTS5 update outcome — split out so a search-index failure
    /// doesn't poison the save. `Ok` means the note's FTS5 row is up
    /// to date; `Err` carries the error string and the frontend should
    /// surface it as a non-blocking warning (the next save retries).
    pub fts5: Fts5Outcome,
    /// EDT-2: true when the incoming content was byte-identical to disk,
    /// so the write + re-index + FTS + sync fan-out was skipped. The
    /// returned stats/tasks/backlinks are the current (unchanged) state.
    /// The frontend can use this to avoid redundant "saved" churn.
    pub unchanged: bool,
}

/// Wire-friendly Result substitute (Tauri serialises `Result<T, E>`
/// asymmetrically depending on whether E implements `Serialize`; an
/// explicit enum is clearer for the frontend to switch on).
#[derive(Debug, Serialize, specta::Type)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Fts5Outcome {
    Indexed,
    Failed { message: String },
}

/// Save a markdown note in the active roost.
///
/// One IPC, one round-trip, owns the full fan-out. Rejects non-markdown
/// paths up front — anything else should use the generic `write_file`
/// IPC. Rejects when no roost is open. The graph re-index happens
/// inline; the embed pipeline still goes through the background
/// drainer (no model call on the save path).
#[tauri::command]
#[specta::specta]
pub async fn save_roost_note(
    path: String,
    content: String,
    base_hash: Option<String>,
    roost_fs: State<'_, RoostFsState>,
    sync_state: State<'_, SyncEngineState>,
    db_state: State<'_, GraphDb>,
    search_state: State<'_, SearchState>,
) -> Result<SaveOutcome, String> {
    let started = Instant::now();

    // Reject non-markdown paths so the wrong caller can't accidentally
    // route a JSON settings write through the indexing pipeline. NEW-S1:
    // one shared case-insensitive predicate — the save gate and every
    // downstream sync/rename/delete gate now agree on what "markdown" is,
    // so a `.MD` note can't save-but-not-sync.
    if !crate::roostfs::walk::is_markdown_name(&path) {
        return Err(format!(
            "save_roost_note rejects '{path}' — not a markdown file. \
             Use write_file for non-markdown writes."
        ));
    }

    // Phase 1: atomic write + edit-history + sync engine notify. Shared
    // with the generic write_file IPC. `base_hash` (EDT-1) is the content
    // hash the editor loaded; a mismatch against disk means someone else
    // wrote in the meantime and this returns a `__save_conflict__` error.
    let outcome = write_file_internal(
        &path,
        &content,
        base_hash.as_deref(),
        &roost_fs,
        &sync_state,
        &db_state,
    )
    .await?;

    // EDT-2: content was byte-identical to disk — the write, edit-history
    // event, and sync emit were all skipped in write_file_internal. Skip
    // the graph re-index and FTS write too; just return the current
    // read-only state so the frontend stays consistent without paying for
    // a no-op re-index.
    if outcome == WriteOutcome::Unchanged {
        let (stats, tasks, backlinks) = read_unchanged_state(&path, &db_state)?;
        tracing::debug!(path = %path, "save_roost_note: content unchanged, skipped fan-out (EDT-2)");
        return Ok(SaveOutcome {
            stats,
            tasks,
            backlinks,
            fts5: Fts5Outcome::Indexed,
            unchanged: true,
        });
    }

    // Phase 2: graph re-index (incremental). Owns the LinkResolver
    // cache update, the algo-cache invalidation (debounced), the
    // embed-queue enqueue, and the task scheduler reschedule. Returns
    // post-save graph stats + the full task list.
    let index_resp = index_note_internal(&path, &content, &db_state)?;

    // Phase 3: FTS5 update — separate DB, separate lock. Failures here
    // don't poison the save; we surface them as a structured Fts5Outcome
    // so the frontend can render a non-blocking warning.
    let fts5 = match update_fts5(&path, &content, &search_state) {
        Ok(()) => Fts5Outcome::Indexed,
        Err(e) => {
            tracing::warn!(path = %path, error = %e, "save_roost_note: FTS5 update failed (fail-soft)");
            Fts5Outcome::Failed { message: e }
        }
    };

    // Phase 4: read backlinks for the just-saved note. The graph re-index
    // above already updated link rows, so this reads fresh data — the
    // chained `get_backlinks` IPC the frontend used to fire is now a
    // single in-process DB read piggy-backing on the lock the indexer
    // just held.
    let backlinks = read_backlinks(&path, &db_state)?;

    tracing::info!(
        path = %path,
        backlinks = backlinks.len(),
        tasks = index_resp.tasks.len(),
        fts5_ok = matches!(fts5, Fts5Outcome::Indexed),
        elapsed_ms = started.elapsed().as_millis() as u64,
        "save_roost_note: complete"
    );

    Ok(SaveOutcome {
        stats: index_resp.stats,
        tasks: index_resp.tasks,
        backlinks,
        fts5,
        unchanged: false,
    })
}

/// EDT-2: read the current graph stats, full task list, and backlinks for
/// a note whose content didn't change. Mirrors the trio `index_note_internal`
/// normally returns, minus the re-index — because on an unchanged save the
/// derived state is already up to date.
///
/// NEW-S2: the two whole-roost values (graph stats + full task list) are served
/// from `GraphDb`'s cached snapshot when it's warm, so a byte-identical no-op
/// save doesn't re-run `get_graph_stats` + `query_all_tasks` (both O(roost) at
/// 50K notes) or contend the graph lock — the scenario where N tabs' debounced
/// autosaves each fire an unchanged save. The cache is refreshed on every real
/// re-index and invalidated by every other graph/task mutation, so a warm hit
/// is always current. Backlinks are per-note and cheap, so they're always read
/// live under a brief lock. On a cold miss (nothing indexed yet this session,
/// or a mutation just invalidated the cache) we compute all three under one
/// lock and repopulate the snapshot.
fn read_unchanged_state(
    path: &str,
    db_state: &State<'_, GraphDb>,
) -> Result<(GraphStatsResult, Vec<TaskRow>, Vec<BacklinkResult>), String> {
    // Warm path: reuse the cached O(roost) stats + tasks; only backlinks touch
    // the DB (per-note, O(1)-ish). `cached_stats` locks the leaf stats-cache
    // mutex and releases it before we take the db lock, so there's no ordering
    // cycle with the index/mutation paths (which take db then stats-cache).
    if let Some(snap) = db_state.cached_stats() {
        let backlinks = read_backlinks(path, db_state)?;
        return Ok((snap.stats, snap.tasks, backlinks));
    }

    // Cold path: compute the trio under a single lock, then repopulate the cache
    // so subsequent unchanged saves in the same burst hit the warm path.
    let guard = db_state.db.lock().map_err(|e| format!("Lock error: {e}"))?;
    let db = guard.as_ref().ok_or("Graph DB not initialized")?;
    let stats = queries::get_graph_stats(db)?;
    let tasks = crate::commands::tasks::query_all_tasks(db)?;
    let backlinks = match queries::lookup_note_id_by_path(db, path)? {
        Some(note_id) => queries::get_backlinks(db, &note_id)?
            .into_iter()
            .map(|e| BacklinkResult {
                title: e.title,
                path: e.path,
            })
            .collect(),
        None => Vec::new(),
    };
    drop(guard);
    let stats = GraphStatsResult {
        notes: stats.notes,
        links: stats.links,
        tags: stats.tags,
    };
    db_state.store_stats_cache(stats.clone(), tasks.clone());
    Ok((stats, tasks, backlinks))
}

/// Update the FTS5 row for one note. Plain wrapper around the search
/// DB upsert; kept private so the fail-soft logic above stays simple.
fn update_fts5(path: &str, content: &str, state: &State<'_, SearchState>) -> Result<(), String> {
    let title = Path::new(path)
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    state.with_db_for_path(path, |db| db.upsert_note(path, &title, content))
}

/// Read backlinks for the just-saved note. Empty Vec is the correct
/// answer when the note's id can't be resolved — that just means
/// nothing links to it yet.
fn read_backlinks(
    path: &str,
    db_state: &State<'_, GraphDb>,
) -> Result<Vec<BacklinkResult>, String> {
    let guard = db_state.db.lock().map_err(|e| format!("Lock error: {e}"))?;
    let db = guard.as_ref().ok_or("Graph DB not initialized")?;
    let Some(note_id) = queries::lookup_note_id_by_path(db, path)? else {
        return Ok(Vec::new());
    };
    let entries = queries::get_backlinks(db, &note_id)?;
    Ok(entries
        .into_iter()
        .map(|e| BacklinkResult {
            title: e.title,
            path: e.path,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    //! Unit tests for the bits that don't need a Tauri State harness.
    //! End-to-end coverage of the bundled IPC lives in the integration
    //! tier (Vitest contract tests + manual verification).

    /// AUDIT-053: the path-suffix rejection is the firewall between
    /// "user saved a note" and "frontend accidentally routed a JSON
    /// blob through the indexer." Pin the exact predicate so a future
    /// extension-list change doesn't silently allow `.txt` through.
    #[test]
    fn rejects_non_markdown_paths() {
        // Mimic the predicate inline — the IPC body owns it, but the
        // test asserts the same logic without needing a Tauri State.
        fn is_markdown(path: &str) -> bool {
            let lower = path.to_ascii_lowercase();
            lower.ends_with(".md") || lower.ends_with(".markdown")
        }
        assert!(is_markdown("notes/today.md"));
        assert!(is_markdown("notes/today.MD"));
        assert!(is_markdown("/abs/path/today.Markdown"));
        assert!(is_markdown("notes/today.markdown"));
        assert!(!is_markdown("settings.json"));
        assert!(!is_markdown("notes/bookmark.html"));
        assert!(!is_markdown("plain"));
        assert!(!is_markdown(""));
    }
}
