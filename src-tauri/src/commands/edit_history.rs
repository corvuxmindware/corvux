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

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use cozo::DbInstance;
use serde::Serialize;
use tauri::{AppHandle, Manager, State};

use crate::commands::files::{to_rel, RoostFsState};
use crate::commands::graph::GraphDb;
use crate::db::queries;
use crate::roostfs::{Entry, RoostFs};

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
        // CO-2: recording the revert event prunes oldest-first, and the event
        // being reverted can BE that oldest row (budget edge). The file has
        // already been rewritten and synced above, so a vanished target must not
        // turn a completed revert into an error — there is simply nothing left
        // to mark (a pruned event cannot be reverted twice).
        if !queries::mark_reverted_if_present(db, &note_id, &event_id, &revert_id)? {
            tracing::info!(
                %note_id, %event_id,
                "revert_edit: reverted event was already pruned by the history budget; not marked"
            );
        }
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
    retention: serde_json::Value,
) -> Result<String, String> {
    let export = serde_json::json!({
        "corvux_edit_history_export": {
            "version": 1,
            "roost_path": roost_path,
            "exported_at_unix": exported_at,
            "scope": scope.unwrap_or("<entire roost>"),
            "event_count": events.len(),
            // CO-2 (additive; `version` stays 1): the limits that bound this
            // history and how much older history those limits already dropped,
            // so an exported trail never reads as "complete" when it is not.
            "retention": retention,
            "events": events,
        }
    });
    serde_json::to_string_pretty(&export)
        .map_err(|e| format!("Failed to serialize edit history: {e}"))
}

/// CO-2: the `retention` block of the export — the active limits plus the
/// cumulative tally of what pruning has already dropped on this device.
fn retention_json(stats: &queries::PruneStats) -> serde_json::Value {
    serde_json::json!({
        "events_per_note_cap": queries::EVENTS_PER_NOTE_CAP,
        "bytes_per_note_budget": queries::NOTE_BYTES_BUDGET,
        "global_bytes_budget": queries::GLOBAL_BYTES_BUDGET,
        "deleted_note_history_days": (queries::ORPHAN_HISTORY_RETENTION_SECS / 86_400.0) as u64,
        "oldest_dropped_first": true,
        "pruned_so_far": {
            "events": stats.events,
            "bytes": stats.bytes,
            "last_pruned_at_unix": stats.last_at,
        },
    })
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
    let retention = retention_json(&queries::prune_stats(db));
    build_export_json(
        &roost_root,
        now_unix_seconds(),
        note_path.as_deref(),
        &events,
        retention,
    )
}

// ── CO-2: low-frequency history maintenance ──────────────────────────────
//
// One pass, run at most once per `MAINTENANCE_INTERVAL_SECS` per roost from the
// warm-start heal (`commands::graph::spawn_warm_heal`), does the three jobs a
// per-save path cannot afford:
//   1. sweep orphan `:edit_event_after_hash` rows (left by the pre-CO-2 prune);
//   2. reap history of notes that are gone for good (not on disk, not in the
//      trash, newest event older than `ORPHAN_HISTORY_RETENTION_SECS`);
//   3. enforce the global byte budget, oldest-first, never a note's newest event.
// The graph mutex is taken per step (never across an `.await` or the whole
// pass), the roost is re-checked as still active at every step, and nothing
// here summarises or rewrites content — it only deletes whole old events.

/// Run the pass at most this often per roost (7 days), tracked in `kv_meta`.
const MAINTENANCE_INTERVAL_SECS: f64 = 7.0 * 24.0 * 60.0 * 60.0;
/// Wait this long after the warm heal before starting, so the pass never
/// competes with the graph pane's first queries.
const MAINTENANCE_START_DELAY_SECS: u64 = 90;
/// `kv_meta` key: unix seconds of the last completed pass.
const KV_MAINTENANCE_AT: &str = "edit_history_maintenance_at";
/// Events deleted per lock acquisition in the global pass.
const GLOBAL_DELETE_CHUNK: usize = 200;

/// Tunables of one pass. `DEFAULT` is production; tests shrink them.
#[derive(Debug, Clone, Copy)]
pub struct MaintenanceLimits {
    pub orphan_retention_secs: f64,
    pub global_bytes: u64,
    pub interval_secs: f64,
}

impl MaintenanceLimits {
    pub const DEFAULT: MaintenanceLimits = MaintenanceLimits {
        orphan_retention_secs: queries::ORPHAN_HISTORY_RETENTION_SECS,
        global_bytes: queries::GLOBAL_BYTES_BUDGET,
        interval_secs: MAINTENANCE_INTERVAL_SECS,
    };
}

/// What one pass did. `ran == false` means the gate said "not due" (or the
/// roost was no longer active) and nothing was touched.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MaintenanceReport {
    pub ran: bool,
    pub sidecars_swept: usize,
    pub orphan_notes_reaped: usize,
    pub orphan_events_reaped: usize,
    pub orphans_kept: usize,
    pub global_pruned: queries::PruneReport,
}

/// Run `f` on the graph DB if `roost_path` is still the active roost, holding
/// the graph mutex only for the duration of `f`. `None` = no DB / roost
/// switched. `block_in_place` on the multi-thread runtime (production) so a
/// slow Cozo scan doesn't park a tokio worker; plain call on a current-thread
/// runtime (unit tests), where `block_in_place` would panic.
fn with_active_db<R>(
    graph: &GraphDb,
    roost_path: &str,
    f: impl FnOnce(&DbInstance) -> R,
) -> Option<R> {
    let run = move || {
        let guard = graph.db.lock().ok()?;
        let active = graph
            .roost_path
            .lock()
            .ok()
            .map(|g| g.as_deref() == Some(roost_path))
            .unwrap_or(false);
        if !active {
            return None;
        }
        let db = guard.as_ref()?;
        Some(f(db))
    };
    match tokio::runtime::Handle::try_current() {
        Ok(h) if h.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(run)
        }
        _ => run(),
    }
}

/// Is a maintenance pass due? A last-run stamp in the FUTURE (clock moved
/// back) counts as due, so a bad clock can't disable the pass forever.
fn maintenance_due(db: &DbInstance, now: f64, interval_secs: f64) -> bool {
    match crate::db::schema::get_kv(db, KV_MAINTENANCE_AT)
        .ok()
        .flatten()
        .and_then(|s| s.parse::<f64>().ok())
    {
        Some(prev) => prev > now || now - prev >= interval_secs,
        None => true,
    }
}

/// The last `note_id` segment a file name produces: NFC, lowercased, final
/// extension stripped — exactly how `note_id_from_abs_path` mints it.
fn id_stem(name: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    let n: String = name.nfc().collect::<String>().to_lowercase();
    match n.rsplit_once('.') {
        Some((stem, _)) => stem.to_string(),
        None => n,
    }
}

