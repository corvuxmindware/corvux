// source_path retained for diagnostics
#![allow(dead_code)]

//! `AGENT.md` parser. Subagents are user-authored markdown files
//! declaring a *system prompt* (the body) plus optional structural
//! overrides in the frontmatter (model, tool allowlist, budget caps).
//!
//! ## Format
//!
//! ```markdown
//! ---
//! name: research-deep
//! description: |
//!   Multi-hop research over the roost. Use when the user asks an
//!   open-ended question that requires reading 5+ notes and synthesizing.
//!   Returns a markdown summary with [[wiki-link]] citations.
//! model: gemini-2.5-pro            # optional; defaults to parent's model
//! tools:                            # optional; defaults to parent-minus-task-minus-destructive
//!   - roost_search
//!   - roost_read
//! max_iterations: 12                # optional; parent's budget if omitted
//! max_wall_clock_secs: 240          # optional
//! max_tool_result_bytes: 200000     # optional
//! ---
//!
//! # Deep research
//!
//! You are a research subagent. Your job is to ...
//! ```
//!
//! Required: `name` (kebab-case, 1–64), `description` (any non-empty).
//! Everything else is optional with documented defaults at the call
//! site (`SubagentDef::tools_with_defaults`, `SubagentDef::budget_or`).

use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::ai::tools::AgentBudget;

/// Parsed AGENT.md ready for `TaskTool` dispatch.
#[derive(Debug, Clone)]
pub struct SubagentDef {
    pub name: String,
    pub description: String,
    pub body: String,
    /// Optional explicit model id (provider-specific — `gemini-2.5-pro`,
    /// `openrouter/anthropic/claude-3.5-sonnet`, …). Today's MVP
    /// **ignores** this field — subagents always inherit the parent's
    /// provider. Kept on the struct so future model-override work
    /// doesn't require a frontmatter migration.
    pub model: Option<String>,
    /// Optional explicit tool allowlist. `None` ⇒ inherit parent's
    /// tools minus `task` (no nesting) and minus Destructive tier.
    pub tools: Option<Vec<String>>,
    pub max_iterations: Option<usize>,
    pub max_wall_clock_secs: Option<u64>,
    pub max_tool_result_bytes: Option<usize>,
    /// Absolute path to the AGENT.md (or `<builtin>` for the bundled
    /// `general` subagent). Used for diagnostic surfaces and to break
    /// roost > global > builtin precedence ties.
    pub source_path: PathBuf,
}

