//! Agent CRUD tools for tasks (Phase 2.2 Week 2).
//!
//! Seven tools the agent uses to manipulate the user's tasks:
//! `create_task`, `update_task`, `complete_task`, `reschedule_task`,
//! `archive_task`, `delete_task`, `query_tasks`.
//!
//! Every write tool records an `:edit_event` with `Actor::Agent` so the
//! History pane (Week 1 trust gate) can show and revert agent edits. No
//! agent write ever bypasses that recording — the trust gate is load-
//! bearing for the entire feature.
//!
//! Why we re-implement the file-mutation half here rather than calling
//! the existing `complete_task` / `add_task_line` Tauri commands: those
//! commands need a `tauri::State` parameter stack that isn't available
//! from inside a `Tool::execute`. The shared parser helpers in
//! `commands::tasks` (`rewrite_checkbox`, `append_task_to_section`,
//! `rewrite_task_text`, `rewrite_task_metadata`, `delete_task_line`,
//! `archive_task`) are `pub(crate)` so this module exercises the same
//! line-rewrite code that the IPC commands ship.

use async_trait::async_trait;
use serde_json::{json, Value};
use tauri::Manager;

use crate::commands::graph::GraphDb;
use crate::commands::tasks::{
    append_task_to_section, archive_task, clean_quick_add_text, count_task_lines_matching,
    delete_task_at_line, delete_task_line, query_all_tasks, rewrite_checkbox,
    rewrite_task_metadata, rewrite_task_text, TaskRow,
};
use crate::db::queries;

use super::note_ref;
use super::registry::{SafetyTier, Tool, ToolContext, ToolError};

// ── Small private helpers shared across the 7 tools ──────────────────

const ANCHOR_MIN_LEN: usize = 3;

async fn mutate_task_note<T, F>(
    ctx: &ToolContext,
    rel: &str,
    mutate: F,
) -> Result<(String, String, T), ToolError>
where
    F: FnOnce(&str) -> Result<(String, T), String>,
{
    crate::commands::files::mutate_roost_note_synced(&ctx.app, rel, mutate)
        .await
        .map_err(ToolError::retryable)
}

/// Re-index a single note + record an `:edit_event` with `Actor::Agent`.
/// Best-effort — failures here log a warning but don't fail the tool,
/// since the file-system mutation already succeeded and is the source
/// of truth.
fn record_agent_write(
    app: &tauri::AppHandle,
    abs_path: &str,
    intent: queries::Intent,
    actor_id: &str,
    before: &str,
    after: &str,
) {
    let Some(graph) = app.try_state::<GraphDb>() else {
        return;
    };
    let Ok(db_guard) = graph.db.lock() else {
        return;
    };
    let Some(db) = db_guard.as_ref() else {
        return;
    };
    let roost_root = match graph.roost_path.lock() {
        Ok(g) => match g.as_deref() {
            Some(v) => v.to_string(),
            None => return,
        },
        Err(_) => return,
    };
    let note_id = queries::note_id_from_abs_path(abs_path, &roost_root);

    // IP-8: full index, not pass1 alone — a task line can carry `[[links]]`
    // (e.g. "- [ ] reply to [[Ravi]]") that must become real edges. IP-A3:
    // reuse the cached resolver (add_id-maintained) instead of an O(roost)
    // rebuild per agent task edit.
    if let Err(e) = graph.index_note_full_cached(db, &roost_root, abs_path, after) {
        tracing::warn!(%note_id, error = %e, "tasks_crud: re-index failed");
    }
    // Do NOT invalidate: index_note_full_cached kept the cached resolver in
    // sync; invalidating would force the next save to rebuild O(roost) (IP-A3).
    graph.mark_algo_cache_dirty();
    // NEW-S2: an agent write changed this note's graph rows + tasks — drop the
    // cached stats snapshot so the unchanged-save fast path recomputes.
    graph.invalidate_stats_cache();
    if let Err(e) = queries::record_edit_event(
        db,
        &note_id,
        abs_path,
        queries::Actor::Agent,
        actor_id,
        intent,
        before,
        after,
    ) {
        tracing::warn!(%note_id, error = %e, "tasks_crud: edit_event record failed");
    }
}

/// If `path`'s filename is `YYYY-MM-DD.md`, return the `YYYY-MM-DD`
/// string. This lets
/// `create_task` recognize "this is a daily note for date X" purely from
/// the resolved path — works whether the user said `note_path = "daily"`,
/// `note_path = "Daily Notes/2026-05-11"`, or `[[2026-05-11]]`.
///
/// Why exact-length filename matching: rules out near-misses like
/// `2026-05-11-meeting.md`, which look daily-ish but aren't. We only want
/// to auto-stamp dates on TRUE daily notes.
fn daily_note_date_from_path(path: &str) -> Option<String> {
    let stem = path.rsplit('/').next()?.trim_end_matches(".md");
    if stem.len() != 10 {
        return None;
    }
    // chrono's parser confirms it's an actual date — rejects `9999-99-99`
    // and similar shape-matches that wouldn't survive a calendar check.
    chrono::NaiveDate::parse_from_str(stem, "%Y-%m-%d")
        .ok()
        .map(|_| stem.to_string())
}

fn require_anchor(args: &Value, key: &str) -> Result<String, ToolError> {
    let raw = args
        .get(key)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ToolError::retryable(format!("missing '{key}'")))?;
    if raw.chars().count() < ANCHOR_MIN_LEN {
        return Err(ToolError::retryable(format!(
            "'{key}' too short ({} chars) — pass at least {ANCHOR_MIN_LEN} chars of the task text to disambiguate",
            raw.chars().count()
        )));
    }
    Ok(raw.to_string())
}

