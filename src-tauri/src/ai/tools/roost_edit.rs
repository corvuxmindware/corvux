//! `roost_edit` — edit an existing note's content. The companion to
//! `roost_write` (which only creates new notes). Together they give the
//! agent full CRUD over the roost.
//!
//! The tool reads the current content, replaces it with `new_content`,
//! records an `:edit_event` with `Actor::Agent`, and re-indexes the note
//! so backlinks/search/graph stay consistent.
//!
//! Safety: classified `Write`. The edit-history trust gate (Phase 2.2)
//! means every edit is visible and revertable from the Edit History pane.

use async_trait::async_trait;
use serde_json::{json, Value};
use tauri::Manager;

use crate::commands::graph::GraphDb;
use crate::db::queries;

use super::note_ref;
use super::registry::{SafetyTier, Tool, ToolContext, ToolError};

pub struct RoostEditTool;

#[async_trait]
impl Tool for RoostEditTool {
    fn name(&self) -> &'static str {
        "edit_note"
    }

    fn description(&self) -> &'static str {
        "Edit an existing note's content. Args: `note_path` (required — absolute \
         or roost-relative path, or 'daily' for today's daily note), \
         `new_content` (required — the FULL replacement content in markdown). \
         The old content is recorded in the Edit History pane so the user can \
         review and revert. Use `roost_read` first to see the current content, \
         then provide the modified version here. Any frontmatter `tags` in \
         `new_content` must already exist in the roost tag inventory (rule 8) \
         — the edit is rejected otherwise."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "note_path": {
                    "type": "string",
                    "description": "Roost-relative path, absolute path, or 'daily' for today's daily note."
                },
                "new_content": {
                    "type": "string",
                    "description": "Full replacement markdown content for the note."
                }
            },
            "required": ["note_path", "new_content"]
        })
    }

    fn safety_tier(&self) -> SafetyTier {
        SafetyTier::Write
    }

    fn timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(15)
    }

    async fn execute(&self, ctx: &ToolContext, args: Value) -> Result<Value, ToolError> {
        let roost = ctx.require_roost(self.name())?.to_string();

        let note_path = args
            .get("note_path")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::fatal("missing 'note_path'"))?;

        let new_content = args
            .get("new_content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::fatal("missing 'new_content'"))?;

        let rel = note_ref::resolve_rel(note_path, ctx, false, false).await?;
        let display_path = note_ref::display_join(&roost, &rel);

        // Tag-vocabulary guard — mirrors `roost_write`. `edit_note` replaces
        // the whole file, so without this an agent could inject novel
        // frontmatter tags the create-path guard would have rejected,
        // fragmenting the graph (#fitness vs #workout vs #exercise). Empty
        // inventory (fresh roost) is the bootstrap exception.
        if let Some(graph) = ctx.app.try_state::<GraphDb>() {
            if let Ok(db_guard) = graph.db.lock() {
                if let Some(db) = db_guard.as_ref() {
                    if let Ok(known) = queries::all_tag_names(db) {
                        let known: std::collections::HashSet<String> = known.into_iter().collect();
                        let novel = novel_frontmatter_tags(new_content, &known);
                        if !novel.is_empty() {
                            let mut examples: Vec<String> =
                                known.iter().take(6).map(|s| format!("#{s}")).collect();
                            examples.sort();
                            return Err(ToolError::retryable(format!(
                                "tag(s) not in roost inventory: {}. Pick from existing tags (e.g. {}) or drop them from the frontmatter. Do not invent new tags — see rule 8 in the system prompt.",
                                novel
                                    .iter()
                                    .map(|t| format!("`#{t}`"))
                                    .collect::<Vec<_>>()
                                    .join(", "),
                                examples.join(", ")
                            )));
                        }
                    }
                }
            }
        }

        // Reserved-path guard (parity with roost_write + the MCP): the agent
        // must not edit Corvux's own internal files even though the note
        // "exists" — .corvux/ sync identity + settings, dotfiles, etc.
        if super::roost_write::is_reserved_path(&rel) {
            return Err(ToolError::fatal(format!(
                "refusing to edit a reserved path '{rel}'. Corvux's own folders (.corvux, .attachments) and dotfiles are off-limits to the agent."
            )));
        }

        let (before, _, ()) =
            crate::commands::files::mutate_roost_note_synced(&ctx.app, &rel, |_| {
                Ok((new_content.to_string(), ()))
            })
            .await
            .map_err(ToolError::retryable)?;
        if before == new_content {
            return Ok(json!({
                "summary": "no changes — new content matches existing content",
                "note_path": display_path,
            }));
        }

        let lines_added = new_content.lines().count() as i64;
        let lines_removed = before.lines().count() as i64;
        let delta = lines_added - lines_removed;

        record_agent_write(
            &ctx.app,
            &display_path,
            queries::Intent::AgentWrite,
            "agent",
            &before,
            new_content,
        );

        let file_name = rel.rsplit('/').next().unwrap_or(&rel);

        let mut result = json!({
            "summary": format!(
                "edited {} ({}{})",
                file_name,
                if delta >= 0 { "+" } else { "" },
                delta
            ),
            "note_path": display_path,
            "file_path": display_path,
            "lines_added": lines_added,
            "lines_removed": lines_removed,
        });
        // Non-blocking link diagnostics (shared with roost_write): flag any
        // [[links]] in the new content that are dangling or ambiguous.
        let rel_dir = rel.rsplit_once('/').map(|(dir, _)| dir).unwrap_or("");
        if let Some(warnings) = super::roost_write::link_warnings_for(ctx, new_content, rel_dir) {
            result["link_warnings"] = warnings;
        }
        Ok(result)
    }
}