#[derive(Debug, Error)]
pub enum SubagentParseError {
    #[error("missing frontmatter (subagent must begin with `---`)")]
    MissingFrontmatter,
    #[error("unterminated frontmatter (no closing `---` line)")]
    UnterminatedFrontmatter,
    #[error("missing required field `{0}`")]
    MissingField(&'static str),
    #[error("invalid subagent name `{0}` (must be lowercase a-z, 0-9, hyphen; 1-64 chars)")]
    InvalidName(String),
    #[error("subagent name `{name}` does not match folder name `{folder}`")]
    NameFolderMismatch { name: String, folder: String },
    #[error("field `{field}` failed to parse: {detail}")]
    BadField { field: &'static str, detail: String },
    #[error("io error reading subagent `{0}`")]
    Io(String),
}

/// Same shape rule we use for skill names — keep both ecosystems aligned.
pub fn is_valid_subagent_name(s: &str) -> bool {
    let len = s.len();
    if !(1..=64).contains(&len) {
        return false;
    }
    s.chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

impl SubagentDef {
    /// Resolve the effective tool list for this subagent at dispatch
    /// time. Either:
    /// - the explicit `tools:` list from frontmatter (deduplicated,
    ///   filtered to actually-registered names so a typo doesn't crash
    ///   the inner loop), OR
    /// - parent's tools minus `task` (no nested delegation) and minus
    ///   Destructive tier (subagents inherit the principle of least
    ///   privilege — explicit opt-in via frontmatter is the only way
    ///   to enable a Destructive write).
    pub fn tools_with_defaults<R: tauri::Runtime + 'static>(
        &self,
        parent: &crate::ai::tools::ToolRegistry<R>,
    ) -> Vec<String> {
        match &self.tools {
            Some(list) => {
                let mut seen = std::collections::HashSet::new();
                list.iter()
                    .filter(|t| parent.get(t).is_some())
                    .filter(|t| seen.insert((*t).clone()))
                    .cloned()
                    .collect()
            }
            None => {
                let mut out: Vec<String> = parent
                    .names()
                    .into_iter()
                    .filter(|n| *n != "task")
                    .filter(|n| {
                        parent
                            .get(n)
                            .map(|t| {
                                !matches!(
                                    t.safety_tier(),
                                    crate::ai::tools::SafetyTier::Destructive
                                )
                            })
                            .unwrap_or(false)
                    })
                    .map(|s| s.to_string())
                    .collect();
                out.sort();
                out
            }
        }
    }

    /// Build the effective budget for the subagent's loop. Any field
    /// not overridden in AGENT.md falls back to the parent's value.
    pub fn budget_or(&self, parent: &AgentBudget) -> AgentBudget {
        AgentBudget {
            max_iterations: self.max_iterations.unwrap_or(parent.max_iterations),
            max_tool_result_bytes: self
                .max_tool_result_bytes
                .unwrap_or(parent.max_tool_result_bytes),
            max_wall_clock_secs: self
                .max_wall_clock_secs
                .unwrap_or(parent.max_wall_clock_secs),
            // `max_tool_calls_per_iter` intentionally inherits — there
            // is no AGENT.md field for it. The parent's per-iter cap is
            // load-bearing for safety (fan-out bound) and shouldn't be
            // weakened from within a subagent definition.
            max_tool_calls_per_iter: parent.max_tool_calls_per_iter,
        }
    }
}

/// Parse AGENT.md content. `folder_name` enforces the convention that
/// the frontmatter `name:` matches its parent directory (same rule as
/// Skills).
pub fn parse_agent_md(
    content: &str,
    folder_name: &str,
    source_path: PathBuf,
) -> Result<SubagentDef, SubagentParseError> {
    let (front, body) = split_frontmatter(content)?;

    let name = scalar_field(front, "name")
        .ok_or(SubagentParseError::MissingField("name"))?
        .trim()
        .to_string();
    if !is_valid_subagent_name(&name) {
        return Err(SubagentParseError::InvalidName(name));
    }
    if name != folder_name {
        return Err(SubagentParseError::NameFolderMismatch {
            name,
            folder: folder_name.to_string(),
        });
    }

    let description = scalar_field(front, "description")
        .ok_or(SubagentParseError::MissingField("description"))?
        .trim()
        .to_string();
    if description.is_empty() {
        return Err(SubagentParseError::MissingField("description"));
    }

    let model = scalar_field(front, "model")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let tools = list_field(front, "tools")?;
    let max_iterations = parse_usize(
        scalar_field(front, "max_iterations").as_deref(),
        "max_iterations",
    )?;
    let max_wall_clock_secs = parse_u64(
        scalar_field(front, "max_wall_clock_secs").as_deref(),
        "max_wall_clock_secs",
    )?;
    let max_tool_result_bytes = parse_usize(
        scalar_field(front, "max_tool_result_bytes").as_deref(),
        "max_tool_result_bytes",
    )?;

    Ok(SubagentDef {
        name,
        description,
        body: body.trim_start().to_string(),
        model,
        tools,
        max_iterations,
        max_wall_clock_secs,
        max_tool_result_bytes,
        source_path,
    })
}

pub fn read_and_parse(agent_md_path: &Path) -> Result<SubagentDef, SubagentParseError> {
    let folder = agent_md_path
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|s| s.to_str())
        .ok_or_else(|| SubagentParseError::Io("could not derive folder name".to_string()))?
        .to_string();
    let content = std::fs::read_to_string(agent_md_path)
        .map_err(|e| SubagentParseError::Io(format!("{}: {}", agent_md_path.display(), e)))?;
    parse_agent_md(&content, &folder, agent_md_path.to_path_buf())
}

// ── shared YAML-lite helpers ────────────────────────────────────────
// Same shape as the skills/manifest.rs parser. Re-implemented (not
// shared) so each parser can evolve its own field-validation rules
// without coupling. The skills shape is name + description; this one
// also handles a flat list (`tools:`) plus a few numeric scalars.

fn split_frontmatter(s: &str) -> Result<(&str, &str), SubagentParseError> {
    let rest = s
        .strip_prefix("---\n")
        .or_else(|| s.strip_prefix("---\r\n"))
        .ok_or(SubagentParseError::MissingFrontmatter)?;
    let mut byte_offset = 0usize;
    for line in rest.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\n', '\r']);
        if trimmed == "---" {
            let front = &rest[..byte_offset];
            let body_start = byte_offset + line.len();
            let body = &rest[body_start..];
            return Ok((front, body));
        }
        byte_offset += line.len();
    }
    let trimmed = rest.trim_end_matches(['\n', '\r']);
    if let Some(front) = trimmed.strip_suffix("---") {
        let front = front.trim_end_matches(['\n', '\r']);
        return Ok((front, ""));
    }
    Err(SubagentParseError::UnterminatedFrontmatter)
}

