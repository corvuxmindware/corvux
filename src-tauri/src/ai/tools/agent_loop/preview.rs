//! Approval-preview computation for write/destructive tools.
//!
//! When the agent loop reaches an approval gate, it builds a before/after
//! diff for tools whose side effects are a single-file mutation. The user
//! sees the exact lines that will change in the pending tool card and can
//! reject the call before any disk write happens.
//!
//! Roost scoping is enforced *by construction* — paths are resolved via
//! `note_ref::resolve` (the same helper real tool execution uses) so the
//! preview can never read outside the active roost (S-04 in
//! `docs/audits/engineering-code-security-audit-2026-05-24.md`).
//!
//! Was split out of `agent_loop/mod.rs` per Q-03 — keeps the preview's
//! filesystem-touching surface in one auditable file.
//!
//! Reads are synchronous. The agent is already paused waiting for the
//! approval gate, so the blocking I/O cost is invisible to UX. Errors
//! silently degrade to a missing preview rather than blocking approval —
//! shipping a partial preview beats blocking the user on a corner case.

use super::super::approval::{DiffKind, DiffPreview};
use super::super::registry::ToolContext;

pub(super) async fn compute_preview_async<R: tauri::Runtime + 'static>(
    tool: &str,
    args: &serde_json::Value,
    ctx: &ToolContext<R>,
) -> Option<DiffPreview> {
    use crate::db::queries::compute_diff;

    match tool {
        "edit_note" => {
            let note_path = args.get("note_path").and_then(|v| v.as_str())?;
            let new_content = args.get("new_content").and_then(|v| v.as_str())?;
            let (path, before) = read_preview_note(note_path, ctx, true).await?;
            let (unified_diff, lines_added, lines_removed) = compute_diff(&before, new_content);
            Some(DiffPreview {
                kind: DiffKind::Edit,
                path,
                unified_diff,
                lines_added,
                lines_removed,
            })
        }
        "roost_write" => {
            let title = args.get("title").and_then(|v| v.as_str()).unwrap_or("");
            let body = args.get("body").and_then(|v| v.as_str()).unwrap_or("");
            let folder = args.get("folder").and_then(|v| v.as_str()).unwrap_or("");
            let path = if folder.is_empty() {
                format!("{title}.md")
            } else {
                format!("{}/{title}.md", folder.trim_matches('/'))
            };
            let (unified_diff, lines_added, lines_removed) = compute_diff("", body);
            Some(DiffPreview {
                kind: DiffKind::Create,
                path,
                unified_diff,
                lines_added,
                lines_removed,
            })
        }
        "delete_task" => {
            let note_path = args.get("note_path").and_then(|v| v.as_str())?;
            let anchor = args.get("text_anchor").and_then(|v| v.as_str())?;
            let (path, content) = read_preview_note(note_path, ctx, false).await?;
            let idx = crate::commands::tasks::find_task_line_index(&content, anchor)?;
            let lines: Vec<&str> = content.split_inclusive('\n').collect();
            let removed = lines.get(idx)?.trim_end_matches('\n');
            Some(DiffPreview {
                kind: DiffKind::Edit,
                path,
                unified_diff: format!(
                    "--- before\n+++ after\n@@ line {} @@\n-{}\n",
                    idx + 1,
                    removed
                ),
                lines_added: 0,
                lines_removed: 1,
            })
        }
        "update_task" | "reschedule_task" | "archive_task" => {
            let note_path = args.get("note_path").and_then(|v| v.as_str())?;
            let anchor = args.get("text_anchor").and_then(|v| v.as_str())?;
            let (path, before) = read_preview_note(note_path, ctx, false).await?;
            let after = match tool {
                "update_task" => crate::commands::tasks::rewrite_task_text(
                    &before,
                    anchor,
                    args.get("new_text").and_then(|v| v.as_str())?,
                )
                .ok()?,
                "reschedule_task" => crate::commands::tasks::rewrite_task_metadata(
                    &before,
                    anchor,
                    args.get("new_date").and_then(|v| v.as_str()).unwrap_or(""),
                    args.get("new_time").and_then(|v| v.as_str()).unwrap_or(""),
                )
                .ok()?,
                "archive_task" => crate::commands::tasks::archive_task(&before, anchor).ok()?,
                _ => unreachable!(),
            };
            let (unified_diff, lines_added, lines_removed) = compute_diff(&before, &after);
            Some(DiffPreview {
                kind: DiffKind::Edit,
                path,
                unified_diff,
                lines_added,
                lines_removed,
            })
        }
        _ => None,
    }
}

