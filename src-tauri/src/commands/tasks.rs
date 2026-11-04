//! Tasks pane backend (Phase 2.1 slices 1–3).
//!
//! - Slice 1: schema + indexer + `list_tasks` / `debug_dump_tasks`.
//! - Slice 2: pane UI consumes `list_tasks`.
//! - Slice 3: `complete_task` flips `[ ]` ↔ `[x]` in the source markdown.
//!
//! The indexer (`db::queries::notes::index_note_tasks`) owns the row
//! lifecycle: every per-save `index_note_pass1` and every init-time
//! `bulk_index_pass1` clear-and-rewrite the rows for the changed note.
//! `complete_task` performs the disk-write half of the round-trip then
//! re-runs the per-note indexer so the pane reflects the new state
//! within the same Tauri call.

use cozo::ScriptMutability;
use serde::Serialize;
use std::collections::BTreeMap;
use tauri::State;

use crate::commands::files::{to_rel, RoostFsState};
use crate::commands::graph::GraphDb;
use crate::db::queries;

#[derive(Debug, Clone, Serialize, specta::Type)]
pub struct TaskRow {
    /// Note id this task lives in (roost-relative, lowercased path stem).
    pub note_id: String,
    /// Absolute path of the source note. Frontend uses this to open the file.
    pub note_path: String,
    /// Title of the source note (for the "in: Foo" pane affordance).
    pub note_title: String,
    /// 1-based line in the body for inline checkboxes; negative virtual
    /// numbers for frontmatter-derived tasks.
    pub source_line: i32,
    pub text: String,
    /// Empty string when no due date is set. `YYYY-MM-DD` or `YYYY-MM-DD HH:MM`.
    pub due_at: String,
    pub completed: bool,
    pub completed_at: String,
    pub linked_person: String,
    pub linked_project: String,
}

/// Return every task row in the roost. Pure function over a `DbInstance`
/// so callers that already hold the graph-db lock (notably `index_note`)
/// can fold the result into their own response without re-entering the
/// IPC handler. Sort order: open before completed; earliest due first;
/// stable tiebreak on path then source line.
pub fn query_all_tasks(db: &cozo::DbInstance) -> Result<Vec<TaskRow>, String> {
    let script = r#"
        ?[note_id, source_line, text, due_at, completed, completed_at, linked_person, linked_project, note_title, note_path] :=
            *task{ note_id, source_line, text, due_at, completed, completed_at, linked_person, linked_project },
            *notes{ id: note_id, title: note_title, path: note_path }
    "#;

    let result = db
        .run_script(script, BTreeMap::new(), ScriptMutability::Immutable)
        .map_err(|e| format!("list_tasks query failed: {e}"))?;

    let mut rows: Vec<TaskRow> = Vec::with_capacity(result.rows.len());
    for row in result.rows.iter() {
        if row.len() < 10 {
            continue;
        }
        let note_id = row[0].get_str().unwrap_or_default().to_string();
        let source_line = match &row[1] {
            cozo::DataValue::Num(cozo::Num::Int(i)) => *i as i32,
            _ => 0,
        };
        let text = row[2].get_str().unwrap_or_default().to_string();
        let due_at = row[3].get_str().unwrap_or_default().to_string();
        let completed = matches!(&row[4], cozo::DataValue::Bool(true));
        let completed_at = row[5].get_str().unwrap_or_default().to_string();
        let linked_person = row[6].get_str().unwrap_or_default().to_string();
        let linked_project = row[7].get_str().unwrap_or_default().to_string();
        let note_title = row[8].get_str().unwrap_or_default().to_string();
        let note_path = row[9].get_str().unwrap_or_default().to_string();
        rows.push(TaskRow {
            note_id,
            note_path,
            note_title,
            source_line,
            text,
            due_at,
            completed,
            completed_at,
            linked_person,
            linked_project,
        });
    }

    // Layer 2: drop rows from user-excluded folders. Query-time (not
    // index-time) so a Settings change applies on the next pane refresh
    // with no re-index. The Tasks pane (`list_tasks`), the post-save fold
    // (`index_note`), and the agent's `query_tasks` all route through here,
    // so a single filter covers all three. The notification scheduler reads
    // `:task` on its own path and applies the same filter there.
    let excludes = excluded_task_folders(db);
    if !excludes.is_empty() {
        rows.retain(|r| task_row_visible(&r.note_id, &excludes));
    }

    // Sort in Rust rather than via :order — Cozo's lexicographic sort on
    // empty strings vs date strings is correct, but composing the multi-key
    // tiebreak in Datalog is more verbose than just doing it here. Roost
    // sizes for tasks are bounded (50K notes × ~5 tasks/note worst case);
    // this is a microsecond operation.
    rows.sort_by(|a, b| {
        a.completed
            .cmp(&b.completed)
            .then_with(|| sort_key_due(&a.due_at).cmp(&sort_key_due(&b.due_at)))
            .then_with(|| a.note_path.cmp(&b.note_path))
            .then_with(|| a.source_line.cmp(&b.source_line))
    });

    Ok(rows)
}

/// Tauri command wrapper around [`query_all_tasks`]. Acquires the lock,
/// delegates, releases.
#[tauri::command]
#[specta::specta]
pub fn list_tasks(db_state: State<'_, GraphDb>) -> Result<Vec<TaskRow>, String> {
    let guard = db_state.db.lock().map_err(|e| format!("Lock error: {e}"))?;
    let db = guard.as_ref().ok_or("Graph DB not initialized")?;
    query_all_tasks(db)
}

/// Sort key: empty due_at goes to the end; otherwise the date string itself
/// sorts correctly (ISO-8601 is lexicographic-friendly).
fn sort_key_due(due_at: &str) -> (u8, &str) {
    if due_at.is_empty() {
        (1, "")
    } else {
        (0, due_at)
    }
}

// ── Tasks-pane folder exclusions (tasks-noise redesign, Layer 2) ──────
//
// A device-local, per-roost list of folders whose checkboxes should not
// surface as tasks (Templates/, Reference/, Archive/, …). Enforced at
// QUERY time — `query_all_tasks` (pane + agent) and the notification
// scheduler share `task_row_visible` — so toggling the setting applies
// instantly: one `starts_with` over rows already in hand, vs. re-reading
// every file (expensive on Android SAF). Stored in `kv_meta` as a JSON
// array; the graph DB is device-local, so the list doesn't travel between
// synced devices (same as every other device setting today).

/// `kv_meta` key holding the JSON-encoded excluded-folder list.
pub(crate) const EXCLUDED_FOLDERS_KEY: &str = "tasks_excluded_folders";

/// Read the excluded folders exactly as stored (original case, roost-
/// relative, forward slashes) for display in Settings and the panel scope
/// picker. Empty when unset or malformed — a bad value never hides tasks.
pub(crate) fn read_excluded_folders_raw(db: &cozo::DbInstance) -> Vec<String> {
    let Ok(Some(raw)) = crate::db::schema::get_kv(db, EXCLUDED_FOLDERS_KEY) else {
        return Vec::new();
    };
    serde_json::from_str::<Vec<String>>(&raw).unwrap_or_default()
}

/// Normalize one folder path for prefix-matching against a `note_id`
/// (which `note_id_from_abs_path` emits lowercased, forward-slashed, no
/// leading/trailing slash, no extension). `None` for entries that
/// normalize to empty (`"/"`, `"  "`) so a junk value can't hide the
/// whole roost.
fn normalize_folder(folder: &str) -> Option<String> {
    // Trim surrounding whitespace AND slashes (a pasted "  Templates  " or a
    // whitespace-only "   " must normalize away), then lowercase to match
    // the lowercased `note_id`.
    let norm = folder
        .replace('\\', "/")
        .trim()
        .trim_matches('/')
        .trim()
        .to_lowercase();
    if norm.is_empty() {
        None
    } else {
        Some(norm)
    }
}

/// Clean a user-supplied folder list for storage: backslashes→`/`,
/// surrounding slashes trimmed, blanks dropped, de-duplicated
/// case-insensitively (first spelling kept, original case preserved for
/// display). Pure so the cleaning rules are unit-testable without a DB.
fn clean_folder_input(folders: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    folders
        .into_iter()
        .map(|f| {
            f.replace('\\', "/")
                .trim()
                .trim_matches('/')
                .trim()
                .to_string()
        })
        .filter(|f| !f.is_empty())
        .filter(|f| seen.insert(f.to_lowercase()))
        .collect()
}