fn scalar_field(frontmatter: &str, key: &str) -> Option<String> {
    let mut lines = frontmatter.lines().peekable();
    while let Some(line) = lines.next() {
        if line.starts_with(char::is_whitespace) {
            continue;
        }
        let Some(rest) = line.strip_prefix(key) else {
            continue;
        };
        let rest = rest.trim_start();
        if !rest.starts_with(':') {
            continue;
        }
        let after_colon = rest[1..].trim();
        if after_colon == "|" || after_colon == "|-" || after_colon == "|+" {
            let mut buf = String::new();
            let mut indent: Option<usize> = None;
            while let Some(next) = lines.peek() {
                if next.trim().is_empty() {
                    if !buf.is_empty() {
                        buf.push('\n');
                    }
                    lines.next();
                    continue;
                }
                let leading = next.chars().take_while(|c| c.is_whitespace()).count();
                let cur_indent = *indent.get_or_insert(leading);
                if leading < cur_indent.max(1) {
                    break;
                }
                let line = lines.next().unwrap();
                if !buf.is_empty() {
                    buf.push('\n');
                }
                buf.push_str(&line[cur_indent.min(line.len())..]);
            }
            return Some(buf);
        }
        let trimmed = after_colon.trim();
        let unquoted = trimmed
            .strip_prefix('"')
            .and_then(|s| s.strip_suffix('"'))
            .or_else(|| {
                trimmed
                    .strip_prefix('\'')
                    .and_then(|s| s.strip_suffix('\''))
            })
            .unwrap_or(trimmed);
        return Some(unquoted.to_string());
    }
    None
}