async fn read_preview_note<R: tauri::Runtime + 'static>(
    note_path: &str,
    ctx: &ToolContext<R>,
    missing_as_empty: bool,
) -> Option<(String, String)> {
    let rel = super::super::note_ref::resolve_rel(note_path, ctx, false, false)
        .await
        .ok()?;
    let root = ctx.require_roost("approval preview").ok()?;
    let display = super::super::note_ref::display_join(root, &rel);
    match super::super::note_ref::read_rel(ctx, &rel).await {
        Ok(content) => Some((display, content)),
        Err(_) if missing_as_empty => Some((display, String::new())),
        Err(_) => None,
    }
}

/// Build a before/after diff payload for write tools so the user can
/// see what will change *before* clicking Approve. Returns `None` for
/// tools without a single before/after target (task ops, sync, etc.) —
/// those keep their existing arg-only display.
#[cfg(test)]
pub(super) fn compute_preview(
    tool: &str,
    args: &serde_json::Value,
    roost: Option<&str>,
) -> Option<DiffPreview> {
    use crate::db::queries::compute_diff;

    match tool {
        "edit_note" => {
            let note_path = args.get("note_path").and_then(|v| v.as_str())?;
            let new_content = args.get("new_content").and_then(|v| v.as_str())?;
            let abs_path = resolve_abs_for_preview(note_path, roost)?;
            let before = std::fs::read_to_string(&abs_path).unwrap_or_default();
            let (unified_diff, lines_added, lines_removed) = compute_diff(&before, new_content);
            Some(DiffPreview {
                kind: DiffKind::Edit,
                path: abs_path,
                unified_diff,
                lines_added,
                lines_removed,
            })
        }
        "roost_write" => {
            let title = args.get("title").and_then(|v| v.as_str()).unwrap_or("");
            let body = args.get("body").and_then(|v| v.as_str()).unwrap_or("");
            let folder = args.get("folder").and_then(|v| v.as_str()).unwrap_or("");
            let display_path = if folder.is_empty() {
                format!("{title}.md")
            } else {
                format!("{}/{title}.md", folder.trim_matches('/'))
            };
            let (unified_diff, lines_added, lines_removed) = compute_diff("", body);
            Some(DiffPreview {
                kind: DiffKind::Create,
                path: display_path,
                unified_diff,
                lines_added,
                lines_removed,
            })
        }
        "delete_task" => {
            // Highest-stakes Write/Destructive task tool — show the
            // exact line being deleted so the user can spot a wrong-
            // anchor mistake before approving. Anchor matching mirrors
            // `tasks::find_task_line_index` (case-sensitive prefix on
            // the task body, after the `- [ ]` marker).
            let note_path = args.get("note_path").and_then(|v| v.as_str())?;
            let anchor = args.get("text_anchor").and_then(|v| v.as_str())?;
            let abs_path = resolve_abs_for_preview(note_path, roost)?;
            let content = std::fs::read_to_string(&abs_path).ok()?;
            let idx = crate::commands::tasks::find_task_line_index(&content, anchor)?;
            // Synthesize a minimal before/after where the matched line
            // is the only deletion. Trim the trailing newline the
            // line-split preserved so the diff doesn't render a
            // dangling blank.
            let lines: Vec<&str> = content.split_inclusive('\n').collect();
            let removed = lines.get(idx)?.trim_end_matches('\n').to_string();
            let unified_diff = format!(
                "--- before\n+++ after\n@@ line {} @@\n-{}\n",
                idx + 1,
                removed
            );
            Some(DiffPreview {
                kind: DiffKind::Edit,
                path: abs_path,
                unified_diff,
                lines_added: 0,
                lines_removed: 1,
            })
        }
        "update_task" => {
            // The text replacement is computed locally via the same
            // helper the tool itself uses at execute time — guarantees
            // the previewed `+` line is what gets written if approved.
            let note_path = args.get("note_path").and_then(|v| v.as_str())?;
            let anchor = args.get("text_anchor").and_then(|v| v.as_str())?;
            let new_text = args.get("new_text").and_then(|v| v.as_str())?;
            let abs_path = resolve_abs_for_preview(note_path, roost)?;
            let before = std::fs::read_to_string(&abs_path).ok()?;
            let after =
                crate::commands::tasks::rewrite_task_text(&before, anchor, new_text).ok()?;
            let (unified_diff, lines_added, lines_removed) = compute_diff(&before, &after);
            Some(DiffPreview {
                kind: DiffKind::Edit,
                path: abs_path,
                unified_diff,
                lines_added,
                lines_removed,
            })
        }
        "reschedule_task" => {
            // Emoji-metadata mutation. Args mirror the tool's: optional
            // `new_date` / `new_time` either set or omit the 📅/⏰
            // token; the literal "clear" removes the token. Default to
            // empty string ("leave as-is") for whichever field is
            // absent, matching `rewrite_task_metadata`'s contract.
            let note_path = args.get("note_path").and_then(|v| v.as_str())?;
            let anchor = args.get("text_anchor").and_then(|v| v.as_str())?;
            let new_date = args.get("new_date").and_then(|v| v.as_str()).unwrap_or("");
            let new_time = args.get("new_time").and_then(|v| v.as_str()).unwrap_or("");
            let abs_path = resolve_abs_for_preview(note_path, roost)?;
            let before = std::fs::read_to_string(&abs_path).ok()?;
            let after =
                crate::commands::tasks::rewrite_task_metadata(&before, anchor, new_date, new_time)
                    .ok()?;
            let (unified_diff, lines_added, lines_removed) = compute_diff(&before, &after);
            Some(DiffPreview {
                kind: DiffKind::Edit,
                path: abs_path,
                unified_diff,
                lines_added,
                lines_removed,
            })
        }
        "archive_task" => {
            // Archive moves the line to an `## Archived` section AND
            // strikes through the text. The diff captures both
            // mutations at once — the user sees the line vanish from
            // its original spot and reappear under Archived.
            let note_path = args.get("note_path").and_then(|v| v.as_str())?;
            let anchor = args.get("text_anchor").and_then(|v| v.as_str())?;
            let abs_path = resolve_abs_for_preview(note_path, roost)?;
            let before = std::fs::read_to_string(&abs_path).ok()?;
            let after = crate::commands::tasks::archive_task(&before, anchor).ok()?;
            let (unified_diff, lines_added, lines_removed) = compute_diff(&before, &after);
            Some(DiffPreview {
                kind: DiffKind::Edit,
                path: abs_path,
                unified_diff,
                lines_added,
                lines_removed,
            })
        }
        _ => None,
    }
}

