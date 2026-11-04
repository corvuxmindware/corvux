//! `note_outline` — heading structure of a single note. The agent's
//! cheap "should I read this?" probe before pulling full content with
//! `roost_read`.

use async_trait::async_trait;
use serde_json::{json, Value};

use super::note_ref;
use super::registry::{SafetyTier, Tool, ToolContext, ToolError};

pub struct NoteOutlineTool;

#[async_trait]
impl Tool for NoteOutlineTool {
    fn name(&self) -> &'static str {
        "note_outline"
    }

    fn description(&self) -> &'static str {
        "Get the heading structure of a note (level + text + line number) \
         WITHOUT reading its body. Use this to decide which sections of a \
         long note are worth pulling with roost_read. Especially useful \
         when a note is over a few KB."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path or [[wiki-link]] title" }
            },
            "required": ["path"]
        })
    }

    fn safety_tier(&self) -> SafetyTier {
        SafetyTier::Read
    }

    fn max_result_bytes(&self) -> usize {
        // Outlines are tiny — never need the global cap. 8KB is enough for
        // notes with hundreds of headings.
        8 * 1024
    }

    async fn execute(&self, ctx: &ToolContext, args: Value) -> Result<Value, ToolError> {
        let roost = ctx.require_roost(self.name())?.to_string();
        let raw = args
            .get("path")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::retryable("missing 'path'"))?
            .to_string();

        let rel = note_ref::resolve_rel(&raw, ctx, false, false).await?;
        let content = note_ref::read_rel(ctx, &rel).await?;
        let abs = note_ref::display_join(&roost, &rel);

        let mut headings: Vec<Value> = Vec::new();
        let mut total_lines = 0usize;
        for (idx, line) in content.lines().enumerate() {
            total_lines = idx + 1;
            let trimmed = line.trim_start();
            if let Some(level) = heading_level(trimmed) {
                let text = trimmed.trim_start_matches('#').trim().to_string();
                headings.push(json!({
                    "level": level,
                    "text": text,
                    "line": idx + 1,
                }));
            }
        }

        Ok(json!({
            "summary": format!("{} heading(s) in {} lines", headings.len(), total_lines),
            "path": abs,
            "title": rel.rsplit('/').next().unwrap_or(&rel).trim_end_matches(".md"),
            "total_lines": total_lines,
            "total_bytes": content.len(),
            "headings": headings,
        }))
    }
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
    let after = line.chars().nth(level)?;
    if after != ' ' {
        return None;
    }
    Some(level)
}

// Path-resolution helpers (build_resolver, resolve_input, ensure_within_roost)
// have moved to `super::note_ref`. Behavior-equivalent roost-boundary tests
// now live in roost_read.rs's test module exercising note_ref directly.
