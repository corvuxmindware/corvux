//! `SKILL.md` parser. Skills are on-demand markdown instruction files
//! the agent can load via the `skill_load` tool. The frontmatter has a
//! deliberately tiny required surface — `name` + `description` — so the
//! rest of the YAML stays free-form for users + future expansion
//! without forcing us to ship a full YAML deserializer.
//!
//! ## Format
//!
//! ```markdown
//! ---
//! name: fitness-summary
//! description: |
//!   Summarize a user's fitness notes for a date window. Use when the
//!   user asks "how did I do this week" or similar.
//! license: MIT
//! ---
//!
//! # Fitness summary
//! ...instructions...
//! ```
//!
//! Required: `name` (must match folder name, kebab-case, 1–64 chars),
//! `description` (free text, supports `|` block scalar).
//!
//! Everything else in the frontmatter is ignored by the parser today —
//! users keep their copy-pasted Claude-Code frontmatter intact, we just
//! don't act on extra fields. If/when an extra field earns a behavior
//! (e.g. `preferred-tools` for auto-selection), it gets parsed here.

use std::path::{Path, PathBuf};

use thiserror::Error;

/// Parsed SKILL.md ready for `skill_list` / `skill_load`.
#[derive(Debug, Clone)]
pub struct SkillManifest {
    pub name: String,
    pub description: String,
    /// Markdown body with frontmatter stripped. Returned by
    /// `skill_load`; the agent reads it as instructions.
    pub body: String,
    /// Absolute path to the SKILL.md on disk. Kept for diagnostic
    /// surfaces (parse-failure logs, future "open skill source" UI) —
    /// the registry indexes by `name`, not path.
    #[allow(dead_code)]
    pub source_path: PathBuf,
}