/// Resolve `note_path` to an absolute path **inside the active roost**
/// for the diff preview read. Returns `None` for paths that escape the
/// roost or contain `..` traversal — the preview UI then sees a
/// missing preview and the approval card falls back to its arg-only
/// display. The actual tool execution will also refuse the path, so
/// nothing is lost by being strict here.
///
/// S-04: the previous implementation returned absolute paths unchanged,
/// which let a prompt-injected `edit_note` with an absolute outside
/// the roost path cause `std::fs::read_to_string` to read e.g.
/// `/etc/hosts` for the diff. The bytes never reached the LLM, but
/// they did reach the renderer in the pending tool event. Routing
/// through `note_ref::resolve` enforces the same roost boundary the
/// real execute path enforces — so preview and execution agree by
/// construction.
#[cfg(test)]
fn resolve_abs_for_preview(note_path: &str, roost: Option<&str>) -> Option<String> {
    let roost = roost?;
    // Build a minimal `ResolveOpts` — no AppHandle (skips the DB
    // title-lookup branch we don't need for preview), no
    // daily-shorthand creation (preview never side-effects), no
    // open-note fallback (preview always has an explicit path arg).
    let opts = super::super::note_ref::ResolveOpts {
        roost,
        app: None,
        open_note_path: None,
        allow_create_daily: false,
        allow_open_fallback: false,
    };
    super::super::note_ref::resolve(note_path, opts)
        .ok()
        .map(|p| p.to_string_lossy().to_string())
}

#[cfg(test)]
mod tests {
    use super::compute_preview;
    use serde_json::json;

    #[test]
    fn roost_write_preview_renders_full_body_as_added() {
        let args = json!({
            "title": "Plan",
            "body": "first line\nsecond line\n",
            "folder": "projects",
        });
        let p = compute_preview("roost_write", &args, Some("/roost")).expect("preview");
        assert_eq!(p.path, "projects/Plan.md");
        assert_eq!(p.lines_removed, 0);
        assert!(p.lines_added >= 2, "added lines = {}", p.lines_added);
        assert!(p.unified_diff.contains("+first line"));
    }

