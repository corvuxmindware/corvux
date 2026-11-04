//! Edit history backend (Phase 2.2 — the trust gate before agent CRUD).
//!
//! Every mutation that touches markdown on disk is recorded as a row in
//! the `:edit_event` relation. The History pane renders these rows; the
//! `revert_edit` command walks them backwards. This module is the only
//! place that talks to `:edit_event` — call sites use `record_event`,
//! `list_events`, `get_event`, `mark_reverted`, `prune_oldest`.
//!
//! EH-4: **edit history is device-local and does NOT sync.** The
//! `:edit_event` relation lives in the per-roost `graph.db` under the app
//! data dir — it is never part of the CRDT sync payload (sync carries
//! markdown deltas only), so each device keeps its own independent record
//! of the edits it made. Retention is bounded per note by
//! `EVENTS_PER_NOTE_CAP` (500 events/note), pruned oldest-first on every
//! record; there is no global cap, so total on-disk size scales with the
//! number of notes touched. Users can purge (per-note or roost-wide) and
//! export the audit trail from Settings -> Privacy (EH-3).
//!
//! Design notes — see `db::schema::create_edit_event_relation` for the
//! field-by-field rationale. The two non-obvious choices:
//!
//! 1. **Diff library = `similar`** (already in the dep tree for the
//!    markdown↔Automerge round-trip in `sync::crdt::roundtrip`). We
//!    evaluated `patience_diff` for cleaner output on structured text;
//!    not worth a new dep — `similar`'s line-level Myers diff is good
//!    enough for the History pane and for inverse-diff-based revert.
//!
//! 2. **Storing `before_content` verbatim** alongside the unified diff
//!    is deliberate. Reverts apply the inverse of the diff, but if a
//!    later edit touched the same lines the inverse no longer matches
//!    cleanly. With `before_content` we can detect the conflict (live
//!    content already differs from the post-event content stored
//!    indirectly via the diff) AND, for the user-confirmed "force
//!    revert" path, just write the verbatim `before_content` back.
//!    Storage cost is bounded by the per-note 500-event prune cap.

use cozo::{DataValue, DbInstance, ScriptMutability};
use serde::Serialize;
use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

/// Maximum events kept per note. Older events are pruned when this is
/// exceeded. Hardcoded for now; promote to a roost setting in a follow-up
/// once the Settings UI grows an "Edit history retention" knob.
pub const EVENTS_PER_NOTE_CAP: usize = 500;

/// Coalesce window. When `record_event` is called within this many
/// seconds of the most recent matching event (same note, same actor,
/// same intent, not reverted), the existing row is updated in-place
/// instead of inserting a new one — the original `before_content` is
/// preserved (so revert restores the start of the burst) but `diff`,
/// `lines_added`, `lines_removed`, and `timestamp` reflect the latest
/// state.
///
/// 30s is calibrated to the editor's auto-save cadence: a typing
/// session at one save every 2s collapses into a single row, but a
/// minute-long pause + new edit gets its own audit entry. Users can
/// see the resulting "session" as one event in the History pane.
pub const COALESCE_WINDOW_SECONDS: f64 = 30.0;

/// Actor that produced an edit. Stringly-typed in the DB; this enum
/// exists so call sites can't typo `"user"` as `"User"`.
#[derive(Debug, Clone, Copy)]
pub enum Actor {
    User,
    Agent,
    System,
    /// A write that reached the roost from OUTSIDE the app's own save path —
    /// MCP sidecar, Obsidian, git, an agent shell, or a peer-sync write. The
    /// filesystem watcher observes these; we can't tell the specific tool
    /// apart, so they all record as `external`. These events are
    /// **visibility-only**: the History pane shows the diff but offers no
    /// revert (`revert_edit` refuses them), because the "before" is
    /// reconstructed from the last-indexed content rather than a genuine
    /// pre-write read, so it is not revert-grade.
    External,
}

impl Actor {
    pub fn as_str(self) -> &'static str {
        match self {
            Actor::User => "user",
            Actor::Agent => "agent",
            Actor::System => "system",
            Actor::External => "external",
        }
    }
}

/// What kind of mutation. `intent` is the human-friendly label rendered
/// in the History pane; the indexer uses it for filtering too.
#[derive(Debug, Clone, Copy)]
pub enum Intent {
    Edit,
    Create,
    Delete,
    ToggleTask,
    AddTask,
    Rename,
    AgentWrite,
    /// EH-5: a revert restoring an earlier event's `before_content`.
    /// Distinct from `Edit` so the History pane can label it ("↩ Reverted")
    /// instead of showing another generic edit row.
    Revert,
}

impl Intent {
    pub fn as_str(self) -> &'static str {
        match self {
            Intent::Edit => "edit",
            Intent::Create => "create",
            Intent::Delete => "delete",
            Intent::ToggleTask => "toggle_task",
            Intent::AddTask => "add_task",
            Intent::Rename => "rename",
            Intent::AgentWrite => "agent_write",
            Intent::Revert => "revert",
        }
    }
}

/// One row out of `:edit_event`. Returned to the frontend by
/// `list_edit_events`; the `diff` field is the unified-diff text used by
/// the inline diff viewer.
#[derive(Debug, Clone, Serialize)]
pub struct EditEvent {
    pub note_id: String,
    pub event_id: String,
    pub actor_type: String,
    pub actor_id: String,
    pub intent: String,
    pub timestamp: f64,
    pub diff: String,
    pub before_content: String,
    pub lines_added: i64,
    pub lines_removed: i64,
    pub reverted_by: String,
    pub note_path: String,
}

/// Compute a unified-line-diff between `before` and `after`, returning the
/// diff text plus (added, removed) line counts. Both the diff and the
/// counts get stored in the `:edit_event` row.
pub fn compute_diff(before: &str, after: &str) -> (String, i64, i64) {
    use similar::{ChangeTag, TextDiff};

    let diff = TextDiff::from_lines(before, after);
    let unified = diff
        .unified_diff()
        .context_radius(3)
        .header("before", "after")
        .to_string();

    let mut added: i64 = 0;
    let mut removed: i64 = 0;
    for change in diff.iter_all_changes() {
        match change.tag() {
            ChangeTag::Insert => added += 1,
            ChangeTag::Delete => removed += 1,
            ChangeTag::Equal => {}
        }
    }
    (unified, added, removed)
}

/// Generate a sortable per-event id. Format: `<unix_ms>-<rand_hex>`.
/// Lexicographic order matches chronological order within the same
/// millisecond width (events from 2026-05-02 sort after events from
/// 2026-05-01). Random suffix avoids collisions on bulk imports.
fn make_event_id() -> String {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    // 8 hex chars of randomness — collision-free at any realistic edit rate.
    let rand: u32 = rand::random();
    format!("{:013}-{:08x}", now_ms, rand)
}

fn now_seconds() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Fetch the most recent event for a note. Used by `record_event` to
/// decide whether to coalesce. Cheap: single-row indexed lookup with
/// `:order -event_id :limit 1`.
fn most_recent_event(db: &DbInstance, note_id: &str) -> Result<Option<EditEvent>, String> {
    let events = list_events(db, note_id, 1)?;
    Ok(events.into_iter().next())
}

