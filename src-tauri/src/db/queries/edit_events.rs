//! Edit history backend (Phase 2.2 — the trust gate before agent CRUD).
//!
//! Every mutation that touches markdown on disk is recorded as a row in
//! the `:edit_event` relation. The History pane renders these rows; the
//! `revert_edit` command walks them backwards. This module is the only
//! place that talks to `:edit_event` — call sites use `record_event`,
//! `list_events`, `get_event`, `mark_reverted`, `prune_note_to_budget`.
//!
//! EH-4: **edit history is device-local and does NOT sync.** The
//! `:edit_event` relation lives in the per-roost `graph.db` under the app
//! data dir — it is never part of the CRDT sync payload (sync carries
//! markdown deltas only), so each device keeps its own independent record
//! of the edits it made. Users can purge (per-note or roost-wide) and
//! export the audit trail from Settings -> Privacy (EH-3).
//!
//! CO-2 retention. Every event stores `before_content` verbatim plus a diff,
//! so a count cap alone let `graph.db` grow with note size x events. History
//! is now bounded four ways, ALWAYS oldest-first and NEVER dropping a note's
//! newest event (so the latest revert point survives any budget):
//!   1. per note by `EVENTS_PER_NOTE_CAP` events AND `NOTE_BYTES_BUDGET`
//!      stored bytes, enforced on every non-coalesced record
//!      (`prune_note_to_budget`);
//!   2. globally by `GLOBAL_BYTES_BUDGET`, enforced by the low-frequency
//!      maintenance pass (`enforce_global_budget` / `plan_global_prune`,
//!      driven by `commands::edit_history::run_history_maintenance`);
//!   3. history of notes that no longer exist (not on disk AND not in the
//!      trash) is reaped once the note's newest event is older than
//!      `ORPHAN_HISTORY_RETENTION_SECS` (`reap_orphan_history`);
//!   4. a note's `:edit_event_after_hash` sidecar row is removed in lockstep
//!      with every event row. ALL removals go through the `rm_*` choke points
//!      below; a guard test fails if a new `:rm edit_event` appears elsewhere.
//!
//! "Stored bytes" = `before_content.len() + diff.len()` per event (the two
//! unbounded columns). Nothing is summarised or rewritten (locked decision
//! #12): pruning only deletes whole old events, and every prune is logged and
//! tallied in `kv_meta` (`prune_stats`, surfaced in the JSON export).
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
//!    Storage cost is bounded by the per-note event cap + byte budget and
//!    the global byte budget (see the CO-2 paragraph above).

use cozo::{DataValue, DbInstance, ScriptMutability};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap};
use std::time::{SystemTime, UNIX_EPOCH};

/// Maximum events kept per note. Older events are pruned when this is
/// exceeded. Hardcoded for now; promote to a roost setting in a follow-up
/// once the Settings UI grows an "Edit history retention" knob.
pub const EVENTS_PER_NOTE_CAP: usize = 500;

/// CO-2: per-note budget on stored bytes (`before_content` + `diff`).
///
/// 16 MiB: a typical 10 KB note at the full 500-event cap stores ~5.5 MB, so
/// ordinary notes NEVER hit this and keep all 500 events. It only binds for
/// large notes (200 KB -> ~80 events kept, 1 MB -> ~16) where 500 x size would
/// otherwise reach hundreds of MB for ONE note. A single event bigger than the
/// budget is still kept (the newest event is never pruned).
pub const NOTE_BYTES_BUDGET: u64 = 16 * 1024 * 1024;

/// CO-2: global budget on stored bytes across ALL notes of one roost.
///
/// Desktop 512 MiB: generous against the realistic 5K-100K-note scale (a
/// 5,000-note roost with 30 retained 10 KB events per note is ~1.5 GB only if
/// EVERY note were edited that much; real use is a small active subset) while
/// still giving `graph.db` a hard ceiling. Mobile 128 MiB: 5K-note scale and
/// scarce phone storage; history there is a convenience, not an archive.
/// Enforced oldest-first by the maintenance pass, so the first things to go are
/// the oldest events of notes nobody has touched for the longest time.
#[cfg(any(target_os = "android", target_os = "ios"))]
pub const GLOBAL_BYTES_BUDGET: u64 = 128 * 1024 * 1024;
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub const GLOBAL_BYTES_BUDGET: u64 = 512 * 1024 * 1024;

/// CO-2: history of a note that no longer exists is kept this long after its
/// newest event (normally the delete event), then reaped.
///
/// 365 days. NOTE the code-verified facts behind this number: the app's trash
/// (`.corvux-trash/`, `roostfs::trash`) has NO retention of its own - entries
/// stay until the user empties the folder - and the 180-day figure in the
/// project notes is the SYNC tombstone window (`TOMBSTONE_RETENTION_SECS`), not
/// a trash window. So 365 days is 2x the tombstone window (a device offline for
/// ~6 months can still bring a deleted note back), and the reaper additionally
/// refuses any note whose name still appears in the trash (see
/// `commands::edit_history`), because a trashed note can be restored forever.
pub const ORPHAN_HISTORY_RETENTION_SECS: f64 = 365.0 * 24.0 * 60.0 * 60.0;

/// CO-2: what a prune removed. Returned (and logged / tallied) so a drop is
/// never silent.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PruneReport {
    /// Whole events removed.
    pub events: usize,
    /// `before_content` + `diff` bytes those events held.
    pub bytes: u64,
}

impl PruneReport {
    pub fn is_empty(&self) -> bool {
        self.events == 0
    }

    pub fn merge(&mut self, other: PruneReport) {
        self.events += other.events;
        self.bytes += other.bytes;
    }
}

/// One event's stored size, as measured by [`note_event_sizes`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventSize {
    pub event_id: String,
    pub bytes: u64,
}

/// Per-note retention limits applied on every record. `0` disables a limit.
#[derive(Debug, Clone, Copy)]
pub struct NoteLimits {
    pub max_events: usize,
    pub max_bytes: u64,
}

impl NoteLimits {
    pub const DEFAULT: NoteLimits = NoteLimits {
        max_events: EVENTS_PER_NOTE_CAP,
        max_bytes: NOTE_BYTES_BUDGET,
    };
}

/// Stored size of one event: the two unbounded columns, in UTF-8 bytes.
fn payload_bytes(before_content: &str, diff: &str) -> u64 {
    before_content.len() as u64 + diff.len() as u64
}

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
    record_event_with_limits(
        db,
        note_id,
        note_path,
        actor,
        actor_id,
        intent,
        before,
        after,
        NoteLimits::DEFAULT,
    )
    .map(|(event_id, _pruned)| event_id)
}

/// The post-state hash the `:edit_event_after_hash` sidecar stores for an
/// event of `intent` whose post-write content is `after`.
///
/// Normally `hash(after)` - what the file holds once the write lands. A
/// `Rename` event is the exception: its `before`/`after` are PATH stamps
/// ("path: old" / "path: new"), not file content, and a rename leaves the
/// content untouched. Hashing the stamp (as this code did before CO-2) made
/// the note's newest `after_hash` never match disk, so after ANY rename every
/// revert needed `force` and the external-write dedup in
/// `commands::edit_history` stopped working. A rename therefore CARRIES FORWARD
/// the note's previous newest hash (already re-keyed by
/// [`migrate_events_to_new_id`]); `None` when there is none (a legacy note with
/// no sidecar) so no bogus hash is invented.
fn after_hash_for(
    db: &DbInstance,
    note_id: &str,
    intent: Intent,
    after: &str,
) -> Result<Option<String>, String> {
    match intent {
        Intent::Rename => latest_after_hash(db, note_id),
        _ => Ok(Some(content_hash(after))),
    }
}