fn require_string(args: &Value, key: &str) -> Result<String, ToolError> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ToolError::retryable(format!("missing '{key}'")))
        .map(str::to_string)
}

fn optional_string(args: &Value, key: &str) -> String {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .unwrap_or("")
        .to_string()
}

/// Resolve `note_path` from args via the unified [`note_ref`] resolver.
/// Kept as a thin wrapper so the 6 task tool execute() bodies don't need
/// to know the option-construction details — flipping `allow_open_fallback`
/// switches between `for_write` (create_task: empty → "daily" via the
/// unified path) and `for_task_op` (update/complete/etc.: empty → open
/// note or error). The unified resolver handles `"daily"` shorthand, UNC
/// prefix normalization, title lookup, and `.md` extension fallback
/// uniformly across every tool that takes a `note_path`.
async fn resolve_target_path(
    args: &Value,
    ctx: &ToolContext,
    allow_open_fallback: bool,
) -> Result<String, ToolError> {
    let raw = args
        .get("note_path")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .unwrap_or("");
    // For `create_task` (allow_open_fallback = false) an empty `note_path`
    // means "use today's daily note" — that's the documented default.
    // Substitute the shorthand so the unified resolver picks it up.
    let resolved_input = if !allow_open_fallback && raw.is_empty() {
        "daily"
    } else {
        raw
    };
    note_ref::resolve_rel(resolved_input, ctx, true, allow_open_fallback).await
}

fn ensure_unique_anchor(content: &str, anchor: &str) -> Result<(), ToolError> {
    match count_task_lines_matching(content, anchor) {
        0 => Err(ToolError::retryable(format!(
            "no task starting with \"{anchor}\" — re-check with query_tasks"
        ))),
        1 => Ok(()),
        n => Err(ToolError::retryable(format!(
            "{n} tasks start with \"{anchor}\" — pass a longer text_anchor to disambiguate"
        ))),
    }
}

/// Where the agent's `text_anchor` matched. Frontmatter and body tasks
/// have separate rewrite paths; we classify once and dispatch.
#[derive(Debug)]
enum AnchorLocation {
    Body,
    Frontmatter,
}

/// Decide which rewrite path applies. Errors when the anchor matches
/// in both regions (ambiguous) or in neither (not found). When the
/// match is unique in the body we still require the body's per-line
/// uniqueness check to pass so a longer anchor surfaces collisions.
fn classify_anchor(content: &str, anchor: &str) -> Result<AnchorLocation, ToolError> {
    let body_count = count_task_lines_matching(content, anchor);
    let frontmatter_count = count_frontmatter_anchor_matches(content, anchor);
    match (body_count, frontmatter_count) {
        (0, 0) => Err(ToolError::retryable(format!(
            "no task matches anchor \"{anchor}\" — re-check with query_tasks"
        ))),
        (n, 0) if n > 1 => Err(ToolError::retryable(format!(
            "{n} body tasks match anchor \"{anchor}\" — pass a longer text_anchor to disambiguate"
        ))),
        (0, n) if n > 1 => Err(ToolError::retryable(format!(
            "{n} frontmatter tasks match anchor \"{anchor}\" — pass a longer text_anchor to disambiguate"
        ))),
        (b, f) if b > 0 && f > 0 => Err(ToolError::retryable(format!(
            "anchor \"{anchor}\" matches {b} body and {f} frontmatter task(s) — pass a longer text_anchor"
        ))),
        (1, 0) => Ok(AnchorLocation::Body),
        (0, 1) => Ok(AnchorLocation::Frontmatter),
        _ => unreachable!("matched all (b, f) combinations above"),
    }
}

/// Count frontmatter task items whose `text:` value starts with `anchor`.
/// Mirrors `count_task_lines_matching` for the YAML region. Returns 0
/// for files without frontmatter or without a `tasks:` block.
fn count_frontmatter_anchor_matches(content: &str, anchor: &str) -> usize {
    use crate::parser::checkboxes::parse_tasks;
    parse_tasks(content.to_string())
        .iter()
        .filter(|t| t.source_line < 0)
        .filter(|t| t.text.starts_with(anchor))
        .count()
}

// ── Tools ─────────────────────────────────────────────────────────────

pub struct CreateTaskTool;
pub struct UpdateTaskTool;
pub struct CompleteTaskTool;
pub struct RescheduleTaskTool;
pub struct ArchiveTaskTool;
pub struct DeleteTaskTool;
pub struct QueryTasksTool;