#[derive(Debug, Error)]
pub enum SkillParseError {
    #[error("missing frontmatter (skill must begin with `---`)")]
    MissingFrontmatter,
    #[error("unterminated frontmatter (no closing `---` line)")]
    UnterminatedFrontmatter,
    #[error("missing required field `{0}`")]
    MissingField(&'static str),
    #[error("invalid skill name `{0}` (must be lowercase a-z, 0-9, hyphen; 1-64 chars)")]
    InvalidName(String),
    #[error("skill name `{name}` does not match folder name `{folder}`")]
    NameFolderMismatch { name: String, folder: String },
    #[error("io error reading skill `{0}`")]
    Io(String),
}

/// Validate the `name` field. Mirrors the constraint shared with
/// Claude Code's skill loader so the same files work on both. We're
/// strict here because the name appears in the prompt blurb verbatim
/// — odd characters would burn tokens and risk injecting markup.
pub fn is_valid_skill_name(s: &str) -> bool {
    let len = s.len();
    if !(1..=64).contains(&len) {
        return false;
    }
    s.chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// Parse SKILL.md content. `folder_name` is the parent folder's name —
/// used to enforce the convention that the `name:` frontmatter field
/// matches the directory it lives in (so `skill_list` and the folder
/// layout never disagree).
pub fn parse_skill_md(
    content: &str,
    folder_name: &str,
    source_path: PathBuf,
) -> Result<SkillManifest, SkillParseError> {
    let (front, body) = split_frontmatter(content)?;

    let name = scalar_field(front, "name")
        .ok_or(SkillParseError::MissingField("name"))?
        .trim()
        .to_string();
    if !is_valid_skill_name(&name) {
        return Err(SkillParseError::InvalidName(name));
    }
    if name != folder_name {
        return Err(SkillParseError::NameFolderMismatch {
            name,
            folder: folder_name.to_string(),
        });
    }

    let description = scalar_field(front, "description")
        .ok_or(SkillParseError::MissingField("description"))?
        .trim()
        .to_string();
    if description.is_empty() {
        return Err(SkillParseError::MissingField("description"));
    }

    Ok(SkillManifest {
        name,
        description,
        body: body.trim_start().to_string(),
        source_path,
    })
}

/// Split `---\n<frontmatter>\n---\n<body>` into `(frontmatter, body)`.
fn split_frontmatter(s: &str) -> Result<(&str, &str), SkillParseError> {
    let rest = s
        .strip_prefix("---\n")
        .or_else(|| s.strip_prefix("---\r\n"))
        .ok_or(SkillParseError::MissingFrontmatter)?;
    // Find closing `---` on its own line. Two valid forms: `---\n` or
    // `---\r\n` or end-of-file. Search line-by-line so we don't get
    // tripped up by a `---` mid-body.
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
    // EOF without closing — also accept if the last line is exactly `---`
    let trimmed = rest.trim_end_matches(['\n', '\r']);
    if let Some(front) = trimmed.strip_suffix("---") {
        let front = front.trim_end_matches(['\n', '\r']);
        return Ok((front, ""));
    }
    Err(SkillParseError::UnterminatedFrontmatter)
}

/// Read a scalar field from the frontmatter. Supports two shapes:
///
/// ```yaml
/// description: one-liner
/// description: |
///   multi-line
///   block scalar
/// ```
///
/// Returns `None` if the field doesn't appear at top level. Indented
/// child keys (under `metadata:` etc.) are ignored — only the
/// top-level key is matched, which is what we want for `name` and
/// `description`.
fn scalar_field(frontmatter: &str, key: &str) -> Option<String> {
    let mut lines = frontmatter.lines().peekable();
    while let Some(line) = lines.next() {
        // Top-level keys live at column 0. Skip indented lines so a
        // nested `description:` under `metadata:` doesn't shadow the
        // real one.
        if line.starts_with(char::is_whitespace) {
            continue;
        }
        let Some(rest) = line.strip_prefix(key) else {
            continue;
        };
        let rest = rest.trim_start();
        if !rest.starts_with(':') {
            // Same prefix but different key (e.g. `name_alt:`).
            continue;
        }
        let after_colon = rest[1..].trim();
        // Block scalar `|` — gather subsequent indented lines.
        if after_colon == "|" || after_colon == "|-" || after_colon == "|+" {
            let mut buf = String::new();
            let mut indent: Option<usize> = None;
            while let Some(next) = lines.peek() {
                if next.trim().is_empty() {
                    // Preserve internal blank lines once the block has
                    // started; skip leading blanks.
                    if !buf.is_empty() {
                        buf.push('\n');
                    }
                    lines.next();
                    continue;
                }
                let leading = next.chars().take_while(|c| c.is_whitespace()).count();
                let cur_indent = *indent.get_or_insert(leading);
                if leading < cur_indent.max(1) {
                    break; // de-indented — block scalar ends here
                }
                let line = lines.next().unwrap();
                if !buf.is_empty() {
                    buf.push('\n');
                }
                buf.push_str(&line[cur_indent.min(line.len())..]);
            }
            return Some(buf);
        }
        // Strip surrounding quotes if present (`"value"` or `'value'`).
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

/// Convenience: read SKILL.md from disk and parse. Used by
/// `SkillRegistry::refresh`.
pub fn read_and_parse(skill_md_path: &Path) -> Result<SkillManifest, SkillParseError> {
    let folder = skill_md_path
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|s| s.to_str())
        .ok_or_else(|| SkillParseError::Io("could not derive folder name".to_string()))?
        .to_string();
    let content = std::fs::read_to_string(skill_md_path)
        .map_err(|e| SkillParseError::Io(format!("{}: {}", skill_md_path.display(), e)))?;
    parse_skill_md(&content, &folder, skill_md_path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn parses_minimal_skill() {
        let md = "---\nname: fit\ndescription: Summary skill.\n---\n\n# Body\n\nDo the thing.\n";
        let m = parse_skill_md(md, "fit", p("/tmp/fit/SKILL.md")).unwrap();
        assert_eq!(m.name, "fit");
        assert_eq!(m.description, "Summary skill.");
        assert!(m.body.starts_with("# Body"));
    }

    #[test]
    fn parses_block_scalar_description() {
        let md = "---\nname: fit\ndescription: |\n  Line one.\n  Line two.\n---\n\nBody\n";
        let m = parse_skill_md(md, "fit", p("/tmp/fit/SKILL.md")).unwrap();
        assert_eq!(m.description, "Line one.\nLine two.");
    }

    #[test]
    fn rejects_missing_name() {
        let md = "---\ndescription: x\n---\n";
        let err = parse_skill_md(md, "fit", p("/tmp/fit/SKILL.md")).unwrap_err();
        assert!(matches!(err, SkillParseError::MissingField("name")));
    }

    #[test]
    fn rejects_missing_description() {
        let md = "---\nname: fit\n---\n";
        let err = parse_skill_md(md, "fit", p("/tmp/fit/SKILL.md")).unwrap_err();
        assert!(matches!(err, SkillParseError::MissingField("description")));
    }

    #[test]
    fn rejects_bad_name_uppercase() {
        let md = "---\nname: Fit\ndescription: x\n---\n";
        let err = parse_skill_md(md, "Fit", p("/tmp/Fit/SKILL.md")).unwrap_err();
        assert!(matches!(err, SkillParseError::InvalidName(_)));
    }

    #[test]
    fn rejects_name_folder_mismatch() {
        let md = "---\nname: fit\ndescription: x\n---\n";
        let err = parse_skill_md(md, "fitness", p("/tmp/fitness/SKILL.md")).unwrap_err();
        assert!(matches!(err, SkillParseError::NameFolderMismatch { .. }));
    }

    #[test]
    fn rejects_missing_frontmatter() {
        let md = "# No frontmatter here\n";
        let err = parse_skill_md(md, "fit", p("/tmp/fit/SKILL.md")).unwrap_err();
        assert!(matches!(err, SkillParseError::MissingFrontmatter));
    }

    #[test]
    fn rejects_unterminated_frontmatter() {
        let md = "---\nname: fit\ndescription: x\n# body but no close\n";
        let err = parse_skill_md(md, "fit", p("/tmp/fit/SKILL.md")).unwrap_err();
        assert!(matches!(err, SkillParseError::UnterminatedFrontmatter));
    }

    #[test]
    fn ignores_unknown_top_level_keys() {
        let md = "---\nname: fit\ndescription: x\nlicense: MIT\ncompatibility: claude-code, corvux\n---\nbody\n";
        let m = parse_skill_md(md, "fit", p("/tmp/fit/SKILL.md")).unwrap();
        assert_eq!(m.name, "fit");
        assert_eq!(m.description, "x");
        assert_eq!(m.body, "body\n");
    }

    #[test]
    fn ignores_nested_description_under_metadata() {
        // Nested `description:` under metadata: must NOT shadow the
        // top-level one. The top-level field stays "real".
        let md = "---\nname: fit\ndescription: real\nmetadata:\n  description: nested\n---\nbody\n";
        let m = parse_skill_md(md, "fit", p("/tmp/fit/SKILL.md")).unwrap();
        assert_eq!(m.description, "real");
    }

    #[test]
    fn strips_quotes_from_scalar_values() {
        let md = "---\nname: fit\ndescription: \"quoted text\"\n---\nbody\n";
        let m = parse_skill_md(md, "fit", p("/tmp/fit/SKILL.md")).unwrap();
        assert_eq!(m.description, "quoted text");
    }

    #[test]
    fn body_excludes_frontmatter_block() {
        let md = "---\nname: fit\ndescription: x\n---\n\n# Heading\nbody line\n";
        let m = parse_skill_md(md, "fit", p("/tmp/fit/SKILL.md")).unwrap();
        assert!(m.body.starts_with("# Heading"));
        assert!(!m.body.contains("name:"));
    }

    #[test]
    fn windows_line_endings_supported() {
        let md = "---\r\nname: fit\r\ndescription: hi\r\n---\r\nbody\r\n";
        let m = parse_skill_md(md, "fit", p("/tmp/fit/SKILL.md")).unwrap();
        assert_eq!(m.name, "fit");
        assert_eq!(m.description, "hi");
    }

    #[test]
    fn long_name_rejected() {
        let name = "a".repeat(65);
        let md = format!("---\nname: {name}\ndescription: x\n---\nbody\n");
        let err = parse_skill_md(&md, &name, p("/tmp/x/SKILL.md")).unwrap_err();
        assert!(matches!(err, SkillParseError::InvalidName(_)));
    }
}
