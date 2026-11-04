use serde::Serialize;

#[derive(Debug, Serialize, Clone, specta::Type)]
pub struct InlineTag {
    pub tag: String,
    #[specta(type = u32)]
    pub start: usize,
    #[specta(type = u32)]
    pub end: usize,
}

/// Extract all tags from markdown content.
///
/// Two sources are merged:
///  1. YAML frontmatter `tags:` field — supports both inline array
///     (`tags: [a, b/c]`) and block list (`tags:\n  - a\n  - b/c`).
///     This is how Obsidian surfaces metadata tags; skipping it was
///     the reason notes like `Ideas.md` looked thinly connected in our
///     graph vs Obsidian's.
///  2. Inline `#tags` in the body, with the usual exclusions: headings,
///     fenced code blocks, inline code spans, purely-numeric tags.
///
/// Offsets returned are byte offsets into the original content string.
#[tauri::command]
#[specta::specta]
pub fn parse_tags(content: String) -> Vec<InlineTag> {
    let mut tags = Vec::new();
    let mut in_code_block = false;

    // Parse YAML frontmatter tags first (if present) and record the byte
    // range of the frontmatter so the inline pass can skip it.
    let (frontmatter_end, frontmatter_tags) = parse_frontmatter_tags(&content);
    tags.extend(frontmatter_tags);

    for line in content.lines() {
        let this_line_start = line.as_ptr() as usize - content.as_ptr() as usize;

        // Skip frontmatter region entirely in the inline pass — we already
        // pulled its tags above and don't want to misparse YAML as body.
        if this_line_start < frontmatter_end {
            continue;
        }

        let trimmed = line.trim();

        // Toggle fenced code blocks
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_code_block = !in_code_block;
            continue;
        }
        if in_code_block {
            continue;
        }

        // Skip headings (lines starting with #)
        if trimmed.starts_with('#') && trimmed.chars().nth(1).is_none_or(|c| c == ' ' || c == '#') {
            continue;
        }

        let line_bytes = line.as_bytes();
        let offset = this_line_start;

        let mut i = 0;
        let mut in_inline_code = false;

        while i < line_bytes.len() {
            // Track inline code spans
            if line_bytes[i] == b'`' {
                in_inline_code = !in_inline_code;
                i += 1;
                continue;
            }
            if in_inline_code {
                i += 1;
                continue;
            }

            if line_bytes[i] == b'#' {
                // Must be preceded by whitespace or start of line
                let preceded_ok = i == 0 || line_bytes[i - 1].is_ascii_whitespace();
                if !preceded_ok {
                    i += 1;
                    continue;
                }

                let tag_start = i;
                i += 1; // skip the #

                // Collect tag chars: alphanumeric, -, _, /
                let mut tag_text = String::new();
                while i < line_bytes.len() {
                    let c = line_bytes[i];
                    if c.is_ascii_alphanumeric() || c == b'-' || c == b'_' || c == b'/' {
                        tag_text.push(c as char);
                        i += 1;
                    } else {
                        break;
                    }
                }

                // Obsidian rule: tags must contain at least one non-numeric
                // character. Without this check, `#1`, `#5`, `#2024` all become
                // tags and pollute the graph — users commonly write `#1`, `#2`
                // as list markers or issue references, not intended tags.
                if !tag_text.is_empty() && tag_text.chars().any(|c| !c.is_ascii_digit()) {
                    tags.push(InlineTag {
                        tag: tag_text,
                        start: offset + tag_start,
                        end: offset + i,
                    });
                }
            } else {
                i += 1;
            }
        }
    }

    tags
}

/// The note's frontmatter `tags:` values only (no inline body `#tags`), in
/// document order and normalized exactly as the indexer normalizes them.
/// Used by `edit_note`'s tag-vocabulary guard, which — like `roost_write` —
/// governs structured frontmatter tags, not prose hashes.
pub fn frontmatter_tag_names(content: &str) -> Vec<String> {
    parse_frontmatter_tags(content)
        .1
        .into_iter()
        .map(|t| t.tag)
        .collect()
}