/// Re-`:put` an existing event row with refreshed mutable payload —
/// used by the coalescing path. Preserves the row's `event_id`,
/// `before_content`, and creation `intent`. `actor_id` overwrites in
/// case the agent-id changed mid-burst (rare but harmless to track).
#[allow(clippy::too_many_arguments)]
fn update_event_payload(
    db: &DbInstance,
    prev: &EditEvent,
    actor_id: &str,
    timestamp: f64,
    diff: &str,
    lines_added: i64,
    lines_removed: i64,
    note_path: &str,
) -> Result<(), String> {
    let mut params = BTreeMap::new();
    params.insert(
        "note_id".to_string(),
        DataValue::Str(prev.note_id.clone().into()),
    );
    params.insert(
        "event_id".to_string(),
        DataValue::Str(prev.event_id.clone().into()),
    );
    params.insert(
        "actor_type".to_string(),
        DataValue::Str(prev.actor_type.clone().into()),
    );
    params.insert(
        "actor_id".to_string(),
        DataValue::Str(actor_id.to_string().into()),
    );
    params.insert(
        "intent".to_string(),
        DataValue::Str(prev.intent.clone().into()),
    );
    params.insert(
        "timestamp".to_string(),
        DataValue::Num(cozo::Num::Float(timestamp)),
    );
    params.insert("diff".to_string(), DataValue::Str(diff.to_string().into()));
    params.insert(
        "before_content".to_string(),
        DataValue::Str(prev.before_content.clone().into()),
    );
    params.insert(
        "lines_added".to_string(),
        DataValue::Num(cozo::Num::Int(lines_added)),
    );
    params.insert(
        "lines_removed".to_string(),
        DataValue::Num(cozo::Num::Int(lines_removed)),
    );
    params.insert(
        "reverted_by".to_string(),
        DataValue::Str(prev.reverted_by.clone().into()),
    );
    params.insert(
        "note_path".to_string(),
        DataValue::Str(note_path.to_string().into()),
    );

    let script = r#"
        ?[note_id, event_id, actor_type, actor_id, intent, timestamp,
          diff, before_content, lines_added, lines_removed, reverted_by, note_path] <-
            [[$note_id, $event_id, $actor_type, $actor_id, $intent, $timestamp,
              $diff, $before_content, $lines_added, $lines_removed, $reverted_by, $note_path]]
        :put edit_event {
            note_id, event_id =>
            actor_type, actor_id, intent, timestamp,
            diff, before_content, lines_added, lines_removed, reverted_by, note_path
        }
    "#;
    db.run_script(script, params, ScriptMutability::Mutable)
        .map_err(|e| format!("update_event_payload failed: {e}"))?;
    Ok(())
}

/// Record one edit event. Returns the new event_id so callers can chain
/// (e.g. `revert_edit` records a new event whose body links back to the
/// original).
///
/// `before` is the file content immediately prior to the write. `after`
/// is what's now on disk (or empty for delete intent). `note_path` is
/// the absolute path; `note_id` is the canonical roost-relative id.
///
/// Errors during recording are surfaced — the caller decides whether to
/// fail the user-visible operation or log-and-continue. For most write
/// paths the right policy is log-and-continue: a missed history entry
/// is recoverable (the markdown is still on disk), failing the save
/// because of a history hiccup is not.
#[allow(clippy::too_many_arguments)]
pub fn record_event(
    db: &DbInstance,
    note_id: &str,
    note_path: &str,
    actor: Actor,
    actor_id: &str,
    intent: Intent,
    before: &str,
    after: &str,
) -> Result<String, String> {
    // Coalesce check — see `COALESCE_WINDOW_SECONDS`. The original
    // `before_content` is preserved so revert still restores the start
    // of the typing burst; only `diff`, line counts, and timestamp move.
    let now = now_seconds();
    if let Some(prev) = most_recent_event(db, note_id)? {
        let same_actor = prev.actor_type == actor.as_str();
        let same_intent = prev.intent == intent.as_str();
        let in_window = (now - prev.timestamp).abs() < COALESCE_WINDOW_SECONDS;
        let not_reverted = prev.reverted_by.is_empty();
        if same_actor && same_intent && in_window && not_reverted {
            let (diff, added, removed) = compute_diff(&prev.before_content, after);
            update_event_payload(db, &prev, actor_id, now, &diff, added, removed, note_path)?;
            // Refresh the sidecar — coalescing replaces the post-state
            // for this event. Without this, divergence-detection would
            // compare against a stale hash from the burst's first save.
            if let Err(e) = put_after_hash(db, note_id, &prev.event_id, &content_hash(after)) {
                tracing::warn!(
                    %note_id, error = %e,
                    "edit_events: after_hash refresh on coalesce failed"
                );
            }
            return Ok(prev.event_id);
        }
    }

    let (diff, added, removed) = compute_diff(before, after);
    let event_id = make_event_id();
    let timestamp = now;

    let mut params = BTreeMap::new();
    params.insert(
        "note_id".to_string(),
        DataValue::Str(note_id.to_string().into()),
    );
    params.insert(
        "event_id".to_string(),
        DataValue::Str(event_id.clone().into()),
    );
    params.insert(
        "actor_type".to_string(),
        DataValue::Str(actor.as_str().to_string().into()),
    );
    params.insert(
        "actor_id".to_string(),
        DataValue::Str(actor_id.to_string().into()),
    );
    params.insert(
        "intent".to_string(),
        DataValue::Str(intent.as_str().to_string().into()),
    );
    params.insert(
        "timestamp".to_string(),
        DataValue::Num(cozo::Num::Float(timestamp)),
    );
    params.insert("diff".to_string(), DataValue::Str(diff.into()));
    params.insert(
        "before_content".to_string(),
        DataValue::Str(before.to_string().into()),
    );
    params.insert(
        "lines_added".to_string(),
        DataValue::Num(cozo::Num::Int(added)),
    );
    params.insert(
        "lines_removed".to_string(),
        DataValue::Num(cozo::Num::Int(removed)),
    );
    params.insert(
        "reverted_by".to_string(),
        DataValue::Str(String::new().into()),
    );
    params.insert(
        "note_path".to_string(),
        DataValue::Str(note_path.to_string().into()),
    );

    let script = r#"
        ?[note_id, event_id, actor_type, actor_id, intent, timestamp,
          diff, before_content, lines_added, lines_removed, reverted_by, note_path] <-
            [[$note_id, $event_id, $actor_type, $actor_id, $intent, $timestamp,
              $diff, $before_content, $lines_added, $lines_removed, $reverted_by, $note_path]]
        :put edit_event {
            note_id, event_id =>
            actor_type, actor_id, intent, timestamp,
            diff, before_content, lines_added, lines_removed, reverted_by, note_path
        }
    "#;

    db.run_script(script, params, ScriptMutability::Mutable)
        .map_err(|e| format!("record_event failed: {e}"))?;

    // Sidecar — store a hash of the post-state so `revert_edit` can spot
    // external divergence later. Best-effort: a failure here doesn't
    // poison the main event row — divergence detection just falls back
    // to the older later-events heuristic for this row.
    if let Err(e) = put_after_hash(db, note_id, &event_id, &content_hash(after)) {
        tracing::warn!(%note_id, error = %e, "edit_events: after_hash sidecar write failed");
    }

    // Best-effort prune. A failure here just means this note's history
    // is one row over the cap until the next save — never user-visible.
    if let Err(e) = prune_oldest(db, note_id, EVENTS_PER_NOTE_CAP) {
        tracing::warn!(%note_id, error = %e, "edit_events: prune after record failed");
    }

    Ok(event_id)
}

