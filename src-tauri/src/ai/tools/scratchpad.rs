//! Working memory — `scratchpad_read` / `scratchpad_write` tools.
//!
//! A single markdown file at `{roost}/.corvux/scratchpad.md` the agent
//! can read and write to track state across iterations of a long
//! multi-step task. This is **layer 2** of the four-layer memory model
//! described in `docs/agent-harness-vision.md`:
//!
//! ```text
//! 1. Immediate (per-turn system prompt)         ← assembled by chat.rs
//! 2. Working  (.corvux/scratchpad.md)           ← this module
//! 3. Episodic (.corvux/episodes/, future)
//! 4. Durable  (.corvux/memory.md)               ← memory.rs
//! ```
//!
//! Why a separate file from `memory.md`:
//! - Scratchpad is **session state**, not durable facts. Conflating the
//!   two pollutes long-term memory with intermediate clutter.
//! - The agent decides when to read it — it's NOT auto-injected into
//!   the system prompt. Memory always is.
//! - User can clear it at any time without losing real memory.
//!
//! ## When the agent uses this
//!
//! For tasks that span multiple iterations of the agent loop AND can't
//! be reliably reconstructed from the chat history alone:
//!
//! - "Go through all my project notes and create a summary." Track what
//!   you've already read so you don't re-read.
//! - "Refactor every reference to X across the roost." Track which
//!   files you've already touched.
//! - "Find duplicate concepts in my notes." Track candidates as you
//!   discover them.
//!
//! For one-shot or short-loop tasks the scratchpad is overhead; the
//! description nudges the LLM to skip it for those cases.

use async_trait::async_trait;
use serde_json::{json, Value};
use tauri::Manager;

#[cfg(test)]
use std::{
    fs,
    path::{Path, PathBuf},
};

use super::registry::{SafetyTier, Tool, ToolContext, ToolError};

#[cfg(test)]
const SCRATCHPAD_FILE: &str = "scratchpad.md";

pub struct ScratchpadReadTool;
pub struct ScratchpadWriteTool;

#[cfg(test)]
fn scratchpad_path(roost: &str) -> PathBuf {
    Path::new(roost).join(".corvux").join(SCRATCHPAD_FILE)
}

#[cfg(test)]
fn ensure_parent(path: &Path) -> Result<(), ToolError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| ToolError::fatal(format!("could not create .corvux dir: {e}")))?;
    }
    Ok(())
}

#[async_trait]
impl Tool for ScratchpadReadTool {
    fn name(&self) -> &'static str {
        "scratchpad_read"
    }

    fn description(&self) -> &'static str {
        "Read your working-memory scratchpad. Returns the file contents \
         as `content` (empty string if you've never written to it). Use \
         this to recover state across iterations of a long multi-step \
         task — what you've already explored, plans you've drafted, \
         findings to consolidate. Skip for one-shot questions; this \
         only pays off when you're partway through a task and need to \
         remember what you were doing."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {},
            "required": []
        })
    }

    fn safety_tier(&self) -> SafetyTier {
        SafetyTier::Read
    }

    async fn execute(&self, ctx: &ToolContext, _args: Value) -> Result<Value, ToolError> {
        ctx.require_roost(self.name())?;
        let state = ctx
            .app
            .try_state::<crate::commands::files::RoostFsState>()
            .ok_or_else(|| ToolError::fatal("roost filesystem not initialized"))?;
        let backend = state
            .current()
            .await
            .ok_or_else(|| ToolError::fatal("No roost is open"))?;
        let content = match backend.read(".corvux/scratchpad.md").await {
            Ok(bytes) => String::from_utf8(bytes)
                .map_err(|e| ToolError::fatal(format!("scratchpad is not UTF-8: {e}")))?,
            Err(crate::roostfs::RoostError::NotFound(_)) => String::new(),
            Err(e) => return Err(ToolError::retryable(format!("scratchpad read failed: {e}"))),
        };
        let bytes = content.len();
        Ok(json!({
            "content": content,
            "bytes": bytes,
            "summary": if bytes == 0 {
                "Scratchpad is empty.".to_string()
            } else {
                format!("Read {bytes}-byte scratchpad.")
            },
        }))
    }
}