#[async_trait]
impl Tool for CreateTaskTool {
    fn name(&self) -> &'static str {
        "create_task"
    }
    fn description(&self) -> &'static str {
        "Append a new `- [ ] {text}` task line to a note. Pass `note_path = \"daily\"` \
         to target today's daily note — auto-created at the configured daily-note \
         folder (default `Daily Notes/YYYY-MM-DD.md`) if it doesn't exist yet. \
         Do NOT roost_read / roost_search to confirm the daily note exists first; \
         just call create_task with `note_path: \"daily\"`. Or pass an absolute or \
         roost-relative path to target a specific note. The task body is taken \
         verbatim — include 📅 YYYY-MM-DD and ⏰ HH:MM tokens directly in `text` if \
         the user wanted a specific due date or time. **Auto-date**: if the target \
         note's filename matches `YYYY-MM-DD.md` (a daily note) AND `text` doesn't \
         already contain `📅`, the tool stamps the daily-note's date as the due \
         date so date queries find it. Returns the 1-based source line of the \
         inserted task."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "note_path": { "type": "string", "description": "Roost-relative path, absolute path, or 'daily' shorthand (auto-creates today's daily note if missing)." },
                "text": { "type": "string" }
            },
            "required": ["text"]
        })
    }
    fn safety_tier(&self) -> SafetyTier {
        SafetyTier::Write
    }
    async fn execute(&self, ctx: &ToolContext, args: Value) -> Result<Value, ToolError> {
        let roost = ctx.require_roost(self.name())?.to_string();
        let raw_text = require_string(&args, "text")?;
        let cleaned = clean_quick_add_text(&raw_text);
        if cleaned.is_empty() {
            return Err(ToolError::retryable("'text' is empty after cleaning"));
        }
        // AUDIT-033: when the agent targets today's daily note (explicit
        // "daily" shorthand or empty `note_path` defaulting to daily),
        // bootstrap the note via RoostFs so creation goes through the
        // sync-notification chain instead of a raw std::fs::write.
        let raw_input = args
            .get("note_path")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .unwrap_or("");
        if raw_input.is_empty() || raw_input.eq_ignore_ascii_case("daily") {
            note_ref::ensure_daily_note(ctx, &roost).await?;
        }
        let path = resolve_target_path(&args, ctx, false).await?;

        // Stamp a due date when the agent gave none (no inline 📅).
        // Convention 0.3: a checkbox is a task ONLY with an effective date, so
        // a dateless `- [ ]` in an ordinary note would silently NOT become a
        // task. create_task's whole job is to make a to-do, so:
        //   - daily note (filename `YYYY-MM-DD.md`) → stamp that note's date;
        //   - any other note → stamp today.
        // The agent can still pass an explicit 📅 in `text` to override.
        let text = if cleaned.contains('📅') {
            cleaned
        } else if let Some(date) = daily_note_date_from_path(&path) {
            format!("{cleaned} 📅 {date}")
        } else {
            let today = chrono::Local::now().format("%Y-%m-%d").to_string();
            format!("{cleaned} 📅 {today}")
        };

        let (before, after, line_no) = mutate_task_note(ctx, &path, |before| {
            let (after, line_no) = append_task_to_section(before, &text);
            Ok((after, line_no))
        })
        .await?;
        let display_path = note_ref::display_join(&roost, &path);
        record_agent_write(
            &ctx.app,
            &display_path,
            queries::Intent::AgentWrite,
            "agent",
            &before,
            &after,
        );

        Ok(json!({
            "summary": format!("created task at {display_path} line {line_no}"),
            "note_path": display_path,
            "source_line": line_no,
            "text": text,
        }))
    }
}

#[async_trait]
impl Tool for UpdateTaskTool {
    fn name(&self) -> &'static str {
        "update_task"
    }
    fn description(&self) -> &'static str {
        "Rewrite the body text of an existing task. Identify the task with \
         `text_anchor` (the first ~10 chars of its current body — must be \
         unique within the note). `new_text` replaces the entire body \
         verbatim, so include any 📅/⏰/#tag/[[link]] metadata you want to \
         keep. Defaults to the currently-open note when `note_path` is \
         omitted."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "note_path": { "type": "string" },
                "text_anchor": { "type": "string" },
                "new_text": { "type": "string" }
            },
            "required": ["text_anchor", "new_text"]
        })
    }
    fn safety_tier(&self) -> SafetyTier {
        SafetyTier::Write
    }
    async fn execute(&self, ctx: &ToolContext, args: Value) -> Result<Value, ToolError> {
        let roost = ctx.require_roost(self.name())?.to_string();
        let anchor = require_anchor(&args, "text_anchor")?;
        let new_text = require_string(&args, "new_text")?;
        let path = resolve_target_path(&args, ctx, true).await?;
        let (before, after, ()) = mutate_task_note(ctx, &path, |before| {
            let location = classify_anchor(before, &anchor).map_err(|e| e.to_string())?;
            let after = match location {
                AnchorLocation::Body => rewrite_task_text(before, &anchor, &new_text)?,
                AnchorLocation::Frontmatter => {
                    use crate::commands::frontmatter_tasks::{
                        rewrite_frontmatter_task, FrontmatterMutation,
                    };
                    rewrite_frontmatter_task(
                        before,
                        &anchor,
                        FrontmatterMutation::UpdateText(new_text.clone()),
                    )?
                }
            };
            Ok((after, ()))
        })
        .await?;
        let display_path = note_ref::display_join(&roost, &path);
        record_agent_write(
            &ctx.app,
            &display_path,
            queries::Intent::AgentWrite,
            "agent",
            &before,
            &after,
        );

        Ok(json!({
            "summary": format!("updated task in {display_path}"),
            "note_path": display_path,
            "new_text": new_text,
        }))
    }
}