/// [`record_event`] with explicit per-note limits, returning what the
/// post-insert prune removed. Coalesced writes (same burst) return an empty
/// report: they rewrite one existing row and never add an event, so there is
/// nothing to prune (their diff can drift by at most one event's worth until
/// the next insert or maintenance pass).
#[allow(clippy::too_many_arguments)]
pub fn record_event_with_limits(
    db: &DbInstance,
    note_id: &str,
    note_path: &str,
    actor: Actor,
    actor_id: &str,
    intent: Intent,
    before: &str,
    after: &str,
    limits: NoteLimits,
) -> Result<(String, PruneReport), String> {
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
            match after_hash_for(db, note_id, intent, after) {
                Ok(Some(h)) => {
                    if let Err(e) = put_after_hash(db, note_id, &prev.event_id, &h) {
                        tracing::warn!(
                            %note_id, error = %e,
                            "edit_events: after_hash refresh on coalesce failed"
                        );
                    }
                }
                Ok(None) => {}
                Err(e) => tracing::warn!(
                    %note_id, error = %e,
                    "edit_events: after_hash lookup on coalesce failed"
                ),
            }
            return Ok((prev.event_id, PruneReport::default()));
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
    match after_hash_for(db, note_id, intent, after) {
        Ok(Some(h)) => {
            if let Err(e) = put_after_hash(db, note_id, &event_id, &h) {
                tracing::warn!(%note_id, error = %e, "edit_events: after_hash sidecar write failed");
            }
        }
        Ok(None) => {}
        Err(e) => {
            tracing::warn!(%note_id, error = %e, "edit_events: after_hash lookup failed");
        }
    }

    // Best-effort prune (count cap + byte budget, oldest-first, never the
    // newest event). A failure here just means this note's history is over
    // budget until the next save — never user-visible. A SUCCESSFUL prune is
    // logged and tallied so dropping history is never silent (CO-2).
    let pruned = match prune_note_to_budget(db, note_id, limits.max_events, limits.max_bytes) {
        Ok(report) => {
            if !report.is_empty() {
                tracing::info!(
                    %note_id,
                    pruned_events = report.events,
                    pruned_bytes = report.bytes,
                    "edit_events: pruned oldest history over the per-note budget"
                );
                record_prune_stats(db, &report);
            }
            report
        }
        Err(e) => {
            tracing::warn!(%note_id, error = %e, "edit_events: prune after record failed");
            PruneReport::default()
        }
    };

    Ok((event_id, pruned))
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

    // CO-2: push `limit` into the query. `most_recent_event` (called on EVERY
    // save for the coalesce check) used to pull all <=500 rows with their full
    // `before_content` just to keep the first one.
    let mut script = String::from(
        r#"
        ?[event_id, actor_type, actor_id, intent, timestamp,
          diff, before_content, lines_added, lines_removed, reverted_by, note_path] :=
            *edit_event{
                note_id, event_id, actor_type, actor_id, intent, timestamp,
                diff, before_content, lines_added, lines_removed, reverted_by, note_path
            },
            note_id == $note_id
        :order -event_id
    "#,
    );
    if limit > 0 {
        script.push_str("        :limit $limit\n");
        params.insert(
            "limit".to_string(),
            DataValue::Num(cozo::Num::Int(limit as i64)),
        );
    }

    let result = db
        .run_script(&script, params, ScriptMutability::Immutable)
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

/// Like [`mark_reverted`], but a missing target is `Ok(false)` instead of an
/// error. `revert_edit` records the revert event FIRST (which prunes), so with
/// a byte budget the event being reverted can be the very oldest row that the
/// prune just dropped; failing the whole revert AFTER the file was already
/// rewritten and synced would be wrong. A pruned target cannot be reverted
/// twice anyway, so there is nothing left to mark.
pub fn mark_reverted_if_present(
    db: &DbInstance,
    note_id: &str,
    event_id: &str,
    revert_event_id: &str,
) -> Result<bool, String> {
    if get_event(db, note_id, event_id)?.is_none() {
        return Ok(false);
    }
    mark_reverted(db, note_id, event_id, revert_event_id)?;
    Ok(true)
}

// ── CO-2: removal choke points ─────────────────────────────────────────
//
// `:edit_event` and its `:edit_event_after_hash` sidecar must never drift:
// before CO-2, `prune_oldest` and the rename path removed events without the
// sidecar (orphan hashes forever; after a rename the new id had NO sidecar
// rows, so divergence detection silently degraded). Every `:rm edit_event` in
// this crate now lives in one of the three `rm_*` functions below, each of
// which removes the sidecar rows too. The guard test
// `edit_event_removals_only_in_choke_points` fails on any new site.

/// Keys per `:rm` script. Keeps each script's param list bounded.
const RM_CHUNK: usize = 400;

/// Remove specific events (`(note_id, event_id)` keys) AND their sidecar rows.
/// The event removal is authoritative (its error is returned); the sidecar
/// removal is best-effort, because an orphan hash only weakens divergence
/// detection and `sweep_orphan_after_hashes` removes it later.
fn rm_event_keys(db: &DbInstance, keys: &[(String, String)]) -> Result<(), String> {
    for chunk in keys.chunks(RM_CHUNK) {
        let rows: Vec<DataValue> = chunk
            .iter()
            .map(|(n, e)| {
                DataValue::List(vec![
                    DataValue::Str(n.as_str().into()),
                    DataValue::Str(e.as_str().into()),
                ])
            })
            .collect();
        let mut params = BTreeMap::new();
        params.insert("rows".to_string(), DataValue::List(rows));
        db.run_script(
            r#"?[note_id, event_id] <- $rows
               :rm edit_event { note_id, event_id }"#,
            params.clone(),
            ScriptMutability::Mutable,
        )
        .map_err(|e| format!("rm_event_keys failed: {e}"))?;
        if let Err(e) = db.run_script(
            r#"?[note_id, event_id] <- $rows
               :rm edit_event_after_hash { note_id, event_id }"#,
            params,
            ScriptMutability::Mutable,
        ) {
            tracing::warn!(error = %e, "edit_events: sidecar removal failed (orphan hash left for the sweep)");
        }
    }
    Ok(())
}

/// Remove EVERY event of one note and EVERY sidecar row of that note (the
/// sidecar is queried by its own key, so orphan hashes are cleaned too).
fn rm_note_rows(db: &DbInstance, note_id: &str) -> Result<(), String> {
    let mut params = BTreeMap::new();
    params.insert(
        "note_id".to_string(),
        DataValue::Str(note_id.to_string().into()),
    );
    db.run_script(
        r#"?[note_id, event_id] := *edit_event{ note_id, event_id }, note_id == $note_id
           :rm edit_event { note_id, event_id }"#,
        params.clone(),
        ScriptMutability::Mutable,
    )
    .map_err(|e| format!("rm_note_rows failed: {e}"))?;
    if let Err(e) = db.run_script(
        r#"?[note_id, event_id] := *edit_event_after_hash{ note_id, event_id }, note_id == $note_id
           :rm edit_event_after_hash { note_id, event_id }"#,
        params,
        ScriptMutability::Mutable,
    ) {
        tracing::warn!(%note_id, error = %e, "edit_events: sidecar purge failed (orphan hashes left for the sweep)");
    }
    Ok(())
}

/// Remove every event and every sidecar row in the roost.
fn rm_all_rows(db: &DbInstance) -> Result<(), String> {
    db.run_script(
        r#"?[note_id, event_id] := *edit_event{ note_id, event_id }
           :rm edit_event { note_id, event_id }"#,
        Default::default(),
        ScriptMutability::Mutable,
    )
    .map_err(|e| format!("rm_all_rows failed: {e}"))?;
    if let Err(e) = db.run_script(
        r#"?[note_id, event_id] := *edit_event_after_hash{ note_id, event_id }
           :rm edit_event_after_hash { note_id, event_id }"#,
        Default::default(),
        ScriptMutability::Mutable,
    ) {
        tracing::warn!(error = %e, "edit_events: sidecar wipe failed");
    }
    Ok(())
}

// ── CO-2: byte accounting + pruning ────────────────────────────────────

/// Stored size of every event of `note_id`, ascending by `event_id` (oldest
/// first). Reads each row's two payload columns once; Cozo's own `length()`
/// counts chars not bytes, so the byte measure has to happen here.
pub fn note_event_sizes(db: &DbInstance, note_id: &str) -> Result<Vec<EventSize>, String> {
    let mut params = BTreeMap::new();
    params.insert(
        "note_id".to_string(),
        DataValue::Str(note_id.to_string().into()),
    );
    let res = db
        .run_script(
            r#"
            ?[event_id, before_content, diff] :=
                *edit_event{ note_id, event_id, before_content, diff },
                note_id == $note_id
            :order event_id
            "#,
            params,
            ScriptMutability::Immutable,
        )
        .map_err(|e| format!("note_event_sizes failed: {e}"))?;
    let mut out = Vec::with_capacity(res.rows.len());
    for r in &res.rows {
        if r.len() < 3 {
            continue;
        }
        out.push(EventSize {
            event_id: r[0].get_str().unwrap_or_default().to_string(),
            bytes: payload_bytes(
                r[1].get_str().unwrap_or_default(),
                r[2].get_str().unwrap_or_default(),
            ),
        });
    }
    Ok(out)
}

/// How many of the OLDEST events (`sizes` ascending by `event_id`) must go so
/// that the note satisfies BOTH limits. Never the newest event, so the result
/// is at most `len - 1`. `0` disables a limit. Pure, so the arithmetic is
/// unit-tested without a DB.
pub fn plan_note_prune(sizes: &[EventSize], max_events: usize, max_bytes: u64) -> usize {
    let n = sizes.len();
    if n <= 1 {
        return 0;
    }
    let mut count = n;
    let mut bytes: u64 = sizes.iter().map(|s| s.bytes).sum();
    let mut drop = 0usize;
    while drop < n - 1
        && ((max_events > 0 && count > max_events) || (max_bytes > 0 && bytes > max_bytes))
    {
        bytes = bytes.saturating_sub(sizes[drop].bytes);
        count -= 1;
        drop += 1;
    }
    drop
}

/// Drop the oldest events of `note_id` until at most `max_events` events AND
/// at most `max_bytes` stored bytes remain (`0` disables a limit). Never drops
/// the note's newest event. Removes the sidecar rows in lockstep. Called
/// best-effort after every non-coalesced `record_event`.
pub fn prune_note_to_budget(
    db: &DbInstance,
    note_id: &str,
    max_events: usize,
    max_bytes: u64,
) -> Result<PruneReport, String> {
    if max_events == 0 && max_bytes == 0 {
        return Ok(PruneReport::default());
    }
    let sizes = note_event_sizes(db, note_id)?;
    let drop = plan_note_prune(&sizes, max_events, max_bytes);
    if drop == 0 {
        return Ok(PruneReport::default());
    }
    let victims = &sizes[..drop];
    let keys: Vec<(String, String)> = victims
        .iter()
        .map(|s| (note_id.to_string(), s.event_id.clone()))
        .collect();
    rm_event_keys(db, &keys)?;
    Ok(PruneReport {
        events: drop,
        bytes: victims.iter().map(|s| s.bytes).sum(),
    })
}

/// Distinct note ids that have at least one event.
pub fn list_note_ids(db: &DbInstance) -> Result<Vec<String>, String> {
    let res = db
        .run_script(
            r#"?[note_id] := *edit_event{ note_id }"#,
            Default::default(),
            ScriptMutability::Immutable,
        )
        .map_err(|e| format!("list_note_ids failed: {e}"))?;
    Ok(res
        .rows
        .iter()
        .filter_map(|r| r.first().and_then(|v| v.get_str()).map(str::to_string))
        .collect())
}

/// One event the global pass decided to drop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GlobalVictim {
    pub note_id: String,
    pub event_id: String,
    pub bytes: u64,
}

