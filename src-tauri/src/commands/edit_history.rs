//! Edit-history IPC surface (Phase 2.2 — the trust gate).
//!
//! Two commands:
//!   - `list_edit_events(note_path, limit?)` — fills the History pane.
//!   - `revert_edit(note_path, event_id, force?)` — restores `before_content`
//!     from the recorded event, with conflict detection unless `force` is set.
//!
//! The actual write paths (`write_file`, `complete_task`, ...) record
//! events via `db::queries::record_edit_event` directly, not through this
//! module. This file is purely the read+revert surface the frontend talks
//! to.

use serde::Serialize;
use tauri::State;

use crate::commands::files::{to_rel, RoostFsState};
use crate::commands::graph::GraphDb;
use crate::db::queries;

/// Frontend wire shape. Matches `db::queries::edit_events::EditEvent`
/// 1:1 today; carved out so we can evolve the IPC surface (e.g. drop
/// `before_content` from the list payload to save bandwidth) without
/// touching the storage type.
#[derive(Debug, Serialize, specta::Type)]
pub struct EditEventDto {
    pub note_id: String,
    pub event_id: String,
    pub actor_type: String,
    pub actor_id: String,
    pub intent: String,
    pub timestamp: f64,
    pub diff: String,
    #[specta(type = i32)]
    pub lines_added: i64,
    #[specta(type = i32)]
    pub lines_removed: i64,
    pub reverted_by: String,
    pub note_path: String,
    /// Hard block: this event has ALREADY been reverted (`reverted_by` set).
    /// Reverting it again is meaningless, so the pane grays out the Revert
    /// button. Distinct from `needs_force` below.
    pub revert_conflicts: bool,
    /// EH-1: soft warning — reverting this event WOULD discard work, so
    /// `revert_edit` will reject it unless `force=true`. True when a later
    /// unreverted edit exists (reverting rolls past it) OR the file changed
    /// outside Corvux's history since the last recorded event (the v4
    /// `after_hash` diverges from disk). The pane keeps Revert enabled but
    /// routes it through the confirm dialog and can show a "will discard
    /// newer changes" hint. This is the deeper check that previously lived
    /// only inside `revert_edit`; surfacing it at list time is the UI
    /// promise the pane couldn't keep before. Most non-latest rows are
    /// `needs_force=true` by nature — that's why it must NOT gray out.
    pub needs_force: bool,
}

/// Resolve the `note_id` for an absolute path the frontend handed us.
/// Mirrors the inline pattern in `tasks.rs` — could be promoted to a
/// shared helper when more commands need it.
fn resolve_note_id(db_state: &State<'_, GraphDb>, note_path: &str) -> Result<String, String> {
    let roost_guard = db_state
        .roost_path
        .lock()
        .map_err(|e| format!("Lock error: {e}"))?;
    let roost_root = roost_guard.as_deref().ok_or("Roost path not set")?;
    Ok(queries::note_id_from_abs_path(note_path, roost_root))
}

/// Read whatever's currently on disk for `note_path`. Used by `revert_edit`
/// to detect conflicts before clobbering the file.
async fn read_current(
    fs_state: &State<'_, RoostFsState>,
    note_path: &str,
) -> Result<String, String> {
    let backend = fs_state
        .current()
        .await
        .ok_or_else(|| "No roost is open".to_string())?;
    let rel = to_rel(note_path, backend.display_root())
        .ok_or_else(|| format!("Access denied: '{note_path}' is outside the active roost"))?;
    let bytes = backend
        .read(&rel)
        .await
        .map_err(|e| format!("Failed to read {note_path}: {e}"))?;
    String::from_utf8(bytes).map_err(|e| format!("File not UTF-8 ({note_path}): {e}"))
}

/// `limit = 0` is treated as "no cap" by the underlying query. We pin
/// the default at 100 here to keep round-trip size reasonable on Android
/// where each `:edit_event` row carries the full `before_content`.
const DEFAULT_LIST_LIMIT: usize = 100;

/// An event "conflicts" (can't be cleanly reverted from the History
/// pane) once it has already been reverted — a non-empty `reverted_by`
/// is the v1 signal. Pulled out so the flag the pane grays the Revert
/// button on has one definition and a regression test.
fn revert_conflicts(ev: &queries::EditEvent) -> bool {
    !ev.reverted_by.is_empty()
}

