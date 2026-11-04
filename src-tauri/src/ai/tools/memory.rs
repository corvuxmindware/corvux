//! Long-term agentic memory — `remember` and `forget` tools.
//!
//! Memory lives in `{roost}/.corvux/memory.md` as a small markdown file
//! (H2 sections as categories, bullet lines as facts). It's prepended to
//! the system prompt on every chat turn so the AI carries what it knows
//! about the user across all sessions — the third memory layer beyond
//! per-turn working memory (RAG) and per-session conversation history.
//!
//! Format:
//! ```markdown
//! # Corvux Memory
//!
//! ## Identity
//! - Solo founder-developer building Corvux
//!
//! ## Preferences
//! - Prefers pushback over sycophancy
//! ```
//!
//! Why a file in the roost: user-readable, user-editable, syncs with the
//! rest of the roost via Automerge, no schema migration ever needed.
//! Why H2 sections: lets the AI cluster facts ("Identity", "Preferences",
//! "Active projects") so the prompt stays scannable as memory grows.

use async_trait::async_trait;
use serde_json::{json, Value};
use tauri::Manager;

use super::registry::{SafetyTier, Tool, ToolContext, ToolError};

const DEFAULT_CATEGORY: &str = "Notes";
const HEADER: &str = "# Corvux Memory\n\n\
Persistent facts the AI knows about you. The AI uses the `remember` and \
`forget` tools to maintain this — you can also edit directly. Injected \
into every chat's system prompt so context survives across sessions.\n\n";

pub struct RememberTool;
pub struct ForgetTool;

// ── Public read helper used by chat.rs to inject memory into the system prompt ──

/// Read the memory file for `roost`, returning the markdown text or
/// `None` when the file doesn't exist yet. Errors (permission, etc.)
/// also yield `None` so a broken memory file can never break chat.
pub async fn read_memory<R: tauri::Runtime>(app: &tauri::AppHandle<R>) -> Option<String> {
    let state = app.try_state::<crate::commands::files::RoostFsState>()?;
    let backend = state.current().await?;
    let bytes = backend.read(".corvux/memory.md").await.ok()?;
    String::from_utf8(bytes)
        .ok()
        .filter(|s| !s.trim().is_empty())
}

// ── Markdown parse / format ──────────────────────────────────────────

#[derive(Debug, Clone)]
struct Section {
    name: String,
    facts: Vec<String>,
}

/// Parse the memory markdown into ordered sections. Anything outside an
/// H2 / bullet (e.g. the file's header paragraph) is dropped — we
/// re-emit a canonical header on every write so the file stays clean.
fn parse_memory(s: &str) -> Vec<Section> {
    let mut sections: Vec<Section> = Vec::new();
    let mut current: Option<Section> = None;
    for line in s.lines() {
        let trimmed = line.trim();
        if let Some(name) = trimmed.strip_prefix("## ") {
            if let Some(s) = current.take() {
                sections.push(s);
            }
            current = Some(Section {
                name: name.trim().to_string(),
                facts: Vec::new(),
            });
        } else if let Some(fact) = trimmed.strip_prefix("- ") {
            let f = fact.trim();
            if !f.is_empty() {
                if let Some(sec) = current.as_mut() {
                    sec.facts.push(f.to_string());
                } else {
                    // Bullet before any heading — bucket into Notes.
                    current = Some(Section {
                        name: DEFAULT_CATEGORY.to_string(),
                        facts: vec![f.to_string()],
                    });
                }
            }
        }
    }
    if let Some(s) = current {
        sections.push(s);
    }
    sections
}

fn format_memory(sections: &[Section]) -> String {
    let mut out = String::from(HEADER);
    for sec in sections {
        if sec.facts.is_empty() {
            continue;
        }
        out.push_str(&format!("## {}\n\n", sec.name));
        for f in &sec.facts {
            out.push_str(&format!("- {f}\n"));
        }
        out.push('\n');
    }
    out
}

// ── remember ────────────────────────────────────────────────────────