/// Excluded folders normalized for matching. Read once per query, then
/// handed to [`task_row_visible`] per row.
pub(crate) fn excluded_task_folders(db: &cozo::DbInstance) -> Vec<String> {
    read_excluded_folders_raw(db)
        .iter()
        .filter_map(|f| normalize_folder(f))
        .collect()
}

/// True when a task's note (identified by its roost-relative `note_id`) is
/// NOT inside any excluded folder. A note is "inside" folder `f` when its
/// id starts with `f/` — so excluding `templates` hides `templates/daily`
/// but never `templates-archive/x` or a root note literally named
/// `templates`. Allocation-free; `excludes` must already be normalized
/// (see [`excluded_task_folders`]).
pub(crate) fn task_row_visible(note_id: &str, excludes: &[String]) -> bool {
    !excludes.iter().any(|folder| {
        note_id.len() > folder.len()
            && note_id.as_bytes()[folder.len()] == b'/'
            && note_id.starts_with(folder.as_str())
    })
}

/// Read the Tasks-pane excluded-folder list (roost-relative, as the user entered it). Device-local; see `read_excluded_folders_raw`.
#[tauri::command]
#[specta::specta]
pub fn get_excluded_task_folders(db_state: State<'_, GraphDb>) -> Result<Vec<String>, String> {
    let guard = db_state.db.lock().map_err(|e| format!("Lock error: {e}"))?;
    let db = guard.as_ref().ok_or("Graph DB not initialized")?;
    Ok(read_excluded_folders_raw(db))
}

/// Replace the Tasks-pane excluded-folder list (see `clean_folder_input`). Applies on the next query, and rebuilds notification timers.
#[tauri::command]
#[specta::specta]
pub fn set_excluded_task_folders(
    folders: Vec<String>,
    db_state: State<'_, GraphDb>,
) -> Result<(), String> {
    let cleaned = clean_folder_input(folders);
    let json = serde_json::to_string(&cleaned).map_err(|e| format!("encode folders: {e}"))?;

    let guard = db_state.db.lock().map_err(|e| format!("Lock error: {e}"))?;
    let db = guard.as_ref().ok_or("Graph DB not initialized")?;
    crate::db::schema::set_kv(db, EXCLUDED_FOLDERS_KEY, &json)?;
    // Drop/re-arm notification timers for the changed exclusion set. Rare
    // call (user toggled a setting), so a full rebuild is fine.
    db_state.notification_scheduler.reschedule_all(db);
    // NEW-S2: the exclusion set is applied at query time, so the visible task
    // list just changed — drop the cached stats snapshot.
    db_state.invalidate_stats_cache();
    Ok(())
}

/// Toggle a checkbox task's completed state in the source markdown.
///
/// Round-trip:
///   1. Read the file via the active RoostFs backend.
///   2. Find the target line — first try `lines[source_line - 1]`; if it
///      isn't a checkbox or the text doesn't match `content_anchor`,
///      fall back to a roost-wide scan for any line whose task body
///      starts with the anchor. The fallback handles external edits that
///      shifted line numbers (vim, Obsidian, sync).
///   3. Flip `[ ]` ↔ `[x]`. When marking complete, append `✅ YYYY-MM-DD`
///      if no completion stamp is present (per ROOST-CONVENTIONS.md). When
///      un-completing, strip the `✅ ...` token.
///   4. Atomic write the new content via `RoostFs::write_atomic`.
///   5. Re-run the per-note task indexer so the row reflects the new
///      state inside the same call. The frontend can then refresh the
///      pane without waiting for a separate `index_note` invoke.
///
/// Frontmatter tasks (`source_line < 0`) are NOT supported in this slice —
/// flipping a frontmatter task requires YAML rewriting, which lands later.
/// Returns the new file content so the frontend can update the open tab.
#[tauri::command]
#[specta::specta]
pub async fn complete_task(
    note_path: String,
    source_line: i32,
    completed: bool,
    content_anchor: String,
    db_state: State<'_, GraphDb>,
    fs_state: State<'_, RoostFsState>,
    sync_state: State<'_, super::sync::SyncEngineState>,
) -> Result<String, String> {
    let generation = fs_state.generation();
    let backend = fs_state
        .current()
        .await
        .ok_or_else(|| "No roost is open".to_string())?;
    let rel = to_rel(&note_path, backend.display_root())
        .ok_or_else(|| format!("Access denied: '{note_path}' is outside the active roost"))?;
    let _mutation_guard = fs_state.lock_note(&rel).await;
    if !fs_state.is_generation_current(generation) {
        return Err("Roost changed before the task update could commit".to_string());
    }

    let bytes = backend
        .read(&rel)
        .await
        .map_err(|e| format!("Failed to read {note_path}: {e}"))?;
    let original =
        String::from_utf8(bytes).map_err(|e| format!("File not UTF-8 ({note_path}): {e}"))?;

    // Frontmatter tasks (negative virtual line numbers) live in the
    // YAML `tasks:` array; route through the YAML rewrite helper.
    // Body tasks stay on the markdown checkbox-flip path.
    let new_content = if source_line <= 0 {
        crate::commands::frontmatter_tasks::rewrite_frontmatter_task(
            &original,
            &content_anchor,
            crate::commands::frontmatter_tasks::FrontmatterMutation::ToggleCompleted(completed),
        )?
    } else {
        rewrite_checkbox(&original, source_line, completed, &content_anchor)?
    };

    backend
        .write_atomic(&rel, new_content.as_bytes())
        .await
        .map_err(|e| format!("Failed to write {note_path}: {e}"))?;

    // Propagate the toggle to sync peers. The file watcher can't be relied
    // on (it's dead on Android SAF) — see `files::notify_sync_local_write`.
    crate::commands::files::notify_sync_local_write(sync_state.inner(), &rel, &new_content).await;

    // Re-run the per-note task indexer. We don't need the full pass1+pass2
    // because checkbox edits don't change wikilinks or tags at the note
    // level — only the `:task` rows change. Cheap and keeps the pane fresh.
    // Also record the toggle as an edit-history event so the History pane
    // can revert it. Best-effort — record errors are logged, not propagated.
    let note_id = {
        let guard = db_state.db.lock().map_err(|e| format!("Lock error: {e}"))?;
        let db = guard.as_ref().ok_or("Graph DB not initialized")?;
        let roost_guard = db_state
            .roost_path
            .lock()
            .map_err(|e| format!("Lock error: {e}"))?;
        let roost_root = roost_guard.as_deref().ok_or("Roost path not set")?;
        let note_id = queries::note_id_from_abs_path(&note_path, roost_root);
        queries::index_note_tasks(db, &note_id, &new_content)?;
        if let Err(e) = queries::record_edit_event(
            db,
            &note_id,
            &note_path,
            queries::Actor::User,
            "user",
            queries::Intent::ToggleTask,
            &original,
            &new_content,
        ) {
            tracing::warn!(%note_id, error = %e, "edit_history: toggle_task record failed");
        }
        note_id
    };
    // Scheduler reschedule outside the DB lock — it takes its own read.
    {
        let guard = db_state.db.lock().map_err(|e| format!("Lock error: {e}"))?;
        if let Some(db_ref) = guard.as_ref() {
            db_state
                .notification_scheduler
                .reschedule_for_note(db_ref, &note_id);
        }
    }
    // NEW-S2: this note's tasks just changed — drop the cached stats snapshot
    // so the unchanged-save fast path re-derives the current task list.
    db_state.invalidate_stats_cache();

    Ok(new_content)
}

/// Pure-string transform: locate the task line, flip its checkbox, and
/// manage the `✅` completion stamp. Returns the new full file content.
///
/// Identity strategy (Desktop bug #4, 0.2.1):
///   1. Trust `lines[source_line - 1]` when it's a checkbox matching
///      `content_anchor` — the recorded line is almost always right.
///   2. Else the line drifted (an external/sync edit moved it). Scan for
///      checkboxes matching the anchor and, when several share the prefix,
///      pick the one NEAREST the recorded line so a prefix collision can't
///      silently flip a different task than the user clicked.
///   3. No match → a clear error (the caller surfaces it), not a silent no-op.
pub(crate) fn rewrite_checkbox(
    content: &str,
    source_line: i32,
    completed: bool,
    content_anchor: &str,
) -> Result<String, String> {
    let lines: Vec<&str> = content.split_inclusive('\n').collect();
    let target_idx = (source_line as usize).saturating_sub(1);

    let chosen_idx =
        if target_idx < lines.len() && line_body_starts_with(lines[target_idx], content_anchor) {
            target_idx
        } else {
            let matches: Vec<usize> = lines
                .iter()
                .enumerate()
                .filter(|(_, l)| line_body_starts_with(l, content_anchor))
                .map(|(i, _)| i)
                .collect();
            match matches.as_slice() {
                [] => {
                    return Err(format!(
                        "Could not find the task to toggle. Source line {source_line} doesn't \
                     match a checkbox starting with \"{content_anchor}\". The note may have \
                     been edited externally — try refreshing."
                    ));
                }
                [only] => *only,
                _ => *matches
                    .iter()
                    .min_by_key(|&&i| i.abs_diff(target_idx))
                    .expect("matches is non-empty"),
            }
        };

    let new_line = flip_checkbox_line(lines[chosen_idx], completed)
        .ok_or_else(|| format!("Line {} is not a recognizable checkbox", chosen_idx + 1))?;

    let mut out = String::with_capacity(content.len() + 16);
    for (i, line) in lines.iter().enumerate() {
        if i == chosen_idx {
            out.push_str(&new_line);
        } else {
            out.push_str(line);
        }
    }
    Ok(out)
}