#[async_trait]
impl Tool for CompleteTaskTool {
    fn name(&self) -> &'static str {
        "complete_task"
    }
    fn description(&self) -> &'static str {
        "Toggle a task between `[ ]` and `[x]`. Identify the task by \
         `text_anchor` (first ~10 chars of its body). `completed` defaults \
         to true. Marking a task complete appends `✅ YYYY-MM-DD`; \
         un-completing strips that token. Defaults to the currently-open \
         note when `note_path` is omitted."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "note_path": { "type": "string" },
                "text_anchor": { "type": "string" },
                "completed": { "type": "boolean", "default": true }
            },
            "required": ["text_anchor"]
        })
    }
    fn safety_tier(&self) -> SafetyTier {
        SafetyTier::Write
    }
    async fn execute(&self, ctx: &ToolContext, args: Value) -> Result<Value, ToolError> {
        let roost = ctx.require_roost(self.name())?.to_string();
        let anchor = require_anchor(&args, "text_anchor")?;
        let completed = args
            .get("completed")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        let path = resolve_target_path(&args, ctx, true).await?;
        let (before, after, ()) = mutate_task_note(ctx, &path, |before| {
            let location = classify_anchor(before, &anchor).map_err(|e| e.to_string())?;
            let after = match location {
                AnchorLocation::Body => rewrite_checkbox(before, 0, completed, &anchor)?,
                AnchorLocation::Frontmatter => {
                    use crate::commands::frontmatter_tasks::{
                        rewrite_frontmatter_task, FrontmatterMutation,
                    };
                    rewrite_frontmatter_task(
                        before,
                        &anchor,
                        FrontmatterMutation::ToggleCompleted(completed),
                    )?
                }
            };
            Ok((after, ()))
        })
        .await?;
        let display_path = note_ref::display_join(&roost, &path);
        record_agent_write(
            &ctx.app,
            &display_path,
            queries::Intent::ToggleTask,
            "agent",
            &before,
            &after,
        );

        Ok(json!({
            "summary": format!(
                "{} task in {}",
                if completed { "completed" } else { "reopened" },
                display_path
            ),
            "note_path": display_path,
            "completed": completed,
        }))
    }
}

#[async_trait]
impl Tool for RescheduleTaskTool {
    fn name(&self) -> &'static str {
        "reschedule_task"
    }
    fn description(&self) -> &'static str {
        "Update the 📅 date and/or ⏰ time on an existing task. Identify \
         the task with `text_anchor`. Pass `new_date` (YYYY-MM-DD) and/or \
         `new_time` (HH:MM) to set; pass the literal string \"clear\" to \
         remove an existing token. Empty/omitted args leave that token \
         unchanged. Defaults to the currently-open note when `note_path` \
         is omitted."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "note_path": { "type": "string" },
                "text_anchor": { "type": "string" },
                "new_date": { "type": "string", "description": "YYYY-MM-DD or 'clear'" },
                "new_time": { "type": "string", "description": "HH:MM or 'clear'" }
            },
            "required": ["text_anchor"]
        })
    }
    fn safety_tier(&self) -> SafetyTier {
        SafetyTier::Write
    }
    async fn execute(&self, ctx: &ToolContext, args: Value) -> Result<Value, ToolError> {
        let roost = ctx.require_roost(self.name())?.to_string();
        let anchor = require_anchor(&args, "text_anchor")?;
        let new_date = optional_string(&args, "new_date");
        let new_time = optional_string(&args, "new_time");
        if new_date.is_empty() && new_time.is_empty() {
            return Err(ToolError::retryable(
                "pass at least one of 'new_date' or 'new_time'",
            ));
        }
        let path = resolve_target_path(&args, ctx, true).await?;
        let (before, after, ()) = mutate_task_note(ctx, &path, |before| {
            let location = classify_anchor(before, &anchor).map_err(|e| e.to_string())?;
            let after = match location {
                AnchorLocation::Body => {
                    rewrite_task_metadata(before, &anchor, &new_date, &new_time)?
                }
                AnchorLocation::Frontmatter => {
                    use crate::commands::frontmatter_tasks::{
                        rewrite_frontmatter_task, FrontmatterMutation,
                    };
                    rewrite_frontmatter_task(
                        before,
                        &anchor,
                        FrontmatterMutation::Reschedule {
                            new_date: new_date.clone(),
                            new_time: new_time.clone(),
                        },
                    )?
                }
            };
            Ok((after, ()))
        })
        .await?;
        let display_path = note_ref::display_join(&roost, &path);
        record_agent_write(
            &ctx.app,
            &display_path,
            queries::Intent::AgentWrite,
            "agent",
            &before,
            &after,
        );

        Ok(json!({
            "summary": format!("rescheduled task in {display_path}"),
            "note_path": display_path,
            "new_date": new_date,
            "new_time": new_time,
        }))
    }
}

#[async_trait]
impl Tool for ArchiveTaskTool {
    fn name(&self) -> &'static str {
        "archive_task"
    }
    fn description(&self) -> &'static str {
        "Soft-archive a task: strikethrough its body and move it under a \
         `## Archived` section at end of file (creating the section if \
         needed). Use this instead of `delete_task` when the task is no \
         longer actionable but the user wants a record. Identify with \
         `text_anchor`. Defaults to the currently-open note."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "note_path": { "type": "string" },
                "text_anchor": { "type": "string" }
            },
            "required": ["text_anchor"]
        })
    }
    fn safety_tier(&self) -> SafetyTier {
        SafetyTier::Write
    }
    async fn execute(&self, ctx: &ToolContext, args: Value) -> Result<Value, ToolError> {
        let roost = ctx.require_roost(self.name())?.to_string();
        let anchor = require_anchor(&args, "text_anchor")?;
        let path = resolve_target_path(&args, ctx, true).await?;
        let (before, after, ()) = mutate_task_note(ctx, &path, |before| {
            ensure_unique_anchor(before, &anchor).map_err(|e| e.to_string())?;
            Ok((archive_task(before, &anchor)?, ()))
        })
        .await?;
        let display_path = note_ref::display_join(&roost, &path);
        record_agent_write(
            &ctx.app,
            &display_path,
            queries::Intent::AgentWrite,
            "agent",
            &before,
            &after,
        );

        Ok(json!({
            "summary": format!("archived task in {display_path}"),
            "note_path": display_path,
        }))
    }
}

