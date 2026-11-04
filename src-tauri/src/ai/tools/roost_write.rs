//! `roost_write` — create or append to a markdown note. Refactor of the
//! legacy `create_note` tool, now backed by the [`Tool`] trait.
//!
//! Safety: classified `Write`. Today the agent loop runs writes without
//! prompting (matches the legacy single-shot UX). When the tool-card
//! approval flow ships, this tool will block on user accept before the
//! filesystem write fires.

use async_trait::async_trait;
use serde_json::{json, Value};
use tauri::Manager;

use crate::commands::graph::GraphDb;
use crate::db::queries;

use super::note_ref;
use super::registry::{SafetyTier, Tool, ToolContext, ToolError};

pub struct RoostWriteTool;

#[async_trait]
impl Tool for RoostWriteTool {
    fn name(&self) -> &'static str {
        // Keep the legacy name as an alias so older system prompts and
        // saved chats keep working — `roost_write` is the new canonical
        // name; we register both via `register_create_note_alias` below.
        "roost_write"
    }

    fn description(&self) -> &'static str {
        "Create a new markdown note in the roost. Args: `title` (required), \
         `body` (required, markdown — escape newlines as \\n), optional \
         `folder` (roost-relative), optional `tags` (≤3 strings — MUST be \
         from the Roost tag inventory in the system prompt; novel tags are \
         rejected to keep the knowledge graph from fragmenting). Use \
         [[wiki-links]] inside the body to connect to other notes. The \
         note is saved automatically; do not repeat the body in your reply."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "title": { "type": "string" },
                "body":  { "type": "string" },
                "folder": { "type": "string" },
                "tags": {
                    "type": "array",
                    "items": { "type": "string" },
                    "maxItems": 3,
                    "description": "Existing roost tags only (see system prompt). Max 3. Omit entirely if no inventory tag fits."
                }
            },
            "required": ["title", "body"]
        })
    }

    fn safety_tier(&self) -> SafetyTier {
        SafetyTier::Write
    }

    fn timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(10)
    }

    async fn execute(&self, ctx: &ToolContext, args: Value) -> Result<Value, ToolError> {
        let roost = ctx.require_roost(self.name())?.to_string();

        let title = args
            .get("title")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::retryable("missing 'title'"))?
            .to_string();

        let body = args
            .get("body")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let folder = args
            .get("folder")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().trim_matches('/').to_string())
            .filter(|s| !s.is_empty());

        let tags: Vec<String> = args
            .get("tags")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();

        // Tag-vocabulary guard. The agent can ONLY use tags that already
        // exist in the roost — picking from the inventory injected into
        // the system prompt. Inventing new tags fragments the knowledge
        // graph (synonym pollution: #fitness vs #workout vs #exercise;
        // singleton tags: #weekly-fitness-summary-puli-w20). Empty
        // inventory (fresh roost) is the one bootstrap case where we
        // accept whatever the agent asked for — the first note's tags
        // get to define the vocabulary.
        if !tags.is_empty() {
            if let Some(graph) = ctx.app.try_state::<GraphDb>() {
                if let Ok(db_guard) = graph.db.lock() {
                    if let Some(db) = db_guard.as_ref() {
                        match queries::all_tag_names(db) {
                            Ok(known) if !known.is_empty() => {
                                let unknown: Vec<&str> = tags
                                    .iter()
                                    .filter(|t| !known.contains(t.as_str()))
                                    .map(String::as_str)
                                    .collect();
                                if !unknown.is_empty() {
                                    // Pull a few example existing tags so
                                    // the LLM has a concrete "use these
                                    // instead" reference in its retry.
                                    let mut examples: Vec<String> =
                                        known.iter().take(6).map(|s| format!("#{s}")).collect();
                                    examples.sort();
                                    return Err(ToolError::retryable(format!(
                                        "tag(s) not in roost inventory: {}. Either pick from existing tags (e.g. {}) or omit `tags` and the user can add one manually. Do not invent new tags — see rule 8 in the system prompt.",
                                        unknown
                                            .iter()
                                            .map(|t| format!("`#{t}`"))
                                            .collect::<Vec<_>>()
                                            .join(", "),
                                        examples.join(", ")
                                    )));
                                }
                            }
                            _ => { /* empty inventory → bootstrap allowed */ }
                        }
                    }
                }
            }
        }

        let safe_title: String = title
            .chars()
            .map(|c| {
                if matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') {
                    '-'
                } else {
                    c
                }
            })
            .collect();

        let folder = folder.as_deref().unwrap_or("");
        if folder.contains("..") {
            return Err(ToolError::fatal("folder cannot contain '..'"));
        }
        let rel = if folder.is_empty() {
            format!("{safe_title}.md")
        } else {
            format!("{}/{safe_title}.md", folder.trim_matches('/'))
        };
        let mut content = String::new();
        let now = chrono_date_iso();
        content.push_str("---\n");
        content.push_str(&format!("title: {title}\n"));
        content.push_str(&format!("created: {now}\n"));
        content.push_str("generated_by: corvux-ai\n");
        if !tags.is_empty() {
            content.push_str("tags:\n");
            for t in &tags {
                content.push_str(&format!("  - {t}\n"));
            }
        }
        content.push_str("---\n\n");
        content.push_str(&format!("# {title}\n\n"));
        content.push_str(&body);
        if !body.ends_with('\n') {
            content.push('\n');
        }

        // AUDIT-033 / PR-C: route through the single write+notify funnel so the
        // new note is announced to sync (and refreshes the tree) even on
        // Android SAF, where there is no file watcher to catch the write.
        if is_reserved_path(&rel) {
            return Err(ToolError::fatal(format!(
                "refusing to write to a reserved path '{rel}'. Corvux's own folders (.corvux, .attachments) and dotfiles are off-limits to the agent."
            )));
        }
        let created = crate::commands::files::create_roost_note_synced(&ctx.app, &rel, &content)
            .await
            .map_err(|e| ToolError::retryable(format!("write: {e}")))?;
        if !created {
            return Err(ToolError::retryable(format!(
                "note already exists at '{}'. Pick a different title or open the existing note.",
                note_ref::display_join(&roost, &rel)
            )));
        }

        let path_str = note_ref::display_join(&roost, &rel);

        // Link diagnostics (non-blocking): surface body `[[links]]` that
        // resolve to nothing (dangling) or to several notes (ambiguous), so
        // the agent can fix a typo / disambiguate instead of silently
        // producing no edge or — worse — the wrong edge. Forward-references
        // are legitimate, hence a warning rather than a rejection.
        let rel_dir = folder.to_lowercase();
        let mut result = json!({
            "summary": format!("created note: {title}"),
            "title": title,
            "tags": tags,
            "file_path": path_str,
        });
        if let Some(warnings) = link_warnings_for(ctx, &body, &rel_dir) {
            result["link_warnings"] = warnings;
        }
        Ok(result)
    }
}