#[async_trait]
impl Tool for RememberTool {
    fn name(&self) -> &'static str {
        "remember"
    }

    fn description(&self) -> &'static str {
        "Persist a fact about the user that should be available in EVERY \
         future chat. Use when the user explicitly asks ('remember that I \
         prefer X', 'don't forget that...') OR when you learn something \
         durably useful (their name, role, recurring preferences, active \
         projects, important constraints). \
         DO NOT use for one-shot context inside a single session — only \
         for facts that should survive across all chats. Idempotent: \
         re-asserting an existing fact is a no-op. \
         Args: `fact` (required, one short line — keep it crisp), \
         `category` (optional — examples: 'Identity', 'Preferences', \
         'Active projects', 'Constraints'; default 'Notes')."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "fact": {
                    "type": "string",
                    "description": "Single durable fact, ~one short line. No newlines."
                },
                "category": {
                    "type": "string",
                    "description": "Section heading. Group similar facts together."
                }
            },
            "required": ["fact"]
        })
    }

    fn safety_tier(&self) -> SafetyTier {
        SafetyTier::Write
    }

    fn timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(5)
    }

    async fn execute(&self, ctx: &ToolContext, args: Value) -> Result<Value, ToolError> {
        ctx.require_roost(self.name())?;
        let fact = args
            .get("fact")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::retryable("missing 'fact'"))?
            // Collapse newlines so the file stays valid markdown bullets.
            .replace('\n', " ")
            .trim()
            .to_string();
        let category = args
            .get("category")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(DEFAULT_CATEGORY)
            .to_string();

        let fact_for_mutation = fact.clone();
        let category_for_mutation = category.clone();
        let (_, _, added) = crate::commands::files::mutate_roost_text_synced(
            &ctx.app,
            ".corvux/memory.md",
            true,
            move |existing| {
                let mut sections = parse_memory(existing);
                let fact_key = fact_for_mutation.to_lowercase();
                let mut added = false;
                if let Some(sec) = sections
                    .iter_mut()
                    .find(|s| s.name.eq_ignore_ascii_case(&category_for_mutation))
                {
                    if !sec.facts.iter().any(|f| f.to_lowercase() == fact_key) {
                        sec.facts.push(fact_for_mutation.clone());
                        added = true;
                    }
                } else {
                    sections.push(Section {
                        name: category_for_mutation,
                        facts: vec![fact_for_mutation],
                    });
                    added = true;
                }
                let next = if added {
                    format_memory(&sections)
                } else {
                    existing.to_string()
                };
                Ok((next, added))
            },
        )
        .await
        .map_err(ToolError::retryable)?;

        Ok(json!({
            "summary": if added {
                format!("remembered: {fact} (category: {category})")
            } else {
                format!("already remembered: {fact}")
            },
            "fact": fact,
            "category": category,
            "added": added,
        }))
    }
}

// ── forget ──────────────────────────────────────────────────────────

#[async_trait]
impl Tool for ForgetTool {
    fn name(&self) -> &'static str {
        "forget"
    }

    fn description(&self) -> &'static str {
        "Remove facts from long-term memory whose text matches `pattern` \
         (case-insensitive substring). Use when the user asks to forget \
         something, or when an old fact is now wrong. Empty memory after \
         removal is fine — the section header drops automatically. Args: \
         `pattern` (required) — be specific to avoid wiping the wrong \
         fact; pass the full sentence when in doubt."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Case-insensitive substring. Matches whole-fact text."
                }
            },
            "required": ["pattern"]
        })
    }

    fn safety_tier(&self) -> SafetyTier {
        // Memory removal is destructive — once gone, no undo unless the
        // user kept the file in git/Automerge history. The tool-card flow
        // (Phase 2) should require explicit confirmation; today the agent
        // loop runs all writes without prompting, matching legacy UX.
        SafetyTier::Destructive
    }

    fn timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(5)
    }

    async fn execute(&self, ctx: &ToolContext, args: Value) -> Result<Value, ToolError> {
        ctx.require_roost(self.name())?;
        let pattern = args
            .get("pattern")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::retryable("missing 'pattern'"))?
            .to_lowercase();

        let pattern_for_mutation = pattern.clone();
        let (_, _, removed) = crate::commands::files::mutate_roost_text_synced(
            &ctx.app,
            ".corvux/memory.md",
            true,
            move |existing| {
                let mut sections = parse_memory(existing);
                let mut removed: Vec<String> = Vec::new();
                for sec in sections.iter_mut() {
                    sec.facts.retain(|f| {
                        if f.to_lowercase().contains(&pattern_for_mutation) {
                            removed.push(f.clone());
                            false
                        } else {
                            true
                        }
                    });
                }
                sections.retain(|s| !s.facts.is_empty());
                let next = if removed.is_empty() {
                    existing.to_string()
                } else {
                    format_memory(&sections)
                };
                Ok((next, removed))
            },
        )
        .await
        .map_err(ToolError::retryable)?;

        if removed.is_empty() {
            return Ok(json!({
                "summary": format!("no memory matched '{pattern}'"),
                "removed": [],
            }));
        }

        Ok(json!({
            "summary": format!("forgot {} fact(s)", removed.len()),
            "removed": removed,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_format_round_trip() {
        let raw = "# Corvux Memory\n\nblurb\n\n## Identity\n\n- Founder-dev\n- Tenglish speaker\n\n## Preferences\n\n- Prefers pushback\n";
        let parsed = parse_memory(raw);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].name, "Identity");
        assert_eq!(parsed[0].facts, vec!["Founder-dev", "Tenglish speaker"]);
        assert_eq!(parsed[1].name, "Preferences");
        let formatted = format_memory(&parsed);
        let reparsed = parse_memory(&formatted);
        assert_eq!(reparsed.len(), 2);
        assert_eq!(reparsed[0].facts.len(), 2);
        assert_eq!(reparsed[1].facts.len(), 1);
    }

    #[test]
    fn empty_input_yields_no_sections() {
        assert!(parse_memory("").is_empty());
        assert!(parse_memory("# header only\n").is_empty());
    }

    #[test]
    fn bullets_without_section_go_to_default_category() {
        let raw = "- orphan fact\n";
        let parsed = parse_memory(raw);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].name, "Notes");
        assert_eq!(parsed[0].facts, vec!["orphan fact"]);
    }

    #[test]
    fn format_skips_empty_sections() {
        let secs = vec![
            Section {
                name: "Identity".into(),
                facts: vec!["x".into()],
            },
            Section {
                name: "Empty".into(),
                facts: vec![],
            },
        ];
        let out = format_memory(&secs);
        assert!(out.contains("## Identity"));
        assert!(!out.contains("## Empty"));
    }
}