/// Read a YAML list. Two accepted shapes:
///
/// ```yaml
/// tools: [roost_search, roost_read]   # inline
/// tools:
///   - roost_search
///   - roost_read                       # block
/// ```
///
/// Returns `Ok(None)` if the key isn't present, `Ok(Some(items))`
/// otherwise. `Err(BadField)` if the value is malformed (e.g. a scalar
/// where a list was expected).
fn list_field(frontmatter: &str, key: &str) -> Result<Option<Vec<String>>, SubagentParseError> {
    let mut lines = frontmatter.lines().peekable();
    while let Some(line) = lines.next() {
        if line.starts_with(char::is_whitespace) {
            continue;
        }
        let Some(rest) = line.strip_prefix(key) else {
            continue;
        };
        let rest = rest.trim_start();
        if !rest.starts_with(':') {
            continue;
        }
        let after_colon = rest[1..].trim();
        // Inline flow form: `[a, b, c]`
        if after_colon.starts_with('[') {
            let inner = after_colon
                .trim_start_matches('[')
                .trim_end_matches(']')
                .trim();
            if inner.is_empty() {
                return Ok(Some(Vec::new()));
            }
            let items: Vec<String> = inner
                .split(',')
                .map(|s| {
                    s.trim()
                        .trim_matches(|c: char| c == '"' || c == '\'')
                        .to_string()
                })
                .filter(|s| !s.is_empty())
                .collect();
            return Ok(Some(items));
        }
        // Block form: subsequent indented lines starting with `-`
        if !after_colon.is_empty() {
            return Err(SubagentParseError::BadField {
                field: "tools",
                detail: format!("expected list, got scalar value `{after_colon}`"),
            });
        }
        let mut items: Vec<String> = Vec::new();
        while let Some(next) = lines.peek() {
            if next.trim().is_empty() {
                lines.next();
                continue;
            }
            if !next.starts_with(char::is_whitespace) {
                break; // de-indented — list ends
            }
            let line = lines.next().unwrap();
            let item = line.trim();
            let Some(stripped) = item.strip_prefix('-') else {
                return Err(SubagentParseError::BadField {
                    field: "tools",
                    detail: format!("expected `- item`, got `{item}`"),
                });
            };
            let v = stripped
                .trim()
                .trim_matches(|c: char| c == '"' || c == '\'')
                .to_string();
            if !v.is_empty() {
                items.push(v);
            }
        }
        return Ok(Some(items));
    }
    Ok(None)
}

fn parse_usize(s: Option<&str>, field: &'static str) -> Result<Option<usize>, SubagentParseError> {
    let Some(raw) = s.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    raw.parse::<usize>()
        .map(Some)
        .map_err(|e| SubagentParseError::BadField {
            field,
            detail: e.to_string(),
        })
}