/// True when some event in `all` is chronologically newer than
/// `target_id` AND has not itself been reverted. This is the heuristic
/// that forces `force=true` on a revert that would clobber a later edit.
///
/// Correctness rests on `event_id` being lexicographically ordered by
/// time (the id format is a fixed-width zero-padded millisecond stamp +
/// random suffix — see `db::queries::edit_events::make_event_id`), so a
/// plain string `>` comparison stands in for "happened after". Extracted
/// from the inline `revert_edit` closure so that assumption has a test.
fn has_later_unreverted(all: &[queries::EditEvent], target_id: &str) -> bool {
    all.iter()
        .any(|e| e.event_id.as_str() > target_id && e.reverted_by.is_empty())
}

/// EH-1: would reverting `ev` require `force=true`? Mirrors the exact
/// `!force` rejection conditions in `revert_edit` so the list-time flag
/// can't lie about what the revert command will actually do. An
/// already-reverted event is not a force case (it's a hard block, surfaced
/// via `revert_conflicts`), and a no-op revert — disk already equals
/// `before_content` — needs no force either (the command returns "nothing to
/// revert", not a clobber). Otherwise force is needed when a later
/// unreverted edit exists OR the file diverged from the recorded
/// `after_hash` (an external edit). Pure fn so the mapping has a regression
/// test without a DB/FS harness.
fn needs_force_to_revert(
    ev: &queries::EditEvent,
    all: &[queries::EditEvent],
    current_disk: &str,
    latest_after_hash: Option<&str>,
) -> bool {
    if !ev.reverted_by.is_empty() {
        return false; // hard-blocked path, surfaced via revert_conflicts
    }
    if current_disk == ev.before_content {
        return false; // no-op revert — revert_edit returns "nothing to revert"
    }
    if has_later_unreverted(all, &ev.event_id) {
        return true;
    }
    // External-edit divergence: the latest recorded after_hash no longer
    // matches disk. Only the tip event can be the one whose after_hash is
    // "latest"; but revert_edit applies this check to any non-later revert,
    // so mirror it identically here.
    match latest_after_hash {
        Some(expected) => expected != queries::content_hash(current_disk),
        None => false,
    }
}

/// Fetch the edit-event log for one note, newest-first.
///
/// `revert_conflicts` is computed in this function rather than in the
/// query layer because it depends on what's currently on disk. An event
/// is "in conflict" when current disk content differs from the
/// post-event content captured in subsequent events — i.e. someone else
/// has edited the file since. We approximate this by comparing the
/// current content against the `before_content` of the next event in
/// chronological order. If they match, the event chain is clean from
/// the named event forward and a clean revert is possible.
#[tauri::command]
#[specta::specta]
pub async fn list_edit_events(
    note_path: String,
    limit: Option<u32>,
    db_state: State<'_, GraphDb>,
    fs_state: State<'_, RoostFsState>,
) -> Result<Vec<EditEventDto>, String> {
    let note_id = resolve_note_id(&db_state, &note_path)?;
    let cap = limit.map(|n| n as usize).unwrap_or(DEFAULT_LIST_LIMIT);

    let events = {
        let guard = db_state.db.lock().map_err(|e| format!("Lock error: {e}"))?;
        let db = guard.as_ref().ok_or("Graph DB not initialized")?;
        queries::list_edit_events(db, &note_id, cap)?
    };

    // Compute the two revert flags. Cheap — read the current file once and
    // look up the latest recorded after_hash once, then a linear scan.
    let current = read_current(&fs_state, &note_path)
        .await
        .unwrap_or_default();
    // EH-1: the v4 divergence signal — the after_hash of the newest recorded
    // event. If it no longer matches disk, the file was edited outside
    // Corvux's history, and any revert needs force. Best-effort: on error we
    // treat it as "no divergence info" (matches revert_edit's fallback).
    let latest_after_hash = {
        let guard = db_state.db.lock().map_err(|e| format!("Lock error: {e}"))?;
        let db = guard.as_ref().ok_or("Graph DB not initialized")?;
        queries::latest_after_hash(db, &note_id).unwrap_or(None)
    };

    // Build DTOs. `revert_conflicts` = already-reverted (hard block, grays
    // out Revert). `needs_force` = reverting would discard work (soft — pane
    // routes through the confirm dialog). Keeping them separate is the whole
    // point of EH-1: folding the would-discard check into revert_conflicts
    // would gray out Revert on nearly every non-latest row.
    let mut dtos: Vec<EditEventDto> = Vec::with_capacity(events.len());
    for ev in events.iter() {
        dtos.push(EditEventDto {
            note_id: ev.note_id.clone(),
            event_id: ev.event_id.clone(),
            actor_type: ev.actor_type.clone(),
            actor_id: ev.actor_id.clone(),
            intent: ev.intent.clone(),
            timestamp: ev.timestamp,
            diff: ev.diff.clone(),
            lines_added: ev.lines_added,
            lines_removed: ev.lines_removed,
            reverted_by: ev.reverted_by.clone(),
            note_path: ev.note_path.clone(),
            revert_conflicts: revert_conflicts(ev),
            needs_force: needs_force_to_revert(ev, &events, &current, latest_after_hash.as_deref()),
        });
    }
    Ok(dtos)
}