/// Decide which events to drop so the roost-wide total fits `budget`
/// (`0` disables). `inventory` is `(note_id, that note's event sizes)`.
/// Oldest events (by `event_id`, a fixed-width ms stamp) go first ACROSS
/// notes, but each note's newest event is untouchable, so the result can stop
/// above `budget` if the newest events alone exceed it (reported by the
/// caller; never pruned). Pure.
pub fn plan_global_prune(inventory: &[(String, Vec<EventSize>)], budget: u64) -> Vec<GlobalVictim> {
    if budget == 0 {
        return Vec::new();
    }
    let total: u64 = inventory
        .iter()
        .flat_map(|(_, sizes)| sizes.iter().map(|s| s.bytes))
        .sum();
    if total <= budget {
        return Vec::new();
    }
    let mut candidates: Vec<GlobalVictim> = Vec::new();
    for (note_id, sizes) in inventory {
        let Some(newest) = sizes.iter().map(|s| s.event_id.as_str()).max() else {
            continue;
        };
        for s in sizes {
            if s.event_id != newest {
                candidates.push(GlobalVictim {
                    note_id: note_id.clone(),
                    event_id: s.event_id.clone(),
                    bytes: s.bytes,
                });
            }
        }
    }
    candidates.sort_by(|a, b| {
        a.event_id
            .cmp(&b.event_id)
            .then_with(|| a.note_id.cmp(&b.note_id))
    });
    let mut remaining = total;
    let mut out = Vec::new();
    for c in candidates {
        if remaining <= budget {
            break;
        }
        remaining = remaining.saturating_sub(c.bytes);
        out.push(c);
    }
    out
}

/// Remove the events a global plan selected (sidecar rows in lockstep).
pub fn delete_global_victims(
    db: &DbInstance,
    victims: &[GlobalVictim],
) -> Result<PruneReport, String> {
    if victims.is_empty() {
        return Ok(PruneReport::default());
    }
    let keys: Vec<(String, String)> = victims
        .iter()
        .map(|v| (v.note_id.clone(), v.event_id.clone()))
        .collect();
    rm_event_keys(db, &keys)?;
    Ok(PruneReport {
        events: victims.len(),
        bytes: victims.iter().map(|v| v.bytes).sum(),
    })
}

/// One-shot global enforcement over a single connection: inventory, plan,
/// delete. Test-only composition: the production maintenance pass runs the same
/// three primitives but releases the graph lock between notes
/// (`commands::edit_history::enforce_global`).
#[cfg(test)]
pub fn enforce_global_budget(db: &DbInstance, budget: u64) -> Result<PruneReport, String> {
    let mut inventory = Vec::new();
    for note_id in list_note_ids(db)? {
        let sizes = note_event_sizes(db, &note_id)?;
        inventory.push((note_id, sizes));
    }
    let victims = plan_global_prune(&inventory, budget);
    delete_global_victims(db, &victims)
}

/// Remove `:edit_event_after_hash` rows that have no matching `:edit_event`
/// (left behind by the pre-CO-2 `prune_oldest`, which never touched the
/// sidecar, and by any interrupted removal). Returns how many were removed.
pub fn sweep_orphan_after_hashes(db: &DbInstance) -> Result<usize, String> {
    let orphans = r#"
        ?[note_id, event_id] :=
            *edit_event_after_hash{ note_id, event_id },
            not *edit_event{ note_id, event_id }
    "#;
    let res = db
        .run_script(orphans, Default::default(), ScriptMutability::Immutable)
        .map_err(|e| format!("sweep_orphan_after_hashes scan failed: {e}"))?;
    let keys: Vec<(String, String)> = res
        .rows
        .iter()
        .filter_map(|r| {
            Some((
                r.first()?.get_str()?.to_string(),
                r.get(1)?.get_str()?.to_string(),
            ))
        })
        .collect();
    for chunk in keys.chunks(RM_CHUNK) {
        let rows: Vec<DataValue> = chunk
            .iter()
            .map(|(n, e)| {
                DataValue::List(vec![
                    DataValue::Str(n.as_str().into()),
                    DataValue::Str(e.as_str().into()),
                ])
            })
            .collect();
        let mut params = BTreeMap::new();
        params.insert("rows".to_string(), DataValue::List(rows));
        db.run_script(
            r#"?[note_id, event_id] <- $rows
               :rm edit_event_after_hash { note_id, event_id }"#,
            params,
            ScriptMutability::Mutable,
        )
        .map_err(|e| format!("sweep_orphan_after_hashes rm failed: {e}"))?;
    }
    Ok(keys.len())
}