/// Parse the `tags:` field from YAML frontmatter.
/// Returns (byte offset where frontmatter ends, list of tags found).
/// If no frontmatter exists, returns (0, []).
///
/// Supports both forms Obsidian accepts:
///  - Inline flow: `tags: [a, b/c, d]`
///  - Block list: `tags:\n  - a\n  - b/c\n  - d`
///  - Single string: `tags: a` or `tag: a`
///  - Comma-separated: `tags: a, b/c, d`
///
/// We don't pull in a full YAML parser — the pattern space is narrow
/// enough that a hand-rolled scanner is simpler, faster, and predictable.
fn parse_frontmatter_tags(content: &str) -> (usize, Vec<InlineTag>) {
    // Frontmatter must start on the very first line with `---`.
    if !content.starts_with("---\n") && !content.starts_with("---\r\n") {
        return (0, Vec::new());
    }

    // Find the closing `---` line. Must be on its own line.
    let after_open = if content.starts_with("---\r\n") { 5 } else { 4 };
    let rest = &content[after_open..];
    let close_rel = find_frontmatter_close(rest);
    let Some((close_line_start, close_line_end)) = close_rel else {
        return (0, Vec::new());
    };
    let frontmatter_body = &rest[..close_line_start];
    let frontmatter_end = after_open + close_line_end;

    let mut tags = Vec::new();
    let mut lines = frontmatter_body.lines().peekable();

    while let Some(line) = lines.next() {
        // Match `tags:` or `tag:` at column 0 (frontmatter keys aren't indented).
        let key_match = line
            .strip_prefix("tags:")
            .or_else(|| line.strip_prefix("tag:"));
        let Some(value) = key_match else {
            continue;
        };
        let value = value.trim();

        if value.is_empty() {
            // Block list form — collect subsequent `  - item` lines.
            while let Some(next) = lines.peek() {
                let t = next.trim_start();
                if let Some(item) = t.strip_prefix("- ") {
                    push_tag(&mut tags, item.trim());
                    lines.next();
                } else if t.is_empty() {
                    lines.next();
                } else {
                    break;
                }
            }
        } else if let Some(inner) = value.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            // Inline flow: `tags: [a, b, c]`
            for raw in inner.split(',') {
                push_tag(&mut tags, raw.trim());
            }
        } else {
            // Single value or comma-separated scalar.
            for raw in value.split(',') {
                push_tag(&mut tags, raw.trim());
            }
        }
    }

    (frontmatter_end, tags)
}

/// Find the `---` line that closes a frontmatter block. Returns
/// (byte offset of the closing line, byte offset just past its newline).
fn find_frontmatter_close(body: &str) -> Option<(usize, usize)> {
    let mut line_start = 0;
    for line in body.split_inclusive('\n') {
        let trimmed_end = line.trim_end_matches(['\r', '\n']);
        if trimmed_end == "---" || trimmed_end == "..." {
            return Some((line_start, line_start + line.len()));
        }
        line_start += line.len();
    }
    None
}