/// True if `line` is a checkbox task whose body starts with `anchor`. Compares
/// the anchor against BOTH the raw body AND the parser's stripped task text,
/// because the stored `content_anchor` is built from the stripped text — a task
/// whose raw line LEADS with 📅/⏰ only matches via the stripped form. Checking
/// both is additive: it never loses a raw match, it only recovers the
/// leading-metadata ones that used to silently fail and revert the toggle
/// (Desktop bug #4, 0.2.1).
pub(crate) fn line_body_starts_with(line: &str, anchor: &str) -> bool {
    if anchor.is_empty() {
        return checkbox_body(line).is_some();
    }
    if let Some(body) = checkbox_body(line) {
        if body.trim_start().starts_with(anchor) {
            return true;
        }
    }
    crate::parser::checkboxes::checkbox_task_text(line).is_some_and(|text| text.starts_with(anchor))
}

/// Return the body of a checkbox task (everything after `- [ ] ` or
/// `- [x] `, trailing newline included). `None` if the line isn't a task.
fn checkbox_body(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    let after_bullet = trimmed
        .strip_prefix("- ")
        .or_else(|| trimmed.strip_prefix("* "))
        .or_else(|| trimmed.strip_prefix("+ "))?;
    if let Some(r) = after_bullet.strip_prefix("[ ] ") {
        return Some(r);
    }
    if let Some(r) = after_bullet.strip_prefix("[x] ") {
        return Some(r);
    }
    if let Some(r) = after_bullet.strip_prefix("[X] ") {
        return Some(r);
    }
    None
}

/// Rewrite a single checkbox line. Preserves indentation, bullet style,
/// and trailing newline. Manages the `✅` stamp per ROOST-CONVENTIONS.
fn flip_checkbox_line(line: &str, mark_complete: bool) -> Option<String> {
    let trailing_newline = if line.ends_with("\r\n") {
        "\r\n"
    } else if line.ends_with('\n') {
        "\n"
    } else {
        ""
    };
    let body_only = line.trim_end_matches('\n').trim_end_matches('\r');

    let indent_len = body_only
        .bytes()
        .take_while(|b| *b == b' ' || *b == b'\t')
        .count();
    let (indent, after_indent) = body_only.split_at(indent_len);

    let bullet = after_indent.chars().next()?;
    if bullet != '-' && bullet != '*' && bullet != '+' {
        return None;
    }
    let after_bullet = &after_indent[bullet.len_utf8()..];
    if !after_bullet.starts_with(' ') {
        return None;
    }
    let after_space = &after_bullet[1..];

    // Strip the existing checkbox marker and capture the text body.
    let body = if let Some(r) = after_space.strip_prefix("[ ] ") {
        r
    } else if let Some(r) = after_space.strip_prefix("[x] ") {
        r
    } else {
        after_space.strip_prefix("[X] ")?
    };

    let new_marker = if mark_complete { "[x]" } else { "[ ]" };

    let body_string = if mark_complete {
        if body.contains("✅") {
            body.to_string()
        } else {
            let stamp = today_iso_local();
            format!("{} ✅ {}", body.trim_end(), stamp)
        }
    } else {
        strip_completion_stamp(body)
    };

    Some(format!(
        "{indent}{bullet} {new_marker} {body_string}{trailing_newline}",
    ))
}

/// Drop the `✅ YYYY-MM-DD` token (and its leading space) from a task body.
/// Other emoji metadata (📅, ⏰, tags, links) is preserved verbatim.
fn strip_completion_stamp(body: &str) -> String {
    let Some(pos) = body.find("✅") else {
        return body.trim_end().to_string();
    };
    let before = body[..pos].trim_end().to_string();
    let after = &body[pos + "✅".len()..];
    let after_trimmed = after.trim_start();
    // Skip past the date token (whitespace-bounded).
    let token_end = after_trimmed
        .find(|c: char| c.is_whitespace())
        .unwrap_or(after_trimmed.len());
    let tail = after_trimmed[token_end..].trim_start();
    if tail.is_empty() {
        before
    } else {
        format!("{before} {tail}")
    }
}

/// Today's date in the user's local timezone, formatted as YYYY-MM-DD.
/// Used to stamp `✅ YYYY-MM-DD` on completed tasks. UTC would give the
/// wrong day for users east/west of UTC during the wrap-around hours
/// (e.g., IST user at 00:30 would have gotten yesterday's date).
fn today_iso_local() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

#[cfg(test)]
mod rewrite_tests {
    use super::*;

    #[test]
    fn flips_open_to_done_and_appends_stamp() {
        let content = "- [ ] hello world\n";
        let out = rewrite_checkbox(content, 1, true, "hello world").unwrap();
        assert!(out.starts_with("- [x] hello world ✅ "));
        assert!(out.ends_with("\n"));
    }

    #[test]
    fn flips_done_to_open_strips_stamp() {
        let content = "- [x] hello ✅ 2026-05-01\n";
        let out = rewrite_checkbox(content, 1, false, "hello").unwrap();
        assert_eq!(out, "- [ ] hello\n");
    }

    #[test]
    fn preserves_indent_and_due_date() {
        let content = "  - [ ] task 📅 2026-05-08\n";
        let out = rewrite_checkbox(content, 1, true, "task").unwrap();
        assert!(out.starts_with("  - [x] task 📅 2026-05-08 ✅ "));
    }

    #[test]
    fn preserves_bullet_style() {
        let content = "* [ ] starred\n+ [ ] plused\n";
        let out1 = rewrite_checkbox(content, 1, true, "starred").unwrap();
        assert!(out1.starts_with("* [x] starred"));
        let out2 = rewrite_checkbox(&out1, 2, true, "plused").unwrap();
        assert!(out2.contains("\n+ [x] plused"));
    }

    #[test]
    fn falls_back_to_anchor_when_line_drifted() {
        // Source line says line 3, but the task has shifted up to line 2
        // because an external edit deleted a line above it. Anchor match
        // recovers it.
        let content = "first line\n- [ ] target task\n";
        let out = rewrite_checkbox(content, 3, true, "target task").unwrap();
        assert!(out.contains("- [x] target task ✅ "));
    }

    #[test]
    fn errors_when_anchor_doesnt_match_anywhere() {
        let content = "- [ ] something else entirely\n";
        let err = rewrite_checkbox(content, 1, true, "ghost task").unwrap_err();
        assert!(err.contains("Could not find"));
    }

    #[test]
    fn idempotent_completion_does_not_double_stamp() {
        let content = "- [x] done ✅ 2026-05-01\n";
        let out = rewrite_checkbox(content, 1, true, "done").unwrap();
        // Already had a stamp; we shouldn't add a second one.
        let stamps = out.matches("✅").count();
        assert_eq!(stamps, 1);
    }

    #[test]
    fn handles_crlf_line_endings() {
        let content = "- [ ] crlf\r\n";
        let out = rewrite_checkbox(content, 1, true, "crlf").unwrap();
        assert!(out.starts_with("- [x] crlf ✅ "));
        assert!(out.ends_with("\r\n"));
    }

    #[test]
    fn does_not_touch_unrelated_lines() {
        let content = "- [ ] one\n- [ ] two\n- [ ] three\n";
        let out = rewrite_checkbox(content, 2, true, "two").unwrap();
        assert!(out.contains("- [ ] one\n"));
        assert!(out.contains("- [x] two ✅ "));
        assert!(out.contains("- [ ] three\n"));
    }

    #[test]
    fn strip_stamp_keeps_other_metadata() {
        // Body `task 📅 2026-05-08 ✅ 2026-05-09` — uncomplete should drop
        // only the ✅ token, keep 📅.
        let content = "- [x] task 📅 2026-05-08 ✅ 2026-05-09\n";
        let out = rewrite_checkbox(content, 1, false, "task").unwrap();
        assert_eq!(out, "- [ ] task 📅 2026-05-08\n");
    }

