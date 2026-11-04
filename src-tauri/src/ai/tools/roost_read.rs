//! `roost_read` — read multiple notes by path or wiki-link title.
//! Caps each note's bytes individually + caps the total. Optionally
//! extracts named heading sections so the agent doesn't pull a 50KB
//! note when it only needs the `## Summary` section.

use async_trait::async_trait;
use serde_json::{json, Value};

use super::note_ref;
use super::registry::{SafetyTier, Tool, ToolContext, ToolError};

pub struct RoostReadTool;

const DEFAULT_MAX_BYTES_PER_NOTE: usize = 16 * 1024;
const HARD_MAX_BYTES_PER_NOTE: usize = 24 * 1024;
const MAX_NOTES_PER_CALL: usize = 10;

#[async_trait]
impl Tool for RoostReadTool {
    fn name(&self) -> &'static str {
        "roost_read"
    }

    fn description(&self) -> &'static str {
        "Read the full markdown content of one or more notes by absolute path, \
         roost-relative path, or [[wiki-link]] title. Use this AFTER roost_search \
         when you need the actual content. Caps at 10 notes per call. Pass \
         `sections` to extract only specific heading sections (recommended for \
         long notes — call note_outline first to discover heading names)."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "paths": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Up to 10 paths or wiki-link titles",
                    "maxItems": MAX_NOTES_PER_CALL
                },
                "sections": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Optional: heading names to extract (e.g. ['Summary', 'Action Items']). Compared case-insensitive, leading '#' stripped. If omitted, returns the full note (truncated)."
                },
                "max_bytes_per_note": {
                    "type": "integer",
                    "default": DEFAULT_MAX_BYTES_PER_NOTE,
                    "description": "Per-note byte cap. Hard ceiling 24KB."
                }
            },
            "required": ["paths"]
        })
    }

    fn safety_tier(&self) -> SafetyTier {
        SafetyTier::Read
    }

    async fn execute(&self, ctx: &ToolContext, args: Value) -> Result<Value, ToolError> {
        let roost = ctx.require_roost(self.name())?.to_string();

        let paths: Vec<String> = args
            .get("paths")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        if paths.is_empty() {
            return Err(ToolError::retryable("'paths' is empty"));
        }

        let sections: Vec<String> = args
            .get("sections")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();

        let max_bytes = args
            .get("max_bytes_per_note")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize)
            .unwrap_or(DEFAULT_MAX_BYTES_PER_NOTE)
            .min(HARD_MAX_BYTES_PER_NOTE);

        let mut notes: Vec<Value> = Vec::with_capacity(paths.len());
        let mut ok_count = 0usize;
        let mut err_count = 0usize;

        for raw in paths.iter().take(MAX_NOTES_PER_CALL) {
            let resolved = note_ref::resolve_rel(raw, ctx, false, false).await.ok();
            let entry = match resolved {
                Some(rel) => match note_ref::read_rel(ctx, &rel).await {
                    Ok(content) => {
                        let body = if sections.is_empty() {
                            content
                        } else {
                            extract_sections(&content, &sections)
                        };
                        let stripped = strip_frontmatter(&body);
                        let (clipped, was_truncated) = clip_bytes(stripped, max_bytes);
                        ok_count += 1;
                        json!({
                            "input": raw,
                            "path": note_ref::display_join(&roost, &rel),
                            "bytes": clipped.len(),
                            "truncated": was_truncated,
                            "sections_filtered": !sections.is_empty(),
                            "content": clipped,
                        })
                    }
                    Err(e) => {
                        err_count += 1;
                        json!({
                            "input": raw,
                            "error": format!("read failed: {e}"),
                        })
                    }
                },
                None => {
                    err_count += 1;
                    json!({
                        "input": raw,
                        "error": "could not resolve path or wiki-link title — try roost_search first",
                    })
                }
            };
            notes.push(entry);
        }

        // Honest reporting: if NOTHING resolved, return retryable error
        // so the chip shows the failure and the LLM tries roost_search
        // instead of synthesizing an answer from nothing.
        if ok_count == 0 {
            return Err(ToolError::retryable(format!(
                "could not resolve any of the {err_count} requested path(s) — call roost_search first to find the right titles"
            )));
        }

        let summary = if err_count == 0 {
            format!("read {ok_count} note(s)")
        } else {
            format!(
                "read {ok_count} of {} note(s) ({err_count} unresolved)",
                ok_count + err_count
            )
        };

        Ok(json!({
            "summary": summary,
            "ok_count": ok_count,
            "error_count": err_count,
            "notes": notes,
        }))
    }
}