/// Fast, collision-resistant hash for the post-event file content.
/// blake3 is already a project dep (used elsewhere for content addressing
/// and notification ids). 16 hex chars (64 bits) is plenty for a sidecar
/// dedup hash — the goal is "did the file change between event-record
/// time and now," not cryptographic identity.
pub fn content_hash(content: &str) -> String {
    let h = blake3::hash(content.as_bytes());
    hex::encode(&h.as_bytes()[..8])
}

fn put_after_hash(
    db: &DbInstance,
    note_id: &str,
    event_id: &str,
    after_hash: &str,
) -> Result<(), String> {
    let mut params = BTreeMap::new();
    params.insert(
        "note_id".to_string(),
        DataValue::Str(note_id.to_string().into()),
    );
    params.insert(
        "event_id".to_string(),
        DataValue::Str(event_id.to_string().into()),
    );
    params.insert(
        "after_hash".to_string(),
        DataValue::Str(after_hash.to_string().into()),
    );
    let script = r#"
        ?[note_id, event_id, after_hash] <- [[$note_id, $event_id, $after_hash]]
        :put edit_event_after_hash {
            note_id, event_id => after_hash
        }
    "#;
    db.run_script(script, params, ScriptMutability::Mutable)
        .map_err(|e| format!("put_after_hash failed: {e}"))?;
    Ok(())
}

/// Look up the recorded `after_hash` for the most recent (lex-greatest
/// `event_id`) event of `note_id`. Returns `None` when no entry exists —
/// either because the note has no events, or because all of its events
/// pre-date the v4 sidecar migration. Callers treat `None` as
/// "divergence info unavailable" and fall back to other heuristics.
pub fn latest_after_hash(db: &DbInstance, note_id: &str) -> Result<Option<String>, String> {
    let mut params = BTreeMap::new();
    params.insert(
        "note_id".to_string(),
        DataValue::Str(note_id.to_string().into()),
    );
    let script = r#"
        ?[event_id, after_hash] :=
            *edit_event_after_hash{ note_id, event_id, after_hash },
            note_id == $note_id
        :order -event_id
        :limit 1
    "#;
    let result = db
        .run_script(script, params, ScriptMutability::Immutable)
        .map_err(|e| format!("latest_after_hash query failed: {e}"))?;
    Ok(result
        .rows
        .first()
        .and_then(|row| row.get(1))
        .and_then(|v| v.get_str())
        .map(str::to_string))
}

/// List the most recent events for a single note, newest first.
/// `limit = 0` is treated as "no limit" — caller pages explicitly when
/// it cares about size. Frontend default is 100.
pub fn list_events(db: &DbInstance, note_id: &str, limit: usize) -> Result<Vec<EditEvent>, String> {
    let mut params = BTreeMap::new();
    params.insert(
        "note_id".to_string(),
        DataValue::Str(note_id.to_string().into()),
    );

    let script = r#"
        ?[event_id, actor_type, actor_id, intent, timestamp,
          diff, before_content, lines_added, lines_removed, reverted_by, note_path] :=
            *edit_event{
                note_id, event_id, actor_type, actor_id, intent, timestamp,
                diff, before_content, lines_added, lines_removed, reverted_by, note_path
            },
            note_id == $note_id
        :order -event_id
    "#;

    let result = db
        .run_script(script, params, ScriptMutability::Immutable)
        .map_err(|e| format!("list_edit_events failed: {e}"))?;

    let mut rows: Vec<EditEvent> = Vec::with_capacity(result.rows.len());
    for r in result.rows.iter() {
        if r.len() < 11 {
            continue;
        }
        let event_id = r[0].get_str().unwrap_or_default().to_string();
        let actor_type = r[1].get_str().unwrap_or_default().to_string();
        let actor_id = r[2].get_str().unwrap_or_default().to_string();
        let intent = r[3].get_str().unwrap_or_default().to_string();
        let timestamp = match &r[4] {
            DataValue::Num(cozo::Num::Float(f)) => *f,
            DataValue::Num(cozo::Num::Int(i)) => *i as f64,
            _ => 0.0,
        };
        let diff = r[5].get_str().unwrap_or_default().to_string();
        let before_content = r[6].get_str().unwrap_or_default().to_string();
        let lines_added = match &r[7] {
            DataValue::Num(cozo::Num::Int(i)) => *i,
            _ => 0,
        };
        let lines_removed = match &r[8] {
            DataValue::Num(cozo::Num::Int(i)) => *i,
            _ => 0,
        };
        let reverted_by = r[9].get_str().unwrap_or_default().to_string();
        let note_path = r[10].get_str().unwrap_or_default().to_string();

        rows.push(EditEvent {
            note_id: note_id.to_string(),
            event_id,
            actor_type,
            actor_id,
            intent,
            timestamp,
            diff,
            before_content,
            lines_added,
            lines_removed,
            reverted_by,
            note_path,
        });
        if limit > 0 && rows.len() >= limit {
            break;
        }
    }
    Ok(rows)
}

/// Fetch a single event by `(note_id, event_id)`. Used by `revert_edit`
/// to read back the `before_content` it must restore.
pub fn get_event(
    db: &DbInstance,
    note_id: &str,
    event_id: &str,
) -> Result<Option<EditEvent>, String> {
    let mut params = BTreeMap::new();
    params.insert(
        "note_id".to_string(),
        DataValue::Str(note_id.to_string().into()),
    );
    params.insert(
        "event_id".to_string(),
        DataValue::Str(event_id.to_string().into()),
    );

    let script = r#"
        ?[actor_type, actor_id, intent, timestamp,
          diff, before_content, lines_added, lines_removed, reverted_by, note_path] :=
            *edit_event{
                note_id, event_id, actor_type, actor_id, intent, timestamp,
                diff, before_content, lines_added, lines_removed, reverted_by, note_path
            },
            note_id == $note_id, event_id == $event_id
    "#;

    let result = db
        .run_script(script, params, ScriptMutability::Immutable)
        .map_err(|e| format!("get_event failed: {e}"))?;
    let row = match result.rows.first() {
        Some(r) if r.len() >= 10 => r,
        _ => return Ok(None),
    };
    let actor_type = row[0].get_str().unwrap_or_default().to_string();
    let actor_id = row[1].get_str().unwrap_or_default().to_string();
    let intent = row[2].get_str().unwrap_or_default().to_string();
    let timestamp = match &row[3] {
        DataValue::Num(cozo::Num::Float(f)) => *f,
        DataValue::Num(cozo::Num::Int(i)) => *i as f64,
        _ => 0.0,
    };
    let diff = row[4].get_str().unwrap_or_default().to_string();
    let before_content = row[5].get_str().unwrap_or_default().to_string();
    let lines_added = match &row[6] {
        DataValue::Num(cozo::Num::Int(i)) => *i,
        _ => 0,
    };
    let lines_removed = match &row[7] {
        DataValue::Num(cozo::Num::Int(i)) => *i,
        _ => 0,
    };
    let reverted_by = row[8].get_str().unwrap_or_default().to_string();
    let note_path = row[9].get_str().unwrap_or_default().to_string();

    Ok(Some(EditEvent {
        note_id: note_id.to_string(),
        event_id: event_id.to_string(),
        actor_type,
        actor_id,
        intent,
        timestamp,
        diff,
        before_content,
        lines_added,
        lines_removed,
        reverted_by,
        note_path,
    }))
}