    #[test]
    fn matches_task_with_leading_date_metadata() {
        // Date written BEFORE the text. The parser stores the stripped text
        // (" buy milk"), and the anchor is built from THAT — so matching only
        // the raw body (which starts with "📅…") used to fail and silently
        // revert the toggle (bug #4). The parsed-text fallback recovers it.
        let content = "- [ ] 📅 2026-05-08 buy milk\n";
        let out = rewrite_checkbox(content, 1, true, " buy milk").unwrap();
        assert!(out.contains("- [x] 📅 2026-05-08 buy milk ✅ "));
    }

    #[test]
    fn prefers_nearest_match_on_prefix_collision_after_drift() {
        // Two tasks share the anchor prefix. The recorded line drifted past the
        // SECOND one. First-match would flip the first task; nearest-to-
        // source_line flips the intended (second) one (bug #4).
        let content = "- [ ] Follow up with the vendor\nnote\n- [ ] Follow up with the vendor\n";
        let out = rewrite_checkbox(content, 4, true, "Follow up with the vendor").unwrap();
        let lines: Vec<&str> = out.lines().collect();
        assert!(
            lines[0].starts_with("- [ ] Follow up"),
            "first must stay open"
        );
        assert!(
            lines[2].starts_with("- [x] Follow up"),
            "nearest (2nd) must flip"
        );
    }

    #[test]
    fn trusts_source_line_over_earlier_prefix_collision() {
        // Both lines share the prefix; source_line points exactly at the 2nd,
        // so it wins even though line 1 also matches the anchor.
        let content = "- [ ] Review the doc\n- [ ] Review the doc\n";
        let out = rewrite_checkbox(content, 2, true, "Review the doc").unwrap();
        let lines: Vec<&str> = out.lines().collect();
        assert!(
            lines[0].starts_with("- [ ] Review the doc"),
            "first stays open"
        );
        assert!(
            lines[1].starts_with("- [x] Review the doc"),
            "exact line flips"
        );
    }

    #[test]
    fn checkbox_task_text_strips_metadata_for_matching() {
        use crate::parser::checkboxes::checkbox_task_text;
        assert_eq!(
            checkbox_task_text("- [ ] task 📅 2026-05-08 ⏰ 09:00").as_deref(),
            Some("task")
        );
        assert_eq!(
            checkbox_task_text("  * [x] done ✅ 2026-05-01").as_deref(),
            Some("done")
        );
        assert_eq!(checkbox_task_text("not a checkbox"), None);
    }
}

#[cfg(test)]
mod folder_exclusion_tests {
    use super::*;

    #[test]
    fn normalize_folder_lowercases_and_normalizes_separators() {
        assert_eq!(normalize_folder("Templates").as_deref(), Some("templates"));
        assert_eq!(normalize_folder("/Archive/").as_deref(), Some("archive"));
        assert_eq!(
            normalize_folder("Areas\\Work").as_deref(),
            Some("areas/work")
        );
        assert_eq!(
            normalize_folder("Daily Notes").as_deref(),
            Some("daily notes")
        );
    }

    #[test]
    fn normalize_folder_drops_empty_and_slash_only() {
        assert_eq!(normalize_folder(""), None);
        assert_eq!(normalize_folder("   "), None);
        assert_eq!(normalize_folder("/"), None);
        assert_eq!(normalize_folder("///"), None);
    }

    #[test]
    fn task_row_visible_hides_notes_inside_excluded_folder() {
        let ex = vec!["templates".to_string()];
        assert!(!task_row_visible("templates/daily", &ex));
        assert!(!task_row_visible("templates/sub/deep", &ex));
    }

    #[test]
    fn task_row_visible_keeps_notes_outside_excluded_folder() {
        let ex = vec!["templates".to_string()];
        assert!(task_row_visible("daily notes/2026-06-10", &ex));
        // Sibling folder sharing a name prefix must NOT be excluded.
        assert!(task_row_visible("templates-archive/x", &ex));
        // A root note literally named like the folder isn't "inside" it.
        assert!(task_row_visible("templates", &ex));
    }

    #[test]
    fn task_row_visible_matches_nested_excluded_folder() {
        let ex = vec!["areas/work".to_string()];
        assert!(!task_row_visible("areas/work/standup", &ex));
        assert!(task_row_visible("areas/home/chores", &ex));
        // The parent of an excluded subfolder is not itself excluded.
        assert!(task_row_visible("areas/overview", &ex));
    }

    #[test]
    fn task_row_visible_empty_excludes_keeps_everything() {
        assert!(task_row_visible("anything/at/all", &[]));
    }

    #[test]
    fn task_row_visible_honors_multiple_excludes() {
        let ex = vec!["templates".to_string(), "archive".to_string()];
        assert!(!task_row_visible("templates/x", &ex));
        assert!(!task_row_visible("archive/old/y", &ex));
        assert!(task_row_visible("projects/active", &ex));
    }

    #[test]
    fn clean_folder_input_trims_dedupes_and_drops_blanks() {
        let out = clean_folder_input(vec![
            "  Templates  ".to_string(),
            "/Archive/".to_string(),
            "templates".to_string(), // case-insensitive dup of "Templates"
            "".to_string(),
            "   ".to_string(),
            "Areas\\Work".to_string(),
        ]);
        // Trimmed; first spelling kept; blanks gone; backslashes normalized.
        assert_eq!(
            out,
            vec![
                "Templates".to_string(),
                "Archive".to_string(),
                "Areas/Work".to_string(),
            ]
        );
    }

    #[test]
    fn excluded_folders_round_trip_through_kv() {
        use cozo::{DbInstance, ScriptMutability};
        let db = DbInstance::new("mem", "", Default::default()).unwrap();
        db.run_script(
            r#":create kv_meta { key: String => value: String }"#,
            Default::default(),
            ScriptMutability::Mutable,
        )
        .unwrap();

        // Unset → empty (no kv row yet).
        assert!(excluded_task_folders(&db).is_empty());
        assert!(read_excluded_folders_raw(&db).is_empty());

        // Store original-case folders; raw read preserves case, the match
        // set is normalized (lowercase, forward-slash).
        let json = serde_json::to_string(&["Templates", "Archive/Old"]).unwrap();
        crate::db::schema::set_kv(&db, EXCLUDED_FOLDERS_KEY, &json).unwrap();
        assert_eq!(
            read_excluded_folders_raw(&db),
            vec!["Templates".to_string(), "Archive/Old".to_string()]
        );
        assert_eq!(
            excluded_task_folders(&db),
            vec!["templates".to_string(), "archive/old".to_string()]
        );

        // Malformed JSON must degrade to empty — never panic, never hide tasks.
        crate::db::schema::set_kv(&db, EXCLUDED_FOLDERS_KEY, "not json").unwrap();
        assert!(read_excluded_folders_raw(&db).is_empty());
        assert!(excluded_task_folders(&db).is_empty());
    }
}

#[cfg(test)]
mod quick_add_tests {
    use super::*;

    #[test]
    fn cleans_leading_bullet_and_checkbox() {
        assert_eq!(clean_quick_add_text("- [ ] foo"), "foo");
        assert_eq!(clean_quick_add_text("- [x] foo"), "foo");
        assert_eq!(clean_quick_add_text("- foo"), "foo");
        assert_eq!(clean_quick_add_text("* foo"), "foo");
        assert_eq!(clean_quick_add_text("+ foo"), "foo");
        assert_eq!(clean_quick_add_text("foo"), "foo");
        assert_eq!(clean_quick_add_text("   foo   "), "foo");
    }

    #[test]
    fn inserts_in_existing_tasks_section() {
        let content = "# Title\n\n## Tasks\n\n- [ ] existing\n\n## Notes\n\nbody\n";
        let (out, line) = append_task_to_section(content, "new task");
        assert_eq!(line, 6);
        assert_eq!(
            out,
            "# Title\n\n## Tasks\n\n- [ ] existing\n- [ ] new task\n\n## Notes\n\nbody\n"
        );
    }

    #[test]
    fn inserts_in_empty_tasks_section() {
        let content = "## Tasks\n\n## Notes\n\nbody\n";
        let (out, line) = append_task_to_section(content, "first");
        assert_eq!(line, 2);
        assert_eq!(out, "## Tasks\n- [ ] first\n\n## Notes\n\nbody\n");
    }