/// Restore the `before_content` recorded for `event_id`, then record a
/// new revert event linked back to the original.
///
/// Conflict policy:
///   - If the live file content differs from the post-event content
///     (i.e. another edit happened on the same lines after this event),
///     fail unless `force = true`. The frontend should surface a
///     confirmation dialog when the server returns this error.
///   - `force = true` blindly writes `before_content` over whatever's
///     on disk. Subsequent edits ARE lost — the user opted in.
///
/// Returns the new file content so the frontend can update the open
/// editor tab without an extra read.
#[tauri::command]
#[specta::specta]
pub async fn revert_edit(
    note_path: String,
    event_id: String,
    force: Option<bool>,
    db_state: State<'_, GraphDb>,
    fs_state: State<'_, RoostFsState>,
    sync_state: State<'_, super::sync::SyncEngineState>,
) -> Result<String, String> {
    let force = force.unwrap_or(false);
    let note_id = resolve_note_id(&db_state, &note_path)?;

    let generation = fs_state.generation();
    let backend = fs_state
        .current()
        .await
        .ok_or_else(|| "No roost is open".to_string())?;
    let rel = to_rel(&note_path, backend.display_root())
        .ok_or_else(|| format!("Access denied: '{note_path}' is outside the active roost"))?;
    let _mutation_guard = fs_state.lock_note(&rel).await;
    if !fs_state.is_generation_current(generation) {
        return Err("Roost changed before the revert could commit".to_string());
    }

    let event = {
        let guard = db_state.db.lock().map_err(|e| format!("Lock error: {e}"))?;
        let db = guard.as_ref().ok_or("Graph DB not initialized")?;
        queries::get_edit_event(db, &note_id, &event_id)?
            .ok_or_else(|| format!("Edit event {event_id} not found for note {note_id}"))?
    };

    if !event.reverted_by.is_empty() {
        return Err(format!(
            "Event {event_id} has already been reverted by {}",
            event.reverted_by
        ));
    }

    // External edits (MCP / Obsidian / git / sync) are visibility-only: their
    // `before_content` is reconstructed from the last-indexed state, not a
    // genuine pre-write read, so it is NOT revert-grade. The History pane hides
    // the Revert button for these; refuse here too as defence-in-depth in case
    // a caller reaches the IPC directly.
    if event.actor_type == queries::Actor::External.as_str() {
        return Err(
            "This change was made outside the editor (MCP, another app, or sync). \
             It's shown in history for visibility only and can't be reverted from here — \
             edit the note directly to change it."
                .to_string(),
        );
    }

    // Read current content for conflict detection + as the `before` of
    // the new revert event.
    let current = read_current(&fs_state, &note_path).await?;

    // Conflict detection: if the recorded `before_content` already
    // matches what's on disk, reverting is a no-op (someone already
    // got us back to that state). If neither equals the recorded
    // after-content (which we don't store directly, so we approximate:
    // if current != before AND the diff isn't applicable cleanly),
    // require `force`.
    if current == event.before_content {
        return Err(
            "Nothing to revert — the file is already at the state this event would restore."
                .to_string(),
        );
    }

    if !force {
        // Cheap heuristic: any event newer than the target on the same
        // note that isn't yet a revert means an unrelated edit happened
        // after. The frontend can still re-issue with `force = true`.
        let later_events = {
            let guard = db_state.db.lock().map_err(|e| format!("Lock error: {e}"))?;
            let db = guard.as_ref().ok_or("Graph DB not initialized")?;
            queries::list_edit_events(db, &note_id, 0)?
        };
        if has_later_unreverted(&later_events, &event_id) {
            return Err(
                "Subsequent edits would be overwritten by this revert. Pass force=true \
                 to discard them and restore the recorded state anyway."
                    .to_string(),
            );
        }

        // Divergence check (v4 sidecar): if the latest recorded event has
        // a stored `after_hash` and it does NOT match the current file's
        // hash, something edited the file outside Corvux's history (e.g.
        // user opened the .md in another editor). The later-events
        // heuristic above can't catch that — no event row exists for an
        // external write. Require force so the user explicitly opts in
        // to discarding those external changes.
        let recorded_after_hash = {
            let guard = db_state.db.lock().map_err(|e| format!("Lock error: {e}"))?;
            let db = guard.as_ref().ok_or("Graph DB not initialized")?;
            queries::latest_after_hash(db, &note_id)?
        };
        if let Some(expected) = recorded_after_hash {
            let actual = queries::content_hash(&current);
            if expected != actual {
                return Err(
                    "This note has changed outside Corvux's edit history since the last \
                     recorded event. Reverting now would overwrite those external changes. \
                     Pass force=true to discard them and restore the recorded state anyway."
                        .to_string(),
                );
            }
        }
    }

    // Perform the revert: write before_content back atomically.
    backend
        .write_atomic(&rel, event.before_content.as_bytes())
        .await
        .map_err(|e| format!("Failed to write {note_path}: {e}"))?;

    // If this revert is undoing a delete, the sync doc is still tombstoned
    // (deleted=true). Writing the file back isn't enough — without clearing
    // the CRDT flag, the next sync reconcile would re-trash it. `restore_note`
    // self-guards: it's a cheap no-op when the note wasn't deleted, so we can
    // call it after any revert. The cleared flag then propagates with the
    // restored content via the watcher (live peers) + queue (offline peers).
    if let Some(engine) = sync_state.engine.lock().await.as_ref() {
        engine
            .restore_note(crate::sync::protocol::NotePath(rel.replace('\\', "/")))
            .await;
    }

    // Propagate the reverted *content* itself. `restore_note` only clears the
    // delete tombstone and leans on the file watcher to carry the bytes — but
    // the watcher is dead on Android SAF, so a revert done on mobile would
    // never reach the laptop. Route through the same path editor saves use.
    crate::commands::files::notify_sync_local_write(
        sync_state.inner(),
        &rel,
        &event.before_content,
    )
    .await;

    // Record the revert as a new event, then chain the original to it.
    let revert_id = {
        let guard = db_state.db.lock().map_err(|e| format!("Lock error: {e}"))?;
        let db = guard.as_ref().ok_or("Graph DB not initialized")?;
        let revert_id = queries::record_edit_event(
            db,
            &note_id,
            &note_path,
            queries::Actor::User,
            "user",
            queries::Intent::Revert, // EH-5: distinct from a plain edit
            &current,
            &event.before_content,
        )?;
        queries::mark_reverted(db, &note_id, &event_id, &revert_id)?;
        revert_id
    };
    let _ = revert_id;

    // Re-run the per-note indexer so backlinks/tags/tasks reflect the restored
    // content. IP-8: full index, not pass1 alone — pass1 rebuilds notes+tags
    // but NOT links, so reverting to content with `[[links]]` used to leave
    // those edges missing (the comment's "backlinks reflect the restored
    // content" was aspirational). Single-note scope.
    {
        let guard = db_state.db.lock().map_err(|e| format!("Lock error: {e}"))?;
        let db = guard.as_ref().ok_or("Graph DB not initialized")?;
        let roost_guard = db_state
            .roost_path
            .lock()
            .map_err(|e| format!("Lock error: {e}"))?;
        let roost_root = roost_guard.as_deref().ok_or("Roost path not set")?;
        // IP-A3: reuse the cached resolver instead of an O(roost) rebuild. A
        // revert only ever restores an EXISTING note's content (never removes
        // the note), so add_id-maintenance keeps the cache correct.
        db_state.index_note_full_cached(db, roost_root, &note_path, &event.before_content)?;
    }
    // Mark algo stale — restored content may have changed the link graph. Do
    // NOT invalidate the resolver: index_note_full_cached kept it in sync, and
    // invalidating would force the next save to rebuild O(roost) (IP-A3).
    db_state.mark_algo_cache_dirty();

    Ok(event.before_content)
}