/// True if a roost-relative path targets one of Corvux's reserved hidden
/// areas (`.corvux/`, `.attachments/`, `.corvux-trash/`) or any dotfile /
/// dot-directory. Agent writes must not mutate Corvux's own internal state
/// (sync identity, settings, trash) — this mirrors the MCP server's refusal
/// (`corvux-mcp/src/roost.rs`). Reads are intentionally unaffected. `.`/`..`
/// are already rejected upstream by the path-escape guards; excluded here so
/// the predicate reads as "a dot-segment that isn't a traversal token".
pub(super) fn is_reserved_path(rel: &str) -> bool {
    rel.split('/')
        .any(|seg| seg.starts_with('.') && seg != "." && seg != "..")
}

/// Build the link resolver from the graph DB and return a `link_warnings`
/// JSON value when `content`'s wiki-links include dangling or ambiguous
/// targets. `None` if the graph is unavailable or every link resolves
/// cleanly. Shared by `roost_write` and `roost_edit`.
pub(super) fn link_warnings_for(ctx: &ToolContext, content: &str, rel_dir: &str) -> Option<Value> {
    let graph = ctx.app.try_state::<GraphDb>()?;
    let guard = graph.db.lock().ok()?;
    let db = guard.as_ref()?;
    let resolver = queries::LinkResolver::build(db).ok()?;
    link_report_to_json(&queries::check_links(content, &resolver, rel_dir))
}