/// Mark an event as reverted by stamping `reverted_by = revert_event_id`.
/// Used by `revert_edit` to chain the revert event back to the original
/// it undid, so the History pane can render "(reverted by X)" affordances.
pub fn mark_reverted(
    db: &DbInstance,
    note_id: &str,
    event_id: &str,
    revert_event_id: &str,
) -> Result<(), String> {
    // Cozo's `:update` requires us to pre-fetch the existing row, mutate
    // the field, and `:put` it back. There is no in-place column update.
    let existing = match get_event(db, note_id, event_id)? {
        Some(e) => e,
        None => {
            return Err(format!(
                "mark_reverted: event {event_id} for note {note_id} not found"
            ));
        }
    };

    let mut params = BTreeMap::new();
    params.insert(
        "note_id".to_string(),
        DataValue::Str(note_id.to_string().into()),
    );
    params.insert(
        "event_id".to_string(),
        DataValue::Str(event_id.to_string().into()),
    );
    params.insert(
        "actor_type".to_string(),
        DataValue::Str(existing.actor_type.into()),
    );
    params.insert(
        "actor_id".to_string(),
        DataValue::Str(existing.actor_id.into()),
    );
    params.insert("intent".to_string(), DataValue::Str(existing.intent.into()));
    params.insert(
        "timestamp".to_string(),
        DataValue::Num(cozo::Num::Float(existing.timestamp)),
    );
    params.insert("diff".to_string(), DataValue::Str(existing.diff.into()));
    params.insert(
        "before_content".to_string(),
        DataValue::Str(existing.before_content.into()),
    );
    params.insert(
        "lines_added".to_string(),
        DataValue::Num(cozo::Num::Int(existing.lines_added)),
    );
    params.insert(
        "lines_removed".to_string(),
        DataValue::Num(cozo::Num::Int(existing.lines_removed)),
    );
    params.insert(
        "reverted_by".to_string(),
        DataValue::Str(revert_event_id.to_string().into()),
    );
    params.insert(
        "note_path".to_string(),
        DataValue::Str(existing.note_path.into()),
    );

    let script = r#"
        ?[note_id, event_id, actor_type, actor_id, intent, timestamp,
          diff, before_content, lines_added, lines_removed, reverted_by, note_path] <-
            [[$note_id, $event_id, $actor_type, $actor_id, $intent, $timestamp,
              $diff, $before_content, $lines_added, $lines_removed, $reverted_by, $note_path]]
        :put edit_event {
            note_id, event_id =>
            actor_type, actor_id, intent, timestamp,
            diff, before_content, lines_added, lines_removed, reverted_by, note_path
        }
    "#;
    db.run_script(script, params, ScriptMutability::Mutable)
        .map_err(|e| format!("mark_reverted failed: {e}"))?;
    Ok(())
}

/// Drop the oldest events for `note_id` until at most `cap` remain.
/// Called best-effort after every `record_event`. Cap = 0 disables.
pub fn prune_oldest(db: &DbInstance, note_id: &str, cap: usize) -> Result<(), String> {
    if cap == 0 {
        return Ok(());
    }
    let mut params = BTreeMap::new();
    params.insert(
        "note_id".to_string(),
        DataValue::Str(note_id.to_string().into()),
    );

    let count_script = r#"
        ?[count(event_id)] := *edit_event{ note_id, event_id }, note_id == $note_id
    "#;
    let count_res = db
        .run_script(count_script, params.clone(), ScriptMutability::Immutable)
        .map_err(|e| format!("prune count failed: {e}"))?;
    let total = match count_res.rows.first().and_then(|r| r.first()) {
        Some(DataValue::Num(cozo::Num::Int(i))) => *i as usize,
        _ => return Ok(()),
    };
    if total <= cap {
        return Ok(());
    }

    let to_drop = total - cap;
    // Pull the oldest `to_drop` ids and remove them. We can't `:rm` on
    // a sub-query directly (Cozo requires the full key), so collect ids
    // first then issue one rm-script per batch.
    let ids_script = r#"
        ?[event_id] := *edit_event{ note_id, event_id }, note_id == $note_id
        :order event_id
        :limit $limit
    "#;
    let mut id_params = params.clone();
    id_params.insert(
        "limit".to_string(),
        DataValue::Num(cozo::Num::Int(to_drop as i64)),
    );
    let ids_res = db
        .run_script(ids_script, id_params, ScriptMutability::Immutable)
        .map_err(|e| format!("prune id-fetch failed: {e}"))?;

    for row in ids_res.rows.iter() {
        let Some(eid) = row.first().and_then(|v| v.get_str()) else {
            continue;
        };
        let mut rm_params = BTreeMap::new();
        rm_params.insert(
            "note_id".to_string(),
            DataValue::Str(note_id.to_string().into()),
        );
        rm_params.insert(
            "event_id".to_string(),
            DataValue::Str(eid.to_string().into()),
        );
        let rm_script = r#"
            ?[note_id, event_id] <- [[$note_id, $event_id]]
            :rm edit_event { note_id, event_id }
        "#;
        if let Err(e) = db.run_script(rm_script, rm_params, ScriptMutability::Mutable) {
            tracing::warn!(%note_id, %eid, error = %e, "prune rm failed");
        }
    }
    Ok(())
}