// ── External-write capture (MCP / Obsidian / git / sync) ─────────────────
//
// Edit history is otherwise recorded only inside the app's own write fan-out
// (`write_file_internal`/`save_roost_note` + the in-app AI tools). Writes that
// reach the roost from OUTSIDE that path — the MCP sidecar, another editor, an
// agent shell, a peer-sync write — arrive via the file watcher's
// `note_written`/`note_deleted` events and were previously invisible to the
// History pane. These commands close that gap. They are recorded as
// `Actor::External` and are **visibility-only** (no revert; see the
// `revert_edit` guard and the History pane): the `before` snapshot is the
// last-indexed content from the search DB, not a genuine pre-write read, so it
// is not revert-grade.
//
// Only reached via the frontend `note_written`/`note_deleted` handlers, which
// fire for LIVE watcher events after the roost is open — never for the initial
// content walk (that indexes directly, off this path). So bulk/cold indexing
// does not flood history, and no explicit warm-vs-cold gate is needed here.

/// Capture an external WRITE. `note_path` is the display path (the form the
/// FTS index + graph node ids are keyed under); `content` is what's now on
/// disk (the caller already read it). Returns true when an event was recorded.
///
/// MUST be called BEFORE the caller reindexes, so the search DB still holds
/// the pre-write content (our `before`). The `after_hash` dedup makes this a
/// no-op for the app's own save echo (whose event + FTS row `save_roost_note`
/// already wrote) and for any duplicate re-emit.
///
/// CONCURRENCY: this reads the search DB, then the graph DB, then writes the
/// graph DB, then re-`upsert`s the search DB — four SEPARATE critical sections,
/// not one. A concurrent `save_roost_note` or a second external write on the
/// same note can interleave, so the recorded `before` is best-effort, not a
/// guaranteed atomic pre-write snapshot. That is ACCEPTED because these events
/// are visibility-only (non-revertible) and coalescing keys on actor, so a
/// slightly-off diff is the worst case — never data loss. If External ever
/// becomes revert-grade, collapse this into a single critical section first.
#[tauri::command]
#[specta::specta]
pub async fn record_external_edit(
    note_path: String,
    content: String,
    db_state: State<'_, GraphDb>,
    search_state: State<'_, crate::commands::search::SearchState>,
) -> Result<bool, String> {
    // Only markdown notes carry history (mirrors `save_roost_note`'s gate).
    if !crate::roostfs::walk::is_markdown_name(&note_path) {
        return Ok(false);
    }
    let note_id = resolve_note_id(&db_state, &note_path)?;

    // 1. The "before" = last-indexed content. If the search DB isn't up yet we
    //    can't get a trustworthy prior state — skip rather than record a bogus
    //    whole-file "create".
    let Some(before) = search_state.with_optional_db_for_path(&note_path, |db| {
        Ok(db.content_for(&note_path)?.unwrap_or_default())
    })?
    else {
        return Ok(false);
    };

    // Genuine no-op — nothing changed since we last indexed.
    if before == content {
        return Ok(false);
    }

    // 2. Dedup: if the new content already matches the last recorded post-state
    //    hash, this is our own save echo (the save path recorded it) — skip.
    let after_hash = queries::content_hash(&content);
    {
        let guard = db_state.db.lock().map_err(|e| format!("Lock error: {e}"))?;
        let db = guard.as_ref().ok_or("Graph DB not initialized")?;
        if let Ok(Some(prev)) = queries::latest_after_hash(db, &note_id) {
            if prev == after_hash {
                return Ok(false);
            }
        }
    }

    // 3. Record. Create intent when there was no prior indexed content (a note
    //    that first appeared outside the app).
    let intent = if before.is_empty() {
        queries::Intent::Create
    } else {
        queries::Intent::Edit
    };
    {
        let guard = db_state.db.lock().map_err(|e| format!("Lock error: {e}"))?;
        let db = guard.as_ref().ok_or("Graph DB not initialized")?;
        queries::record_edit_event(
            db,
            &note_id,
            &note_path,
            queries::Actor::External,
            "external",
            intent,
            &before,
            &content,
        )?;
    }

    // 4. Advance the search baseline to the new content so the NEXT external
    //    edit diffs against THIS state (and search stays fresh for external
    //    writes, which otherwise only reconcile lazily). Canonical upsert
    //    (content + note_meta mtime). Best-effort — a missed baseline advance
    //    only means the next external edit's diff spans two writes.
    {
        let title = std::path::Path::new(&note_path)
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        if let Err(e) = search_state.with_db_for_path(&note_path, |db| {
            db.upsert_note(&note_path, &title, &content)
        }) {
            tracing::warn!(%note_path, error = %e, "record_external_edit: FTS baseline advance failed");
        }
    }

    Ok(true)
}