#[async_trait]
impl Tool for DeleteTaskTool {
    fn name(&self) -> &'static str {
        "delete_task"
    }
    fn description(&self) -> &'static str {
        "Hard-delete a task line from a note. Prefer `archive_task` when the \
         user might want a record; use `delete_task` only when the user \
         explicitly says \"delete\" or the task was a mistake. Identify \
         with `text_anchor` (first ~10 chars of the task body). **When you \
         are deleting a task you just CREATED or modified earlier in this \
         conversation, also pass `source_line` from that earlier tool's \
         result** — it pins the delete to the exact line you meant, so two \
         tasks that share an anchor prefix don't trip you into deleting the \
         wrong one. Reversible via the History pane within the \
         500-event-per-note retention window."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "note_path": {
                    "type": "string",
                    "description": "Roost-relative path like 'Daily Notes/2026-05-11.md', absolute path within the roost, or 'daily' shorthand. If you got this string from a prior tool result (query_tasks, roost_read, roost_search), pass it back UNCHANGED — do NOT reformat to a bare title like '2026-05-11' or drop the folder. The bare-title shortcut works for roost_read (title-based lookup) but NOT here."
                },
                "text_anchor": { "type": "string" },
                "source_line": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "1-based line number of the task to delete. Use the `source_line` returned by `create_task` or `query_tasks` to target a specific row when multiple tasks share an anchor."
                }
            },
            "required": ["text_anchor"]
        })
    }
    fn safety_tier(&self) -> SafetyTier {
        SafetyTier::Destructive
    }
    async fn execute(&self, ctx: &ToolContext, args: Value) -> Result<Value, ToolError> {
        let roost = ctx.require_roost(self.name())?.to_string();
        let anchor = require_anchor(&args, "text_anchor")?;
        let source_line = args
            .get("source_line")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize);
        let path = resolve_target_path(&args, ctx, true).await?;
        let (before, after, ()) = mutate_task_note(ctx, &path, |before| {
            let after = if let Some(line_no) = source_line {
                delete_task_at_line(before, line_no, &anchor)?
            } else {
                let location = classify_anchor(before, &anchor).map_err(|e| e.to_string())?;
                match location {
                    AnchorLocation::Body => delete_task_line(before, &anchor)?,
                    AnchorLocation::Frontmatter => {
                        use crate::commands::frontmatter_tasks::{
                            rewrite_frontmatter_task, FrontmatterMutation,
                        };
                        rewrite_frontmatter_task(before, &anchor, FrontmatterMutation::Delete)?
                    }
                }
            };
            Ok((after, ()))
        })
        .await?;
        let display_path = note_ref::display_join(&roost, &path);
        record_agent_write(
            &ctx.app,
            &display_path,
            queries::Intent::Delete,
            "agent",
            &before,
            &after,
        );

        Ok(json!({
            "summary": format!("deleted task in {display_path}"),
            "note_path": display_path,
        }))
    }
}

#[async_trait]
impl Tool for QueryTasksTool {
    fn name(&self) -> &'static str {
        "query_tasks"
    }
    fn description(&self) -> &'static str {
        "Read-only listing of tasks across the roost, filtered by status, \
         due-date bucket, project, person, or note. Args: `status` \
         (\"open\" | \"done\" | \"all\", default open), `bucket` \
         (\"overdue\" | \"today\" | \"this_week\" | \"later\" | \
         \"no_date\"), `note_path`, `project`, `person`, `limit` (default \
         50). Returns a summary plus the matching task rows."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "status": { "type": "string", "enum": ["open", "done", "all"] },
                "bucket": {
                    "type": "string",
                    "enum": ["overdue", "today", "this_week", "later", "no_date"]
                },
                "note_path": { "type": "string" },
                "project": { "type": "string" },
                "person": { "type": "string" },
                "limit": { "type": "integer", "default": 50 }
            }
        })
    }
    fn safety_tier(&self) -> SafetyTier {
        SafetyTier::Read
    }
    async fn execute(&self, ctx: &ToolContext, args: Value) -> Result<Value, ToolError> {
        let _roost = ctx.require_roost(self.name())?;
        let Some(graph) = ctx.app.try_state::<GraphDb>() else {
            return Err(ToolError::fatal("graph DB not initialized"));
        };
        let rows = {
            let guard = graph
                .db
                .lock()
                .map_err(|e| ToolError::retryable(format!("graph lock: {e}")))?;
            let db = guard
                .as_ref()
                .ok_or_else(|| ToolError::fatal("graph DB not initialized — open a roost first"))?;
            query_all_tasks(db).map_err(ToolError::retryable)?
        };

        let status = optional_string(&args, "status");
        let bucket = optional_string(&args, "bucket");
        let project = optional_string(&args, "project");
        let person = optional_string(&args, "person");
        let note_filter_raw = optional_string(&args, "note_path");
        let limit = args
            .get("limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(50)
            .min(500) as usize;

        let today = today_iso_local();
        let week_end = iso_local_offset(6);

        // Resolve "daily" shorthand to today's date string so substring
        // matching hits paths like `Daily Notes/2026-05-11.md`. Without
        // this, `note_path: "daily"` would substring-match nothing (the
        // folder is capitalized; the filter is case-sensitive too). Also
        // lowercase both sides so callers don't have to guess casing.
        let note_filter_resolved = if note_filter_raw.eq_ignore_ascii_case("daily") {
            today.clone()
        } else {
            note_filter_raw
        };
        let note_filter_lower = note_filter_resolved.to_lowercase();

        let filtered = apply_task_filter(
            &rows,
            &TaskFilter {
                status: status.as_str(),
                bucket: bucket.as_str(),
                project: project.as_str(),
                person: person.as_str(),
                note_filter_lower: note_filter_lower.as_str(),
                today: today.as_str(),
                week_end: week_end.as_str(),
                limit,
            },
        );

        let payload: Vec<Value> = filtered
            .iter()
            .map(|r| {
                json!({
                    "note_path": r.note_path,
                    "note_title": r.note_title,
                    "source_line": r.source_line,
                    "text": r.text,
                    "due_at": r.due_at,
                    "completed": r.completed,
                    "linked_project": r.linked_project,
                    "linked_person": r.linked_person,
                })
            })
            .collect();

        Ok(json!({
            "summary": format!("{} task(s) match", payload.len()),
            "count": payload.len(),
            "tasks": payload,
        }))
    }
}