/// Re-key every event from `old_note_id` to `new_note_id` and update
/// each row's `note_path`. Preserves `event_id`, `timestamp`,
/// `before_content`, `diff`, line counts, and `reverted_by` so revert
/// chains stay intact across renames.
///
/// Implementation: walk the old note's events, write each one back
/// under the new key, then drop the old rows. Two-phase rather than
/// one fused script because Cozo's `:put` doesn't easily support
/// renaming a key column inside the projection.
///
/// Best-effort tolerant: a write failure mid-loop leaves duplicates in
/// the new id (which appear in the History pane twice for that event)
/// — acceptable, the alternative is silently losing history. Caller
/// logs failures.
pub fn migrate_events_to_new_id(
    db: &DbInstance,
    old_note_id: &str,
    new_note_id: &str,
    new_note_path: &str,
) -> Result<(), String> {
    let events = list_events(db, old_note_id, 0)?;
    if events.is_empty() {
        return Ok(());
    }

    for ev in &events {
        let mut params = BTreeMap::new();
        params.insert(
            "note_id".to_string(),
            DataValue::Str(new_note_id.to_string().into()),
        );
        params.insert(
            "event_id".to_string(),
            DataValue::Str(ev.event_id.clone().into()),
        );
        params.insert(
            "actor_type".to_string(),
            DataValue::Str(ev.actor_type.clone().into()),
        );
        params.insert(
            "actor_id".to_string(),
            DataValue::Str(ev.actor_id.clone().into()),
        );
        params.insert(
            "intent".to_string(),
            DataValue::Str(ev.intent.clone().into()),
        );
        params.insert(
            "timestamp".to_string(),
            DataValue::Num(cozo::Num::Float(ev.timestamp)),
        );
        params.insert("diff".to_string(), DataValue::Str(ev.diff.clone().into()));
        params.insert(
            "before_content".to_string(),
            DataValue::Str(ev.before_content.clone().into()),
        );
        params.insert(
            "lines_added".to_string(),
            DataValue::Num(cozo::Num::Int(ev.lines_added)),
        );
        params.insert(
            "lines_removed".to_string(),
            DataValue::Num(cozo::Num::Int(ev.lines_removed)),
        );
        params.insert(
            "reverted_by".to_string(),
            DataValue::Str(ev.reverted_by.clone().into()),
        );
        params.insert(
            "note_path".to_string(),
            DataValue::Str(new_note_path.to_string().into()),
        );

        let script = r#"
            ?[note_id, event_id, actor_type, actor_id, intent, timestamp,
              diff, before_content, lines_added, lines_removed, reverted_by, note_path] <-
                [[$note_id, $event_id, $actor_type, $actor_id, $intent, $timestamp,
                  $diff, $before_content, $lines_added, $lines_removed, $reverted_by, $note_path]]
            :put edit_event {
                note_id, event_id =>
                actor_type, actor_id, intent, timestamp,
                diff, before_content, lines_added, lines_removed, reverted_by, note_path
            }
        "#;
        if let Err(e) = db.run_script(script, params, ScriptMutability::Mutable) {
            tracing::warn!(
                %old_note_id,
                %new_note_id,
                event_id = %ev.event_id,
                error = %e,
                "migrate_events_to_new_id: write failed for one row"
            );
        }
    }

    purge_note_events(db, old_note_id)?;
    Ok(())
}

/// Drop every event for a note. Called by `purge_note` when a file is
/// deleted/renamed, so orphan history doesn't outlive the note.
pub fn purge_note_events(db: &DbInstance, note_id: &str) -> Result<(), String> {
    let mut params = BTreeMap::new();
    params.insert(
        "note_id".to_string(),
        DataValue::Str(note_id.to_string().into()),
    );
    let script = r#"
        ?[note_id, event_id] := *edit_event{ note_id, event_id }, note_id == $note_id
        :rm edit_event { note_id, event_id }
    "#;
    db.run_script(script, params, ScriptMutability::Mutable)
        .map_err(|e| format!("purge_note_events failed: {e}"))?;
    Ok(())
}

/// EH-3: user-facing per-note history purge. Like `purge_note_events` but
/// (a) returns the number of events removed and (b) ALSO clears the
/// `edit_event_after_hash` sidecar rows for the note so no orphan hashes
/// linger. Distinct from `purge_note_events` (which the rename/migrate path
/// deliberately keeps sidecar-agnostic) so those internal callers are
/// unaffected. GDPR: this is the "erase this note's edit trail" primitive.
pub fn purge_note_history(db: &DbInstance, note_id: &str) -> Result<usize, String> {
    let count = list_events(db, note_id, 0)?.len();
    purge_note_events(db, note_id)?;
    // Sidecar (best-effort — a leftover hash only weakens divergence
    // detection for a note whose events are gone anyway).
    let mut params = BTreeMap::new();
    params.insert(
        "note_id".to_string(),
        DataValue::Str(note_id.to_string().into()),
    );
    let _ = db.run_script(
        r#"?[note_id, event_id] := *edit_event_after_hash{ note_id, event_id }, note_id == $note_id
           :rm edit_event_after_hash { note_id, event_id }"#,
        params,
        ScriptMutability::Mutable,
    );
    Ok(count)
}

/// EH-3: roost-wide history clear. Drops EVERY `:edit_event` row (and its
/// `edit_event_after_hash` sidecar) across all notes and returns how many
/// events were removed. Used by the Settings "Clear edit history" control
/// and any full erase-my-data flow. Irreversible and device-local.
pub fn purge_all_events(db: &DbInstance) -> Result<usize, String> {
    let count_res = db
        .run_script(
            r#"?[count(event_id)] := *edit_event{ event_id }"#,
            Default::default(),
            ScriptMutability::Immutable,
        )
        .map_err(|e| format!("purge_all count failed: {e}"))?;
    let total = match count_res.rows.first().and_then(|r| r.first()) {
        Some(DataValue::Num(cozo::Num::Int(i))) => *i as usize,
        _ => 0,
    };
    if total == 0 {
        return Ok(0);
    }
    db.run_script(
        r#"?[note_id, event_id] := *edit_event{ note_id, event_id }
           :rm edit_event { note_id, event_id }"#,
        Default::default(),
        ScriptMutability::Mutable,
    )
    .map_err(|e| format!("purge_all_events failed: {e}"))?;
    // Sidecar — best-effort.
    let _ = db.run_script(
        r#"?[note_id, event_id] := *edit_event_after_hash{ note_id, event_id }
           :rm edit_event_after_hash { note_id, event_id }"#,
        Default::default(),
        ScriptMutability::Mutable,
    );
    Ok(total)
}