/// Capture an external DELETE. `note_path` is the display path. Reads the
/// last-indexed content as the `before`, records a delete event, and returns
/// true when one was recorded. MUST be called BEFORE the caller removes the
/// note from the search index (otherwise the `before` is already gone).
///
/// Only invoked for genuine external deletes — the frontend's `note_deleted`
/// handler bails on self-write echoes before reaching this.
#[tauri::command]
#[specta::specta]
pub async fn record_external_delete(
    note_path: String,
    db_state: State<'_, GraphDb>,
    search_state: State<'_, crate::commands::search::SearchState>,
) -> Result<bool, String> {
    if !crate::roostfs::walk::is_markdown_name(&note_path) {
        return Ok(false);
    }
    let note_id = resolve_note_id(&db_state, &note_path)?;

    let Some(before) = search_state.with_optional_db_for_path(&note_path, |db| {
        Ok(db.content_for(&note_path)?.unwrap_or_default())
    })?
    else {
        return Ok(false);
    };
    // Nothing was indexed for this note → nothing meaningful to record.
    if before.is_empty() {
        return Ok(false);
    }

    let guard = db_state.db.lock().map_err(|e| format!("Lock error: {e}"))?;
    let db = guard.as_ref().ok_or("Graph DB not initialized")?;
    // Dedup, mirroring the edit twin's after_hash check: the app's own delete
    // (`delete_file`) records a User delete event whose after_hash is the hash
    // of empty content. If that's already the latest recorded state, this
    // note_deleted echo is our own delete — skip, so it isn't double-recorded
    // as External. This is ordering-independent, unlike leaning on the FTS row
    // being purged before the watcher fires (which suppresses the common case
    // via the `before.is_empty()` guard above but is fragile to reordering).
    if let Ok(Some(prev)) = queries::latest_after_hash(db, &note_id) {
        if prev == queries::content_hash("") {
            return Ok(false);
        }
    }
    queries::record_edit_event(
        db,
        &note_id,
        &note_path,
        queries::Actor::External,
        "external",
        queries::Intent::Delete,
        &before,
        "",
    )?;
    Ok(true)
}