    #[test]
    fn appends_section_when_no_tasks_heading() {
        let content = "# Title\n\nSome body.\n";
        let (out, _line) = append_task_to_section(content, "first");
        assert!(out.ends_with("## Tasks\n\n- [ ] first\n"));
    }

    #[test]
    fn matches_tasks_heading_case_insensitively() {
        let content = "## TASKS\n\n- [ ] one\n";
        let (out, _) = append_task_to_section(content, "two");
        assert!(out.contains("- [ ] one\n- [ ] two\n"));
    }

    #[test]
    fn preserves_due_date_metadata_in_input() {
        let content = "## Tasks\n\n";
        let (out, _) = append_task_to_section(content, "review 📅 2026-05-08");
        assert!(out.contains("- [ ] review 📅 2026-05-08\n"));
    }

    #[test]
    fn handles_file_with_no_trailing_newline() {
        let content = "## Tasks\n\n- [ ] one";
        let (out, _) = append_task_to_section(content, "two");
        assert!(out.contains("- [ ] one\n- [ ] two\n"));
    }
}

#[cfg(test)]
mod agent_crud_tests {
    use super::*;

    #[test]
    fn find_task_line_index_picks_first_match() {
        let content = "intro\n- [ ] alpha\n- [ ] beta\n";
        assert_eq!(find_task_line_index(content, "alpha"), Some(1));
        assert_eq!(find_task_line_index(content, "beta"), Some(2));
        assert_eq!(find_task_line_index(content, "ghost"), None);
    }

    #[test]
    fn count_task_lines_matching_returns_zero_one_or_many() {
        let content = "- [ ] alpha\n- [ ] alphabeta\n- [ ] gamma\n";
        assert_eq!(count_task_lines_matching(content, "alpha"), 2);
        assert_eq!(count_task_lines_matching(content, "gamma"), 1);
        assert_eq!(count_task_lines_matching(content, "delta"), 0);
    }

    #[test]
    fn rewrite_task_text_swaps_body_preserves_marker_and_indent() {
        let content = "  - [x] old text 📅 2026-05-08\n";
        let out = rewrite_task_text(content, "old text", "new text 📅 2026-05-09").unwrap();
        // Indent + marker preserved; body fully replaced.
        assert_eq!(out, "  - [x] new text 📅 2026-05-09\n");
    }

    #[test]
    fn rewrite_task_text_errors_on_missing_anchor() {
        let content = "- [ ] something\n";
        let err = rewrite_task_text(content, "ghost", "new").unwrap_err();
        assert!(err.contains("ghost"));
    }

    #[test]
    fn rewrite_task_metadata_inserts_missing_date() {
        let content = "- [ ] standup\n";
        let out = rewrite_task_metadata(content, "standup", "2026-05-08", "").unwrap();
        assert_eq!(out, "- [ ] standup 📅 2026-05-08\n");
    }

    #[test]
    fn rewrite_task_metadata_replaces_existing_date_and_time() {
        let content = "- [ ] standup 📅 2026-05-08 ⏰ 10:00\n";
        let out = rewrite_task_metadata(content, "standup", "2026-05-09", "11:30").unwrap();
        assert_eq!(out, "- [ ] standup 📅 2026-05-09 ⏰ 11:30\n");
    }

    #[test]
    fn rewrite_task_metadata_clear_removes_token() {
        let content = "- [ ] standup 📅 2026-05-08 ⏰ 10:00\n";
        let out = rewrite_task_metadata(content, "standup", "clear", "").unwrap();
        // Date cleared, time preserved.
        assert!(!out.contains("📅"));
        assert!(out.contains("⏰ 10:00"));
    }

    #[test]
    fn rewrite_task_metadata_preserves_other_metadata() {
        let content = "- [ ] standup 📅 2026-05-08 #project/x\n";
        let out = rewrite_task_metadata(content, "standup", "2026-05-09", "").unwrap();
        assert!(out.contains("📅 2026-05-09"));
        assert!(out.contains("#project/x"));
    }

    #[test]
    fn delete_task_line_removes_target_and_keeps_rest() {
        let content = "intro\n- [ ] alpha\n- [ ] beta\nend\n";
        let out = delete_task_line(content, "beta").unwrap();
        assert_eq!(out, "intro\n- [ ] alpha\nend\n");
    }

    #[test]
    fn delete_task_line_errors_on_missing_anchor() {
        let content = "- [ ] alpha\n";
        let err = delete_task_line(content, "ghost").unwrap_err();
        assert!(err.contains("ghost"));
    }

    #[test]
    fn archive_task_creates_section_and_strikethroughs() {
        let content = "## Tasks\n\n- [ ] take notes\n";
        let out = archive_task(content, "take notes").unwrap();
        assert!(out.contains("## Archived"));
        assert!(out.contains("~~take notes~~"));
        // Original line gone, archived line marked complete.
        assert!(!out.contains("- [ ] take notes\n"));
        assert!(out.contains("- [x] ~~take notes~~"));
    }

    #[test]
    fn archive_task_appends_to_existing_archived_section() {
        let content = "## Tasks\n\n- [ ] keep\n- [ ] drop\n\n## Archived\n\n- [x] ~~old~~\n";
        let out = archive_task(content, "drop").unwrap();
        assert!(out.contains("- [ ] keep\n"));
        assert!(!out.contains("- [ ] drop\n"));
        assert!(out.contains("~~old~~"));
        assert!(out.contains("~~drop~~"));
        // Only one ## Archived heading.
        assert_eq!(out.matches("## Archived").count(), 1);
    }

    #[test]
    fn archive_task_strips_completion_stamp_before_strikethrough() {
        let content = "## Tasks\n\n- [x] foo ✅ 2026-05-01\n";
        let out = archive_task(content, "foo").unwrap();
        // Stamp was inside the body, archive should drop it.
        assert!(out.contains("~~foo~~"));
        assert!(!out.contains("✅"));
    }
}