/// Strip quotes, validate, and append a frontmatter tag.
/// Applies the same Obsidian rules as inline tags:
///  - must contain at least one non-digit character
///  - chars restricted to alphanumeric, `-`, `_`, `/`
fn push_tag(out: &mut Vec<InlineTag>, raw: &str) {
    let stripped = raw.trim().trim_matches(['"', '\'']).trim_start_matches('#'); // tolerate `- #tag` in YAML
    if stripped.is_empty() {
        return;
    }
    // Reject if any disallowed char.
    if !stripped
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '/')
    {
        return;
    }
    // Reject purely-numeric tags (matches Obsidian rule).
    if !stripped.chars().any(|c| !c.is_ascii_digit()) {
        return;
    }
    // Frontmatter tags have no meaningful inline offset — report 0..0 so
    // downstream consumers that expect offsets still get a valid range.
    out.push(InlineTag {
        tag: stripped.to_string(),
        start: 0,
        end: 0,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_simple_tag() {
        let tags = parse_tags("Hello #world there".to_string());
        assert_eq!(tags.len(), 1);
        assert_eq!(tags[0].tag, "world");
    }

    #[test]
    fn test_multiple_tags() {
        let tags = parse_tags("A #first and #second tag".to_string());
        assert_eq!(tags.len(), 2);
        assert_eq!(tags[0].tag, "first");
        assert_eq!(tags[1].tag, "second");
    }

    #[test]
    fn test_heading_not_tag() {
        let tags = parse_tags("# Heading\n\nSome #real-tag".to_string());
        assert_eq!(tags.len(), 1);
        assert_eq!(tags[0].tag, "real-tag");
    }

    #[test]
    fn test_code_block_ignored() {
        let tags = parse_tags("```\n#not-a-tag\n```\n\n#real-tag".to_string());
        assert_eq!(tags.len(), 1);
        assert_eq!(tags[0].tag, "real-tag");
    }

    #[test]
    fn test_inline_code_ignored() {
        let tags = parse_tags("Use `#not-tag` but #real-tag".to_string());
        assert_eq!(tags.len(), 1);
        assert_eq!(tags[0].tag, "real-tag");
    }

    #[test]
    fn test_tag_with_slash() {
        let tags = parse_tags("#project/corvux is good".to_string());
        assert_eq!(tags.len(), 1);
        assert_eq!(tags[0].tag, "project/corvux");
    }

    #[test]
    fn test_hash_in_middle_of_word_ignored() {
        let tags = parse_tags("email@#notag or foo#bar".to_string());
        assert_eq!(tags.len(), 0);
    }

    #[test]
    fn test_numeric_only_tags_ignored() {
        // Matches Obsidian: #1, #5, #2024 are not tags. Users write these as
        // list markers or issue refs — treating them as tags pollutes the graph.
        let tags = parse_tags("See #1 and #5 and #2024 but #v2 is real".to_string());
        assert_eq!(tags.len(), 1);
        assert_eq!(tags[0].tag, "v2");
    }

    #[test]
    fn test_frontmatter_inline_array() {
        let content = "---\ntype: brainstorm\ntags: [domain/work, project/corvux, topic/ml]\n---\n\nbody here";
        let tags = parse_tags(content.to_string());
        let names: Vec<String> = tags.iter().map(|t| t.tag.clone()).collect();
        assert!(names.contains(&"domain/work".to_string()));
        assert!(names.contains(&"project/corvux".to_string()));
        assert!(names.contains(&"topic/ml".to_string()));
        assert_eq!(tags.len(), 3);
    }

    #[test]
    fn test_frontmatter_block_list() {
        let content = "---\ntags:\n  - domain/work\n  - topic/sync\n  - ml\n---\n\nbody";
        let tags = parse_tags(content.to_string());
        let names: Vec<String> = tags.iter().map(|t| t.tag.clone()).collect();
        assert_eq!(names, vec!["domain/work", "topic/sync", "ml"]);
    }

    #[test]
    fn test_frontmatter_does_not_capture_body_hashes_as_headings() {
        // Regression: frontmatter YAML looks like headings (`# Title`) if parser
        // doesn't skip the frontmatter region. Ensure we only keep tags: content.
        let content = "---\ntitle: Ideas\ntags: [a/b]\n---\n\n# Real Heading\n\nInline #hello-tag.";
        let tags = parse_tags(content.to_string());
        let names: Vec<String> = tags.iter().map(|t| t.tag.clone()).collect();
        assert_eq!(names, vec!["a/b", "hello-tag"]);
    }

    #[test]
    fn test_frontmatter_tag_singular_key() {
        // Obsidian also accepts `tag:` (singular) alongside `tags:`.
        let content = "---\ntag: single/value\n---\nbody";
        let tags = parse_tags(content.to_string());
        assert_eq!(tags.len(), 1);
        assert_eq!(tags[0].tag, "single/value");
    }

    #[test]
    fn test_frontmatter_rejects_numeric_tag() {
        let content = "---\ntags: [1, 2024, real-one]\n---\nbody";
        let tags = parse_tags(content.to_string());
        let names: Vec<String> = tags.iter().map(|t| t.tag.clone()).collect();
        assert_eq!(names, vec!["real-one"]);
    }

    #[test]
    fn test_no_frontmatter_still_parses_body() {
        let tags = parse_tags("no frontmatter\n\nbut #a-body-tag".to_string());
        assert_eq!(tags.len(), 1);
        assert_eq!(tags[0].tag, "a-body-tag");
    }
}