// ── EH-3: purge + export of edit history ─────────────────────────────────
//
// Privacy/GDPR surface. Edit history is device-local (see the module docs in
// `db::queries::edit_events`), so purge is a real erase — no other device
// holds a copy to re-sync it back. Export dumps the audit trail to a JSON
// string the frontend copies to the clipboard / writes to a file.

/// Current wall-clock in whole seconds since the epoch. Best-effort — a
/// clock before 1970 (impossible in practice) coalesces to 0.
fn now_unix_seconds() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Build the JSON export blob. Pure (no DB/FS) so its shape is unit-tested
/// without a Tauri harness. `scope` is the note path for a single-note
/// export, or `None` for the whole roost.
fn build_export_json(
    roost_path: &str,
    exported_at: f64,
    scope: Option<&str>,
    events: &[queries::EditEvent],
) -> Result<String, String> {
    let export = serde_json::json!({
        "corvux_edit_history_export": {
            "version": 1,
            "roost_path": roost_path,
            "exported_at_unix": exported_at,
            "scope": scope.unwrap_or("<entire roost>"),
            "event_count": events.len(),
            "events": events,
        }
    });
    serde_json::to_string_pretty(&export)
        .map_err(|e| format!("Failed to serialize edit history: {e}"))
}

/// EH-3: clear the edit history for one note. Returns the number of events
/// removed. Irreversible + device-local.
#[tauri::command]
#[specta::specta]
pub async fn purge_note_edit_history(
    note_path: String,
    db_state: State<'_, GraphDb>,
) -> Result<u32, String> {
    let note_id = resolve_note_id(&db_state, &note_path)?;
    let guard = db_state.db.lock().map_err(|e| format!("Lock error: {e}"))?;
    let db = guard.as_ref().ok_or("Graph DB not initialized")?;
    let removed = queries::purge_note_history(db, &note_id)?;
    Ok(removed as u32)
}

/// EH-3: clear ALL edit history for the whole roost. Returns the number of
/// events removed. Irreversible + device-local — the "erase my edit trail"
/// control in Settings.
#[tauri::command]
#[specta::specta]
pub async fn purge_roost_edit_history(db_state: State<'_, GraphDb>) -> Result<u32, String> {
    let guard = db_state.db.lock().map_err(|e| format!("Lock error: {e}"))?;
    let db = guard.as_ref().ok_or("Graph DB not initialized")?;
    let removed = queries::purge_all_events(db)?;
    Ok(removed as u32)
}