// ── CO-2: prune tally (kv_meta) ────────────────────────────────────────

/// `kv_meta` key holding `"<events>,<bytes>,<last_unix_seconds>"`. No schema
/// change: `kv_meta` is a generic key/value relation every `graph.db` has.
const KV_PRUNE_STATS: &str = "edit_history_prune_stats";

/// Cumulative prune tally for this roost's `graph.db`.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct PruneStats {
    pub events: u64,
    pub bytes: u64,
    /// Unix seconds of the most recent prune, if any.
    pub last_at: Option<f64>,
}

/// Read the cumulative tally. A missing/garbled key reads as "never pruned".
pub fn prune_stats(db: &DbInstance) -> PruneStats {
    let Ok(Some(raw)) = crate::db::schema::get_kv(db, KV_PRUNE_STATS) else {
        return PruneStats::default();
    };
    let mut it = raw.split(',');
    let events = it.next().and_then(|s| s.parse().ok());
    let bytes = it.next().and_then(|s| s.parse().ok());
    let last_at = it.next().and_then(|s| s.parse::<f64>().ok());
    match (events, bytes) {
        (Some(events), Some(bytes)) => PruneStats {
            events,
            bytes,
            last_at,
        },
        _ => PruneStats::default(),
    }
}

/// Add `report` to the cumulative tally. Best-effort: bookkeeping must never
/// fail a save, and the structured log line is the primary record.
pub fn record_prune_stats(db: &DbInstance, report: &PruneReport) {
    if report.is_empty() {
        return;
    }
    let cur = prune_stats(db);
    let value = format!(
        "{},{},{}",
        cur.events + report.events as u64,
        cur.bytes + report.bytes,
        now_seconds()
    );
    if let Err(e) = crate::db::schema::set_kv(db, KV_PRUNE_STATS, &value) {
        tracing::debug!(error = %e, "edit_events: prune tally not persisted");
    }
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
/// The `:edit_event_after_hash` sidecar is re-keyed with the events (CO-2):
/// before, a rename dropped every hash, so the renamed note had no divergence
/// signal until its next edit. The old rows (events AND sidecar) are removed
/// ONLY when every event row was copied; if any copy fails the old rows are
/// kept (they are then orphans the reaper clears after the retention window)
/// and an error is returned — never silently losing history.
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

    // Sidecar rows to carry over, keyed by event id. A read failure only
    // degrades divergence detection for this note (same as a legacy row).
    let hashes = note_after_hashes(db, old_note_id).unwrap_or_else(|e| {
        tracing::warn!(%old_note_id, error = %e, "migrate_events_to_new_id: sidecar read failed");
        HashMap::new()
    });
    let mut failed = 0usize;

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
        match db.run_script(script, params, ScriptMutability::Mutable) {
            Ok(_) => {
                // Re-key the sidecar row with its event. Best-effort per row
                // (a missing hash only weakens divergence detection for that
                // row, exactly like a pre-v4 event).
                if let Some(h) = hashes.get(&ev.event_id) {
                    if let Err(e) = put_after_hash(db, new_note_id, &ev.event_id, h) {
                        tracing::warn!(
                            %old_note_id,
                            %new_note_id,
                            event_id = %ev.event_id,
                            error = %e,
                            "migrate_events_to_new_id: sidecar re-key failed for one row"
                        );
                    }
                }
            }
            Err(e) => {
                failed += 1;
                tracing::warn!(
                    %old_note_id,
                    %new_note_id,
                    event_id = %ev.event_id,
                    error = %e,
                    "migrate_events_to_new_id: write failed for one row"
                );
            }
        }
    }

    if failed > 0 {
        // Keep the old rows: they are the only copy of the rows that failed.
        return Err(format!(
            "migrate_events_to_new_id: {failed} of {} rows failed to copy; old rows kept",
            events.len()
        ));
    }
    purge_note_events(db, old_note_id)?;
    Ok(())
}

/// Sidecar rows of one note as `event_id -> after_hash`.
fn note_after_hashes(db: &DbInstance, note_id: &str) -> Result<HashMap<String, String>, String> {
    let mut params = BTreeMap::new();
    params.insert(
        "note_id".to_string(),
        DataValue::Str(note_id.to_string().into()),
    );
    let res = db
        .run_script(
            r#"
            ?[event_id, after_hash] :=
                *edit_event_after_hash{ note_id, event_id, after_hash },
                note_id == $note_id
            "#,
            params,
            ScriptMutability::Immutable,
        )
        .map_err(|e| format!("note_after_hashes failed: {e}"))?;
    Ok(res
        .rows
        .iter()
        .filter_map(|r| {
            Some((
                r.first()?.get_str()?.to_string(),
                r.get(1)?.get_str()?.to_string(),
            ))
        })
        .collect())
}

/// Drop every event for a note AND its `edit_event_after_hash` sidecar rows.
/// Used by the rename/migrate path (after the rows were re-keyed), the
/// user-facing purge, and the orphan reaper — so no caller can leave a
/// sidecar behind (CO-2; this used to be deliberately sidecar-agnostic).
pub fn purge_note_events(db: &DbInstance, note_id: &str) -> Result<(), String> {
    rm_note_rows(db, note_id)
}

/// EH-3: user-facing per-note history purge. Like `purge_note_events` but
/// returns the number of events removed. GDPR: this is the "erase this
/// note's edit trail" primitive.
pub fn purge_note_history(db: &DbInstance, note_id: &str) -> Result<usize, String> {
    let count = list_events(db, note_id, 0)?.len();
    purge_note_events(db, note_id)?;
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
    // Unconditional (even at 0 events): an erase-my-data control must also
    // clear any orphan sidecar hashes a past prune left behind.
    rm_all_rows(db)?;
    Ok(total)
}

// ── CO-2: orphan reaper (storage half) ─────────────────────────────────

/// A note whose newest event is older than the retention cutoff - a reaping
/// CANDIDATE only. Whether the note really is gone (not on disk, not in the
/// trash) is decided by the caller from the filesystem
/// (`commands::edit_history`); this module never touches the roost.
#[derive(Debug, Clone, PartialEq)]
pub struct OrphanCandidate {
    pub note_id: String,
    /// `note_path` of the note's newest event (display path at record time).
    pub note_path: String,
    pub newest_ts: f64,
    pub events: usize,
}

/// What a reap removed (test-only; see [`reap_orphan_history`]).
#[cfg(test)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReapReport {
    pub notes: usize,
    pub events: usize,
    /// Candidates the caller chose to keep (live, or restorable from trash).
    pub kept: usize,
}