/// Pull the named heading sections from `content`. Section name match is
/// case-insensitive, ignores leading `#` markers, and trims whitespace.
/// If no section matches, returns an empty string — caller surfaces that
/// as `truncated=false, bytes=0`, signalling the agent to try without
/// `sections` or pick different headings.
fn extract_sections(content: &str, names: &[String]) -> String {
    let want: Vec<String> = names
        .iter()
        .map(|s| s.trim().trim_start_matches('#').trim().to_lowercase())
        .filter(|s| !s.is_empty())
        .collect();
    if want.is_empty() {
        return content.to_string();
    }

    let mut out = String::new();
    let mut current_match: Option<usize> = None; // section heading level when matched
    for line in content.lines() {
        let trimmed = line.trim_start();
        if let Some(level) = heading_level(trimmed) {
            let title = trimmed.trim_start_matches('#').trim().to_lowercase();
            if want.iter().any(|w| title.contains(w)) {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(line);
                out.push('\n');
                current_match = Some(level);
                continue;
            }
            // A heading at the same or higher level (lower number) ends
            // the current section.
            if let Some(m) = current_match {
                if level <= m {
                    current_match = None;
                }
            }
        }
        if current_match.is_some() {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

fn heading_level(line: &str) -> Option<usize> {
    let mut level = 0;
    for ch in line.chars() {
        if ch == '#' {
            level += 1;
        } else {
            break;
        }
    }
    if level == 0 || level > 6 {
        return None;
    }
    // ATX heading must have a space after the hashes.
    let after = line.chars().nth(level)?;
    if after != ' ' {
        return None;
    }
    Some(level)
}

/// Drop the YAML frontmatter block at the top of a note. Matches the
/// existing `parser/tags.rs` convention so the agent doesn't see
/// metadata-noise lines like `created: 2026-04-26` in note content.
fn strip_frontmatter(content: &str) -> String {
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        return content.to_string();
    }
    let after_first = &trimmed[3..];
    if let Some(end) = after_first.find("\n---") {
        let rest = &after_first[end + 4..];
        return rest.trim_start_matches('\n').to_string();
    }
    content.to_string()
}

/// Clip a string to at most `max` bytes on a UTF-8 boundary. Returns
/// (clipped, was_truncated).
fn clip_bytes(s: String, max: usize) -> (String, bool) {
    if s.len() <= max {
        return (s, false);
    }
    let mut cut = max;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    let mut out = s[..cut].to_string();
    out.push_str("\n…[truncated]");
    (out, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_named_section() {
        let md = "# Title\n\n## Summary\nshort.\n\n## Details\nlong.\n";
        let out = extract_sections(md, &["Summary".to_string()]);
        assert!(out.contains("Summary"));
        assert!(out.contains("short."));
        assert!(!out.contains("long."));
    }

    #[test]
    fn extract_sections_case_insensitive_partial() {
        let md = "## Action Items\n- a\n## Other\n- b\n";
        let out = extract_sections(md, &["action".to_string()]);
        assert!(out.contains("- a"));
        assert!(!out.contains("- b"));
    }

    #[test]
    fn frontmatter_stripped() {
        let md = "---\ntitle: x\n---\n\n# Real content\n";
        assert!(!strip_frontmatter(md).contains("title: x"));
        assert!(strip_frontmatter(md).contains("Real content"));
    }

    #[test]
    fn rejects_absolute_path_outside_roost() {
        // Roost-boundary enforcement lives in `note_ref::resolve` now; test
        // it directly against the unified resolver (no AppHandle needed —
        // title lookup just no-ops when app is None).
        let roost = tempfile::tempdir().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(outside.path(), b"secret").unwrap();
        let opts = note_ref::ResolveOpts {
            roost: roost.path().to_str().unwrap(),
            app: None,
            open_note_path: None,
            allow_create_daily: false,
            allow_open_fallback: false,
        };
        let resolved = note_ref::resolve(outside.path().to_str().unwrap(), opts);
        assert!(
            resolved.is_err() || resolved.as_ref().ok().filter(|p| p.exists()).is_none(),
            "absolute path outside roost must not resolve, got {resolved:?}"
        );
    }

    #[test]
    fn accepts_path_inside_roost() {
        let roost = tempfile::tempdir().unwrap();
        let note_path = roost.path().join("note.md");
        std::fs::write(&note_path, b"# hi\n").unwrap();
        let opts = note_ref::ResolveOpts {
            roost: roost.path().to_str().unwrap(),
            app: None,
            open_note_path: None,
            allow_create_daily: false,
            allow_open_fallback: false,
        };
        let resolved = note_ref::resolve(note_path.to_str().unwrap(), opts);
        assert!(
            resolved.is_ok(),
            "note inside roost must resolve, got {resolved:?}"
        );
    }
}