/// EH-3: export the edit-event audit trail to a pretty-printed JSON string.
/// Pass a note path to export one note's history, or `null` for the whole
/// roost. The frontend writes the returned string to a file / clipboard.
#[tauri::command]
#[specta::specta]
pub async fn export_edit_history(
    note_path: Option<String>,
    db_state: State<'_, GraphDb>,
) -> Result<String, String> {
    let roost_root = db_state
        .roost_path
        .lock()
        .map_err(|e| format!("Lock error: {e}"))?
        .clone()
        .unwrap_or_default();

    let guard = db_state.db.lock().map_err(|e| format!("Lock error: {e}"))?;
    let db = guard.as_ref().ok_or("Graph DB not initialized")?;
    let events = match note_path.as_deref() {
        Some(path) => {
            let note_id = queries::note_id_from_abs_path(path, &roost_root);
            queries::list_edit_events(db, &note_id, 0)?
        }
        None => queries::list_all_events(db)?,
    };
    build_export_json(
        &roost_root,
        now_unix_seconds(),
        note_path.as_deref(),
        &events,
    )
}

#[cfg(test)]
mod tests {
    //! `list_edit_events` / `revert_edit` are async commands over a managed
    //! `GraphDb` + `RoostFsState` + `SyncEngineState` stack and the storage
    //! layer (`db::queries::edit_events`) is already unit-tested, so we
    //! cover the file-local conflict heuristics those commands compute on
    //! top of the query results.
    use super::*;
    use crate::db::queries::EditEvent;

    /// Build a minimal `EditEvent` — only the fields the conflict
    /// heuristics read (`event_id`, `reverted_by`) carry meaning here.
    fn ev(event_id: &str, reverted_by: &str) -> EditEvent {
        EditEvent {
            note_id: "n".into(),
            event_id: event_id.into(),
            actor_type: "user".into(),
            actor_id: "user".into(),
            intent: "edit".into(),
            timestamp: 0.0,
            diff: String::new(),
            before_content: String::new(),
            lines_added: 0,
            lines_removed: 0,
            reverted_by: reverted_by.into(),
            note_path: "/v/n.md".into(),
        }
    }

    // ── revert_conflicts ────────────────────────────────────────────

    /// An un-reverted event is cleanly revertable; a reverted one isn't.
    #[test]
    fn revert_conflicts_tracks_reverted_by() {
        assert!(!revert_conflicts(&ev("e1", "")));
        assert!(revert_conflicts(&ev("e1", "e2")));
    }

    // ── has_later_unreverted ────────────────────────────────────────

    /// A strictly-later, un-reverted event blocks a clean revert. Relies
    /// on the zero-padded-millisecond id format sorting chronologically
    /// as plain strings.
    #[test]
    fn later_unreverted_event_blocks_revert() {
        let target = "0000000001000-aaaaaaaa";
        let events = vec![
            ev(target, ""),
            ev("0000000002000-bbbbbbbb", ""), // newer, not reverted → blocks
        ];
        assert!(has_later_unreverted(&events, target));
    }

    /// A later event that has ITSELF been reverted does not block — its
    /// effect is already undone, so restoring `target` won't lose work.
    #[test]
    fn later_but_reverted_event_does_not_block() {
        let target = "0000000001000-aaaaaaaa";
        let events = vec![
            ev(target, ""),
            ev("0000000002000-bbbbbbbb", "0000000003000-cccccccc"),
        ];
        assert!(!has_later_unreverted(&events, target));
    }

    /// Data-safety rule for external-edit history: a later EXTERNAL write
    /// (MCP / Obsidian / sync) must count as "later work" so reverting an
    /// earlier USER edit warns/forces before clobbering it. The gate keys on
    /// event_id order, not actor — this test pins that External participates.
    #[test]
    fn later_external_edit_blocks_reverting_an_earlier_user_edit() {
        let target = "0000000001000-aaaaaaaa";
        let mut later = ev("0000000002000-bbbbbbbb", "");
        later.actor_type = "external".into();
        let events = vec![ev(target, ""), later];
        assert!(
            has_later_unreverted(&events, target),
            "a later external write must block a clean revert of an earlier user edit"
        );
    }

    /// Only-earlier events (lexicographically `<= target`) never block —
    /// reverting the newest recorded edit is always clean.
    #[test]
    fn only_earlier_events_do_not_block() {
        let target = "0000000005000-aaaaaaaa";
        let events = vec![
            ev("0000000001000-zzzzzzzz", ""),
            ev("0000000004000-yyyyyyyy", ""),
            ev(target, ""),
        ];
        assert!(!has_later_unreverted(&events, target));
    }