/// `"<unix-secs>[-<n>].<name>"` -> `<name>` (the trash layout from
/// `roostfs::trash::trash_name`). `None` when `name` doesn't have that shape.
fn strip_trash_prefix(name: &str) -> Option<&str> {
    let (head, rest) = name.split_once('.')?;
    let all_digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let mut parts = head.splitn(2, '-');
    if !all_digits(parts.next()?) {
        return None;
    }
    if let Some(n) = parts.next() {
        if !all_digits(n) {
            return None;
        }
    }
    (!rest.is_empty()).then_some(rest)
}

/// Id stems of every markdown file found in the trash listing. Both the raw
/// file name (files nested inside a trashed FOLDER keep their own name) and the
/// timestamp-stripped name (a directly trashed note is `<ts>.<name>`) are
/// recorded — over-matching only ever KEEPS history, never loses it.
pub(crate) fn trash_markdown_stems(entries: &[Entry]) -> HashSet<String> {
    let mut out = HashSet::new();
    for e in entries {
        if e.is_dir || !crate::roostfs::walk::is_markdown_name(&e.name) {
            continue;
        }
        out.insert(id_stem(&e.name));
        if let Some(rest) = strip_trash_prefix(&e.name) {
            if crate::roostfs::walk::is_markdown_name(rest) {
                out.insert(id_stem(rest));
            }
        }
    }
    out
}

/// `None` = the trash could not be listed, so the reaper must NOT run (an
/// unreadable trash is not an empty trash). An absent trash folder is empty.
async fn trash_stems(backend: &dyn RoostFs) -> Option<HashSet<String>> {
    use crate::roostfs::RoostError;
    let trash = crate::roostfs::trash::TRASH_DIR;
    match backend.exists(trash).await {
        Ok(false) => {
            // `exists()` answers "absent" for a refused path (e.g. a trash
            // symlinked out of the roost) as well as for a missing one. Only a
            // listing that says "not there" — not "refused" or "failed" — makes
            // an empty trash a definite answer.
            return match backend.list(trash).await {
                Err(RoostError::NotFound(_) | RoostError::NotDir(_)) => Some(HashSet::new()),
                Ok(_) => {
                    tracing::warn!(
                        "edit_history: trash exists() and list() disagree; skipping orphan reap"
                    );
                    None
                }
                Err(e) => {
                    tracing::warn!(error = %e, "edit_history: trash absence not confirmed; skipping orphan reap");
                    None
                }
            };
        }
        Ok(true) => {}
        Err(e) => {
            tracing::warn!(error = %e, "edit_history: trash existence check failed; skipping orphan reap");
            return None;
        }
    }
    match backend.list_recursive(trash).await {
        Ok(entries) => Some(trash_markdown_stems(&entries)),
        Err(e) => {
            tracing::warn!(error = %e, "edit_history: trash listing failed; skipping orphan reap");
            None
        }
    }
}

/// Pure keep/reap decision from the listings. `true` = KEEP the history:
/// the note is on disk, or a note with the same name sits in the trash (a
/// trashed note can be restored at any time — the trash has no auto-expiry).
/// Matching the trash by name stem is deliberately coarse: trash entries are
/// `<ts>.<basename>`, so the folder is unknown and a same-named note elsewhere
/// is indistinguishable; the error direction is "keep too much".
pub(crate) fn keep_orphan_candidate(
    cand: &queries::OrphanCandidate,
    live_ids: &HashSet<String>,
    trash_stems: &HashSet<String>,
) -> bool {
    if live_ids.contains(&cand.note_id) {
        return true;
    }
    let stem = cand.note_id.rsplit('/').next().unwrap_or(&cand.note_id);
    trash_stems.contains(stem)
}

/// What the backend says about a candidate's recorded path. ONLY `Gone` may
/// lead to a reap; `Unknown` is "I could not tell" and must be treated exactly
/// like `Exists` (fail safe: an irreversible delete needs a definite answer).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PathState {
    Exists,
    Gone,
    Unknown,
}

/// Last line of defence before an irreversible delete: ask the backend about
/// the candidate's recorded path directly. The `disk_abs` listing skips hidden
/// folders, so a note moved under e.g. `.archive/` is invisible there but still
/// exists. Every way of NOT getting a definite "not there" is `Unknown`:
/// an empty recorded path, a path that does not map under the current roost
/// root (drive-letter change, moved roost, SAF vs desktop path forms), a
/// backend error, and a refused path. `exists() == false` alone is NOT enough:
/// backends answer "absent" for a refused path (a symlink out of the roost, an
/// unusable name) as well as for a missing one, so "absent" is confirmed with a
/// read, and only `NotFound` counts as gone.
async fn path_state(backend: &dyn RoostFs, note_path: &str) -> PathState {
    use crate::roostfs::RoostError;
    if note_path.is_empty() {
        return PathState::Unknown;
    }
    let Some(rel) = to_rel(note_path, backend.display_root()) else {
        return PathState::Unknown;
    };
    match backend.exists(&rel).await {
        Ok(true) => PathState::Exists,
        Err(_) => PathState::Unknown,
        Ok(false) => match backend.read(&rel).await {
            Ok(_) => PathState::Exists, // exists() was wrong: it is there
            Err(RoostError::NotFound(_)) => PathState::Gone,
            Err(_) => PathState::Unknown,
        },
    }
}

/// True when any segment of a note id starts with `.`. The app's listings
/// (walk, trash scan) skip dot-prefixed entries at every depth, so for such a
/// note neither "not on disk" nor "not in the trash" can be established from
/// them (e.g. a hidden sub-folder inside a trashed folder). Never reaped.
fn has_hidden_segment(note_id: &str) -> bool {
    note_id.split('/').any(|seg| seg.starts_with('.'))
}

/// The single reap decision. EVERY condition must give a definite answer in
/// the "gone" direction; anything missing, uncertain or contradictory keeps the
/// history. Pure, so each branch has a test.
fn may_reap(
    cand: &queries::OrphanCandidate,
    live_ids: &HashSet<String>,
    trash_stems: &HashSet<String>,
    state: PathState,
) -> bool {
    !keep_orphan_candidate(cand, live_ids, trash_stems)
        && !has_hidden_segment(&cand.note_id)
        && state == PathState::Gone
}

/// A system clock before this (Nov 2023) is a dead battery / unset clock, not
/// a date: retention arithmetic on it is meaningless, so no reap runs.
const MIN_SANE_UNIX_SECS: f64 = 1_700_000_000.0;