    #[test]
    fn edit_note_preview_with_missing_file_treats_before_as_empty() {
        let args = json!({
            "note_path": "does-not-exist.md",
            "new_content": "hello\n",
        });
        let tmp = tempfile::tempdir().unwrap();
        let p = compute_preview("edit_note", &args, Some(tmp.path().to_str().unwrap()))
            .expect("preview");
        assert_eq!(p.lines_removed, 0);
        assert!(p.lines_added >= 1);
    }

    #[test]
    fn edit_note_preview_diffs_real_content() {
        let tmp = tempfile::tempdir().unwrap();
        let note = tmp.path().join("a.md");
        std::fs::write(&note, "old\n").unwrap();
        let args = json!({
            "note_path": "a.md",
            "new_content": "new\n",
        });
        let p = compute_preview("edit_note", &args, Some(tmp.path().to_str().unwrap()))
            .expect("preview");
        assert!(p.unified_diff.contains("-old"));
        assert!(p.unified_diff.contains("+new"));
        assert_eq!(p.lines_added, 1);
        assert_eq!(p.lines_removed, 1);
    }

    #[test]
    fn read_tools_have_no_preview() {
        let args = json!({ "paths": ["a.md"] });
        assert!(compute_preview("roost_read", &args, Some("/roost")).is_none());
    }

    /// S-04: an absolute path outside the active roost must NOT cause
    /// the preview to read from disk. The previous implementation
    /// returned the absolute path unchanged and let `read_to_string`
    /// happily slurp e.g. /etc/hosts into the pending-tool event.
    #[test]
    fn preview_refuses_absolute_path_outside_roost() {
        let roost = tempfile::tempdir().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(outside.path(), "OUTSIDE_ROOST_SENTINEL\n").unwrap();
        let outside_abs = outside.path().to_string_lossy().to_string();

        for tool in [
            "edit_note",
            "delete_task",
            "update_task",
            "reschedule_task",
            "archive_task",
        ] {
            let args = json!({
                "note_path": outside_abs,
                "new_content": "x",
                "text_anchor": "anything",
                "new_text": "x",
                "new_date": "2026-05-09",
            });
            let p = compute_preview(tool, &args, Some(roost.path().to_str().unwrap()));
            assert!(
                p.is_none(),
                "{tool} returned a preview for an outside-roost absolute path; expected None"
            );
            // Belt + suspenders: even if a preview were built somehow,
            // the sentinel must not appear in its serialized form.
            if let Some(prev) = p {
                let serialized = format!("{prev:?}");
                assert!(
                    !serialized.contains("OUTSIDE_ROOST_SENTINEL"),
                    "{tool} preview leaked outside content: {serialized}"
                );
            }
        }
    }

    /// S-04: a `..` traversal in `note_path` must be refused at the
    /// preview layer, same as it is at execute time.
    #[test]
    fn preview_refuses_dot_dot_traversal() {
        let roost = tempfile::tempdir().unwrap();
        let args = json!({
            "note_path": "../../etc/passwd",
            "new_content": "x",
        });
        let p = compute_preview("edit_note", &args, Some(roost.path().to_str().unwrap()));
        assert!(p.is_none(), "edit_note must refuse '..' traversal");
    }

    #[test]
    fn delete_task_preview_shows_matched_line_as_removal() {
        let tmp = tempfile::tempdir().unwrap();
        let note = tmp.path().join("daily.md");
        std::fs::write(
            &note,
            "# Today\n\n- [ ] go to gym\n- [ ] buy milk\n- [ ] call dad\n",
        )
        .unwrap();
        let args = json!({
            "note_path": "daily.md",
            "text_anchor": "buy milk",
        });
        let p = compute_preview("delete_task", &args, Some(tmp.path().to_str().unwrap()))
            .expect("preview");
        assert_eq!(p.lines_added, 0);
        assert_eq!(p.lines_removed, 1);
        assert!(
            p.unified_diff.contains("-- [ ] buy milk"),
            "diff should mark the matched task line as removed; got:\n{}",
            p.unified_diff
        );
    }