/// Does this note already supply a date that its bare checkboxes inherit —
/// either a daily-note filename (`YYYY-MM-DD.md`) or a top-level frontmatter
/// `date:` (convention 0.3)? Quick-add uses this to decide whether a dateless
/// task needs an explicit due date stamped so it actually becomes a task.
fn note_supplies_date(note_path: &str, content: &str) -> bool {
    let stem = std::path::Path::new(note_path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    if stem.len() == 10 && chrono::NaiveDate::parse_from_str(stem, "%Y-%m-%d").is_ok() {
        return true;
    }
    crate::parser::checkboxes::frontmatter_date(content).is_some()
}

/// Append a `- [ ] {text}` line to a note. Slice-4 quick-add target.
///
/// Placement strategy:
///   1. If the note has a `## Tasks` heading, insert at the end of that
///      section (just before the next h2 or end of file). Daily-note
///      templates ship with `## Tasks` so quick-adds land in the obvious
///      spot.
///   2. Otherwise append `\n## Tasks\n\n- [ ] {text}` to the end of the
///      file, creating the section.
///
/// `text` is taken verbatim — `📅 / ⏰ / #tag / [[link]]` metadata in
/// the user's input is preserved so the parser picks it up on the next
/// indexer pass. Stripping a leading `- [ ] ` if the user re-typed the
/// bullet (so we don't end up with `- [ ] - [ ] foo`).
///
/// Returns the new line number (1-based) so the frontend can
/// optionally jump to the source.
#[tauri::command]
#[specta::specta]
pub async fn add_task_line(
    note_path: String,
    text: String,
    fs_state: State<'_, RoostFsState>,
    db_state: State<'_, GraphDb>,
    sync_state: State<'_, super::sync::SyncEngineState>,
) -> Result<u32, String> {
    let cleaned = clean_quick_add_text(&text);
    if cleaned.is_empty() {
        return Err("Task text is empty".to_string());
    }

    let generation = fs_state.generation();
    let backend = fs_state
        .current()
        .await
        .ok_or_else(|| "No roost is open".to_string())?;
    let rel = to_rel(&note_path, backend.display_root())
        .ok_or_else(|| format!("Access denied: '{note_path}' is outside the active roost"))?;
    let _mutation_guard = fs_state.lock_note(&rel).await;
    if !fs_state.is_generation_current(generation) {
        return Err("Roost changed before the task update could commit".to_string());
    }

    let bytes = backend
        .read(&rel)
        .await
        .map_err(|e| format!("Failed to read {note_path}: {e}"))?;
    let original =
        String::from_utf8(bytes).map_err(|e| format!("File not UTF-8 ({note_path}): {e}"))?;

    // Convention 0.3: a checkbox is a task ONLY with an effective date. A
    // quick-add is a deliberate to-do, so stamp today's date when the user
    // typed no 📅 AND the note doesn't already supply a date its checkboxes
    // inherit (a daily-note filename or a frontmatter `date:`). An explicit
    // 📅 the user typed always wins.
    let cleaned = if cleaned.contains('📅') || note_supplies_date(&note_path, &original) {
        cleaned
    } else {
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        format!("{cleaned} 📅 {today}")
    };

    let (new_content, new_line) = append_task_to_section(&original, &cleaned);

    backend
        .write_atomic(&rel, new_content.as_bytes())
        .await
        .map_err(|e| format!("Failed to write {note_path}: {e}"))?;

    // Propagate the new task line to sync peers (watcher is dead on Android
    // SAF) — see `files::notify_sync_local_write`.
    crate::commands::files::notify_sync_local_write(sync_state.inner(), &rel, &new_content).await;

    // Full index (notes row + tags + tasks + resolved links) rather than just
    // `index_note_tasks`. Quick-add can target a brand-new daily note that the
    // indexer has never seen — without the `:notes` row, `list_tasks` joins
    // `*task × *notes` and silently drops the new task. IP-8: use
    // `index_note_full` (not pass1 alone) so a `[[link]]` in the quick-add body
    // (e.g. "reply to [[Ravi]]") becomes a real edge, not an invisible link.
    // Single-note scope, and gets tags right too.  Also record the add as an
    // edit-history event.
    let note_id = {
        let guard = db_state.db.lock().map_err(|e| format!("Lock error: {e}"))?;
        let db = guard.as_ref().ok_or("Graph DB not initialized")?;
        let roost_guard = db_state
            .roost_path
            .lock()
            .map_err(|e| format!("Lock error: {e}"))?;
        let roost_root = roost_guard.as_deref().ok_or("Roost path not set")?;
        // IP-A3: reuse the cached resolver (add_id-maintained) instead of an
        // O(roost) rebuild per quick-add. See `GraphDb::index_note_full_cached`.
        db_state.index_note_full_cached(db, roost_root, &note_path, &new_content)?;
        let note_id = queries::note_id_from_abs_path(&note_path, roost_root);
        if let Err(e) = queries::record_edit_event(
            db,
            &note_id,
            &note_path,
            queries::Actor::User,
            "user",
            queries::Intent::AddTask,
            &original,
            &new_content,
        ) {
            tracing::warn!(%note_id, error = %e, "edit_history: add_task record failed");
        }
        note_id
    };
    // IP-A3: index_note_full_cached kept the cached resolver in sync (it
    // add_id'd any brand-new daily note), so we must NOT invalidate it — that
    // would force the next editor save to rebuild O(roost), the cost this fix
    // removes. Just mark the algo cache stale since links may have changed.
    db_state.mark_algo_cache_dirty();
    {
        let guard = db_state.db.lock().map_err(|e| format!("Lock error: {e}"))?;
        if let Some(db_ref) = guard.as_ref() {
            db_state
                .notification_scheduler
                .reschedule_for_note(db_ref, &note_id);
        }
    }
    // NEW-S2: a task line was added — drop the cached stats snapshot.
    db_state.invalidate_stats_cache();

    Ok(new_line)
}

/// AUDIT-005: notification "Snooze" action invokes this command. Was
/// only ever an agent-tool branch before — the IPC didn't exist, so the
/// frontend `invoke("reschedule_task", ...)` call silently failed and
/// every Snooze tap was a no-op.
///
/// Anchor-based dispatch: parses tasks, finds the single task whose
/// `text` starts with `text_anchor`, and routes to either the body
/// rewrite (`rewrite_task_metadata`) or the frontmatter rewrite
/// (`frontmatter_tasks::rewrite_frontmatter_task`) based on where the
/// match landed. Refuses on no-match, multi-match within one region, or
/// cross-region ambiguity — the snooze button gets a single specific
/// task anchor from the original notification payload, so any ambiguity
/// here is a real bug worth surfacing.
///
/// `new_date` / `new_time` accept `""` (don't touch the token), a
/// `YYYY-MM-DD` / `HH:MM` value (replace or insert), or the literal
/// string `"clear"` (remove the token). Same semantics as the agent's
/// `reschedule_task` tool.
#[tauri::command]
#[specta::specta]
pub async fn reschedule_task(
    note_path: String,
    text_anchor: String,
    new_date: String,
    new_time: String,
    fs_state: State<'_, RoostFsState>,
    db_state: State<'_, GraphDb>,
    sync_state: State<'_, super::sync::SyncEngineState>,
) -> Result<(), String> {
    if text_anchor.is_empty() {
        return Err("text_anchor is required".to_string());
    }
    if new_date.is_empty() && new_time.is_empty() {
        return Err("pass at least one of new_date or new_time".to_string());
    }

    let generation = fs_state.generation();
    let backend = fs_state
        .current()
        .await
        .ok_or_else(|| "No roost is open".to_string())?;
    let rel = to_rel(&note_path, backend.display_root())
        .ok_or_else(|| format!("Access denied: '{note_path}' is outside the active roost"))?;
    let _mutation_guard = fs_state.lock_note(&rel).await;
    if !fs_state.is_generation_current(generation) {
        return Err("Roost changed before the task update could commit".to_string());
    }

    let bytes = backend
        .read(&rel)
        .await
        .map_err(|e| format!("Failed to read {note_path}: {e}"))?;
    let original =
        String::from_utf8(bytes).map_err(|e| format!("File not UTF-8 ({note_path}): {e}"))?;

    // Classify the anchor — body vs frontmatter — by re-parsing tasks.
    // Cheap (single note, in-memory) and avoids depending on the agent
    // tool's `classify_anchor` (which lives in tasks_crud.rs and pulls
    // in ToolError plumbing that doesn't belong on an IPC boundary).
    let parsed = crate::parser::checkboxes::parse_tasks(original.clone());
    let matches: Vec<&crate::parser::checkboxes::ParsedTask> = parsed
        .iter()
        .filter(|t| t.text.starts_with(&text_anchor))
        .collect();
    let target = match matches.as_slice() {
        [] => {
            return Err(format!(
                "No task found starting with \"{text_anchor}\" in {note_path}"
            ))
        }
        [t] => *t,
        many => {
            return Err(format!(
                "{} tasks match anchor \"{text_anchor}\" — anchor is ambiguous",
                many.len()
            ))
        }
    };

    let is_frontmatter = target.source_line < 0;
    let new_content = if is_frontmatter {
        crate::commands::frontmatter_tasks::rewrite_frontmatter_task(
            &original,
            &text_anchor,
            crate::commands::frontmatter_tasks::FrontmatterMutation::Reschedule {
                new_date: new_date.clone(),
                new_time: new_time.clone(),
            },
        )?
    } else {
        rewrite_task_metadata(&original, &text_anchor, &new_date, &new_time)?
    };

    backend
        .write_atomic(&rel, new_content.as_bytes())
        .await
        .map_err(|e| format!("Failed to write {note_path}: {e}"))?;

    // Propagate the reschedule to sync peers (watcher is dead on Android
    // SAF) — see `files::notify_sync_local_write`.
    crate::commands::files::notify_sync_local_write(sync_state.inner(), &rel, &new_content).await;

    // Re-index the per-note task rows so the Tasks pane reflects the
    // new schedule immediately, and rebuild the notification timers so
    // the snooze actually fires at the new time. Both are best-effort:
    // an indexer hiccup must not roll back the user's save.
    let note_id = {
        let guard = db_state.db.lock().map_err(|e| format!("Lock error: {e}"))?;
        let db = guard.as_ref().ok_or("Graph DB not initialized")?;
        let roost_guard = db_state
            .roost_path
            .lock()
            .map_err(|e| format!("Lock error: {e}"))?;
        let roost_root = roost_guard.as_deref().ok_or("Roost path not set")?;
        let note_id = queries::note_id_from_abs_path(&note_path, roost_root);
        if let Err(e) = queries::index_note_tasks(db, &note_id, &new_content) {
            tracing::warn!(%note_id, error = %e, "reschedule_task: re-index failed");
        }
        if let Err(e) = queries::record_edit_event(
            db,
            &note_id,
            &note_path,
            queries::Actor::User,
            "user",
            queries::Intent::Edit,
            &original,
            &new_content,
        ) {
            tracing::warn!(%note_id, error = %e, "edit_history: reschedule record failed");
        }
        note_id
    };
    {
        let guard = db_state.db.lock().map_err(|e| format!("Lock error: {e}"))?;
        if let Some(db_ref) = guard.as_ref() {
            db_state
                .notification_scheduler
                .reschedule_for_note(db_ref, &note_id);
        }
    }
    // NEW-S2: the task's due date changed — drop the cached stats snapshot.
    db_state.invalidate_stats_cache();

    Ok(())
}

/// Strip a leading bullet/checkbox if the user re-typed `- [ ] foo` in
/// the quick-add box. We re-add a fresh prefix unconditionally.
pub(crate) fn clean_quick_add_text(input: &str) -> String {
    let trimmed = input.trim();
    let stripped = trimmed
        .strip_prefix("- [ ] ")
        .or_else(|| trimmed.strip_prefix("- [x] "))
        .or_else(|| trimmed.strip_prefix("- [X] "))
        .or_else(|| trimmed.strip_prefix("- "))
        .or_else(|| trimmed.strip_prefix("* "))
        .or_else(|| trimmed.strip_prefix("+ "))
        .unwrap_or(trimmed);
    stripped.trim().to_string()
}

/// Append a new `- [ ] {text}` line to a note. Returns
/// (new_content, 1-based line number of the inserted line).
pub(crate) fn append_task_to_section(content: &str, text: &str) -> (String, u32) {
    // Locate `## Tasks` heading (case-insensitive on the heading word).
    let lines: Vec<&str> = content.split_inclusive('\n').collect();
    let tasks_heading_idx = lines.iter().position(|l| {
        let t = l.trim_end();
        let t = t.trim_end_matches(|c: char| c.is_whitespace());
        t.eq_ignore_ascii_case("## tasks")
    });

    let new_line = format!("- [ ] {text}\n");

    match tasks_heading_idx {
        Some(h_idx) => {
            // Find the end of the section: next h2 (or higher) or EOF.
            // Insert just before that boundary; back over trailing blank
            // lines so we don't leave a gap.
            let mut end = lines.len();
            for (i, line) in lines.iter().enumerate().skip(h_idx + 1) {
                let t = line.trim_start();
                if t.starts_with("# ") || t.starts_with("## ") || t.starts_with("### ") {
                    end = i;
                    break;
                }
            }
            let mut insert_idx = end;
            while insert_idx > h_idx + 1 {
                let prev = lines[insert_idx - 1].trim_end_matches(['\r', '\n']);
                if prev.is_empty() {
                    insert_idx -= 1;
                } else {
                    break;
                }
            }

            let mut out = String::with_capacity(content.len() + new_line.len());
            for line in &lines[..insert_idx] {
                out.push_str(line);
            }
            // Ensure the inserted line is on its own row even when the
            // preceding content didn't end with `\n` (last line of file).
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            out.push_str(&new_line);
            for line in &lines[insert_idx..] {
                out.push_str(line);
            }

            // Inserted line is at `insert_idx` (0-based). Convert to 1-based.
            (out, (insert_idx as u32) + 1)
        }
        None => {
            // No `## Tasks` heading: append a new section at end of file.
            let mut out = content.to_string();
            // Make sure we have a clean blank line before the new section.
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            if !out.ends_with("\n\n") {
                out.push('\n');
            }
            let header = "## Tasks\n\n";
            out.push_str(header);
            // 1-based line number of the inserted task line.
            let line_no = (out.matches('\n').count() as u32) + 1;
            out.push_str(&new_line);
            (out, line_no)
        }
    }
}

// ── Phase 2.2 Week 2 — agent-CRUD mutation helpers ───────────────────
//
// The agent tools in `ai/tools/tasks_crud.rs` reuse `rewrite_checkbox` /
// `append_task_to_section` for the existing toggle + add paths. Update,
// reschedule, archive, and delete need their own line-rewrite helpers
// because the existing checkbox flipper is purpose-built. Keeping them
// here (rather than in the tool module) so the unit tests below
// exercise the same code that ships in the agent loop.

/// Locate the first checkbox line whose body starts with `anchor`. Used
/// by every agent-CRUD tool to identify the target task. Returns the
/// 0-based line index. `None` when no match is found.
///
/// Anchor matching is case-sensitive; the agent should pass at least
/// 8–12 chars to disambiguate. Multiple matches → the caller decides
/// (the tools today require uniqueness and error otherwise — the agent
/// retries with a longer anchor).
pub(crate) fn find_task_line_index(content: &str, anchor: &str) -> Option<usize> {
    let lines: Vec<&str> = content.split_inclusive('\n').collect();
    // Pass 1: strict prefix match (current behavior). Preferred when the
    // anchor is the first ~10 chars of the task body — the documented use.
    let mut found: Option<usize> = None;
    for (i, line) in lines.iter().enumerate() {
        if line_body_starts_with(line, anchor) {
            if found.is_some() {
                // Two matches → caller can re-scan; we return the FIRST
                // and let `count_task_lines_matching` enforce uniqueness
                // when the tool needs to.
                return found;
            }
            found = Some(i);
        }
    }
    if found.is_some() {
        return found;
    }
    // Pass 2: substring fallback. Agents (and humans) often anchor by a
    // fragment from the middle of the task body — "going to kondagattu"
    // when the task is "need to go to kondagattu". Accept the substring
    // only when it hits EXACTLY ONE task — multiple substring matches
    // stay ambiguous and the caller errors with the existing "no match"
    // message. This makes anchor matching more forgiving without
    // sacrificing uniqueness guarantees.
    if anchor.is_empty() {
        return None;
    }
    let mut sub_found: Option<usize> = None;
    for (i, line) in lines.iter().enumerate() {
        if let Some(body) = checkbox_body(line) {
            if body.trim_start().contains(anchor) {
                if sub_found.is_some() {
                    return None;
                }
                sub_found = Some(i);
            }
        }
    }
    sub_found
}

/// Count checkbox lines whose body starts with `anchor`. Used alongside
/// `find_task_line_index` so the tool can fail fast on ambiguous input.
pub(crate) fn count_task_lines_matching(content: &str, anchor: &str) -> usize {
    // Dual-mode to mirror `find_task_line_index`: prefix matches take
    // precedence; substring matches are the fallback for fragment anchors.
    let prefix_count = content
        .split_inclusive('\n')
        .filter(|l| line_body_starts_with(l, anchor))
        .count();
    if prefix_count > 0 || anchor.is_empty() {
        return prefix_count;
    }
    content
        .split_inclusive('\n')
        .filter_map(checkbox_body)
        .filter(|b| b.trim_start().contains(anchor))
        .count()
}

/// Replace the body text of the task line at `idx`. Preserves indentation,
/// bullet style, checkbox marker (`[ ]` / `[x]`), and trailing newline.
/// `new_text` should NOT include the `- [ ]` prefix.
fn replace_task_body(line: &str, new_text: &str) -> Option<String> {
    let trailing_newline = if line.ends_with("\r\n") {
        "\r\n"
    } else if line.ends_with('\n') {
        "\n"
    } else {
        ""
    };
    let body_only = line.trim_end_matches('\n').trim_end_matches('\r');
    let indent_len = body_only
        .bytes()
        .take_while(|b| *b == b' ' || *b == b'\t')
        .count();
    let (indent, after_indent) = body_only.split_at(indent_len);
    let bullet = after_indent.chars().next()?;
    if bullet != '-' && bullet != '*' && bullet != '+' {
        return None;
    }
    let after_bullet = &after_indent[bullet.len_utf8()..];
    if !after_bullet.starts_with(' ') {
        return None;
    }
    let after_space = &after_bullet[1..];
    let marker = if after_space.starts_with("[ ] ") {
        "[ ]"
    } else if after_space.starts_with("[x] ") || after_space.starts_with("[X] ") {
        "[x]"
    } else {
        return None;
    };
    Some(format!(
        "{indent}{bullet} {marker} {new_text}{trailing_newline}",
    ))
}

/// Update the body text of the task whose body starts with `anchor`.
/// Returns the new file content. Uses the first match.
pub(crate) fn rewrite_task_text(
    content: &str,
    anchor: &str,
    new_text: &str,
) -> Result<String, String> {
    let lines: Vec<&str> = content.split_inclusive('\n').collect();
    let idx = find_task_line_index(content, anchor)
        .ok_or_else(|| format!("No task starting with \"{anchor}\""))?;
    let new_line = replace_task_body(lines[idx], new_text)
        .ok_or_else(|| format!("Line {} is not a recognizable checkbox", idx + 1))?;
    let mut out = String::with_capacity(content.len() + new_text.len());
    for (i, line) in lines.iter().enumerate() {
        if i == idx {
            out.push_str(&new_line);
        } else {
            out.push_str(line);
        }
    }
    Ok(out)
}

/// Update the `📅` and/or `⏰` tokens on the task whose body starts with
/// `anchor`. Empty `new_date` / `new_time` means "leave as-is"; passing
/// the literal string `"clear"` removes that token from the body.
///
/// Tokens are appended to the body if missing, replaced if present, and
/// removed when the input is `"clear"`. Other emoji metadata (`✅`,
/// `#tags`, `[[links]]`) is preserved verbatim.
pub(crate) fn rewrite_task_metadata(
    content: &str,
    anchor: &str,
    new_date: &str,
    new_time: &str,
) -> Result<String, String> {
    let lines: Vec<&str> = content.split_inclusive('\n').collect();
    let idx = find_task_line_index(content, anchor)
        .ok_or_else(|| format!("No task starting with \"{anchor}\""))?;
    let target = lines[idx];

    // Extract body (after `[ ] ` / `[x] `).
    let body = checkbox_body(target)
        .ok_or_else(|| format!("Line {} is not a recognizable checkbox", idx + 1))?;
    let body_owned = body
        .trim_end_matches('\n')
        .trim_end_matches('\r')
        .to_string();

    let mut new_body = body_owned;
    if !new_date.is_empty() {
        new_body = upsert_emoji_token(
            &new_body,
            '📅',
            if new_date == "clear" { "" } else { new_date },
        );
    }
    if !new_time.is_empty() {
        new_body = upsert_emoji_token(
            &new_body,
            '⏰',
            if new_time == "clear" { "" } else { new_time },
        );
    }

    let new_line = replace_task_body(target, &new_body)
        .ok_or_else(|| format!("Line {} is not a recognizable checkbox", idx + 1))?;
    let mut out = String::with_capacity(content.len() + 16);
    for (i, line) in lines.iter().enumerate() {
        if i == idx {
            out.push_str(&new_line);
        } else {
            out.push_str(line);
        }
    }
    Ok(out)
}

/// Insert / replace / delete an emoji-prefixed metadata token in a task
/// body. `value = ""` removes the token (and its emoji); otherwise the
/// token is replaced if present, appended if missing.
fn upsert_emoji_token(body: &str, emoji: char, value: &str) -> String {
    let emoji_str = emoji.to_string();
    let pos = body.find(&emoji_str);
    if let Some(p) = pos {
        let before = body[..p].trim_end();
        let after = &body[p + emoji_str.len()..];
        let after_trimmed = after.trim_start();
        let token_end = after_trimmed
            .find(|c: char| c.is_whitespace())
            .unwrap_or(after_trimmed.len());
        let tail = after_trimmed[token_end..].trim_start();
        let mut out = String::with_capacity(body.len() + 16);
        out.push_str(before);
        if !value.is_empty() {
            if !out.is_empty() {
                out.push(' ');
            }
            out.push(emoji);
            out.push(' ');
            out.push_str(value);
        }
        if !tail.is_empty() {
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str(tail);
        }
        out
    } else if value.is_empty() {
        // Asked to clear a token that wasn't there — no-op.
        body.to_string()
    } else {
        let mut out = body.trim_end().to_string();
        if !out.is_empty() {
            out.push(' ');
        }
        out.push(emoji);
        out.push(' ');
        out.push_str(value);
        out
    }
}

/// Delete the task line whose body starts with `anchor`. Returns the
/// new file content with the line (and its trailing newline) removed.
pub(crate) fn delete_task_line(content: &str, anchor: &str) -> Result<String, String> {
    let lines: Vec<&str> = content.split_inclusive('\n').collect();
    let idx = find_task_line_index(content, anchor)
        .ok_or_else(|| format!("No task starting with \"{anchor}\""))?;
    let mut out = String::with_capacity(content.len());
    for (i, line) in lines.iter().enumerate() {
        if i == idx {
            continue;
        }
        out.push_str(line);
    }
    Ok(out)
}

/// Delete the task at `one_based_line` exactly, but only if its body
/// starts with `anchor`. The anchor is a sanity check: if the file
/// shifted between when the caller observed the line and now, we'd
/// rather error than blow away the wrong line. Used by `delete_task`
/// when the agent has a precise `source_line` from a prior
/// `create_task` / `query_tasks` result — disambiguates between two
/// tasks that share an anchor prefix.
pub(crate) fn delete_task_at_line(
    content: &str,
    one_based_line: usize,
    anchor: &str,
) -> Result<String, String> {
    if one_based_line == 0 {
        return Err("source_line is 1-based; got 0".to_string());
    }
    let lines: Vec<&str> = content.split_inclusive('\n').collect();
    let idx = one_based_line - 1;
    if idx >= lines.len() {
        return Err(format!(
            "source_line {one_based_line} out of range (file has {} lines)",
            lines.len()
        ));
    }
    if !line_body_starts_with(lines[idx], anchor) {
        return Err(format!(
            "Line {one_based_line} does not start with task body \"{anchor}\". \
             File may have been edited since the create. Retry without \
             `source_line` to disambiguate by text_anchor alone."
        ));
    }
    let mut out = String::with_capacity(content.len());
    for (i, line) in lines.iter().enumerate() {
        if i == idx {
            continue;
        }
        out.push_str(line);
    }
    Ok(out)
}

/// Archive the task whose body starts with `anchor`: prefix the body
/// with `~~strikethrough~~` and move the line under a `## Archived`
/// section at end of file (creating the section if missing). Removes
/// the line from its original position to keep the source from listing
/// the same task twice.
pub(crate) fn archive_task(content: &str, anchor: &str) -> Result<String, String> {
    let lines: Vec<&str> = content.split_inclusive('\n').collect();
    let idx = find_task_line_index(content, anchor)
        .ok_or_else(|| format!("No task starting with \"{anchor}\""))?;
    let target = lines[idx];
    let body = checkbox_body(target)
        .ok_or_else(|| format!("Line {} is not a recognizable checkbox", idx + 1))?
        .trim_end_matches('\n')
        .trim_end_matches('\r')
        .to_string();
    let trailing_newline = if target.ends_with("\r\n") {
        "\r\n"
    } else {
        "\n"
    };

    // Indent + bullet style come from the original line so the archived
    // task still parses as a checkbox (just with a strikethrough body).
    let body_only = target.trim_end_matches('\n').trim_end_matches('\r');
    let indent_len = body_only
        .bytes()
        .take_while(|b| *b == b' ' || *b == b'\t')
        .count();
    let indent = &body_only[..indent_len];

    // Build the archived line. Mark complete since archiving implies
    // the task is no longer active.
    let stripped_body = strip_completion_stamp(&body);
    let archived_body = format!("~~{}~~", stripped_body.trim());
    let archived_line = format!("{indent}- [x] {archived_body}{trailing_newline}");

    // Drop the source line, then ensure an `## Archived` section exists
    // and append the archived line under it.
    let mut without_source = String::with_capacity(content.len());
    for (i, line) in lines.iter().enumerate() {
        if i == idx {
            continue;
        }
        without_source.push_str(line);
    }

    let archived_heading_idx = without_source.split_inclusive('\n').position(|l| {
        let t = l.trim_end();
        t.eq_ignore_ascii_case("## archived")
    });

    let result = match archived_heading_idx {
        Some(_) => {
            // Append the archived line at the end of the Archived section
            // (which we treat as "everything after the heading until EOF").
            let mut out = String::with_capacity(without_source.len() + archived_line.len());
            out.push_str(&without_source);
            if !out.ends_with('\n') {
                out.push('\n');
            }
            out.push_str(&archived_line);
            out
        }
        None => {
            let mut out = without_source;
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            if !out.ends_with("\n\n") {
                out.push('\n');
            }
            out.push_str("## Archived\n\n");
            out.push_str(&archived_line);
            out
        }
    };

    Ok(result)
}

/// Slice-1 demo command: returns every task row as a debug-formatted string.
/// Frontend calls this from the dev console to verify the indexer ran on a
/// real roost. Will be removed once the Tasks pane (slice 2) is in.
#[tauri::command]
#[specta::specta]
pub fn debug_dump_tasks(db_state: State<'_, GraphDb>) -> Result<Vec<String>, String> {
    let rows = list_tasks(db_state)?;
    Ok(rows
        .into_iter()
        .map(|r| {
            format!(
                "[{}] {} | due={} | line={} | note={} | tags(person={},project={})",
                if r.completed { "x" } else { " " },
                r.text,
                if r.due_at.is_empty() {
                    "—"
                } else {
                    &r.due_at
                },
                r.source_line,
                r.note_title,
                if r.linked_person.is_empty() {
                    "—"
                } else {
                    &r.linked_person
                },
                if r.linked_project.is_empty() {
                    "—"
                } else {
                    &r.linked_project
                },
            )
        })
        .collect())
}