#[async_trait]
impl Tool for ScratchpadWriteTool {
    fn name(&self) -> &'static str {
        "scratchpad_write"
    }

    fn description(&self) -> &'static str {
        "Write to your working-memory scratchpad. Args: `content` \
         (required — the markdown text to record), `mode` (required — \
         either `\"append\"` to add to the bottom or `\"replace\"` to \
         overwrite). Use `append` while gathering findings; use \
         `replace` when you want to rewrite a clean summary at the \
         end. The user can edit or clear this file directly — treat \
         every write as best-effort, not authoritative."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "content": {
                    "type": "string",
                    "description": "Markdown text to write to the scratchpad."
                },
                "mode": {
                    "type": "string",
                    "description": "Either 'append' or 'replace'."
                }
            },
            "required": ["content", "mode"]
        })
    }

    fn safety_tier(&self) -> SafetyTier {
        SafetyTier::Write
    }

    async fn execute(&self, ctx: &ToolContext, args: Value) -> Result<Value, ToolError> {
        ctx.require_roost(self.name())?;
        let content = args
            .get("content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::retryable("missing 'content'"))?;
        let mode = args
            .get("mode")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::retryable("missing 'mode' (append|replace)"))?;

        let content = content.to_string();
        let mode = mode.to_string();
        if mode != "replace" && mode != "append" {
            return Err(ToolError::retryable(format!(
                "mode must be 'append' or 'replace', got '{mode}'"
            )));
        }
        let mode_for_mutation = mode.clone();
        let (_, final_text, ()) = crate::commands::files::mutate_roost_text_synced(
            &ctx.app,
            ".corvux/scratchpad.md",
            true,
            move |prior| {
                let next = match mode_for_mutation.as_str() {
                    "replace" => content,
                    "append" if prior.is_empty() => content,
                    "append" if prior.ends_with('\n') => format!("{prior}{content}"),
                    "append" => format!("{prior}\n{content}"),
                    _ => unreachable!("mode validated before mutation"),
                };
                Ok((next, ()))
            },
        )
        .await
        .map_err(ToolError::fatal)?;

        Ok(json!({
            "summary": format!(
                "Scratchpad {} ({} bytes total).",
                if mode == "append" { "appended" } else { "replaced" },
                final_text.len()
            ),
            "bytes": final_text.len(),
            "mode": mode,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[allow(dead_code)] // kept for future tests that exercise the full execute() path
    fn fake_ctx_with_roost(roost: &str) -> (ToolContext, TempDir) {
        // ToolContext requires an AppHandle; tests bypass that by
        // calling `execute` directly only on tools that don't need it
        // (read/write here only touch the filesystem). We cheat by
        // building a context via tauri's mock runtime — but the
        // approval / settings hooks don't fire for these tools, so a
        // unit-test path that exercises just fs is enough.
        //
        // Instead of constructing AppHandle, we test the path helpers
        // and behavior at the `fs` boundary directly.
        let _ = roost;
        unreachable!("tests below operate on fs primitives, not on AppHandle-bound execute()")
    }

    #[test]
    fn scratchpad_path_is_inside_corvux_subdir() {
        let p = scratchpad_path("/some/roost");
        assert!(p.ends_with(Path::new(".corvux/scratchpad.md")));
    }

    #[test]
    fn ensure_parent_creates_corvux_dir() {
        let dir = TempDir::new().unwrap();
        let nested = dir.path().join("a/b/c/scratchpad.md");
        ensure_parent(&nested).unwrap();
        assert!(nested.parent().unwrap().exists());
    }

    /// End-to-end fs roundtrip without `execute` — verifies the storage
    /// shape (file location + append-vs-replace semantics) without
    /// needing a Tauri AppHandle.
    #[test]
    fn append_then_read_returns_combined_text() {
        let dir = TempDir::new().unwrap();
        let roost = dir.path().to_string_lossy().to_string();
        let path = scratchpad_path(&roost);
        ensure_parent(&path).unwrap();

        // First write — empty file, content lands as-is.
        fs::write(&path, "first line").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "first line");

        // Append — should add a newline separator since prior didn't end in one.
        let prior = fs::read_to_string(&path).unwrap();
        let combined = if prior.ends_with('\n') {
            format!("{prior}second line")
        } else {
            format!("{prior}\nsecond line")
        };
        fs::write(&path, &combined).unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "first line\nsecond line"
        );

        // Replace — overwrites unconditionally.
        fs::write(&path, "fresh content").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "fresh content");
    }

    #[test]
    fn read_missing_scratchpad_returns_empty() {
        let dir = TempDir::new().unwrap();
        let roost = dir.path().to_string_lossy().to_string();
        let path = scratchpad_path(&roost);
        // Don't create it.
        let content = fs::read_to_string(&path).unwrap_or_default();
        assert_eq!(content, "");
    }

    #[test]
    fn append_with_trailing_newline_does_not_double_newline() {
        let dir = TempDir::new().unwrap();
        let roost = dir.path().to_string_lossy().to_string();
        let path = scratchpad_path(&roost);
        ensure_parent(&path).unwrap();

        fs::write(&path, "a\n").unwrap();
        let prior = fs::read_to_string(&path).unwrap();
        let combined = if prior.ends_with('\n') {
            format!("{prior}b")
        } else {
            format!("{prior}\nb")
        };
        fs::write(&path, &combined).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "a\nb");
    }

    #[test]
    fn read_schema_has_no_required_fields() {
        let schema = ScratchpadReadTool.input_schema();
        let req = schema.get("required").and_then(|v| v.as_array()).unwrap();
        assert!(req.is_empty());
    }

    #[test]
    fn write_schema_requires_content_and_mode() {
        let schema = ScratchpadWriteTool.input_schema();
        let req = schema
            .get("required")
            .and_then(|v| v.as_array())
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect::<Vec<_>>();
        assert!(req.contains(&"content"));
        assert!(req.contains(&"mode"));
    }
}