    #[test]
    fn delete_task_preview_returns_none_when_anchor_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let note = tmp.path().join("daily.md");
        std::fs::write(&note, "- [ ] only one task\n").unwrap();
        let args = json!({
            "note_path": "daily.md",
            "text_anchor": "no such task",
        });
        let p = compute_preview("delete_task", &args, Some(tmp.path().to_str().unwrap()));
        // No match → no preview. The tool itself will error at execution
        // time; the preview just degrades gracefully.
        assert!(p.is_none());
    }

    #[test]
    fn delete_task_preview_returns_none_when_file_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let args = json!({
            "note_path": "does-not-exist.md",
            "text_anchor": "anything",
        });
        let p = compute_preview("delete_task", &args, Some(tmp.path().to_str().unwrap()));
        assert!(p.is_none());
    }

    #[test]
    fn update_task_preview_shows_text_swap() {
        let tmp = tempfile::tempdir().unwrap();
        let note = tmp.path().join("daily.md");
        std::fs::write(&note, "- [ ] write the doc 📅 2026-05-09\n").unwrap();
        let args = json!({
            "note_path": "daily.md",
            "text_anchor": "write the doc",
            "new_text": "publish the doc",
        });
        let p = compute_preview("update_task", &args, Some(tmp.path().to_str().unwrap()))
            .expect("preview");
        assert!(p
            .unified_diff
            .contains("-- [ ] write the doc 📅 2026-05-09"));
        assert!(p.unified_diff.contains("+- [ ] publish the doc"));
        assert_eq!(p.lines_added, 1);
        assert_eq!(p.lines_removed, 1);
    }

    #[test]
    fn reschedule_task_preview_shows_date_change() {
        let tmp = tempfile::tempdir().unwrap();
        let note = tmp.path().join("daily.md");
        std::fs::write(&note, "- [ ] standup 📅 2026-05-09\n").unwrap();
        let args = json!({
            "note_path": "daily.md",
            "text_anchor": "standup",
            "new_date": "2026-05-12",
        });
        let p = compute_preview("reschedule_task", &args, Some(tmp.path().to_str().unwrap()))
            .expect("preview");
        assert!(p.unified_diff.contains("2026-05-12"));
        // Old date should appear in the `-` line.
        assert!(p.unified_diff.contains("-- [ ] standup 📅 2026-05-09"));
    }

    #[test]
    fn reschedule_task_preview_clear_removes_date_token() {
        let tmp = tempfile::tempdir().unwrap();
        let note = tmp.path().join("daily.md");
        std::fs::write(&note, "- [ ] standup 📅 2026-05-09\n").unwrap();
        let args = json!({
            "note_path": "daily.md",
            "text_anchor": "standup",
            "new_date": "clear",
        });
        let p = compute_preview("reschedule_task", &args, Some(tmp.path().to_str().unwrap()))
            .expect("preview");
        // The `+` line should drop the 📅 token entirely.
        assert!(
            !p.unified_diff
                .lines()
                .any(|l| l.starts_with('+') && l.contains("📅")),
            "after-line should not contain 📅; diff:\n{}",
            p.unified_diff
        );
    }

    #[test]
    fn archive_task_preview_shows_strikethrough_and_section_move() {
        let tmp = tempfile::tempdir().unwrap();
        let note = tmp.path().join("daily.md");
        std::fs::write(&note, "- [ ] old chore\n- [ ] today\n").unwrap();
        let args = json!({
            "note_path": "daily.md",
            "text_anchor": "old chore",
        });
        let p = compute_preview("archive_task", &args, Some(tmp.path().to_str().unwrap()))
            .expect("preview");
        // Original line removed from its spot.
        assert!(p.unified_diff.contains("-- [ ] old chore"));
        // Archive marker added — either the `## Archived` header or the
        // strikethrough form of the task (depending on the helper's
        // exact output, both are acceptable signals of "archived").
        let added: String = p
            .unified_diff
            .lines()
            .filter(|l| l.starts_with('+'))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            added.contains("Archived") || added.contains("~~old chore"),
            "expected archive marker in added lines; got:\n{added}"
        );
    }

    #[test]
    fn task_previews_return_none_when_anchor_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let note = tmp.path().join("daily.md");
        std::fs::write(&note, "- [ ] only one\n").unwrap();
        for tool in ["update_task", "reschedule_task", "archive_task"] {
            let args = json!({
                "note_path": "daily.md",
                "text_anchor": "ghost",
                "new_text": "x",
                "new_date": "2026-05-09",
            });
            let p = compute_preview(tool, &args, Some(tmp.path().to_str().unwrap()));
            assert!(
                p.is_none(),
                "{tool} should degrade to no-preview on missing anchor"
            );
        }
    }
}