/// `(notes_reaped, events_reaped, kept)`; `None` = the roost switched mid-pass.
async fn reap_orphans(
    graph: &GraphDb,
    roost_path: &str,
    backend: &dyn RoostFs,
    disk_abs: &[String],
    candidates: Vec<queries::OrphanCandidate>,
    cutoff: f64,
) -> Option<(usize, usize, usize)> {
    // Rails for an irreversible delete: an empty disk listing means "the roost
    // is unreadable / unmounted", not "every note was deleted".
    if disk_abs.is_empty() {
        tracing::warn!("edit_history: empty disk listing; skipping orphan reap");
        return Some((0, 0, candidates.len()));
    }
    let Some(trash) = trash_stems(backend).await else {
        return Some((0, 0, candidates.len()));
    };
    let live: HashSet<String> = disk_abs
        .iter()
        .map(|abs| queries::note_id_from_abs_path(abs, roost_path))
        .collect();

    let mut victims: Vec<String> = Vec::new();
    let mut kept = 0usize;
    for c in &candidates {
        // The cheap, pure checks first; only a candidate that survives them
        // costs a backend round-trip. `Unknown` is the placeholder for "not
        // probed" — it can never lead to a reap on its own, and `may_reap`
        // re-applies the same pure checks regardless.
        let state = if keep_orphan_candidate(c, &live, &trash) || has_hidden_segment(&c.note_id) {
            PathState::Unknown
        } else {
            path_state(backend, &c.note_path).await
        };
        if may_reap(c, &live, &trash, state) {
            victims.push(c.note_id.clone());
        } else {
            kept += 1;
        }
    }
    if victims.is_empty() {
        return Some((0, 0, kept));
    }
    let (notes, events) = with_active_db(graph, roost_path, |db| {
        let (mut notes, mut events) = (0usize, 0usize);
        for id in &victims {
            // Re-checks the newest event under the lock: the note may have been
            // re-created or edited since the candidate scan.
            match queries::reap_note_if_still_orphan(db, id, cutoff) {
                Ok(n) if n > 0 => {
                    notes += 1;
                    events += n;
                }
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!(error = %e, "edit_history: orphan reap failed for one note")
                }
            }
        }
        (notes, events)
    })?;
    Some((notes, events, kept))
}

/// Global byte budget: inventory every note (lock per note), plan oldest-first
/// across notes, delete in chunks (lock per chunk). `None` = roost switched.
async fn enforce_global(
    graph: &GraphDb,
    roost_path: &str,
    budget: u64,
) -> Option<queries::PruneReport> {
    let mut report = queries::PruneReport::default();
    if budget == 0 {
        return Some(report);
    }
    let ids = match with_active_db(graph, roost_path, queries::list_note_ids)? {
        Ok(ids) => ids,
        Err(e) => {
            tracing::warn!(error = %e, "edit_history: global budget skipped (note list failed)");
            return Some(report);
        }
    };
    let mut inventory = Vec::with_capacity(ids.len());
    for id in ids {
        match with_active_db(graph, roost_path, |db| queries::note_event_sizes(db, &id))? {
            Ok(sizes) => inventory.push((id, sizes)),
            Err(e) => tracing::warn!(error = %e, "edit_history: size scan failed for one note"),
        }
        tokio::task::yield_now().await;
    }
    let victims = queries::plan_global_prune(&inventory, budget);
    if victims.is_empty() {
        let total: u64 = inventory
            .iter()
            .flat_map(|(_, s)| s.iter().map(|e| e.bytes))
            .sum();
        if total > budget {
            // Only each note's newest event is left, and those alone exceed the
            // budget. Never pruned (the latest revert point); say so.
            tracing::warn!(
                total_bytes = total,
                budget_bytes = budget,
                "edit_history: newest events alone exceed the global budget; nothing more to prune"
            );
        }
        return Some(report);
    }
    for chunk in victims.chunks(GLOBAL_DELETE_CHUNK) {
        match with_active_db(graph, roost_path, |db| {
            queries::delete_global_victims(db, chunk)
        })? {
            Ok(r) => report.merge(r),
            Err(e) => {
                tracing::warn!(error = %e, "edit_history: global prune chunk failed; stopping");
                break;
            }
        }
        tokio::task::yield_now().await;
    }
    if !report.is_empty() {
        tracing::info!(
            pruned_events = report.events,
            pruned_bytes = report.bytes,
            budget_bytes = budget,
            "edit_history: pruned oldest history over the global budget"
        );
        with_active_db(graph, roost_path, |db| {
            queries::record_prune_stats(db, &report)
        })?;
    }
    Some(report)
}

/// Run one maintenance pass if it is due. `disk_abs` is the warm heal's
/// listing of every markdown file on disk (absolute display paths). Returns
/// what was done; never fails the caller — each step logs and carries on.
pub async fn run_history_maintenance(
    graph: &GraphDb,
    roost_path: &str,
    backend: &dyn RoostFs,
    disk_abs: &[String],
    now: f64,
    limits: MaintenanceLimits,
) -> MaintenanceReport {
    let mut report = MaintenanceReport::default();

    let due = with_active_db(graph, roost_path, |db| {
        maintenance_due(db, now, limits.interval_secs)
    });
    if due != Some(true) {
        return report;
    }
    report.ran = true;

    // 1. Orphan sidecars (cheap anti-join).
    match with_active_db(graph, roost_path, queries::sweep_orphan_after_hashes) {
        Some(Ok(n)) => report.sidecars_swept = n,
        Some(Err(e)) => tracing::warn!(error = %e, "edit_history: sidecar sweep failed"),
        None => return report,
    }

    // 2. Orphan history of notes that are gone for good.
    let cutoff = now - limits.orphan_retention_secs;
    let candidates = if now < MIN_SANE_UNIX_SECS {
        // An unset / reset clock makes "older than a year" meaningless.
        tracing::warn!("edit_history: system clock looks unset; skipping orphan reap");
        Vec::new()
    } else {
        match with_active_db(graph, roost_path, |db| {
            queries::orphan_candidates(db, cutoff)
        }) {
            Some(Ok(c)) => c,
            Some(Err(e)) => {
                // Cannot list candidates -> reap nothing.
                tracing::warn!(error = %e, "edit_history: orphan scan failed");
                Vec::new()
            }
            None => return report,
        }
    };
    if !candidates.is_empty() {
        match reap_orphans(graph, roost_path, backend, disk_abs, candidates, cutoff).await {
            Some((notes, events, kept)) => {
                report.orphan_notes_reaped = notes;
                report.orphan_events_reaped = events;
                report.orphans_kept = kept;
            }
            None => return report,
        }
    }

    // 3. Global byte budget.
    match enforce_global(graph, roost_path, limits.global_bytes).await {
        Some(r) => report.global_pruned = r,
        None => return report,
    }

    // Stamp last, and only for a pass that ran to the end on the same roost.
    let _ = with_active_db(graph, roost_path, |db| {
        if let Err(e) = crate::db::schema::set_kv(db, KV_MAINTENANCE_AT, &now.to_string()) {
            tracing::warn!(error = %e, "edit_history: could not stamp maintenance time");
        }
    });
    report
}