/// Frontmatter tags in `content` that aren't already in `known` (the roost
/// tag inventory). Empty `known` → none, so a fresh roost's first edit can
/// define the vocabulary (matches `roost_write`'s bootstrap rule). This is
/// the pure, testable core of the edit-path tag guard.
fn novel_frontmatter_tags(content: &str, known: &std::collections::HashSet<String>) -> Vec<String> {
    if known.is_empty() {
        return Vec::new();
    }
    crate::parser::tags::frontmatter_tag_names(content)
        .into_iter()
        .filter(|t| !known.contains(t))
        .collect()
}

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

    // IP-8: full index (pass1 + pass2 + dangling promotion), not pass1 alone —
    // an AI edit that writes `[[links]]` must produce real edges, not invisible
    // links that only appear after the next editor save. IP-A3: reuse the
    // cached resolver (add_id-maintained) instead of an O(roost) rebuild.
    if let Err(e) = graph.index_note_full_cached(db, &roost_root, abs_path, after) {
        tracing::warn!(%note_id, error = %e, "roost_edit: re-index failed");
    }
    // index_note_full_cached kept the cached resolver in sync (add_id'd any new
    // note id), so do NOT invalidate it — that would force the next editor save
    // to rebuild O(roost) (IP-A3). Just mark the algo cache stale (links changed).
    graph.mark_algo_cache_dirty();
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
        tracing::warn!(%note_id, error = %e, "roost_edit: edit_event record failed");
    }
}

// Resolver helpers and tests moved to `super::note_ref`. The path-rejection
// behaviors previously asserted here (`..`, empty input) are covered there
// alongside the title-lookup and UNC-prefix tests.

#[cfg(test)]
mod tag_guard_tests {
    use super::novel_frontmatter_tags;
    use std::collections::HashSet;

    fn known(tags: &[&str]) -> HashSet<String> {
        tags.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn flags_only_tags_outside_the_inventory() {
        let inv = known(&["project/corvux", "sync"]);
        let content = "---\ntags:\n  - sync\n  - brand-new\n---\n\n# X\n\nbody\n";
        assert_eq!(novel_frontmatter_tags(content, &inv), vec!["brand-new"]);
    }

    #[test]
    fn all_known_tags_pass() {
        let inv = known(&["sync", "project/corvux"]);
        let content = "---\ntags: [sync, project/corvux]\n---\n\n# X\n";
        assert!(novel_frontmatter_tags(content, &inv).is_empty());
    }

    #[test]
    fn empty_inventory_bootstraps_anything() {
        let inv = HashSet::new();
        let content = "---\ntags:\n  - whatever\n---\n";
        assert!(novel_frontmatter_tags(content, &inv).is_empty());
    }

    #[test]
    fn no_frontmatter_has_no_novel_tags() {
        let inv = known(&["sync"]);
        assert!(novel_frontmatter_tags("# just a body\n\nno frontmatter", &inv).is_empty());
    }

    #[test]
    fn inline_body_tags_are_not_governed_here() {
        // Mirrors roost_write: only structured frontmatter tags are guarded,
        // not prose #hashes — so a body #newidea does not trip the guard.
        let inv = known(&["sync"]);
        let content = "---\ntags:\n  - sync\n---\n\n# X\n\nan inline #newidea here\n";
        assert!(novel_frontmatter_tags(content, &inv).is_empty());
    }
}