/// In-memory filter inputs for [`apply_task_filter`]. Pure data — no
/// roost, DB, or chrono coupling, so tests can fabricate scenarios.
#[derive(Debug, Default)]
struct TaskFilter<'a> {
    /// `"" | "open" | "done" | "all"` — empty is treated as "open".
    status: &'a str,
    /// `"" | "overdue" | "today" | "this_week" | "later" | "no_date"`.
    bucket: &'a str,
    project: &'a str,
    person: &'a str,
    /// Already-lowercased substring to match against `note_path`.
    note_filter_lower: &'a str,
    /// `YYYY-MM-DD` reference date. Bucket comparisons are string-based
    /// (ISO-8601 sorts lexicographically the same as chronologically).
    today: &'a str,
    /// `YYYY-MM-DD`, typically `today + 6`. Upper bound for "this_week"
    /// and lower bound (exclusive) for "later".
    week_end: &'a str,
    limit: usize,
}

/// Pure in-memory filter for `query_tasks` — extracted from
/// [`QueryTasksTool::execute`] so bucket / date / project / person /
/// note-path logic can be unit-tested without a Tauri context. The
/// execute() body builds a `TaskFilter` and calls this; nothing else
/// about the tool's contract changes.
fn apply_task_filter<'a>(rows: &'a [TaskRow], f: &TaskFilter) -> Vec<&'a TaskRow> {
    rows.iter()
        .filter(|r| match f.status {
            "" | "open" => !r.completed,
            "done" => r.completed,
            "all" => true,
            _ => !r.completed,
        })
        .filter(|r| {
            if f.bucket.is_empty() {
                return true;
            }
            // `due_at` is either empty, `YYYY-MM-DD`, or `YYYY-MM-DD HH:MM`.
            // Strip the time half so date comparisons stay clean.
            let due_date = r.due_at.split_whitespace().next().unwrap_or("");
            match f.bucket {
                "overdue" => !due_date.is_empty() && due_date < f.today,
                "today" => due_date == f.today,
                "this_week" => !due_date.is_empty() && due_date > f.today && due_date <= f.week_end,
                "later" => !due_date.is_empty() && due_date > f.week_end,
                "no_date" => due_date.is_empty(),
                _ => true,
            }
        })
        .filter(|r| f.project.is_empty() || r.linked_project.eq_ignore_ascii_case(f.project))
        .filter(|r| f.person.is_empty() || r.linked_person.eq_ignore_ascii_case(f.person))
        .filter(|r| {
            f.note_filter_lower.is_empty()
                || r.note_path.to_lowercase().contains(f.note_filter_lower)
        })
        .take(f.limit)
        .collect()
}

fn today_iso_local() -> String {
    use chrono::Local;
    Local::now().format("%Y-%m-%d").to_string()
}

fn iso_local_offset(days: i64) -> String {
    use chrono::{Duration, Local};
    (Local::now() + Duration::days(days))
        .format("%Y-%m-%d")
        .to_string()
}

#[cfg(test)]
mod tests {
    //! Unit tests for the pure helpers in this module. The `execute()` /
    //! tool dispatch paths need a `tauri::AppHandle` and a live CozoDB,
    //! so they're not covered here — the agent-loop integration suite
    //! (separate) exercises those. What this module covers:
    //!
    //! - `apply_task_filter`: all five buckets, status modes, project /
    //!   person / note-path filters, limit cap, datetime-in-due-at edge.
    //! - `daily_note_date_from_path`: positive + the three rejection
    //!   shapes (length mismatch, calendar-invalid, non-date stem).
    //! - `require_anchor` / `require_string` / `optional_string`:
    //!   missing / empty / whitespace / too-short.
    //! - `ensure_unique_anchor`: zero / one / many matches.
    //! - `classify_anchor`: body, frontmatter, ambiguous, missing.
    //!
    //! New CRUD bugs should add a regression test here BEFORE the fix —
    //! per the testing-discipline rule in CLAUDE.md.

    use super::*;
    use serde_json::json;

    // ── Test fixtures ──────────────────────────────────────────────

    fn row(due_at: &str, completed: bool) -> TaskRow {
        TaskRow {
            note_id: "note".into(),
            note_path: "Daily Notes/2026-05-15.md".into(),
            note_title: "2026-05-15".into(),
            source_line: 1,
            text: "task text".into(),
            due_at: due_at.into(),
            completed,
            completed_at: String::new(),
            linked_person: String::new(),
            linked_project: String::new(),
        }
    }

    fn row_with(due_at: &str, completed: bool, path: &str, person: &str, project: &str) -> TaskRow {
        let mut r = row(due_at, completed);
        r.note_path = path.into();
        r.linked_person = person.into();
        r.linked_project = project.into();
        r
    }