/// Format a [`queries::LinkReport`] into the `link_warnings` payload the agent
/// sees in the tool result. `None` for a clean report. Pure — unit-tested.
pub(super) fn link_report_to_json(report: &queries::LinkReport) -> Option<Value> {
    if report.is_empty() {
        return None;
    }
    let mut summary = Vec::new();
    if !report.dangling.is_empty() {
        summary.push(format!(
            "{} link(s) point at notes that don't exist: {}",
            report.dangling.len(),
            report
                .dangling
                .iter()
                .map(|t| format!("[[{t}]]"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if !report.ambiguous.is_empty() {
        let listed: Vec<String> = report
            .ambiguous
            .iter()
            .map(|(t, c)| format!("[[{t}]] (could be {})", c.join(" / ")))
            .collect();
        summary.push(format!(
            "{} ambiguous link(s): {}",
            report.ambiguous.len(),
            listed.join("; ")
        ));
    }
    Some(json!({
        "dangling": report.dangling,
        "ambiguous": report
            .ambiguous
            .iter()
            .map(|(t, c)| json!({ "target": t, "could_be": c }))
            .collect::<Vec<_>>(),
        "summary": summary.join(". "),
        "advice": "Not blocking. Fix a typo, create the missing note, or use a full path like [[Folder/Note]] to disambiguate. A dangling link makes no graph connection until its target exists.",
    }))
}

/// Minimal ISO date stamp (YYYY-MM-DD). Avoids the chrono dep — copied
/// verbatim from the legacy `tools.rs` so the on-disk output format
/// matches existing AI-generated notes byte-for-byte.
fn chrono_date_iso() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = (secs / 86400) as i64;
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Howard Hinnant's days→(year,month,day). Valid 1970–2099.
fn civil_from_days(z: i64) -> (i32, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y as i32, m, d)
}

/// Legacy-name shim. The previous `create_note` tool stays available so
/// LLM responses generated against older system prompts (or chat history
/// from before this refactor) still execute successfully.
pub struct CreateNoteAliasTool;

#[async_trait]
impl Tool for CreateNoteAliasTool {
    fn name(&self) -> &'static str {
        "create_note"
    }
    fn description(&self) -> &'static str {
        RoostWriteTool.description()
    }
    fn input_schema(&self) -> Value {
        RoostWriteTool.input_schema()
    }
    fn safety_tier(&self) -> SafetyTier {
        SafetyTier::Write
    }
    async fn execute(&self, ctx: &ToolContext, args: Value) -> Result<Value, ToolError> {
        RoostWriteTool.execute(ctx, args).await
    }
}

#[cfg(test)]
mod reserved_path_tests {
    use super::is_reserved_path;

    #[test]
    fn rejects_corvux_internal_folders_and_dotfiles() {
        assert!(is_reserved_path(".corvux/roost-settings.json"));
        assert!(is_reserved_path(".attachments/img.png"));
        assert!(is_reserved_path(".corvux-trash/old.md"));
        assert!(is_reserved_path("Notes/.secret.md")); // dotfile in a subfolder
        assert!(is_reserved_path(".stignore"));
    }

    #[test]
    fn allows_ordinary_note_paths() {
        assert!(!is_reserved_path("Notes/Atlas.md"));
        assert!(!is_reserved_path("Daily/2026-06-13.md"));
        assert!(!is_reserved_path("note.with.dots.md")); // dots mid-name are fine
        assert!(!is_reserved_path("a/b/c.md"));
    }
}

#[cfg(test)]
mod link_warning_tests {
    use super::link_report_to_json;
    use crate::db::queries::LinkReport;

    #[test]
    fn clean_report_yields_no_warning() {
        assert!(link_report_to_json(&LinkReport::default()).is_none());
    }

    #[test]
    fn dangling_and_ambiguous_serialize_for_the_agent() {
        let report = LinkReport {
            dangling: vec!["Ghost".into()],
            ambiguous: vec![(
                "Index".into(),
                vec!["work/index".into(), "personal/index".into()],
            )],
        };
        let v = link_report_to_json(&report).expect("non-empty report → Some");
        assert_eq!(v["dangling"][0], "Ghost");
        assert_eq!(v["ambiguous"][0]["target"], "Index");
        assert_eq!(v["ambiguous"][0]["could_be"][0], "work/index");
        let summary = v["summary"].as_str().unwrap();
        assert!(summary.contains("[[Ghost]]"), "summary: {summary}");
        assert!(summary.contains("[[Index]]"), "summary: {summary}");
    }
}