/// Notes whose NEWEST event is older than `cutoff_ts` (unix seconds), oldest
/// first. One aggregate scan; cheap enough for the weekly maintenance pass.
pub fn orphan_candidates(db: &DbInstance, cutoff_ts: f64) -> Result<Vec<OrphanCandidate>, String> {
    let res = db
        .run_script(
            r#"
            ?[note_id, max(timestamp), count(event_id)] :=
                *edit_event{ note_id, event_id, timestamp }
            "#,
            Default::default(),
            ScriptMutability::Immutable,
        )
        .map_err(|e| format!("orphan_candidates failed: {e}"))?;
    let mut out = Vec::new();
    for r in &res.rows {
        if r.len() < 3 {
            continue;
        }
        let newest_ts = match &r[1] {
            DataValue::Num(cozo::Num::Float(f)) => *f,
            DataValue::Num(cozo::Num::Int(i)) => *i as f64,
            _ => continue, // unreadable timestamp: never a reaping candidate
        };
        if newest_ts >= cutoff_ts {
            continue;
        }
        let note_id = r[0].get_str().unwrap_or_default().to_string();
        let events = match &r[2] {
            DataValue::Num(cozo::Num::Int(i)) => *i as usize,
            _ => 0,
        };
        let note_path = list_events(db, &note_id, 1)?
            .into_iter()
            .next()
            .map(|e| e.note_path)
            .unwrap_or_default();
        out.push(OrphanCandidate {
            note_id,
            note_path,
            newest_ts,
            events,
        });
    }
    out.sort_by(|a, b| {
        a.newest_ts
            .partial_cmp(&b.newest_ts)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    Ok(out)
}

/// Purge `note_id`'s history, but ONLY if its newest event is still older
/// than `cutoff_ts`. Re-checked under the caller's DB lock because the
/// candidate list is taken, then the filesystem is consulted (async, lock
/// released), and in between the note may have been re-created or edited.
/// Returns the number of events removed (0 when the note came back to life).
pub fn reap_note_if_still_orphan(
    db: &DbInstance,
    note_id: &str,
    cutoff_ts: f64,
) -> Result<usize, String> {
    match list_events(db, note_id, 1)?.into_iter().next() {
        None => Ok(0),
        Some(newest) if newest.timestamp >= cutoff_ts => Ok(0),
        Some(_) => purge_note_history(db, note_id),
    }
}

/// One-connection reap: every candidate for which `keep` returns false is
/// purged (events + sidecar). `keep` is the caller's "is this note live or
/// restorable?" decision. Test-only composition: the production pass runs the
/// same primitives across lock/await boundaries
/// (`commands::edit_history::reap_orphans`), because the keep decision needs
/// async filesystem facts.
#[cfg(test)]
pub fn reap_orphan_history(
    db: &DbInstance,
    now_secs: f64,
    retention_secs: f64,
    keep: &mut dyn FnMut(&OrphanCandidate) -> bool,
) -> Result<ReapReport, String> {
    let cutoff = now_secs - retention_secs;
    let mut report = ReapReport::default();
    for cand in orphan_candidates(db, cutoff)? {
        if keep(&cand) {
            report.kept += 1;
            continue;
        }
        let removed = reap_note_if_still_orphan(db, &cand.note_id, cutoff)?;
        if removed > 0 {
            report.notes += 1;
            report.events += removed;
        }
    }
    Ok(report)
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
        let report = prune_note_to_budget(&db, "n", 2, 0).expect("prune");
        assert_eq!(report.events, 3, "5 events pruned to a cap of 2 drops 3");

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
        let report = prune_note_to_budget(&db, "n", 10, 0).expect("prune");
        assert!(report.is_empty(), "below the cap nothing is pruned");
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

    // ── CO-2: byte budgets, sidecar lockstep, rename, reaper ─────────────

    /// Sorted `(note_id, event_id)` of every `:edit_event` row.
    fn event_keys(db: &DbInstance) -> Vec<(String, String)> {
        keys_of(db, "edit_event")
    }

    /// Sorted `(note_id, event_id)` of every `:edit_event_after_hash` row.
    fn sidecar_keys(db: &DbInstance) -> Vec<(String, String)> {
        keys_of(db, "edit_event_after_hash")
    }

    fn keys_of(db: &DbInstance, rel: &str) -> Vec<(String, String)> {
        let res = db
            .run_script(
                &format!("?[note_id, event_id] := *{rel}{{ note_id, event_id }}"),
                Default::default(),
                ScriptMutability::Immutable,
            )
            .expect("scan keys");
        let mut v: Vec<(String, String)> = res
            .rows
            .iter()
            .map(|r| {
                (
                    r[0].get_str().unwrap().to_string(),
                    r[1].get_str().unwrap().to_string(),
                )
            })
            .collect();
        v.sort();
        v
    }

    /// The invariant CO-2 exists to keep: every event has exactly one sidecar
    /// row and every sidecar row has an event (for events recorded through
    /// `record_event`).
    fn assert_lockstep(db: &DbInstance) {
        assert_eq!(
            event_keys(db),
            sidecar_keys(db),
            "edit_event and edit_event_after_hash must stay in lockstep"
        );
    }

    fn payload_total(db: &DbInstance, note_id: &str) -> u64 {
        note_event_sizes(db, note_id)
            .unwrap()
            .iter()
            .map(|s| s.bytes)
            .sum()
    }

    /// Record `n` distinct, non-coalescing events (actors cycle, so two
    /// consecutive events never share an actor) of roughly `len` bytes each.
    fn record_n(db: &DbInstance, note_id: &str, n: usize, len: usize) -> Vec<String> {
        let actors = [
            (Actor::User, "user"),
            (Actor::Agent, "agent"),
            (Actor::System, "watcher"),
            (Actor::External, "external"),
        ];
        let mut ids = Vec::new();
        for i in 0..n {
            let (actor, aid) = actors[i % actors.len()];
            let before = format!("{i}:{}", "x".repeat(len));
            let after = format!("{before}!");
            let id = record_event(
                db,
                note_id,
                &format!("/r/{note_id}.md"),
                actor,
                aid,
                Intent::Edit,
                &before,
                &after,
            )
            .unwrap();
            ids.push(id);
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        ids
    }

    fn fresh_db_with_kv() -> DbInstance {
        let db = fresh_db();
        db.run_script(
            r#":create kv_meta { key: String => value: String }"#,
            Default::default(),
            ScriptMutability::Mutable,
        )
        .expect("create kv_meta");
        db
    }

    /// Insert an event + sidecar row at an explicit timestamp (the reaper
    /// works on `timestamp`, which `record_event` always sets to "now").
    fn put_raw_event(db: &DbInstance, note_id: &str, ts: f64, note_path: &str) -> String {
        let event_id = format!("{:013}-{:08x}", (ts * 1000.0) as u64, 7);
        let mut p = BTreeMap::new();
        p.insert("n".to_string(), DataValue::Str(note_id.into()));
        p.insert("e".to_string(), DataValue::Str(event_id.clone().into()));
        p.insert("t".to_string(), DataValue::Num(cozo::Num::Float(ts)));
        p.insert("p".to_string(), DataValue::Str(note_path.into()));
        db.run_script(
            r#"?[note_id, event_id, actor_type, actor_id, intent, timestamp,
                 diff, before_content, lines_added, lines_removed, reverted_by, note_path]
               <- [[$n, $e, "user", "user", "edit", $t, "d", "before", 1, 1, "", $p]]
               :put edit_event { note_id, event_id => actor_type, actor_id, intent,
                 timestamp, diff, before_content, lines_added, lines_removed, reverted_by, note_path }"#,
            p,
            ScriptMutability::Mutable,
        )
        .expect("put raw event");
        put_after_hash(db, note_id, &event_id, "h").expect("put raw sidecar");
        event_id
    }

    fn sizes(v: &[u64]) -> Vec<EventSize> {
        v.iter()
            .enumerate()
            .map(|(i, b)| EventSize {
                event_id: format!("{i:013}-x"),
                bytes: *b,
            })
            .collect()
    }

    #[test]
    fn budget_constants_are_sane() {
        assert_eq!(EVENTS_PER_NOTE_CAP, 500, "existing count cap is unchanged");
        // Compile-time facts about constants: checked in a const block (clippy
        // `assertions_on_constants`), so a bad edit fails the build, not a run.
        const {
            assert!(NOTE_BYTES_BUDGET < GLOBAL_BYTES_BUDGET);
            assert!(
                ORPHAN_HISTORY_RETENTION_SECS >= 180.0 * 86_400.0,
                "orphan retention must be at least the 180-day sync tombstone window"
            );
        }
        // A typical 10 KB note at the full count cap must NOT hit the per-note
        // byte budget (the budget is for large notes only).
        assert!((EVENTS_PER_NOTE_CAP as u64) * (10 * 1024 + 2 * 1024) < NOTE_BYTES_BUDGET);
    }

    #[test]
    fn plan_note_prune_count_only() {
        assert_eq!(plan_note_prune(&sizes(&[1; 10]), 4, 0), 6);
        assert_eq!(plan_note_prune(&sizes(&[1; 3]), 4, 0), 0);
    }

    #[test]
    fn plan_note_prune_bytes_only() {
        // 10 x 100 = 1000 bytes; a 350-byte budget keeps the newest 3.
        assert_eq!(plan_note_prune(&sizes(&[100; 10]), 0, 350), 7);
        // Exactly at the budget is fine.
        assert_eq!(plan_note_prune(&sizes(&[100; 10]), 0, 1000), 0);
    }

    #[test]
    fn plan_note_prune_never_drops_the_newest_event() {
        // The newest event alone exceeds the budget: it is still kept.
        assert_eq!(plan_note_prune(&sizes(&[10, 10, 1000]), 0, 5), 2);
        assert_eq!(plan_note_prune(&sizes(&[5]), 1, 1), 0);
        assert_eq!(plan_note_prune(&sizes(&[]), 1, 1), 0);
    }

    #[test]
    fn plan_note_prune_zero_disables_a_limit_and_the_stricter_limit_wins() {
        assert_eq!(plan_note_prune(&sizes(&[5, 5, 5]), 0, 0), 0);
        // 6 x 10: max 5 events would drop 1, max 30 bytes would drop 3.
        assert_eq!(plan_note_prune(&sizes(&[10; 6]), 5, 30), 3);
    }

    #[test]
    fn byte_budget_prunes_oldest_keeps_newest_and_sidecar_in_sync() {
        let db = fresh_db();
        let ids = record_n(&db, "n", 6, 300);
        assert_lockstep(&db);
        let measured = note_event_sizes(&db, "n").unwrap();
        assert_eq!(measured.len(), 6);
        // Room for the newest two events only.
        let budget: u64 = measured.iter().rev().take(2).map(|s| s.bytes).sum::<u64>() + 1;

        let report = prune_note_to_budget(&db, "n", 0, budget).unwrap();
        assert_eq!(report.events, 4, "the four oldest go");
        assert_eq!(
            report.bytes,
            measured[..4].iter().map(|s| s.bytes).sum::<u64>(),
            "the report counts exactly the bytes removed"
        );
        let left: Vec<String> = list_events(&db, "n", 0)
            .unwrap()
            .into_iter()
            .map(|e| e.event_id)
            .collect();
        assert_eq!(
            left,
            vec![ids[5].clone(), ids[4].clone()],
            "newest two kept"
        );
        assert!(payload_total(&db, "n") <= budget);
        assert_lockstep(&db);

        // An absurdly small budget still keeps the newest event (and its hash).
        prune_note_to_budget(&db, "n", 0, 1).unwrap();
        let left = list_events(&db, "n", 0).unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].event_id, ids[5]);
        assert_lockstep(&db);
        assert!(latest_after_hash(&db, "n").unwrap().is_some());
    }

    #[test]
    fn count_prune_removes_the_sidecar_too() {
        // Regression (CO-2 class c): prune_oldest removed `:edit_event` rows
        // only, so every pruned event left an orphan sidecar hash forever.
        let db = fresh_db();
        record_n(&db, "n", 5, 10);
        prune_note_to_budget(&db, "n", 2, 0).unwrap();
        assert_eq!(event_keys(&db).len(), 2);
        assert_lockstep(&db);
    }

    #[test]
    fn record_event_enforces_limits_on_insert_and_reports_the_prune() {
        let db = fresh_db();
        let limits = NoteLimits {
            max_events: 3,
            max_bytes: 0,
        };
        let actors = [
            (Actor::User, "user"),
            (Actor::Agent, "agent"),
            (Actor::System, "watcher"),
        ];
        let mut total = PruneReport::default();
        for i in 0..5 {
            let (actor, aid) = actors[i % 3];
            let (_, rep) = record_event_with_limits(
                &db,
                "n",
                "/n.md",
                actor,
                aid,
                Intent::Edit,
                &format!("v{i}"),
                &format!("v{}", i + 1),
                limits,
            )
            .unwrap();
            total.merge(rep);
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert_eq!(list_events(&db, "n", 0).unwrap().len(), 3);
        assert_eq!(total.events, 2, "events 4 and 5 each pushed one out");
        assert!(total.bytes > 0, "the dropped payload is measured");
        assert_lockstep(&db);
    }

    #[test]
    fn byte_budget_on_insert_keeps_the_note_under_budget() {
        let db = fresh_db();
        let limits = NoteLimits {
            max_events: 0,
            max_bytes: 2_000,
        };
        let actors = [
            (Actor::User, "u"),
            (Actor::Agent, "a"),
            (Actor::System, "s"),
        ];
        let mut last = None;
        for i in 0..8 {
            let (actor, aid) = actors[i % 3];
            let before = format!("{i}{}", "y".repeat(300));
            let after = format!("{before}.");
            let (id, _) = record_event_with_limits(
                &db,
                "n",
                "/n.md",
                actor,
                aid,
                Intent::Edit,
                &before,
                &after,
                limits,
            )
            .unwrap();
            last = Some(id);
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert!(payload_total(&db, "n") <= 2_000);
        assert_eq!(
            list_events(&db, "n", 1).unwrap()[0].event_id,
            last.unwrap(),
            "the newest event is always kept"
        );
        assert_lockstep(&db);
    }

    #[test]
    fn prune_tally_accumulates_in_kv_meta() {
        let db = fresh_db_with_kv();
        assert_eq!(prune_stats(&db), PruneStats::default());
        record_prune_stats(
            &db,
            &PruneReport {
                events: 2,
                bytes: 100,
            },
        );
        record_prune_stats(
            &db,
            &PruneReport {
                events: 1,
                bytes: 50,
            },
        );
        record_prune_stats(&db, &PruneReport::default()); // no-op
        let s = prune_stats(&db);
        assert_eq!((s.events, s.bytes), (3, 150));
        assert!(s.last_at.is_some());
    }

    #[test]
    fn record_event_tallies_a_prune_for_the_export() {
        let db = fresh_db_with_kv();
        let limits = NoteLimits {
            max_events: 1,
            max_bytes: 0,
        };
        for (i, (actor, aid)) in [(Actor::User, "u"), (Actor::Agent, "a")].iter().enumerate() {
            record_event_with_limits(
                &db,
                "n",
                "/n.md",
                *actor,
                aid,
                Intent::Edit,
                &format!("v{i}"),
                &format!("v{}", i + 1),
                limits,
            )
            .unwrap();
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert_eq!(
            prune_stats(&db).events,
            1,
            "the drop was tallied, not silent"
        );
    }

    #[test]
    fn rename_keeps_after_hash_so_divergence_detection_survives() {
        // Mirrors `files.rs::migrate_edit_history_on_rename` exactly: migrate
        // the rows, then stamp a Rename event on the new id.
        let db = fresh_db();
        record_event(
            &db,
            "old",
            "/r/old.md",
            Actor::User,
            "user",
            Intent::Edit,
            "v0\n",
            "v1\n",
        )
        .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(2));
        record_event(
            &db,
            "old",
            "/r/old.md",
            Actor::Agent,
            "agent",
            Intent::Edit,
            "v1\n",
            "v2\n",
        )
        .unwrap();

        migrate_events_to_new_id(&db, "old", "new", "/r/new.md").unwrap();
        record_event(
            &db,
            "new",
            "/r/new.md",
            Actor::System,
            "rename",
            Intent::Rename,
            "path: /r/old.md\n",
            "path: /r/new.md\n",
        )
        .unwrap();

        // The newest recorded hash is still the CONTENT hash of what is on
        // disk — not the hash of the rename's path stamp, and not missing.
        assert_eq!(
            latest_after_hash(&db, "new").unwrap(),
            Some(content_hash("v2\n"))
        );
        assert_ne!(
            latest_after_hash(&db, "new").unwrap(),
            Some(content_hash("path: /r/new.md\n"))
        );
        // Each migrated event kept ITS OWN hash.
        let hashes = note_after_hashes(&db, "new").unwrap();
        assert_eq!(hashes.len(), 3, "2 migrated events + the rename event");
        assert!(hashes.values().any(|h| *h == content_hash("v1\n")));
        // Nothing is left under the old id, sidecar included.
        assert_eq!(latest_after_hash(&db, "old").unwrap(), None);
        assert!(list_events(&db, "old", 0).unwrap().is_empty());
        assert_lockstep(&db);
    }

    #[test]
    fn rename_event_on_a_legacy_note_invents_no_hash() {
        // A pre-v4 note has events but no sidecar rows. A rename must not
        // fabricate a hash (that would make every revert falsely "diverged").
        let db = fresh_db();
        record_event(
            &db,
            "old",
            "/r/old.md",
            Actor::User,
            "user",
            Intent::Edit,
            "a",
            "b",
        )
        .unwrap();
        db.run_script(
            r#"?[note_id, event_id] := *edit_event_after_hash{ note_id, event_id }
               :rm edit_event_after_hash { note_id, event_id }"#,
            Default::default(),
            ScriptMutability::Mutable,
        )
        .unwrap();

        migrate_events_to_new_id(&db, "old", "new", "/r/new.md").unwrap();
        record_event(
            &db,
            "new",
            "/r/new.md",
            Actor::System,
            "rename",
            Intent::Rename,
            "path: o\n",
            "path: n\n",
        )
        .unwrap();
        assert_eq!(latest_after_hash(&db, "new").unwrap(), None);
        assert_eq!(list_events(&db, "new", 0).unwrap().len(), 2);
    }

    #[test]
    fn purge_note_events_removes_the_sidecar_too() {
        let db = fresh_db();
        record_n(&db, "a", 2, 5);
        record_n(&db, "b", 2, 5);
        purge_note_events(&db, "a").unwrap();
        assert_eq!(latest_after_hash(&db, "a").unwrap(), None);
        assert!(latest_after_hash(&db, "b").unwrap().is_some());
        assert_lockstep(&db);
    }

    #[test]
    fn purge_all_clears_orphan_sidecars_even_with_zero_events() {
        let db = fresh_db();
        // Orphan hashes as a pre-CO-2 prune left them: no event rows at all.
        put_after_hash(&db, "ghost", "0000000000001-aaaaaaaa", "h1").unwrap();
        put_after_hash(&db, "ghost", "0000000000002-bbbbbbbb", "h2").unwrap();
        assert_eq!(purge_all_events(&db).unwrap(), 0);
        assert!(
            sidecar_keys(&db).is_empty(),
            "erase-my-data clears orphans too"
        );
    }

    #[test]
    fn sweep_removes_only_orphan_sidecars() {
        // Legacy-DB simulation: orphan hashes accumulated by the old prune.
        let db = fresh_db();
        record_n(&db, "a", 2, 5);
        put_after_hash(&db, "a", "0000000000001-aaaaaaaa", "old-pruned").unwrap();
        put_after_hash(&db, "ghost", "0000000000002-bbbbbbbb", "ghost").unwrap();
        assert_eq!(sweep_orphan_after_hashes(&db).unwrap(), 2);
        assert_lockstep(&db);
        assert_eq!(event_keys(&db).len(), 2, "no event row is touched");
        assert_eq!(sweep_orphan_after_hashes(&db).unwrap(), 0, "idempotent");
    }

    #[test]
    fn plan_global_prune_is_oldest_first_and_spares_each_notes_newest() {
        let inv = vec![
            ("a".to_string(), sizes(&[10, 10, 10])), // ids 0,1,2
            ("b".to_string(), {
                let mut s = sizes(&[10, 10, 10]);
                for (i, e) in s.iter_mut().enumerate() {
                    e.event_id = format!("{:013}-y", i + 10); // all newer than a's
                }
                s
            }),
        ];
        // total 60; budget 30 => drop 3 oldest non-newest: a0, a1, then b10.
        let v = plan_global_prune(&inv, 30);
        let got: Vec<(&str, &str)> = v
            .iter()
            .map(|g| (g.note_id.as_str(), g.event_id.as_str()))
            .collect();
        assert_eq!(
            got,
            vec![
                ("a", "0000000000000-x"),
                ("a", "0000000000001-x"),
                ("b", "0000000000010-y"),
            ]
        );
        assert!(
            !v.iter().any(|g| g.event_id == "0000000000002-x"),
            "a's newest event is never planned for deletion"
        );
        // Under budget and zero budget are no-ops.
        assert!(plan_global_prune(&inv, 60).is_empty());
        assert!(plan_global_prune(&inv, 0).is_empty());
    }

    #[test]
    fn plan_global_prune_stops_at_the_newest_events_floor() {
        let inv = vec![
            ("a".to_string(), sizes(&[100, 100])),
            ("b".to_string(), sizes(&[100, 100])),
        ];
        // Budget 1: only the two OLDER events can go; the floor stays above 1.
        let v = plan_global_prune(&inv, 1);
        assert_eq!(v.len(), 2);
    }

    #[test]
    fn global_budget_prunes_oldest_across_notes_and_keeps_each_newest() {
        let db = fresh_db();
        let a = record_n(&db, "a", 4, 200);
        let b = record_n(&db, "b", 4, 200);
        let total = payload_total(&db, "a") + payload_total(&db, "b");
        let budget = total / 2;

        let rep = enforce_global_budget(&db, budget).unwrap();
        assert!(rep.events >= 3, "roughly half of 8 events must go");
        let after = payload_total(&db, "a") + payload_total(&db, "b");
        assert!(after <= budget, "total {after} must fit {budget}");
        assert_eq!(rep.bytes, total - after, "the report is exact");

        // Each note's newest event survives.
        let a_ids: Vec<String> = list_events(&db, "a", 0)
            .unwrap()
            .into_iter()
            .map(|e| e.event_id)
            .collect();
        let b_ids: Vec<String> = list_events(&db, "b", 0)
            .unwrap()
            .into_iter()
            .map(|e| e.event_id)
            .collect();
        assert!(a_ids.contains(&a[3]) && b_ids.contains(&b[3]));
        // Oldest-first across notes: a (older) loses events before b does.
        assert!(a_ids.len() <= b_ids.len());
        assert_lockstep(&db);
    }

    #[test]
    fn global_budget_floor_is_one_event_per_note() {
        let db = fresh_db();
        let a = record_n(&db, "a", 4, 50);
        let b = record_n(&db, "b", 4, 50);
        let rep = enforce_global_budget(&db, 1).unwrap();
        assert_eq!(rep.events, 6);
        assert_eq!(event_keys(&db).len(), 2);
        assert_eq!(list_events(&db, "a", 0).unwrap()[0].event_id, a[3]);
        assert_eq!(list_events(&db, "b", 0).unwrap()[0].event_id, b[3]);
        assert_lockstep(&db);
    }

    #[test]
    fn byte_accounting_is_exact_across_revert_and_purge() {
        let db = fresh_db();
        let e1 = record_event(
            &db,
            "n",
            "/n.md",
            Actor::User,
            "user",
            Intent::Edit,
            "aaaa",
            "bbbb",
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
            "bbbb",
            "cccc",
        )
        .unwrap();

        // Independent recomputation straight from the stored rows.
        let recompute = |db: &DbInstance| -> u64 {
            list_events(db, "n", 0)
                .unwrap()
                .iter()
                .map(|e| (e.before_content.len() + e.diff.len()) as u64)
                .sum()
        };
        let base = payload_total(&db, "n");
        assert_eq!(base, recompute(&db));
        assert!(base > 0);

        // Marking an event reverted re-puts the row but changes no bytes.
        mark_reverted(&db, "n", &e1, "rv").unwrap();
        assert_eq!(
            payload_total(&db, "n"),
            base,
            "revert marking is byte-neutral"
        );

        // The revert event itself adds EXACTLY its own payload.
        std::thread::sleep(std::time::Duration::from_millis(2));
        let (rv, _) = record_event_with_limits(
            &db,
            "n",
            "/n.md",
            Actor::User,
            "user",
            Intent::Revert,
            "cccc",
            "aaaa",
            NoteLimits::DEFAULT,
        )
        .unwrap();
        let rv_row = get_event(&db, "n", &rv).unwrap().unwrap();
        assert_eq!(
            payload_total(&db, "n"),
            base + (rv_row.before_content.len() + rv_row.diff.len()) as u64
        );
        assert_eq!(payload_total(&db, "n"), recompute(&db));
        assert_lockstep(&db);

        // Purge returns every byte.
        assert_eq!(purge_note_history(&db, "n").unwrap(), 3);
        assert_eq!(payload_total(&db, "n"), 0);
        assert_lockstep(&db);
    }

    #[test]
    fn mark_reverted_if_present_tolerates_a_pruned_target() {
        let db = fresh_db();
        let limits = NoteLimits {
            max_events: 2,
            max_bytes: 0,
        };
        let (e1, _) = record_event_with_limits(
            &db,
            "n",
            "/n.md",
            Actor::User,
            "user",
            Intent::Edit,
            "v0",
            "v1",
            limits,
        )
        .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let (e2, _) = record_event_with_limits(
            &db,
            "n",
            "/n.md",
            Actor::Agent,
            "agent",
            Intent::Edit,
            "v1",
            "v2",
            limits,
        )
        .unwrap();

        // Present target: marked.
        assert!(mark_reverted_if_present(&db, "n", &e2, "rv-x").unwrap());
        assert_eq!(
            get_event(&db, "n", &e2).unwrap().unwrap().reverted_by,
            "rv-x"
        );

        // `revert_edit` records the revert event FIRST; at the budget edge that
        // prune drops the oldest row — which is the very event being reverted.
        std::thread::sleep(std::time::Duration::from_millis(2));
        let (rv, pruned) = record_event_with_limits(
            &db,
            "n",
            "/n.md",
            Actor::System,
            "watcher",
            Intent::Revert,
            "v2",
            "v0",
            limits,
        )
        .unwrap();
        assert_eq!(pruned.events, 1);
        assert!(get_event(&db, "n", &e1).unwrap().is_none(), "target pruned");
        // The strict variant errors (the old behaviour that failed a completed
        // revert); the tolerant one reports "nothing to mark".
        assert!(mark_reverted(&db, "n", &e1, &rv).is_err());
        assert!(!mark_reverted_if_present(&db, "n", &e1, &rv).unwrap());
    }

    #[test]
    fn reaper_reaps_only_old_orphans_and_honours_keep() {
        let db = fresh_db();
        let now = 1_800_000_000.0_f64;
        let old = now - ORPHAN_HISTORY_RETENTION_SECS - 10.0 * 86_400.0;
        put_raw_event(&db, "gone", old, "/r/gone.md");
        put_raw_event(&db, "live-old", old, "/r/live-old.md");
        put_raw_event(&db, "trashed-old", old, "/r/trashed-old.md");
        put_raw_event(
            &db,
            "recent-gone",
            now - 10.0 * 86_400.0,
            "/r/recent-gone.md",
        );

        let mut asked: Vec<String> = Vec::new();
        let mut keep = |c: &OrphanCandidate| {
            asked.push(c.note_id.clone());
            c.note_id == "live-old" || c.note_id == "trashed-old"
        };
        let rep = reap_orphan_history(&db, now, ORPHAN_HISTORY_RETENTION_SECS, &mut keep).unwrap();
        assert_eq!((rep.notes, rep.events, rep.kept), (1, 1, 2));
        asked.sort();
        assert_eq!(
            asked,
            vec!["gone", "live-old", "trashed-old"],
            "a note with a recent event is not even a candidate"
        );

        assert!(list_events(&db, "gone", 0).unwrap().is_empty());
        assert_eq!(
            latest_after_hash(&db, "gone").unwrap(),
            None,
            "sidecar reaped too"
        );
        for kept in ["live-old", "trashed-old", "recent-gone"] {
            assert_eq!(list_events(&db, kept, 0).unwrap().len(), 1, "{kept} kept");
            assert!(latest_after_hash(&db, kept).unwrap().is_some());
        }
        assert_lockstep(&db);
    }

    #[test]
    fn a_note_with_any_recent_event_is_never_an_orphan_candidate() {
        let db = fresh_db();
        let now = 1_800_000_000.0_f64;
        let old = now - ORPHAN_HISTORY_RETENTION_SECS - 86_400.0;
        put_raw_event(&db, "n", old, "/r/n.md");
        put_raw_event(&db, "n", now - 86_400.0, "/r/n.md");
        let cands = orphan_candidates(&db, now - ORPHAN_HISTORY_RETENTION_SECS).unwrap();
        assert!(cands.is_empty(), "the newest event decides, not the oldest");
    }

    #[test]
    fn reap_rechecks_the_newest_event_so_a_revived_note_survives() {
        let db = fresh_db();
        let now = 1_800_000_000.0_f64;
        let cutoff = now - ORPHAN_HISTORY_RETENTION_SECS;
        put_raw_event(&db, "n", cutoff - 86_400.0, "/r/n.md");
        let cands = orphan_candidates(&db, cutoff).unwrap();
        assert_eq!(cands.len(), 1);
        assert_eq!(cands[0].note_path, "/r/n.md");
        // Between the scan and the delete the note is re-created / edited.
        put_raw_event(&db, "n", now, "/r/n.md");
        assert_eq!(reap_note_if_still_orphan(&db, "n", cutoff).unwrap(), 0);
        assert_eq!(list_events(&db, "n", 0).unwrap().len(), 2, "history intact");
    }

    #[test]
    fn edit_event_removals_only_in_choke_points() {
        // Ratchet (CO-2): every `:rm edit_event` must sit in one of the
        // sidecar-aware choke points, so no future delete path can forget the
        // `:edit_event_after_hash` row again. The sidecar-only orphan sweep is
        // the one other legitimate remover (it only deletes rows with no event).
        use std::path::{Path, PathBuf};

        fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
            for entry in std::fs::read_dir(dir).unwrap().flatten() {
                let p = entry.path();
                if p.is_dir() {
                    walk(&p, out);
                } else if p.extension().is_some_and(|x| x == "rs") {
                    out.push(p);
                }
            }
        }

        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        walk(&src, &mut files);
        let mut problems: Vec<String> = Vec::new();
        for f in &files {
            let rel = f.to_string_lossy().replace('\\', "/");
            let text = std::fs::read_to_string(f).unwrap();
            if !rel.ends_with("db/queries/edit_events.rs") {
                if text.contains(":rm edit_event") {
                    problems.push(format!(
                        "{rel}: removes edit-history rows outside edit_events.rs"
                    ));
                }
                continue;
            }
            // Production half only.
            let prod = text.split("#[cfg(test)]\nmod tests").next().unwrap();
            let mut current_fn = String::new();
            for line in prod.lines() {
                let t = line.trim_start();
                if t.starts_with("//") {
                    continue;
                }
                if let Some(rest) = t.strip_prefix("pub fn ").or_else(|| t.strip_prefix("fn ")) {
                    current_fn = rest.split(['(', '<']).next().unwrap_or("").to_string();
                }
                let events_rm =
                    line.contains(":rm edit_event ") || line.contains(":rm edit_event{");
                let sidecar_rm = line.contains(":rm edit_event_after_hash");
                const EVENT_FNS: [&str; 3] = ["rm_event_keys", "rm_note_rows", "rm_all_rows"];
                if events_rm && !EVENT_FNS.contains(&current_fn.as_str()) {
                    problems.push(format!("`:rm edit_event` in fn `{current_fn}`"));
                }
                if sidecar_rm
                    && !EVENT_FNS.contains(&current_fn.as_str())
                    && current_fn != "sweep_orphan_after_hashes"
                {
                    problems.push(format!("`:rm edit_event_after_hash` in fn `{current_fn}`"));
                }
            }
        }
        assert!(
            problems.is_empty(),
            "edit-history rows must only be removed through the rm_* choke points:\n{}",
            problems.join("\n")
        );
    }
}