    /// The target id itself (equal, not strictly greater) must not count
    /// as "later" — the `>` is strict so an event never blocks itself.
    #[test]
    fn target_event_does_not_block_itself() {
        let target = "0000000005000-aaaaaaaa";
        let events = vec![ev(target, "")];
        assert!(!has_later_unreverted(&events, target));
    }

    /// Higher millisecond stamp sorts after a lower one even when the
    /// random hex suffix would sort the other way — the fixed-width
    /// timestamp prefix dominates. This is the load-bearing assumption
    /// behind using string `>` for "happened after".
    #[test]
    fn millisecond_prefix_dominates_random_suffix() {
        let target = "0000000002000-ffffffff"; // big suffix, smaller time
        let events = vec![ev("0000000003000-00000000", "")]; // small suffix, later time
        assert!(
            has_later_unreverted(&events, target),
            "the later timestamp must win regardless of suffix bytes"
        );
    }

    // ── EH-1: needs_force_to_revert ─────────────────────────────────────

    /// `ev` with a specific `before_content`, for the no-op/divergence cases.
    fn ev_before(event_id: &str, reverted_by: &str, before: &str) -> EditEvent {
        let mut e = ev(event_id, reverted_by);
        e.before_content = before.into();
        e
    }

    #[test]
    fn needs_force_false_for_already_reverted() {
        // Hard-blocked path is surfaced by revert_conflicts, not needs_force.
        let e = ev_before("e2", "e3", "old");
        assert!(!needs_force_to_revert(
            &e,
            std::slice::from_ref(&e),
            "disk",
            None
        ));
    }

    #[test]
    fn needs_force_false_for_noop_revert() {
        // Disk already equals before_content → revert is a no-op, no force.
        let e = ev_before("e1", "", "same");
        assert!(!needs_force_to_revert(
            &e,
            std::slice::from_ref(&e),
            "same",
            None
        ));
    }

    #[test]
    fn needs_force_true_when_a_later_edit_exists() {
        let target = ev_before("0000000001000-aaaa", "", "old");
        let all = vec![target.clone(), ev("0000000002000-bbbb", "")];
        assert!(needs_force_to_revert(&target, &all, "current disk", None));
    }

    #[test]
    fn needs_force_false_for_clean_tip_revert() {
        // Newest event, disk differs from before_content, after_hash matches
        // disk → a clean revert, no force needed.
        let disk = "current tip content";
        let tip = ev_before("0000000009000-zzzz", "", "prior content");
        let matching_hash = queries::content_hash(disk);
        assert!(!needs_force_to_revert(
            &tip,
            std::slice::from_ref(&tip),
            disk,
            Some(&matching_hash),
        ));
    }

    #[test]
    fn needs_force_true_on_external_divergence() {
        // Newest event, no later edits, but the recorded after_hash doesn't
        // match disk → the file changed outside Corvux → force required.
        let disk = "content edited in another app";
        let tip = ev_before("0000000009000-zzzz", "", "prior content");
        let stale_hash = queries::content_hash("what corvux last wrote");
        assert!(needs_force_to_revert(
            &tip,
            std::slice::from_ref(&tip),
            disk,
            Some(&stale_hash),
        ));
    }

    // ── EH-3: build_export_json shape ───────────────────────────────────

    #[test]
    fn export_json_is_well_formed_and_carries_events() {
        let events = vec![
            ev_before("0000000002000-bbbb", "", "later"),
            ev_before("0000000001000-aaaa", "", "earlier"),
        ];
        let out = build_export_json("/roost", 1234.5, Some("/roost/n.md"), &events).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&out).expect("valid JSON");
        let root = &parsed["corvux_edit_history_export"];
        assert_eq!(root["version"], 1);
        assert_eq!(root["roost_path"], "/roost");
        assert_eq!(root["scope"], "/roost/n.md");
        assert_eq!(root["event_count"], 2);
        assert_eq!(root["events"].as_array().unwrap().len(), 2);
        // Full audit fields survive serialization (before_content included).
        assert_eq!(root["events"][0]["event_id"], "0000000002000-bbbb");
        assert_eq!(root["events"][0]["before_content"], "later");
    }

    #[test]
    fn export_json_roost_scope_when_none() {
        let out = build_export_json("/roost", 0.0, None, &[]).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
        let root = &parsed["corvux_edit_history_export"];
        assert_eq!(root["scope"], "<entire roost>");
        assert_eq!(root["event_count"], 0);
        assert!(root["events"].as_array().unwrap().is_empty());
    }
}