fn parse_u64(s: Option<&str>, field: &'static str) -> Result<Option<u64>, SubagentParseError> {
    let Some(raw) = s.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    raw.parse::<u64>()
        .map(Some)
        .map_err(|e| SubagentParseError::BadField {
            field,
            detail: e.to_string(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn parses_minimal_agent() {
        let md = "---\nname: gen\ndescription: General delegation\n---\n\nYou are the general subagent.\n";
        let a = parse_agent_md(md, "gen", p("/tmp/gen/AGENT.md")).unwrap();
        assert_eq!(a.name, "gen");
        assert_eq!(a.description, "General delegation");
        assert!(a.body.contains("general subagent"));
        assert!(a.tools.is_none());
        assert!(a.model.is_none());
        assert!(a.max_iterations.is_none());
    }

    #[test]
    fn parses_full_frontmatter() {
        let md = r#"---
name: research-deep
description: |
  Multi-hop research.
  Use when the user asks open-ended questions.
model: gemini-2.5-pro
tools:
  - roost_search
  - roost_read
  - graph_query
max_iterations: 12
max_wall_clock_secs: 240
max_tool_result_bytes: 200000
---

Body here.
"#;
        let a = parse_agent_md(md, "research-deep", p("/tmp/research-deep/AGENT.md")).unwrap();
        assert_eq!(a.name, "research-deep");
        assert!(a.description.contains("Multi-hop"));
        assert!(a.description.contains("open-ended"));
        assert_eq!(a.model.as_deref(), Some("gemini-2.5-pro"));
        assert_eq!(
            a.tools.as_deref(),
            Some(
                &[
                    "roost_search".to_string(),
                    "roost_read".to_string(),
                    "graph_query".to_string()
                ][..]
            )
        );
        assert_eq!(a.max_iterations, Some(12));
        assert_eq!(a.max_wall_clock_secs, Some(240));
        assert_eq!(a.max_tool_result_bytes, Some(200_000));
        assert!(a.body.starts_with("Body here."));
    }

    #[test]
    fn parses_inline_tools_list() {
        let md = "---\nname: x\ndescription: d\ntools: [a, b, c]\n---\nbody\n";
        let a = parse_agent_md(md, "x", p("/tmp/x/AGENT.md")).unwrap();
        assert_eq!(
            a.tools.as_deref(),
            Some(&["a".to_string(), "b".to_string(), "c".to_string()][..])
        );
    }

    #[test]
    fn rejects_scalar_where_list_expected() {
        let md = "---\nname: x\ndescription: d\ntools: oops\n---\nbody\n";
        let err = parse_agent_md(md, "x", p("/tmp/x/AGENT.md")).unwrap_err();
        assert!(matches!(
            err,
            SubagentParseError::BadField { field: "tools", .. }
        ));
    }

    #[test]
    fn rejects_bad_numeric_field() {
        let md = "---\nname: x\ndescription: d\nmax_iterations: oops\n---\nbody\n";
        let err = parse_agent_md(md, "x", p("/tmp/x/AGENT.md")).unwrap_err();
        assert!(matches!(
            err,
            SubagentParseError::BadField {
                field: "max_iterations",
                ..
            }
        ));
    }

    #[test]
    fn rejects_missing_required_fields() {
        let no_name = "---\ndescription: d\n---\nbody\n";
        assert!(matches!(
            parse_agent_md(no_name, "x", p("/tmp/x/AGENT.md")).unwrap_err(),
            SubagentParseError::MissingField("name")
        ));

        let no_desc = "---\nname: x\n---\nbody\n";
        assert!(matches!(
            parse_agent_md(no_desc, "x", p("/tmp/x/AGENT.md")).unwrap_err(),
            SubagentParseError::MissingField("description")
        ));
    }

    #[test]
    fn rejects_name_folder_mismatch() {
        let md = "---\nname: alpha\ndescription: d\n---\nbody\n";
        let err = parse_agent_md(md, "beta", p("/tmp/beta/AGENT.md")).unwrap_err();
        assert!(matches!(err, SubagentParseError::NameFolderMismatch { .. }));
    }

    #[test]
    fn budget_inherits_when_unset() {
        let base = AgentBudget::default();
        let def = SubagentDef {
            name: "x".into(),
            description: "d".into(),
            body: String::new(),
            model: None,
            tools: None,
            max_iterations: None,
            max_wall_clock_secs: None,
            max_tool_result_bytes: None,
            source_path: p("/tmp"),
        };
        let b = def.budget_or(&base);
        assert_eq!(b.max_iterations, base.max_iterations);
        assert_eq!(b.max_tool_result_bytes, base.max_tool_result_bytes);
        assert_eq!(b.max_wall_clock_secs, base.max_wall_clock_secs);
        // Always inherits — never overrideable from frontmatter.
        assert_eq!(b.max_tool_calls_per_iter, base.max_tool_calls_per_iter);
    }

    #[test]
    fn budget_overrides_only_set_fields() {
        let base = AgentBudget {
            max_iterations: 8,
            max_tool_result_bytes: 100_000,
            max_wall_clock_secs: 300,
            max_tool_calls_per_iter: 8,
        };
        let def = SubagentDef {
            name: "x".into(),
            description: "d".into(),
            body: String::new(),
            model: None,
            tools: None,
            max_iterations: Some(12),
            max_wall_clock_secs: None,
            max_tool_result_bytes: Some(200_000),
            source_path: p("/tmp"),
        };
        let b = def.budget_or(&base);
        assert_eq!(b.max_iterations, 12);
        assert_eq!(b.max_tool_result_bytes, 200_000);
        // Untouched fields stay at parent's value.
        assert_eq!(b.max_wall_clock_secs, 300);
        assert_eq!(b.max_tool_calls_per_iter, 8);
    }

    #[test]
    fn rejects_bad_name_chars() {
        let md = "---\nname: Bad_Name\ndescription: d\n---\nbody\n";
        let err = parse_agent_md(md, "Bad_Name", p("/tmp/Bad_Name/AGENT.md")).unwrap_err();
        assert!(matches!(err, SubagentParseError::InvalidName(_)));
    }

    #[test]
    fn ignores_unknown_top_level_keys() {
        let md = "---\nname: x\ndescription: d\nlicense: MIT\nauthor: bob\n---\nbody\n";
        let a = parse_agent_md(md, "x", p("/tmp/x/AGENT.md")).unwrap();
        assert_eq!(a.name, "x");
        assert_eq!(a.description, "d");
    }
}