    fn filter(bucket: &str, today: &str, week_end: &str) -> TaskFilter<'static> {
        // SAFETY: tests only — borrow lifetimes are bounded by the
        // strings being string literals (`'static`).
        let bucket: &'static str = Box::leak(bucket.to_string().into_boxed_str());
        let today: &'static str = Box::leak(today.to_string().into_boxed_str());
        let week_end: &'static str = Box::leak(week_end.to_string().into_boxed_str());
        TaskFilter {
            status: "open",
            bucket,
            project: "",
            person: "",
            note_filter_lower: "",
            today,
            week_end,
            limit: 100,
        }
    }

    // ── apply_task_filter: bucket semantics ────────────────────────

    #[test]
    fn overdue_matches_dates_strictly_before_today() {
        let rows = vec![
            row("2026-05-11", false),
            row("2026-05-15", false),
            row("2026-05-16", false), // today — NOT overdue
            row("2026-05-17", false), // future — NOT overdue
        ];
        let f = filter("overdue", "2026-05-16", "2026-05-22");
        let got = apply_task_filter(&rows, &f);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].due_at, "2026-05-11");
        assert_eq!(got[1].due_at, "2026-05-15");
    }

    #[test]
    fn overdue_excludes_empty_due_at() {
        // Regression: a no-date task must never appear in "overdue".
        let rows = vec![row("", false), row("2026-05-11", false)];
        let f = filter("overdue", "2026-05-16", "2026-05-22");
        let got = apply_task_filter(&rows, &f);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].due_at, "2026-05-11");
    }

    #[test]
    fn overdue_handles_datetime_due_at() {
        // `due_at` of "YYYY-MM-DD HH:MM" must classify by date only.
        let rows = vec![
            row("2026-05-13 10:00", false),
            row("2026-05-16 23:59", false), // today w/ time — NOT overdue
        ];
        let f = filter("overdue", "2026-05-16", "2026-05-22");
        let got = apply_task_filter(&rows, &f);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].due_at, "2026-05-13 10:00");
    }

    #[test]
    fn today_matches_exact_date_only() {
        let rows = vec![
            row("2026-05-15", false),
            row("2026-05-16", false),       // today
            row("2026-05-16 10:00", false), // today w/ time
            row("2026-05-17", false),
        ];
        let f = filter("today", "2026-05-16", "2026-05-22");
        let got = apply_task_filter(&rows, &f);
        assert_eq!(got.len(), 2, "exact + datetime variants both match today");
    }

    #[test]
    fn this_week_is_strictly_after_today_through_week_end() {
        let rows = vec![
            row("2026-05-16", false), // today — excluded
            row("2026-05-17", false), // included (day after today)
            row("2026-05-22", false), // included (week_end boundary)
            row("2026-05-23", false), // excluded (past week_end)
        ];
        let f = filter("this_week", "2026-05-16", "2026-05-22");
        let got = apply_task_filter(&rows, &f);
        assert_eq!(got.len(), 2);
    }

    #[test]
    fn later_is_strictly_after_week_end() {
        let rows = vec![
            row("2026-05-22", false), // week_end — NOT later
            row("2026-05-23", false), // later
            row("", false),           // no_date — never "later"
        ];
        let f = filter("later", "2026-05-16", "2026-05-22");
        let got = apply_task_filter(&rows, &f);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].due_at, "2026-05-23");
    }

    #[test]
    fn no_date_only_matches_empty_due_at() {
        let rows = vec![row("", false), row("2026-05-15", false)];
        let f = filter("no_date", "2026-05-16", "2026-05-22");
        let got = apply_task_filter(&rows, &f);
        assert_eq!(got.len(), 1);
        assert!(got[0].due_at.is_empty());
    }

    #[test]
    fn empty_bucket_returns_all_open() {
        let rows = vec![row("2026-05-11", false), row("", false)];
        let f = filter("", "2026-05-16", "2026-05-22");
        let got = apply_task_filter(&rows, &f);
        assert_eq!(got.len(), 2);
    }

    #[test]
    fn unknown_bucket_falls_back_to_all() {
        // Hard-coded `_ => true` — surfaces a bad bucket name as
        // permissive rather than silently filtering everything out.
        let rows = vec![row("2026-05-11", false)];
        let f = filter("not-a-bucket", "2026-05-16", "2026-05-22");
        let got = apply_task_filter(&rows, &f);
        assert_eq!(got.len(), 1);
    }

    // ── status modes ───────────────────────────────────────────────

    #[test]
    fn status_open_excludes_completed() {
        let rows = vec![row("2026-05-11", false), row("2026-05-11", true)];
        let mut f = filter("", "2026-05-16", "2026-05-22");
        f.status = "open";
        let got = apply_task_filter(&rows, &f);
        assert_eq!(got.len(), 1);
        assert!(!got[0].completed);
    }

    #[test]
    fn status_empty_treated_as_open() {
        let rows = vec![row("2026-05-11", false), row("2026-05-11", true)];
        let mut f = filter("", "2026-05-16", "2026-05-22");
        f.status = "";
        let got = apply_task_filter(&rows, &f);
        assert_eq!(got.len(), 1);
    }

    #[test]
    fn status_done_excludes_open() {
        let rows = vec![row("2026-05-11", false), row("2026-05-11", true)];
        let mut f = filter("", "2026-05-16", "2026-05-22");
        f.status = "done";
        let got = apply_task_filter(&rows, &f);
        assert_eq!(got.len(), 1);
        assert!(got[0].completed);
    }

    #[test]
    fn status_all_returns_both() {
        let rows = vec![row("2026-05-11", false), row("2026-05-11", true)];
        let mut f = filter("", "2026-05-16", "2026-05-22");
        f.status = "all";
        let got = apply_task_filter(&rows, &f);
        assert_eq!(got.len(), 2);
    }

    // ── project / person / note filters ────────────────────────────

    #[test]
    fn project_filter_case_insensitive() {
        let rows = vec![
            row_with("2026-05-11", false, "n.md", "", "Corvux"),
            row_with("2026-05-11", false, "n.md", "", "other"),
        ];
        let mut f = filter("", "2026-05-16", "2026-05-22");
        f.project = "corvux";
        let got = apply_task_filter(&rows, &f);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].linked_project, "Corvux");
    }

    #[test]
    fn person_filter_case_insensitive() {
        let rows = vec![
            row_with("2026-05-11", false, "n.md", "Sunny", ""),
            row_with("2026-05-11", false, "n.md", "ravi", ""),
        ];
        let mut f = filter("", "2026-05-16", "2026-05-22");
        f.person = "SUNNY";
        let got = apply_task_filter(&rows, &f);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].linked_person, "Sunny");
    }

    #[test]
    fn note_path_filter_lowercased_substring() {
        let rows = vec![
            row_with("2026-05-11", false, "Daily Notes/2026-05-11.md", "", ""),
            row_with("2026-05-11", false, "Projects/Corvux.md", "", ""),
        ];
        let mut f = filter("", "2026-05-16", "2026-05-22");
        // Caller passes already-lowercased substring — this test asserts
        // we don't accidentally re-uppercase or double-strip.
        f.note_filter_lower = "daily";
        let got = apply_task_filter(&rows, &f);
        assert_eq!(got.len(), 1);
        assert!(got[0].note_path.contains("Daily"));
    }

    #[test]
    fn limit_caps_results() {
        let rows: Vec<TaskRow> = (0..10).map(|_| row("2026-05-11", false)).collect();
        let mut f = filter("overdue", "2026-05-16", "2026-05-22");
        f.limit = 3;
        let got = apply_task_filter(&rows, &f);
        assert_eq!(got.len(), 3);
    }

    // ── daily_note_date_from_path ──────────────────────────────────

    #[test]
    fn daily_note_path_recognizes_iso_filename() {
        assert_eq!(
            daily_note_date_from_path("Daily Notes/2026-05-11.md"),
            Some("2026-05-11".to_string())
        );
    }

    #[test]
    fn daily_note_path_rejects_dated_meeting_note() {
        // Length mismatch — `2026-05-11-meeting` is 18 chars, not 10.
        assert_eq!(daily_note_date_from_path("2026-05-11-meeting.md"), None);
    }

    #[test]
    fn daily_note_path_rejects_invalid_calendar_date() {
        // Shape-matches `\d{4}-\d{2}-\d{2}` but isn't a real date.
        assert_eq!(daily_note_date_from_path("9999-99-99.md"), None);
    }

    #[test]
    fn daily_note_path_rejects_non_date_stem() {
        assert_eq!(daily_note_date_from_path("hello.md"), None);
    }

    // ── arg helpers ────────────────────────────────────────────────

    #[test]
    fn require_anchor_rejects_missing() {
        assert!(require_anchor(&json!({}), "text_anchor").is_err());
    }

    #[test]
    fn require_anchor_rejects_too_short_after_trim() {
        // `"  ab  "` trims to `"ab"` (2 chars) — below `ANCHOR_MIN_LEN`.
        assert!(require_anchor(&json!({"text_anchor": "  ab  "}), "text_anchor").is_err());
    }

    #[test]
    fn require_anchor_accepts_valid() {
        let v = require_anchor(&json!({"text_anchor": "going hyd"}), "text_anchor").unwrap();
        assert_eq!(v, "going hyd");
    }

    #[test]
    fn require_string_rejects_whitespace_only() {
        assert!(require_string(&json!({"x": "   "}), "x").is_err());
    }

    #[test]
    fn optional_string_returns_empty_when_missing() {
        assert_eq!(optional_string(&json!({}), "x"), "");
    }

    #[test]
    fn optional_string_trims() {
        assert_eq!(optional_string(&json!({"x": "  val  "}), "x"), "val");
    }

    // ── anchor lookup ──────────────────────────────────────────────

    #[test]
    fn ensure_unique_anchor_zero_matches_errors() {
        assert!(ensure_unique_anchor("- [ ] hello\n", "world").is_err());
    }

    #[test]
    fn ensure_unique_anchor_one_match_ok() {
        assert!(ensure_unique_anchor("- [ ] hello world\n", "hello").is_ok());
    }

    #[test]
    fn ensure_unique_anchor_multiple_matches_errors() {
        let content = "- [ ] hello one\n- [ ] hello two\n";
        assert!(ensure_unique_anchor(content, "hello").is_err());
    }

    #[test]
    fn classify_anchor_body_match() {
        let r = classify_anchor("- [ ] going hyd\n", "going hyd").unwrap();
        assert!(matches!(r, AnchorLocation::Body));
    }

    #[test]
    fn classify_anchor_not_found_errors() {
        assert!(classify_anchor("- [ ] hello\n", "missing").is_err());
    }

    #[test]
    fn classify_anchor_frontmatter_match() {
        let content = "---\ntasks:\n  - text: \"frontmatter task\"\n---\nbody content\n";
        let r = classify_anchor(content, "frontmatter task").unwrap();
        assert!(matches!(r, AnchorLocation::Frontmatter));
    }

    #[test]
    fn classify_anchor_ambiguous_body_and_frontmatter_errors() {
        let content = "---\ntasks:\n  - text: \"shared task\"\n---\n- [ ] shared task\n";
        assert!(classify_anchor(content, "shared task").is_err());
    }
}