/// Warm-heal hook: schedule the maintenance pass on a detached task. Cheap
/// when not due (one `kv_meta` read, then the task ends and drops the listing).
/// `disk_abs` is the heal's markdown listing for `roost_path`.
pub fn spawn_history_maintenance(
    app: AppHandle,
    roost_path: String,
    backend: Arc<dyn RoostFs>,
    disk_abs: Vec<String>,
) {
    tauri::async_runtime::spawn(async move {
        let state = app.state::<GraphDb>();
        let limits = MaintenanceLimits::DEFAULT;
        let due = with_active_db(&state, &roost_path, |db| {
            maintenance_due(db, now_unix_seconds(), limits.interval_secs)
        });
        if due != Some(true) {
            return;
        }
        tokio::time::sleep(Duration::from_secs(MAINTENANCE_START_DELAY_SECS)).await;
        let report = run_history_maintenance(
            &state,
            &roost_path,
            backend.as_ref(),
            &disk_abs,
            now_unix_seconds(),
            limits,
        )
        .await;
        if report.ran {
            tracing::info!(?report, "edit_history: maintenance pass complete");
        }
    });
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
        let out = build_export_json(
            "/roost",
            1234.5,
            Some("/roost/n.md"),
            &events,
            serde_json::Value::Null,
        )
        .unwrap();
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
        let out = build_export_json("/roost", 0.0, None, &[], serde_json::Value::Null).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
        let root = &parsed["corvux_edit_history_export"];
        assert_eq!(root["scope"], "<entire roost>");
        assert_eq!(root["event_count"], 0);
        assert!(root["events"].as_array().unwrap().is_empty());
    }

    /// CO-2: the export states the limits and how much history pruning already
    /// dropped, so an exported trail never reads as complete when it is not.
    #[test]
    fn export_json_carries_retention_limits_and_prune_tally() {
        let stats = queries::PruneStats {
            events: 7,
            bytes: 4096,
            last_at: Some(1_700_000_000.0),
        };
        let out = build_export_json("/roost", 0.0, None, &[], retention_json(&stats)).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
        let r = &parsed["corvux_edit_history_export"]["retention"];
        assert_eq!(r["events_per_note_cap"], queries::EVENTS_PER_NOTE_CAP);
        assert_eq!(r["bytes_per_note_budget"], queries::NOTE_BYTES_BUDGET);
        assert_eq!(r["global_bytes_budget"], queries::GLOBAL_BYTES_BUDGET);
        assert_eq!(r["deleted_note_history_days"], 365);
        assert_eq!(r["oldest_dropped_first"], true);
        assert_eq!(r["pruned_so_far"]["events"], 7);
        assert_eq!(r["pruned_so_far"]["bytes"], 4096);
        assert_eq!(r["pruned_so_far"]["last_pruned_at_unix"], 1_700_000_000.0);
        // A history that was never pruned says so explicitly (null, not absent).
        let none = retention_json(&queries::PruneStats::default());
        assert_eq!(none["pruned_so_far"]["events"], 0);
        assert!(none["pruned_so_far"]["last_pruned_at_unix"].is_null());
    }

    // ── CO-2: orphan reaper + maintenance pass ───────────────────────────

    use crate::roostfs::native::NativeFs;
    use tempfile::TempDir;

    fn entry(name: &str, is_dir: bool) -> Entry {
        Entry {
            name: name.to_string(),
            rel_path: format!(".corvux-trash/{name}"),
            is_dir,
        }
    }

    fn cand(note_id: &str) -> queries::OrphanCandidate {
        queries::OrphanCandidate {
            note_id: note_id.to_string(),
            note_path: format!("/r/{note_id}.md"),
            newest_ts: 0.0,
            events: 1,
        }
    }

    #[test]
    fn strip_trash_prefix_matches_the_trash_name_layout() {
        assert_eq!(strip_trash_prefix("1700000000.a.md"), Some("a.md"));
        assert_eq!(strip_trash_prefix("1700000000-3.a.md"), Some("a.md"));
        assert_eq!(
            strip_trash_prefix("1700000000.notes.v2.md"),
            Some("notes.v2.md")
        );
        assert_eq!(strip_trash_prefix("a.md"), None);
        assert_eq!(strip_trash_prefix("17x.a.md"), None);
        assert_eq!(strip_trash_prefix("1700-.a.md"), None);
        assert_eq!(strip_trash_prefix("1700."), None);
    }

    #[test]
    fn trash_stems_cover_trashed_notes_and_notes_inside_trashed_folders() {
        let entries = vec![
            entry("1700000000.Daily Note.md", false), // trashed note
            entry("1700000001.Proj", true),           // trashed folder (dir)
            entry("Inner.MD", false),                 // note inside it: raw name
            entry("1700000002.pic.png", false),       // not markdown
            entry("1700000003-1.Café.markdown", false),
        ];
        let stems = trash_markdown_stems(&entries);
        assert!(
            stems.contains("daily note"),
            "ts prefix stripped, lowercased"
        );
        assert!(stems.contains("inner"), "nested note keeps its own name");
        assert!(stems.contains("café"));
        assert!(!stems.contains("pic"), "non-markdown ignored");
        assert!(!stems.contains("proj"), "a folder is not a note");
    }

    #[test]
    fn keep_decision_is_live_or_restorable_from_trash() {
        let live: HashSet<String> = ["work/live".to_string()].into();
        let trash: HashSet<String> = ["restorable".to_string()].into();
        assert!(
            keep_orphan_candidate(&cand("work/live"), &live, &trash),
            "on disk"
        );
        assert!(
            keep_orphan_candidate(&cand("anywhere/restorable"), &live, &trash),
            "same name in the trash (folder unknown) is kept"
        );
        assert!(
            !keep_orphan_candidate(&cand("work/gone"), &live, &trash),
            "not on disk and not in the trash is reapable"
        );
    }

    /// Everything a maintenance test needs: a REAL schema (`init_db`, so
    /// `kv_meta` and both edit relations exist exactly as in production), a
    /// roost dir, and the managed-state shape the pass runs against.
    struct Fixture {
        // Struct fields drop in declaration order: the db handles go FIRST and
        // the temp dirs LAST (Windows can't delete a db file that is still open).
        graph: GraphDb,
        db: DbInstance,
        fs: NativeFs,
        root: String,
        _db_dir: TempDir,
        _roost: TempDir,
    }

    fn fixture() -> Fixture {
        let roost = TempDir::new().unwrap();
        let db_dir = TempDir::new().unwrap();
        let db = crate::db::schema::init_db(db_dir.path()).expect("init_db");
        let root = roost.path().to_string_lossy().to_string();
        let graph = GraphDb::new();
        *graph.db.lock().unwrap() = Some(db.clone());
        *graph.roost_path.lock().unwrap() = Some(root.clone());
        let fs = NativeFs::new(roost.path());
        Fixture {
            _roost: roost,
            _db_dir: db_dir,
            graph,
            db,
            root,
            fs,
        }
    }

    /// An event + its sidecar row at `ts`, written raw (record_event always
    /// stamps "now"). `before` sets the stored payload size.
    fn seed(db: &DbInstance, note_id: &str, ts: f64, note_path: &str, before: &str) -> String {
        use cozo::{DataValue, ScriptMutability};
        use std::collections::BTreeMap;
        let event_id = format!("{:013}-{:08x}", (ts * 1000.0) as u64, 1);
        let mut p = BTreeMap::new();
        p.insert("n".to_string(), DataValue::Str(note_id.into()));
        p.insert("e".to_string(), DataValue::Str(event_id.clone().into()));
        p.insert("t".to_string(), DataValue::Num(cozo::Num::Float(ts)));
        p.insert("p".to_string(), DataValue::Str(note_path.into()));
        p.insert("b".to_string(), DataValue::Str(before.into()));
        db.run_script(
            r#"?[note_id, event_id, actor_type, actor_id, intent, timestamp,
                 diff, before_content, lines_added, lines_removed, reverted_by, note_path]
               <- [[$n, $e, "user", "user", "edit", $t, "", $b, 1, 1, "", $p]]
               :put edit_event { note_id, event_id => actor_type, actor_id, intent,
                 timestamp, diff, before_content, lines_added, lines_removed, reverted_by, note_path }"#,
            p.clone(),
            ScriptMutability::Mutable,
        )
        .expect("seed event");
        db.run_script(
            r#"?[note_id, event_id, after_hash] <- [[$n, $e, "h"]]
               :put edit_event_after_hash { note_id, event_id => after_hash }"#,
            p,
            ScriptMutability::Mutable,
        )
        .expect("seed sidecar");
        event_id
    }

    fn event_count(db: &DbInstance, note_id: &str) -> usize {
        queries::list_edit_events(db, note_id, 0).unwrap().len()
    }

    fn sidecar_count(db: &DbInstance, note_id: &str) -> usize {
        use cozo::{DataValue, ScriptMutability};
        let mut p = std::collections::BTreeMap::new();
        p.insert("n".to_string(), DataValue::Str(note_id.into()));
        db.run_script(
            r#"?[event_id] := *edit_event_after_hash{ note_id, event_id }, note_id == $n"#,
            p,
            ScriptMutability::Immutable,
        )
        .unwrap()
        .rows
        .len()
    }

    const NOW: f64 = 1_800_000_000.0;
    const DAY: f64 = 86_400.0;

    fn test_limits(global_bytes: u64) -> MaintenanceLimits {
        MaintenanceLimits {
            orphan_retention_secs: queries::ORPHAN_HISTORY_RETENTION_SECS,
            global_bytes,
            interval_secs: MAINTENANCE_INTERVAL_SECS,
        }
    }

    #[tokio::test]
    async fn maintenance_reaps_only_old_notes_that_are_gone_for_good() {
        let fx = fixture();
        let old = NOW - queries::ORPHAN_HISTORY_RETENTION_SECS - 30.0 * DAY;
        let p = |n: &str| format!("{}/{n}.md", fx.root);

        // On disk and in the trash, as the app lays them out.
        std::fs::write(fx._roost.path().join("live.md"), "x").unwrap();
        std::fs::create_dir_all(fx._roost.path().join(".corvux-trash")).unwrap();
        std::fs::write(
            fx._roost
                .path()
                .join(".corvux-trash")
                .join("1700000000.trashed.md"),
            "x",
        )
        .unwrap();
        // A real note under a HIDDEN folder: the heal's listing skips it, but
        // it exists.
        std::fs::create_dir_all(fx._roost.path().join(".archive")).unwrap();
        std::fs::write(fx._roost.path().join(".archive").join("shelved.md"), "x").unwrap();

        seed(&fx.db, "gone", old, &p("gone"), "gone body");
        seed(&fx.db, "live", old, &p("live"), "live body");
        seed(&fx.db, "trashed", old, &p("trashed"), "trashed body");
        seed(
            &fx.db,
            ".archive/shelved",
            old,
            &format!("{}/.archive/shelved.md", fx.root),
            "shelved body",
        );
        seed(
            &fx.db,
            "recent-gone",
            NOW - 10.0 * DAY,
            &p("recent-gone"),
            "recent",
        );

        // What the warm heal hands over: markdown on disk, hidden dirs skipped.
        let disk_abs = vec![p("live")];
        let rep =
            run_history_maintenance(&fx.graph, &fx.root, &fx.fs, &disk_abs, NOW, test_limits(0))
                .await;

        assert!(rep.ran);
        assert_eq!(rep.orphan_notes_reaped, 1, "only `gone` is reapable");
        assert_eq!(rep.orphan_events_reaped, 1);
        assert_eq!(rep.orphans_kept, 3, "live + trashed + hidden-folder note");
        assert_eq!(event_count(&fx.db, "gone"), 0);
        assert_eq!(sidecar_count(&fx.db, "gone"), 0, "sidecar reaped with it");
        for kept in ["live", "trashed", ".archive/shelved", "recent-gone"] {
            assert_eq!(event_count(&fx.db, kept), 1, "{kept} must be kept");
            assert_eq!(sidecar_count(&fx.db, kept), 1, "{kept} sidecar kept");
        }

        // Gated: a second pass inside the interval does nothing at all.
        seed(&fx.db, "gone2", old, &p("gone2"), "later orphan");
        let again = run_history_maintenance(
            &fx.graph,
            &fx.root,
            &fx.fs,
            &disk_abs,
            NOW + DAY,
            test_limits(0),
        )
        .await;
        assert!(!again.ran);
        assert_eq!(event_count(&fx.db, "gone2"), 1);
        // ...and runs again once the interval has passed.
        let later = run_history_maintenance(
            &fx.graph,
            &fx.root,
            &fx.fs,
            &disk_abs,
            NOW + 8.0 * DAY,
            test_limits(0),
        )
        .await;
        assert!(later.ran);
        assert_eq!(event_count(&fx.db, "gone2"), 0);
    }

    #[tokio::test]
    async fn maintenance_refuses_to_reap_on_an_empty_disk_listing() {
        // An unmounted / unreadable roost lists nothing. That is not "every
        // note was deleted": an irreversible delete must not follow from it.
        let fx = fixture();
        let old = NOW - queries::ORPHAN_HISTORY_RETENTION_SECS - 30.0 * DAY;
        seed(&fx.db, "n", old, &format!("{}/n.md", fx.root), "body");
        let rep =
            run_history_maintenance(&fx.graph, &fx.root, &fx.fs, &[], NOW, test_limits(0)).await;
        assert!(rep.ran);
        assert_eq!(rep.orphan_notes_reaped, 0);
        assert_eq!(event_count(&fx.db, "n"), 1);
    }

    #[tokio::test]
    async fn maintenance_does_nothing_when_the_roost_is_no_longer_active() {
        let fx = fixture();
        let old = NOW - queries::ORPHAN_HISTORY_RETENTION_SECS - 30.0 * DAY;
        seed(&fx.db, "n", old, &format!("{}/n.md", fx.root), "body");
        let rep = run_history_maintenance(
            &fx.graph,
            "/some/other/roost",
            &fx.fs,
            &[format!("{}/x.md", fx.root)],
            NOW,
            test_limits(0),
        )
        .await;
        assert!(!rep.ran, "a switched roost's DB must never be touched");
        assert_eq!(event_count(&fx.db, "n"), 1);
    }

    #[tokio::test]
    async fn maintenance_cleans_a_legacy_db_and_enforces_the_global_budget() {
        // An EXISTING graph.db: no schema change was needed for CO-2, so the
        // upgrade path is "old rows keep working and the first pass tidies
        // them". Legacy state = orphan sidecar rows from the old prune + a DB
        // far over budget, on the real production schema.
        let fx = fixture();
        let recent = NOW - 2.0 * DAY;
        let live = |n: &str| format!("{}/{n}.md", fx.root);
        let big = "z".repeat(10_000);
        // Two live notes, three 10 KB events each, oldest-first by ts.
        for (i, n) in ["a", "b"].iter().enumerate() {
            std::fs::write(fx._roost.path().join(format!("{n}.md")), "x").unwrap();
            for k in 0..3 {
                seed(&fx.db, n, recent + (i * 10 + k) as f64, &live(n), &big);
            }
        }
        // Orphan sidecars the pre-CO-2 prune left behind.
        for k in 0..4 {
            use cozo::{DataValue, ScriptMutability};
            let mut p = std::collections::BTreeMap::new();
            p.insert(
                "e".to_string(),
                DataValue::Str(format!("{k:013}-dead").into()),
            );
            fx.db
                .run_script(
                    r#"?[note_id, event_id, after_hash] <- [["a", $e, "stale"]]
                       :put edit_event_after_hash { note_id, event_id => after_hash }"#,
                    p,
                    ScriptMutability::Mutable,
                )
                .unwrap();
        }
        assert_eq!(sidecar_count(&fx.db, "a"), 3 + 4);

        let disk_abs = vec![live("a"), live("b")];
        // Budget for ~2 of the 6 events.
        let rep = run_history_maintenance(
            &fx.graph,
            &fx.root,
            &fx.fs,
            &disk_abs,
            NOW,
            test_limits(25_000),
        )
        .await;

        assert!(rep.ran);
        assert_eq!(rep.sidecars_swept, 4, "orphan hashes removed");
        assert_eq!(
            rep.global_pruned.events, 4,
            "6 events x 10 KB -> 2 fit 25 KB"
        );
        assert_eq!(rep.global_pruned.bytes, 40_000);
        // Oldest-first across notes, newest of each note kept, sidecar in step.
        for n in ["a", "b"] {
            assert_eq!(
                event_count(&fx.db, n),
                1,
                "{n}: only its newest event is left"
            );
            assert_eq!(sidecar_count(&fx.db, n), 1, "{n}: sidecar pruned in step");
        }
        // The prune is visible: tallied for the export, not silent.
        let stats = queries::prune_stats(&fx.db);
        assert_eq!((stats.events, stats.bytes), (4, 40_000));
        let retention = retention_json(&stats);
        assert_eq!(retention["pruned_so_far"]["events"], 4);
    }

    #[tokio::test]
    async fn maintenance_leaves_an_under_budget_roost_untouched() {
        let fx = fixture();
        std::fs::write(fx._roost.path().join("a.md"), "x").unwrap();
        let path = format!("{}/a.md", fx.root);
        seed(&fx.db, "a", NOW - DAY, &path, "small");
        let rep = run_history_maintenance(
            &fx.graph,
            &fx.root,
            &fx.fs,
            &[path],
            NOW,
            MaintenanceLimits::DEFAULT,
        )
        .await;
        assert!(rep.ran);
        assert_eq!(rep.global_pruned, queries::PruneReport::default());
        assert_eq!(rep.orphan_notes_reaped, 0);
        assert_eq!(event_count(&fx.db, "a"), 1);
    }

    // ── CO-2 review F4: an UNCERTAIN answer never reaps ──────────────────
    //
    // The reaper deletes history irreversibly, so every signal it consults
    // must give a DEFINITE "gone"; "could not tell" (unmappable path, backend
    // error, refused path, unreadable/unconfirmed trash, odd clock, hidden
    // folder, case/Unicode drift) keeps the history. One test per branch, plus
    // a control proving the same setup DOES reap when every signal is definite.

    use crate::roostfs::{RoostError, RoostFs};

    /// Injectable failures for the probes the reaper makes.
    #[derive(Default, Clone, Copy)]
    struct Faults {
        /// Every per-note `exists()` errors (the trash probe is separate).
        exists_err: bool,
        /// Every per-note `exists()` says "absent" (what a refused path looks like).
        exists_false: bool,
        /// `read()` is refused (path escapes / unusable name).
        read_refused: bool,
        /// `read()` fails with an I/O error.
        read_io_err: bool,
        /// `exists(.corvux-trash)` errors.
        trash_exists_err: bool,
        /// `exists(.corvux-trash)` says "absent".
        trash_exists_false: bool,
        /// Listing the trash fails with an I/O error.
        trash_list_err: bool,
        /// Listing the trash is REFUSED (trash symlinked out of the roost).
        trash_list_refused: bool,
    }

    /// A real `NativeFs` whose probes can be made to fail on demand.
    struct FaultFs {
        inner: NativeFs,
        f: Faults,
    }

    fn is_trash(rel: &str) -> bool {
        rel.trim_matches('/') == ".corvux-trash"
    }

    impl FaultFs {
        fn new(root: &std::path::Path, f: Faults) -> Self {
            Self {
                inner: NativeFs::new(root),
                f,
            }
        }

        fn trash_list_fault(&self, rel_dir: &str) -> Option<RoostError> {
            if !is_trash(rel_dir) {
                return None;
            }
            if self.f.trash_list_err {
                return Some(RoostError::Io("injected".into()));
            }
            if self.f.trash_list_refused {
                return Some(RoostError::OutsideRoost("injected".into()));
            }
            None
        }
    }

    #[async_trait::async_trait]
    impl RoostFs for FaultFs {
        async fn list(&self, rel_dir: &str) -> Result<Vec<Entry>, RoostError> {
            if let Some(e) = self.trash_list_fault(rel_dir) {
                return Err(e);
            }
            self.inner.list(rel_dir).await
        }
        async fn list_recursive(&self, rel_dir: &str) -> Result<Vec<Entry>, RoostError> {
            if let Some(e) = self.trash_list_fault(rel_dir) {
                return Err(e);
            }
            self.inner.list_recursive(rel_dir).await
        }
        async fn read(&self, rel: &str) -> Result<Vec<u8>, RoostError> {
            if self.f.read_refused {
                return Err(RoostError::OutsideRoost("injected".into()));
            }
            if self.f.read_io_err {
                return Err(RoostError::Io("injected".into()));
            }
            self.inner.read(rel).await
        }
        async fn write_atomic(&self, rel: &str, bytes: &[u8]) -> Result<(), RoostError> {
            self.inner.write_atomic(rel, bytes).await
        }
        async fn create(&self, rel: &str, initial: &[u8]) -> Result<(), RoostError> {
            self.inner.create(rel, initial).await
        }
        async fn trash(&self, rel: &str) -> Result<(), RoostError> {
            self.inner.trash(rel).await
        }
        async fn rename(&self, from_rel: &str, to_rel: &str) -> Result<(), RoostError> {
            self.inner.rename(from_rel, to_rel).await
        }
        async fn mkdir(&self, rel_dir: &str) -> Result<(), RoostError> {
            self.inner.mkdir(rel_dir).await
        }
        async fn rmdir(&self, rel_dir: &str) -> Result<(), RoostError> {
            self.inner.rmdir(rel_dir).await
        }
        async fn exists(&self, rel: &str) -> Result<bool, RoostError> {
            if is_trash(rel) {
                // The trash probe has its OWN faults, so each rail is tested in
                // isolation: `exists_err` / `exists_false` below only ever hit
                // the per-note probe.
                if self.f.trash_exists_err {
                    return Err(RoostError::Io("injected".into()));
                }
                if self.f.trash_exists_false {
                    return Ok(false);
                }
                return self.inner.exists(rel).await;
            }
            if self.f.exists_err {
                return Err(RoostError::Io("injected".into()));
            }
            if self.f.exists_false {
                return Ok(false);
            }
            self.inner.exists(rel).await
        }
        fn display_root(&self) -> &str {
            self.inner.display_root()
        }
        fn root_id(&self) -> String {
            self.inner.root_id()
        }
    }

    /// Seed one OLD note whose history is reapable on its face (>1 year old,
    /// not in the listing, not in the trash) recorded under `note_path`, run a
    /// maintenance pass over `fs`, and say whether its history was removed.
    async fn reaped(
        fx: &Fixture,
        fs: &dyn RoostFs,
        note_id: &str,
        note_path: &str,
        now: f64,
    ) -> bool {
        let old = now - queries::ORPHAN_HISTORY_RETENTION_SECS - 30.0 * DAY;
        seed(&fx.db, note_id, old, note_path, "body");
        // A non-empty listing that does not contain the note.
        let disk = vec![format!("{}/other.md", fx.root)];
        let rep =
            run_history_maintenance(&fx.graph, &fx.root, fs, &disk, now, test_limits(0)).await;
        assert!(rep.ran, "the pass itself must have run");
        let removed = event_count(&fx.db, note_id) == 0;
        if !removed {
            assert_eq!(sidecar_count(&fx.db, note_id), 1, "sidecar kept with it");
        }
        removed
    }

    #[tokio::test]
    async fn control_a_definitely_gone_note_is_reaped() {
        // Same setup as every test below, with NO fault: proves the tests below
        // keep history because of the fault, not because the reaper never fires.
        let fx = fixture();
        let fs = FaultFs::new(fx._roost.path(), Faults::default());
        let p = format!("{}/gone.md", fx.root);
        assert!(reaped(&fx, &fs, "gone", &p, NOW).await);
    }

    #[tokio::test]
    async fn a_path_outside_the_current_root_never_reaps() {
        // Moved roost / drive-letter change / SAF-vs-desktop path forms: the
        // recorded path cannot be mapped under the current root, so "does it
        // exist?" cannot be asked. That is UNKNOWN, not "gone".
        for (i, bad) in [
            "/old/drive/roost/gone.md",
            r"\\?\C:\old\roost\gone.md",
            "content://com.android.externalstorage.documents/tree/x/gone.md",
            "",
        ]
        .iter()
        .enumerate()
        {
            let fx = fixture();
            let fs = FaultFs::new(fx._roost.path(), Faults::default());
            assert!(
                !reaped(&fx, &fs, &format!("gone{i}"), bad, NOW).await,
                "unmappable path {bad:?} must keep the history"
            );
        }
    }

    #[tokio::test]
    async fn an_exists_error_never_reaps() {
        let fx = fixture();
        let fs = FaultFs::new(
            fx._roost.path(),
            Faults {
                exists_err: true,
                ..Faults::default()
            },
        );
        let p = format!("{}/gone.md", fx.root);
        assert!(!reaped(&fx, &fs, "gone", &p, NOW).await);
    }

    #[tokio::test]
    async fn absent_but_read_refused_never_reaps() {
        // `exists()` answers "absent" for a refused path too; the confirming
        // read says "refused", which is not "not found".
        let fx = fixture();
        let fs = FaultFs::new(
            fx._roost.path(),
            Faults {
                exists_false: true,
                read_refused: true,
                ..Faults::default()
            },
        );
        let p = format!("{}/gone.md", fx.root);
        assert!(!reaped(&fx, &fs, "gone", &p, NOW).await);
    }

    #[tokio::test]
    async fn absent_but_read_failing_never_reaps() {
        let fx = fixture();
        let fs = FaultFs::new(
            fx._roost.path(),
            Faults {
                exists_false: true,
                read_io_err: true,
                ..Faults::default()
            },
        );
        let p = format!("{}/gone.md", fx.root);
        assert!(!reaped(&fx, &fs, "gone", &p, NOW).await);
    }

    #[tokio::test]
    async fn absent_per_exists_but_readable_never_reaps() {
        // exists() lies ("absent") while the file is really there.
        let fx = fixture();
        std::fs::write(fx._roost.path().join("gone.md"), "still here").unwrap();
        let fs = FaultFs::new(
            fx._roost.path(),
            Faults {
                exists_false: true,
                ..Faults::default()
            },
        );
        let p = format!("{}/gone.md", fx.root);
        assert!(!reaped(&fx, &fs, "gone", &p, NOW).await);
    }

    #[tokio::test]
    async fn an_unreadable_trash_never_reaps() {
        // Each way the trash can fail to give a definite listing.
        for (label, f) in [
            (
                "exists() errors",
                Faults {
                    trash_exists_err: true,
                    ..Faults::default()
                },
            ),
            (
                "listing errors",
                Faults {
                    trash_list_err: true,
                    ..Faults::default()
                },
            ),
        ] {
            // The listing fault only bites when the trash exists.
            let fx = fixture();
            std::fs::create_dir_all(fx._roost.path().join(".corvux-trash")).unwrap();
            let fs = FaultFs::new(fx._roost.path(), f);
            let p = format!("{}/gone.md", fx.root);
            assert!(
                !reaped(&fx, &fs, "gone", &p, NOW).await,
                "trash {label}: history must be kept"
            );
        }
    }

    #[tokio::test]
    async fn a_trash_whose_absence_is_unconfirmed_never_reaps() {
        // The trash is symlinked out of the roost: exists() says "absent" but a
        // listing is REFUSED. That is not an empty trash.
        let fx = fixture();
        let fs = FaultFs::new(
            fx._roost.path(),
            Faults {
                trash_exists_false: true,
                trash_list_refused: true,
                ..Faults::default()
            },
        );
        let p = format!("{}/gone.md", fx.root);
        assert!(!reaped(&fx, &fs, "gone", &p, NOW).await);
    }

    #[tokio::test]
    async fn a_trash_that_disagrees_with_itself_never_reaps() {
        // exists() says absent but the folder lists fine.
        let fx = fixture();
        std::fs::create_dir_all(fx._roost.path().join(".corvux-trash")).unwrap();
        let fs = FaultFs::new(
            fx._roost.path(),
            Faults {
                trash_exists_false: true,
                ..Faults::default()
            },
        );
        let p = format!("{}/gone.md", fx.root);
        assert!(!reaped(&fx, &fs, "gone", &p, NOW).await);
    }

    #[tokio::test]
    async fn a_trash_path_occupied_by_a_file_never_reaps() {
        // Real filesystem state (the sync engine tests create it too): a FILE
        // named `.corvux-trash`. exists() is true, listing it fails.
        let fx = fixture();
        std::fs::write(fx._roost.path().join(".corvux-trash"), b"not a dir").unwrap();
        let p = format!("{}/gone.md", fx.root);
        assert!(!reaped(&fx, &fx.fs, "gone", &p, NOW).await);
    }

    #[tokio::test]
    async fn a_hidden_folder_note_is_never_reaped() {
        // Listings skip dot-prefixed entries at every depth, so neither "not on
        // disk" nor "not in the trash" is knowable for such a note.
        for id in [".archive/gone", "proj/.drafts/gone"] {
            let fx = fixture();
            let p = format!("{}/{id}.md", fx.root);
            assert!(!reaped(&fx, &fx.fs, id, &p, NOW).await, "{id}");
        }
    }

    #[tokio::test]
    async fn an_unset_clock_never_reaps() {
        // A reset clock (here 2001) makes "older than a year" meaningless.
        let fx = fixture();
        let p = format!("{}/gone.md", fx.root);
        assert!(!reaped(&fx, &fx.fs, "gone", &p, 1_000_000_000.0).await);
    }

    #[tokio::test]
    async fn case_and_unicode_drift_between_id_and_listing_keep_the_history() {
        // Ids are NFC + lowercase; listings can hand back mixed case or NFD
        // (macOS / SAF). Both must still match the live set.
        let fx = fixture();
        let old = NOW - queries::ORPHAN_HISTORY_RETENTION_SECS - 30.0 * DAY;
        seed(
            &fx.db,
            "notes/a",
            old,
            &format!("{}/Notes/A.md", fx.root),
            "x",
        );
        seed(
            &fx.db,
            "caf\u{e9}/x",
            old,
            &format!("{}/caf\u{e9}/x.md", fx.root),
            "x",
        );
        seed(
            &fx.db,
            "really-gone",
            old,
            &format!("{}/really-gone.md", fx.root),
            "x",
        );
        let disk = vec![
            format!("{}/Notes/A.md", fx.root),       // mixed case
            format!("{}/cafe\u{301}/x.md", fx.root), // NFD
        ];
        let rep =
            run_history_maintenance(&fx.graph, &fx.root, &fx.fs, &disk, NOW, test_limits(0)).await;
        assert!(rep.ran);
        assert_eq!(event_count(&fx.db, "notes/a"), 1, "mixed-case listing");
        assert_eq!(event_count(&fx.db, "caf\u{e9}/x"), 1, "NFD listing");
        assert_eq!(
            event_count(&fx.db, "really-gone"),
            0,
            "control: a real orphan"
        );
    }

    #[test]
    fn trash_name_drift_keeps_the_history() {
        // A trashed note whose file name came back NFD / upper-case still
        // matches the NFC lowercase id stem.
        let stems = trash_markdown_stems(&[entry("1700000000.CAFE\u{301}.MD", false)]);
        let live: HashSet<String> = HashSet::new();
        assert!(keep_orphan_candidate(
            &cand("docs/caf\u{e9}"),
            &live,
            &stems
        ));
    }

    #[test]
    fn may_reap_requires_every_signal_to_say_gone() {
        let live: HashSet<String> = ["on/disk".to_string()].into();
        let trash: HashSet<String> = ["trashed".to_string()].into();
        let none = HashSet::new();
        // The one reapable shape: not live, not trashed, not hidden, Gone.
        assert!(may_reap(&cand("a/gone"), &live, &trash, PathState::Gone));
        // Each other signal alone blocks it.
        assert!(
            !may_reap(&cand("on/disk"), &live, &trash, PathState::Gone),
            "live"
        );
        assert!(
            !may_reap(&cand("x/trashed"), &live, &trash, PathState::Gone),
            "trashed"
        );
        assert!(
            !may_reap(&cand(".h/gone"), &live, &trash, PathState::Gone),
            "hidden"
        );
        assert!(
            !may_reap(&cand("a/gone"), &live, &trash, PathState::Exists),
            "exists"
        );
        assert!(
            !may_reap(&cand("a/gone"), &live, &trash, PathState::Unknown),
            "unknown"
        );
        // With empty listings the ONLY thing standing is the path state.
        assert!(!may_reap(&cand("a/gone"), &none, &none, PathState::Unknown));
        assert!(may_reap(&cand("a/gone"), &none, &none, PathState::Gone));
    }

    #[test]
    fn hidden_segment_detection_covers_any_depth() {
        assert!(has_hidden_segment(".a/b"));
        assert!(has_hidden_segment("a/.b/c"));
        assert!(has_hidden_segment("a/b/.c"));
        assert!(!has_hidden_segment("a/b.c/d"));
        assert!(!has_hidden_segment("plain"));
    }
}