/// EH-3: list EVERY event across all notes, newest-first. Backs the
/// roost-wide export. `event_id` is a fixed-width millisecond stamp so a
/// plain `-event_id` order is chronological across notes too.
pub fn list_all_events(db: &DbInstance) -> Result<Vec<EditEvent>, String> {
    let script = r#"
        ?[note_id, event_id, actor_type, actor_id, intent, timestamp,
          diff, before_content, lines_added, lines_removed, reverted_by, note_path] :=
            *edit_event{
                note_id, event_id, actor_type, actor_id, intent, timestamp,
                diff, before_content, lines_added, lines_removed, reverted_by, note_path
            }
        :order -event_id
    "#;
    let result = db
        .run_script(script, Default::default(), ScriptMutability::Immutable)
        .map_err(|e| format!("list_all_events failed: {e}"))?;

    let mut rows: Vec<EditEvent> = Vec::with_capacity(result.rows.len());
    for r in result.rows.iter() {
        if r.len() < 12 {
            continue;
        }
        let note_id = r[0].get_str().unwrap_or_default().to_string();
        let event_id = r[1].get_str().unwrap_or_default().to_string();
        let actor_type = r[2].get_str().unwrap_or_default().to_string();
        let actor_id = r[3].get_str().unwrap_or_default().to_string();
        let intent = r[4].get_str().unwrap_or_default().to_string();
        let timestamp = match &r[5] {
            DataValue::Num(cozo::Num::Float(f)) => *f,
            DataValue::Num(cozo::Num::Int(i)) => *i as f64,
            _ => 0.0,
        };
        let diff = r[6].get_str().unwrap_or_default().to_string();
        let before_content = r[7].get_str().unwrap_or_default().to_string();
        let lines_added = match &r[8] {
            DataValue::Num(cozo::Num::Int(i)) => *i,
            _ => 0,
        };
        let lines_removed = match &r[9] {
            DataValue::Num(cozo::Num::Int(i)) => *i,
            _ => 0,
        };
        let reverted_by = r[10].get_str().unwrap_or_default().to_string();
        let note_path = r[11].get_str().unwrap_or_default().to_string();
        rows.push(EditEvent {
            note_id,
            event_id,
            actor_type,
            actor_id,
            intent,
            timestamp,
            diff,
            before_content,
            lines_added,
            lines_removed,
            reverted_by,
            note_path,
        });
    }
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cozo::DbInstance;

    fn fresh_db() -> DbInstance {
        let db = DbInstance::new("mem", "", Default::default()).expect("mem db");
        db.run_script(
            r#":create edit_event {
                note_id: String,
                event_id: String,
                =>
                actor_type: String,
                actor_id: String,
                intent: String,
                timestamp: Float,
                diff: String,
                before_content: String,
                lines_added: Int,
                lines_removed: Int,
                reverted_by: String,
                note_path: String
            }"#,
            Default::default(),
            ScriptMutability::Mutable,
        )
        .expect("create relation");
        db.run_script(
            r#":create edit_event_after_hash {
                note_id: String,
                event_id: String,
                =>
                after_hash: String
            }"#,
            Default::default(),
            ScriptMutability::Mutable,
        )
        .expect("create after_hash sidecar");
        db
    }

    #[test]
    fn compute_diff_counts_added_and_removed_lines() {
        let (_diff, added, removed) = compute_diff("a\nb\nc\n", "a\nb-modified\nc\nd\n");
        assert_eq!(added, 2, "modified b + new d count as inserts");
        assert_eq!(removed, 1, "old b counts as a delete");
    }

    #[test]
    fn compute_diff_no_change_is_zero_zero() {
        let (_diff, added, removed) = compute_diff("same\n", "same\n");
        assert_eq!(added, 0);
        assert_eq!(removed, 0);
    }

    #[test]
    fn record_then_list_round_trips() {
        let db = fresh_db();
        let id = record_event(
            &db,
            "notes/foo",
            "/roost/notes/foo.md",
            Actor::User,
            "user",
            Intent::Edit,
            "before\n",
            "after\n",
        )
        .expect("record");
        assert!(!id.is_empty());

        let events = list_events(&db, "notes/foo", 10).expect("list");
        assert_eq!(events.len(), 1);
        let e = &events[0];
        assert_eq!(e.event_id, id);
        assert_eq!(e.intent, "edit");
        assert_eq!(e.actor_type, "user");
        assert_eq!(e.before_content, "before\n");
        assert_eq!(e.lines_added, 1);
        assert_eq!(e.lines_removed, 1);
        assert_eq!(e.reverted_by, "");
    }

    #[test]
    fn external_actor_records_as_external_string() {
        let db = fresh_db();
        record_event(
            &db,
            "notes/ext",
            "/roost/notes/ext.md",
            Actor::External,
            "external",
            Intent::Edit,
            "before\n",
            "after\n",
        )
        .expect("record external");
        let events = list_events(&db, "notes/ext", 10).expect("list");
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0].actor_type, "external",
            "external write must be attributed to the External actor"
        );
    }

    #[test]
    fn delete_event_after_hash_is_the_empty_content_hash() {
        // record_external_delete dedups the app's own delete by checking
        // whether the note's latest after_hash is already the hash of empty
        // content (delete_file records a delete event with after=""). Lock that
        // signal so the dedup can't silently rot.
        let db = fresh_db();
        record_event(
            &db,
            "notes/d",
            "/roost/notes/d.md",
            Actor::User,
            "user",
            Intent::Delete,
            "gone\n",
            "",
        )
        .expect("record delete");
        let h = latest_after_hash(&db, "notes/d")
            .expect("query")
            .expect("after_hash present");
        assert_eq!(
            h,
            content_hash(""),
            "a delete's after_hash must equal the empty-content hash the dedup checks"
        );
    }

    #[test]
    fn external_does_not_coalesce_into_a_prior_user_edit() {
        // A user edit followed immediately by an external (MCP/Obsidian) write
        // must stay TWO rows — coalescing keys on actor_type, so the external
        // change can't be silently folded into the user's edit and mislabeled.
        let db = fresh_db();
        record_event(
            &db,
            "notes/n",
            "/roost/notes/n.md",
            Actor::User,
            "user",
            Intent::Edit,
            "v0\n",
            "v1\n",
        )
        .expect("user edit");
        record_event(
            &db,
            "notes/n",
            "/roost/notes/n.md",
            Actor::External,
            "external",
            Intent::Edit,
            "v1\n",
            "v2\n",
        )
        .expect("external edit");
        let events = list_events(&db, "notes/n", 10).expect("list");
        assert_eq!(events.len(), 2, "different actors must not coalesce");
        let actors: Vec<&str> = events.iter().map(|e| e.actor_type.as_str()).collect();
        assert!(actors.contains(&"user") && actors.contains(&"external"));
    }

    #[test]
    fn external_edits_coalesce_with_each_other_preserving_first_before() {
        // A burst of external writes to the same note (same actor + intent, in
        // window) coalesces to one row whose before_content is the START of the
        // burst — so the diff view shows the whole external change, not just
        // the last sliver.
        let db = fresh_db();
        record_event(
            &db,
            "notes/b",
            "/roost/notes/b.md",
            Actor::External,
            "external",
            Intent::Edit,
            "a\n",
            "a\nb\n",
        )
        .expect("ext 1");
        record_event(
            &db,
            "notes/b",
            "/roost/notes/b.md",
            Actor::External,
            "external",
            Intent::Edit,
            "a\nb\n",
            "a\nb\nc\n",
        )
        .expect("ext 2");
        let events = list_events(&db, "notes/b", 10).expect("list");
        assert_eq!(events.len(), 1, "same-actor burst coalesces");
        assert_eq!(
            events[0].before_content, "a\n",
            "coalesce preserves the first burst's before_content"
        );
    }

    #[test]
    fn list_returns_newest_first() {
        let db = fresh_db();
        // Different actors force two distinct rows even within the
        // coalesce window — coalescing only merges same-actor edits.
        let _ = record_event(
            &db,
            "n",
            "/n.md",
            Actor::User,
            "user",
            Intent::Edit,
            "v0",
            "v1",
        )
        .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let id2 = record_event(
            &db,
            "n",
            "/n.md",
            Actor::Agent,
            "agent",
            Intent::Edit,
            "v1",
            "v2",
        )
        .unwrap();
        let events = list_events(&db, "n", 10).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event_id, id2, "newest event must come first");
    }

    #[test]
    fn list_isolates_by_note_id() {
        let db = fresh_db();
        let _ = record_event(
            &db,
            "a",
            "/a.md",
            Actor::User,
            "user",
            Intent::Edit,
            "x",
            "y",
        )
        .unwrap();
        let _ = record_event(
            &db,
            "b",
            "/b.md",
            Actor::User,
            "user",
            Intent::Edit,
            "p",
            "q",
        )
        .unwrap();
        assert_eq!(list_events(&db, "a", 0).unwrap().len(), 1);
        assert_eq!(list_events(&db, "b", 0).unwrap().len(), 1);
        assert_eq!(list_events(&db, "c", 0).unwrap().len(), 0);
    }

    #[test]
    fn mark_reverted_sets_reverted_by() {
        let db = fresh_db();
        let original = record_event(
            &db,
            "n",
            "/n.md",
            Actor::User,
            "user",
            Intent::Edit,
            "v0",
            "v1",
        )
        .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(2));
        // Different actor on the revert keeps the two rows distinct
        // through the coalesce window.
        let revert = record_event(
            &db,
            "n",
            "/n.md",
            Actor::System,
            "watcher",
            Intent::Edit,
            "v1",
            "v0",
        )
        .unwrap();
        mark_reverted(&db, "n", &original, &revert).expect("mark");

        let events = list_events(&db, "n", 0).unwrap();
        let original_row = events.iter().find(|e| e.event_id == original).unwrap();
        assert_eq!(original_row.reverted_by, revert);
    }

    #[test]
    fn prune_drops_oldest_until_cap() {
        let db = fresh_db();
        // Alternate actors per event so coalescing doesn't merge them
        // — we want 5 distinct rows so the prune-to-2 assertion is
        // exercising prune logic, not the coalesce path.
        let actors = [
            (Actor::User, "user"),
            (Actor::Agent, "agent"),
            (Actor::System, "watcher"),
            (Actor::User, "user2"),
            (Actor::Agent, "agent2"),
        ];
        let mut ids = Vec::new();
        for (i, (actor, aid)) in actors.iter().enumerate() {
            let id = record_event(
                &db,
                "n",
                "/n.md",
                *actor,
                aid,
                Intent::Edit,
                &format!("v{i}"),
                &format!("v{}", i + 1),
            )
            .unwrap();
            ids.push(id);
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        prune_oldest(&db, "n", 2).expect("prune");

        let events = list_events(&db, "n", 0).unwrap();
        assert_eq!(events.len(), 2);
        // Newest-first; the surviving two should be the last two we recorded.
        assert_eq!(events[0].event_id, ids[4]);
        assert_eq!(events[1].event_id, ids[3]);
    }

    #[test]
    fn prune_noop_below_cap() {
        let db = fresh_db();
        let actors = [
            (Actor::User, "user"),
            (Actor::Agent, "agent"),
            (Actor::System, "watcher"),
        ];
        for (i, (actor, aid)) in actors.iter().enumerate() {
            let _ = record_event(
                &db,
                "n",
                "/n.md",
                *actor,
                aid,
                Intent::Edit,
                &format!("v{i}"),
                &format!("v{}", i + 1),
            )
            .unwrap();
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        prune_oldest(&db, "n", 10).expect("prune");
        assert_eq!(list_events(&db, "n", 0).unwrap().len(), 3);
    }

    #[test]
    fn purge_note_events_drops_only_target_note() {
        let db = fresh_db();
        let _ = record_event(
            &db,
            "a",
            "/a.md",
            Actor::User,
            "user",
            Intent::Edit,
            "x",
            "y",
        )
        .unwrap();
        let _ = record_event(
            &db,
            "b",
            "/b.md",
            Actor::User,
            "user",
            Intent::Edit,
            "p",
            "q",
        )
        .unwrap();
        purge_note_events(&db, "a").unwrap();
        assert_eq!(list_events(&db, "a", 0).unwrap().len(), 0);
        assert_eq!(list_events(&db, "b", 0).unwrap().len(), 1);
    }

    #[test]
    fn coalesce_same_actor_same_intent_within_window_returns_same_id() {
        let db = fresh_db();
        let id1 = record_event(
            &db,
            "n",
            "/n.md",
            Actor::User,
            "user",
            Intent::Edit,
            "a",
            "b",
        )
        .unwrap();
        let id2 = record_event(
            &db,
            "n",
            "/n.md",
            Actor::User,
            "user",
            Intent::Edit,
            "b",
            "c",
        )
        .unwrap();
        // Coalesced — second call updated the existing row in-place.
        assert_eq!(id1, id2);
        assert_eq!(list_events(&db, "n", 0).unwrap().len(), 1);
    }

    #[test]
    fn coalesce_preserves_original_before_content() {
        let db = fresh_db();
        record_event(
            &db,
            "n",
            "/n.md",
            Actor::User,
            "user",
            Intent::Edit,
            "v0",
            "v1",
        )
        .unwrap();
        record_event(
            &db,
            "n",
            "/n.md",
            Actor::User,
            "user",
            Intent::Edit,
            "v1",
            "v2",
        )
        .unwrap();
        let events = list_events(&db, "n", 0).unwrap();
        assert_eq!(events.len(), 1);
        // Coalesced row's `before_content` is the FIRST burst's before
        // — revert restores the start of the typing session, not the
        // intermediate state.
        assert_eq!(events[0].before_content, "v0");
        // Diff is computed against that original before, so it shows
        // v0 → v2 not v1 → v2.
        assert!(
            events[0].diff.contains("-v0"),
            "diff should reference original 'v0' not intermediate 'v1', got: {}",
            events[0].diff
        );
    }

    #[test]
    fn no_coalesce_across_actors() {
        let db = fresh_db();
        record_event(
            &db,
            "n",
            "/n.md",
            Actor::User,
            "user",
            Intent::Edit,
            "a",
            "b",
        )
        .unwrap();
        record_event(
            &db,
            "n",
            "/n.md",
            Actor::Agent,
            "agent",
            Intent::Edit,
            "b",
            "c",
        )
        .unwrap();
        // Different actors stay as separate audit rows even within the
        // window — agent and user edits should never be conflated.
        assert_eq!(list_events(&db, "n", 0).unwrap().len(), 2);
    }

    #[test]
    fn no_coalesce_across_intents() {
        let db = fresh_db();
        record_event(
            &db,
            "n",
            "/n.md",
            Actor::User,
            "user",
            Intent::Edit,
            "a",
            "b",
        )
        .unwrap();
        record_event(
            &db,
            "n",
            "/n.md",
            Actor::User,
            "user",
            Intent::ToggleTask,
            "b",
            "c",
        )
        .unwrap();
        assert_eq!(list_events(&db, "n", 0).unwrap().len(), 2);
    }

    #[test]
    fn no_coalesce_after_revert_chain() {
        let db = fresh_db();
        let original = record_event(
            &db,
            "n",
            "/n.md",
            Actor::User,
            "user",
            Intent::Edit,
            "v0",
            "v1",
        )
        .unwrap();
        // Mark the original as reverted so the coalesce check on the
        // next edit refuses to merge into it.
        mark_reverted(&db, "n", &original, "fake-revert-id").unwrap();
        record_event(
            &db,
            "n",
            "/n.md",
            Actor::User,
            "user",
            Intent::Edit,
            "v1",
            "v2",
        )
        .unwrap();
        assert_eq!(list_events(&db, "n", 0).unwrap().len(), 2);
    }

    #[test]
    fn migrate_events_preserves_event_ids_and_rekeys_note() {
        let db = fresh_db();
        // Distinct actors prevent the coalesce path from merging the
        // two events — we want both rows to survive the migrate intact.
        let id_a = record_event(
            &db,
            "old-id",
            "/old/path.md",
            Actor::User,
            "user",
            Intent::Edit,
            "v0",
            "v1",
        )
        .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let id_b = record_event(
            &db,
            "old-id",
            "/old/path.md",
            Actor::Agent,
            "agent",
            Intent::Edit,
            "v1",
            "v2",
        )
        .unwrap();

        migrate_events_to_new_id(&db, "old-id", "new-id", "/new/path.md").unwrap();

        // Old id is empty; new id has both events with the same ids.
        assert_eq!(list_events(&db, "old-id", 0).unwrap().len(), 0);
        let migrated = list_events(&db, "new-id", 0).unwrap();
        assert_eq!(migrated.len(), 2);
        let ids: Vec<&String> = migrated.iter().map(|e| &e.event_id).collect();
        assert!(ids.contains(&&id_a));
        assert!(ids.contains(&&id_b));
        assert!(migrated.iter().all(|e| e.note_path == "/new/path.md"));
    }

    #[test]
    fn migrate_preserves_revert_chain() {
        let db = fresh_db();
        let original = record_event(
            &db,
            "old",
            "/old.md",
            Actor::User,
            "user",
            Intent::Edit,
            "v0",
            "v1",
        )
        .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let revert = record_event(
            &db,
            "old",
            "/old.md",
            Actor::System,
            "watcher",
            Intent::Edit,
            "v1",
            "v0",
        )
        .unwrap();
        mark_reverted(&db, "old", &original, &revert).unwrap();

        migrate_events_to_new_id(&db, "old", "new", "/new.md").unwrap();

        let events = list_events(&db, "new", 0).unwrap();
        let original_row = events
            .iter()
            .find(|e| e.event_id == original)
            .expect("original event survives migrate");
        assert_eq!(
            original_row.reverted_by, revert,
            "revert chain must survive migrate"
        );
    }

    #[test]
    fn migrate_no_events_is_noop() {
        let db = fresh_db();
        migrate_events_to_new_id(&db, "ghost", "new", "/new.md").unwrap();
        assert_eq!(list_events(&db, "new", 0).unwrap().len(), 0);
    }

    #[test]
    fn after_hash_is_recorded_for_each_event() {
        let db = fresh_db();
        let id = record_event(
            &db,
            "n",
            "/n.md",
            Actor::User,
            "user",
            Intent::Edit,
            "before\n",
            "after\n",
        )
        .unwrap();
        let h = latest_after_hash(&db, "n").unwrap();
        assert_eq!(h, Some(content_hash("after\n")));
        // And it's keyed by the event id so a re-record (different note)
        // doesn't shadow this one.
        let _ = id;
    }

    #[test]
    fn latest_after_hash_returns_most_recent_event() {
        let db = fresh_db();
        record_event(
            &db,
            "n",
            "/n.md",
            Actor::User,
            "user",
            Intent::Edit,
            "v0",
            "v1",
        )
        .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(2));
        record_event(
            &db,
            "n",
            "/n.md",
            Actor::Agent,
            "agent",
            Intent::Edit,
            "v1",
            "v2",
        )
        .unwrap();
        assert_eq!(
            latest_after_hash(&db, "n").unwrap(),
            Some(content_hash("v2")),
            "latest event's after_hash, not the first one"
        );
    }

    #[test]
    fn latest_after_hash_is_none_for_unknown_note() {
        let db = fresh_db();
        assert_eq!(latest_after_hash(&db, "ghost").unwrap(), None);
    }

    #[test]
    fn coalesce_refreshes_after_hash() {
        let db = fresh_db();
        let _ = record_event(
            &db,
            "n",
            "/n.md",
            Actor::User,
            "user",
            Intent::Edit,
            "v0",
            "v1",
        )
        .unwrap();
        // Same actor + intent within coalesce window — the existing event
        // is updated in place rather than a new row written.
        let _ = record_event(
            &db,
            "n",
            "/n.md",
            Actor::User,
            "user",
            Intent::Edit,
            "v0",
            "v2",
        )
        .unwrap();
        assert_eq!(
            latest_after_hash(&db, "n").unwrap(),
            Some(content_hash("v2")),
            "sidecar must reflect the latest post-state, not the first burst's"
        );
    }

    // ── EH-3: purge_note_history / purge_all_events / list_all_events ────

    #[test]
    fn purge_note_history_removes_rows_and_returns_count() {
        let db = fresh_db();
        // Two distinct rows on note "a" (different actors dodge coalesce).
        record_event(
            &db,
            "a",
            "/a.md",
            Actor::User,
            "user",
            Intent::Edit,
            "x",
            "y",
        )
        .unwrap();
        record_event(
            &db,
            "a",
            "/a.md",
            Actor::Agent,
            "agent",
            Intent::Edit,
            "y",
            "z",
        )
        .unwrap();
        record_event(
            &db,
            "b",
            "/b.md",
            Actor::User,
            "user",
            Intent::Edit,
            "p",
            "q",
        )
        .unwrap();

        let removed = purge_note_history(&db, "a").unwrap();
        assert_eq!(removed, 2, "returns the number of events removed");
        assert_eq!(list_events(&db, "a", 0).unwrap().len(), 0, "note a cleared");
        assert_eq!(
            list_events(&db, "b", 0).unwrap().len(),
            1,
            "note b untouched"
        );
        // Sidecar for the purged note is gone too.
        assert_eq!(latest_after_hash(&db, "a").unwrap(), None);
        assert!(latest_after_hash(&db, "b").unwrap().is_some());
    }

    #[test]
    fn purge_note_history_on_empty_note_returns_zero() {
        let db = fresh_db();
        assert_eq!(purge_note_history(&db, "ghost").unwrap(), 0);
    }

    #[test]
    fn purge_all_events_clears_every_note_and_counts() {
        let db = fresh_db();
        record_event(
            &db,
            "a",
            "/a.md",
            Actor::User,
            "user",
            Intent::Edit,
            "x",
            "y",
        )
        .unwrap();
        record_event(
            &db,
            "b",
            "/b.md",
            Actor::User,
            "user",
            Intent::Edit,
            "p",
            "q",
        )
        .unwrap();
        record_event(
            &db,
            "c",
            "/c.md",
            Actor::User,
            "user",
            Intent::Edit,
            "m",
            "n",
        )
        .unwrap();

        let removed = purge_all_events(&db).unwrap();
        assert_eq!(removed, 3, "counts all events across all notes");
        assert!(list_all_events(&db).unwrap().is_empty());
        // All sidecars gone.
        assert_eq!(latest_after_hash(&db, "a").unwrap(), None);
        assert_eq!(latest_after_hash(&db, "c").unwrap(), None);
    }

    #[test]
    fn purge_all_events_on_empty_db_returns_zero() {
        let db = fresh_db();
        assert_eq!(purge_all_events(&db).unwrap(), 0);
    }

    #[test]
    fn list_all_events_spans_notes_newest_first() {
        let db = fresh_db();
        record_event(
            &db,
            "a",
            "/a.md",
            Actor::User,
            "user",
            Intent::Edit,
            "x",
            "y",
        )
        .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let newest = record_event(
            &db,
            "b",
            "/b.md",
            Actor::User,
            "user",
            Intent::Edit,
            "p",
            "q",
        )
        .unwrap();

        let all = list_all_events(&db).unwrap();
        assert_eq!(all.len(), 2, "both notes' events surface");
        assert_eq!(all[0].event_id, newest, "newest across notes comes first");
        // note_id is populated correctly per row (index 0 of the projection).
        let ids: Vec<&str> = all.iter().map(|e| e.note_id.as_str()).collect();
        assert!(ids.contains(&"a") && ids.contains(&"b"));
    }
}
