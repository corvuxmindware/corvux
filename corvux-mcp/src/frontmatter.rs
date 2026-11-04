//! Lightweight YAML-frontmatter handling for Corvux notes.
//!
//! Corvux's "Properties" are the YAML block at the top of a note. This
//! module reads them as structured data, filters notes by them, and edits
//! them surgically — WITHOUT a full YAML parser and without reformatting
//! the rest of the note (the body is preserved verbatim, byte-for-byte).
//!
//! It covers the shapes Corvux actually writes:
//!   key: scalar
//!   key: [a, b]          (flow list)
//!   key:                 (block list)
//!     - a
//!     - b

/// A parsed property value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PropValue {
    Scalar(String),
    List(Vec<String>),
}

/// Split a note into `(frontmatter_inner, body)`. `frontmatter_inner` is the
/// verbatim text BETWEEN the opening and closing `---` fences (or `None` if
/// the note has no well-formed frontmatter). `body` is everything after the
/// closing fence, byte-for-byte.
pub fn split(content: &str) -> (Option<String>, String) {
    let first_nl = match content.find('\n') {
        Some(i) => i,
        None => return (None, content.to_string()),
    };
    if content[..first_nl].trim() != "---" {
        return (None, content.to_string());
    }
    let after_open = first_nl + 1;
    let mut offset = after_open;
    for line in content[after_open..].split_inclusive('\n') {
        let t = line.trim_end_matches(['\n', '\r']).trim();
        if t == "---" || t == "..." {
            let fm_inner = content[after_open..offset].to_string();
            let body = content[offset + line.len()..].to_string();
            return (Some(fm_inner), body);
        }
        offset += line.len();
    }
    (None, content.to_string()) // no closing fence → not frontmatter
}

/// Parse frontmatter inner text into ordered `(key, value)` pairs.
pub fn parse(fm_inner: &str) -> Vec<(String, PropValue)> {
    let lines: Vec<&str> = fm_inner.lines().collect();
    let mut props: Vec<(String, PropValue)> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        // top-level keys are not indented
        if line.starts_with([' ', '\t']) || line.trim().is_empty() {
            i += 1;
            continue;
        }
        let Some(colon) = line.find(':') else {
            i += 1;
            continue;
        };
        let key = line[..colon].trim().to_string();
        let val = line[colon + 1..].trim();
        if val.is_empty() {
            // possible block list
            let mut items = Vec::new();
            let mut j = i + 1;
            while j < lines.len() {
                let l = lines[j];
                let t = l.trim_start();
                if l.starts_with([' ', '\t']) && t.starts_with("- ") {
                    items.push(unquote(t[2..].trim()));
                    j += 1;
                } else {
                    break;
                }
            }
            if items.is_empty() {
                props.push((key, PropValue::Scalar(String::new())));
                i += 1;
            } else {
                props.push((key, PropValue::List(items)));
                i = j;
            }
        } else if val.starts_with('[') && val.ends_with(']') {
            let inner = &val[1..val.len() - 1];
            let items = inner
                .split(',')
                .map(|s| unquote(s.trim()))
                .filter(|s| !s.is_empty())
                .collect();
            props.push((key, PropValue::List(items)));
            i += 1;
        } else {
            props.push((key, PropValue::Scalar(unquote(val))));
            i += 1;
        }
    }
    props
}

/// Human-readable one-line-per-property summary for `read_note`.
pub fn render(props: &[(String, PropValue)]) -> String {
    let mut s = String::new();
    for (k, v) in props {
        match v {
            PropValue::Scalar(val) => s.push_str(&format!("- {k}: {val}\n")),
            PropValue::List(items) => s.push_str(&format!("- {k}: {}\n", items.join(", "))),
        }
    }
    s
}

/// Does this note match `key = value`? Case-insensitive. For `key` =
/// "tag"/"tags" it checks both the frontmatter `tags` AND inline `#tags` in
/// the body (Corvux treats both as tags).
pub fn matches(content: &str, key: &str, value: &str) -> bool {
    let key_l = key.to_lowercase();
    let val_l = value.trim().trim_start_matches('#').to_lowercase();
    let (fm, body) = split(content);

    if key_l == "tag" || key_l == "tags" {
        if let Some(fm_inner) = &fm {
            for (k, v) in parse(fm_inner) {
                if k.eq_ignore_ascii_case("tags") && value_contains(&v, &val_l) {
                    return true;
                }
            }
        }
        return body_has_inline_tag(&body, &val_l);
    }

    if let Some(fm_inner) = fm {
        for (k, v) in parse(&fm_inner) {
            if k.to_lowercase() == key_l {
                return value_contains(&v, &val_l);
            }
        }
    }
    false
}

/// Upsert a scalar property `key: value`, preserving the rest of the note.
/// Creates the frontmatter block if the note has none. If the key already
/// exists (scalar or list), it's replaced by the new scalar. Errors if `key`
/// or `value` contains a newline/carriage-return (would fabricate a new
/// frontmatter line) or `key` contains a colon (would shift the `key:value`
/// boundary this module's own first-colon-wins parser uses).
pub fn set_property(content: &str, key: &str, value: &str) -> Result<String, String> {
    reject_structural_chars(key, "key", true)?;
    reject_structural_chars(value, "value", false)?;
    let (fm, body) = split(content);
    let new_line = format!("{key}: {}", yaml_scalar(value));
    Ok(match fm {
        Some(fm_inner) => {
            let mut out = remove_key(&fm_inner, key);
            out.push(new_line);
            reassemble(&out, &body)
        }
        None => format!("---\n{new_line}\n---\n\n{content}"),
    })
}

/// Merge `new_tags` into the note's `tags` (dedup, case-insensitive),
/// writing them back as a block list. Strips a leading `#`. Creates
/// frontmatter if missing. Errors if any tag contains a newline/carriage-
/// return or colon — see `set_property` for why.
pub fn add_tags(content: &str, new_tags: &[String]) -> Result<String, String> {
    let clean: Vec<String> = new_tags
        .iter()
        .map(|t| t.trim().trim_start_matches('#').to_string())
        .filter(|t| !t.is_empty())
        .collect();
    for t in &clean {
        reject_structural_chars(t, "tag", true)?;
    }
    if clean.is_empty() {
        return Ok(content.to_string());
    }
    let (fm, body) = split(content);
    let (existing, base_lines) = match &fm {
        Some(fm_inner) => {
            let existing = parse(fm_inner)
                .into_iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("tags"))
                .map(|(_, v)| match v {
                    PropValue::List(items) => items,
                    PropValue::Scalar(s) if !s.is_empty() => vec![s],
                    _ => vec![],
                })
                .unwrap_or_default();
            (existing, remove_key(fm_inner, "tags"))
        }
        None => (Vec::new(), Vec::new()),
    };

    let mut merged = existing;
    for t in &clean {
        if !merged.iter().any(|e| e.eq_ignore_ascii_case(t)) {
            merged.push(t.clone());
        }
    }

    let mut out = base_lines;
    out.push("tags:".to_string());
    for t in &merged {
        out.push(format!("  - {}", yaml_scalar(t)));
    }
    Ok(reassemble(&out, &body))
}

/// Render `s` as a YAML scalar safe to splice into a `key: value` line or a
/// `  - value` list item without corrupting frontmatter structure or being
/// silently reinterpreted as a non-string type on re-parse. Quotes+escapes
/// when needed; otherwise emits `s` bare (keeps today's plain look for
/// ordinary values). Not a full YAML serializer — this module only ever
/// emits the flow-scalar subset it also parses (see the module doc), so this
/// only needs to cover that subset safely. Callers must reject embedded
/// newlines before calling this (`reject_structural_chars`) — unlike a real
/// YAML engine, this module's parser has no multi-line scalar story, so a
/// newline can only ever mean "fabricate a new line of structure," never
/// "continue this value."
pub fn yaml_scalar(s: &str) -> String {
    if needs_quoting(s) {
        quote_and_escape(s)
    } else {
        s.to_string()
    }
}

/// Reject a key/tag/value containing a newline or carriage return (either
/// of which would let the caller fabricate a new frontmatter line), and —
/// when `reject_colon` is set (keys and tags, not values) — a colon (which
/// would shift the `key:value` boundary `parse`'s first-colon-wins scan
/// uses). Returns a descriptive error naming `what`. Also used by `server.rs`
/// for non-YAML structural splices (a Markdown `title`/`heading`) that share
/// the same "embedded newline fabricates new structure" hazard.
pub fn reject_structural_chars(s: &str, what: &str, reject_colon: bool) -> Result<(), String> {
    if s.chars().any(char::is_control) {
        return Err(format!("{what} cannot contain control characters: {s:?}"));
    }
    if reject_colon && s.contains(':') {
        return Err(format!("{what} cannot contain a colon: {s:?}"));
    }
    Ok(())
}

fn needs_quoting(s: &str) -> bool {
    if s.is_empty() || s.trim() != s {
        return true; // empty, or leading/trailing whitespace
    }
    if s.contains(": ") || s.ends_with(':') {
        return true;
    }
    if s.starts_with("- ") || s == "-" || s.starts_with('#') || s.contains(" #") {
        return true;
    }
    if matches!(
        s.chars().next(),
        Some(
            '[' | ']'
                | '{'
                | '}'
                | ','
                | '&'
                | '*'
                | '!'
                | '|'
                | '>'
                | '%'
                | '@'
                | '`'
                | '"'
                | '\''
                | '?'
                | ':'
        )
    ) {
        return true;
    }
    let lower = s.to_ascii_lowercase();
    if matches!(
        lower.as_str(),
        "true" | "false" | "null" | "yes" | "no" | "~"
    ) {
        return true; // would be reinterpreted as a non-string scalar
    }
    if s.parse::<f64>().is_ok() {
        return true; // bare numeric-looking string
    }
    false
}

fn quote_and_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Every tag in a note: frontmatter `tags`/`tag` plus inline `#tags` in the
/// body — the same two sources Corvux's indexer turns into graph tags. Tags
/// keep their original casing; de-duplication is the caller's job. Powers the
/// `list_tags` vocabulary tool and the write-path canonicalizer so an LLM can
/// reuse existing tags instead of forking near-duplicates (#ml vs #ML).
pub fn all_tags(content: &str) -> Vec<String> {
    let (fm, body) = split(content);
    let mut out = Vec::new();
    if let Some(fm_inner) = &fm {
        for (k, v) in parse(fm_inner) {
            if k.eq_ignore_ascii_case("tags") || k.eq_ignore_ascii_case("tag") {
                match v {
                    PropValue::List(items) => {
                        out.extend(items.into_iter().map(|t| clean_tag(&t)));
                    }
                    PropValue::Scalar(s) if !s.trim().is_empty() => out.push(clean_tag(&s)),
                    _ => {}
                }
            }
        }
    }
    out.extend(extract_inline_tags(&body));
    out.retain(|t| !t.is_empty());
    out
}

/// Inline `#tags` in body text. Mirrors the app parser's rules: the `#` must
/// be at line start or preceded by whitespace; tag chars are alphanumeric /
/// `-` / `_` / `/`; a tag must contain at least one non-digit (so `#1` /
/// `#2026` aren't tags); fenced/inline code and `# headings` are skipped.
fn extract_inline_tags(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_code_block = false;
    for line in body.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_code_block = !in_code_block;
            continue;
        }
        if in_code_block {
            continue;
        }
        // Skip ATX headings ("# ", "## ", …).
        if trimmed.starts_with('#') && trimmed[1..].starts_with([' ', '#']) {
            continue;
        }
        let bytes = line.as_bytes();
        let mut i = 0;
        let mut in_inline_code = false;
        while i < bytes.len() {
            if bytes[i] == b'`' {
                in_inline_code = !in_inline_code;
                i += 1;
                continue;
            }
            if in_inline_code {
                i += 1;
                continue;
            }
            if bytes[i] == b'#' && (i == 0 || bytes[i - 1].is_ascii_whitespace()) {
                i += 1;
                let start = i;
                while i < bytes.len() {
                    let c = bytes[i];
                    if c.is_ascii_alphanumeric() || c == b'-' || c == b'_' || c == b'/' {
                        i += 1;
                    } else {
                        break;
                    }
                }
                let tag = &line[start..i];
                if !tag.is_empty() && tag.chars().any(|c| !c.is_ascii_digit()) {
                    out.push(tag.to_string());
                }
            } else {
                i += 1;
            }
        }
    }
    out
}

/// Normalize one tag string the way both write tools and the indexer expect:
/// trim, drop a leading `#`, strip wrapping quotes.
fn clean_tag(raw: &str) -> String {
    raw.trim()
        .trim_matches(['"', '\''])
        .trim_start_matches('#')
        .trim()
        .to_string()
}

// ── internals ───────────────────────────────────────────────────────────

/// Frontmatter lines with `key` (and any block-list items under it) removed.
fn remove_key(fm_inner: &str, key: &str) -> Vec<String> {
    let lines: Vec<&str> = fm_inner.lines().collect();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if !line.starts_with([' ', '\t']) {
            if let Some(colon) = line.find(':') {
                if line[..colon].trim().eq_ignore_ascii_case(key) {
                    i += 1;
                    // Every following indented line belongs to this top-level
                    // YAML key, whether it is a list, nested mapping, folded
                    // scalar, or comment. Removing only `- ` list items left
                    // orphaned nested mappings and malformed frontmatter.
                    while i < lines.len() && lines[i].starts_with([' ', '\t']) {
                        i += 1;
                    }
                    continue;
                }
            }
        }
        out.push(line.to_string());
        i += 1;
    }
    out
}

fn reassemble(fm_lines: &[String], body: &str) -> String {
    let mut s = String::from("---\n");
    for l in fm_lines {
        s.push_str(l);
        s.push('\n');
    }
    s.push_str("---\n");
    // Keep a blank line between the frontmatter and the body. A note that
    // already had frontmatter carries its blank line at the start of `body`
    // (so this won't double it); one that didn't needs it added.
    if !body.is_empty() && !body.starts_with('\n') {
        s.push('\n');
    }
    s.push_str(body);
    s
}

fn value_contains(v: &PropValue, val_l: &str) -> bool {
    match v {
        PropValue::Scalar(s) => s.to_lowercase() == *val_l,
        PropValue::List(items) => items.iter().any(|x| x.to_lowercase() == *val_l),
    }
}

fn body_has_inline_tag(body: &str, tag_l: &str) -> bool {
    let needle = format!("#{tag_l}");
    body.split(|c: char| !(c.is_alphanumeric() || matches!(c, '#' | '_' | '-' | '/')))
        .any(|tok| tok.to_lowercase() == needle)
}

fn unquote(s: &str) -> String {
    let s = s.trim();
    if s.starts_with('"') && s.ends_with('"') && s.len() >= 2 {
        return unescape_double_quoted(&s[1..s.len() - 1]);
    }
    if s.starts_with('\'') && s.ends_with('\'') && s.len() >= 2 {
        // Single-quoted YAML scalars have no backslash escapes — this
        // module never emits them (`yaml_scalar` always double-quotes), but
        // an existing note may still have one written by hand.
        return s[1..s.len() - 1].to_string();
    }
    s.to_string()
}

/// Reverse `quote_and_escape`'s `\"`/`\\` escaping. An unrecognized escape
/// sequence is kept literal (backslash and all) rather than silently
/// dropping the backslash, so a value written by something other than
/// `yaml_scalar` doesn't lose data on round-trip.
fn unescape_double_quoted(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('"') => out.push('"'),
            Some('\\') => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOTE: &str = "---\ndate: 2026-06-01\ntype: capture\nstatus: draft\ntags:\n  - chat\n  - mcp\n---\n\n# Title\n\nbody line\n";

    #[test]
    fn split_separates_fm_and_body_verbatim() {
        let (fm, body) = split(NOTE);
        assert!(fm.is_some());
        assert_eq!(body, "\n# Title\n\nbody line\n");
    }

    #[test]
    fn split_none_without_frontmatter() {
        let (fm, body) = split("# Just a note\n\nbody");
        assert!(fm.is_none());
        assert_eq!(body, "# Just a note\n\nbody");
    }

    #[test]
    fn parse_scalars_and_lists() {
        let (fm, _) = split(NOTE);
        let props = parse(&fm.unwrap());
        assert_eq!(
            props[0],
            ("date".into(), PropValue::Scalar("2026-06-01".into()))
        );
        assert_eq!(
            props[1],
            ("type".into(), PropValue::Scalar("capture".into()))
        );
        assert_eq!(
            props[2],
            ("status".into(), PropValue::Scalar("draft".into()))
        );
        assert_eq!(
            props[3],
            (
                "tags".into(),
                PropValue::List(vec!["chat".into(), "mcp".into()])
            )
        );
    }

    #[test]
    fn parse_flow_list() {
        let (fm, _) = split("---\ntags: [a, b, c]\n---\nbody");
        let props = parse(&fm.unwrap());
        assert_eq!(
            props[0],
            (
                "tags".into(),
                PropValue::List(vec!["a".into(), "b".into(), "c".into()])
            )
        );
    }

    #[test]
    fn matches_scalar_list_and_inline_tags() {
        assert!(matches(NOTE, "status", "draft"));
        assert!(matches(NOTE, "status", "DRAFT")); // case-insensitive
        assert!(!matches(NOTE, "status", "done"));
        assert!(matches(NOTE, "type", "capture"));
        // frontmatter tag (with or without #)
        assert!(matches(NOTE, "tag", "mcp"));
        assert!(matches(NOTE, "tags", "#chat"));
        assert!(!matches(NOTE, "tag", "missing"));
        // inline #tag in the body
        assert!(matches("# x\n\nthis is #urgent today", "tag", "urgent"));
    }

    #[test]
    fn set_property_replaces_scalar_preserves_body() {
        let out = set_property(NOTE, "status", "done").unwrap();
        assert!(out.contains("status: done"));
        assert!(!out.contains("status: draft"));
        assert!(out.contains("\n# Title\n\nbody line\n")); // body untouched
        assert!(out.contains("date: 2026-06-01")); // other props kept
    }

    #[test]
    fn set_property_creates_frontmatter_when_missing() {
        let out = set_property("# Note\n\nbody", "status", "draft").unwrap();
        assert_eq!(out, "---\nstatus: draft\n---\n\n# Note\n\nbody");
    }

    #[test]
    fn add_tags_merges_dedup_and_keeps_body() {
        let out = add_tags(NOTE, &["chat".into(), "important".into(), "#new".into()]).unwrap();
        // existing chat/mcp kept, new ones appended, chat not duplicated
        let tags_section = &out[out.find("tags:").unwrap()..out.find("---\n\n# Title").unwrap()];
        assert_eq!(tags_section.matches("- chat").count(), 1);
        assert!(tags_section.contains("- mcp"));
        assert!(tags_section.contains("- important"));
        assert!(tags_section.contains("- new"));
        assert!(out.contains("\n# Title\n\nbody line\n"));
    }

    #[test]
    fn add_tags_creates_frontmatter_when_missing() {
        let out = add_tags("# Note\n\nbody", &["alpha".into()]).unwrap();
        assert!(out.starts_with("---\ntags:\n  - alpha\n---\n\n# Note"));
    }

    // ── Cluster C: YAML/structural injection ────────────────────────────

    #[test]
    fn set_property_escapes_colon_and_newline_in_value() {
        let out = set_property("# N\n\nbody", "title", "My Note: A Story").unwrap();
        let (fm, _) = split(&out);
        let props = parse(&fm.unwrap());
        assert_eq!(
            props[0],
            ("title".into(), PropValue::Scalar("My Note: A Story".into())),
            "value containing ': ' must round-trip exactly, got: {out}"
        );

        // A literal newline in a value can't be encoded as a one-line YAML
        // scalar by this hand-rolled parser — reject it outright.
        assert!(set_property("# N\n\nbody", "title", "line1\nline2").is_err());
    }

    #[test]
    fn set_property_rejects_key_containing_colon_or_newline() {
        assert!(set_property("# N\n\nbody", "bad:key", "x").is_err());
        assert!(set_property("# N\n\nbody", "bad\nkey", "x").is_err());
    }

    #[test]
    fn set_property_replaces_nested_mapping_without_orphans() {
        let note = "---\nproject:\n  name: Atlas\n  owner: Dee\nstatus: draft\n---\n\nbody";
        let out = set_property(note, "project", "New").unwrap();
        assert!(out.contains("project: New"));
        assert!(!out.contains("name: Atlas"));
        assert!(!out.contains("owner: Dee"));
        assert!(out.contains("status: draft"));
    }

    #[test]
    fn add_tags_escapes_special_chars_and_round_trips() {
        // Tags may not contain a colon (see add_tags_rejects_tag_containing_
        // colon_or_newline) — exercise quoting via a leading-dash and a
        // keyword-looking tag instead.
        let out = add_tags("# N\n\nbody", &["true".into(), "- weird".into()]).unwrap();
        let (fm, _) = split(&out);
        let props = parse(&fm.unwrap());
        let tags = match &props[0].1 {
            PropValue::List(items) => items.clone(),
            other => panic!("expected a list, got {other:?}"),
        };
        assert!(tags.contains(&"true".to_string()), "got: {tags:?}");
        assert!(tags.contains(&"- weird".to_string()), "got: {tags:?}");
    }

    #[test]
    fn add_tags_rejects_tag_containing_colon_or_newline() {
        assert!(add_tags("# N\n\nbody", &["bad:tag".into()]).is_err());
        assert!(add_tags("# N\n\nbody", &["bad\ntag".into()]).is_err());
    }

    #[test]
    fn yaml_scalar_quotes_bare_boolean_and_numeric_looking_strings() {
        for v in ["true", "false", "null", "123", "1.5", "yes", "no"] {
            let quoted = yaml_scalar(v);
            assert_eq!(quoted, format!("\"{v}\""), "expected {v} to be quoted");
            // And it must round-trip back to the exact original string, not
            // be reinterpreted as a bool/number/null.
            assert_eq!(unquote(&quoted), v);
        }
    }

    #[test]
    fn yaml_scalar_leaves_ordinary_values_bare() {
        for v in ["draft", "project/corvux", "Atlas", "2026-06-01"] {
            assert_eq!(yaml_scalar(v), v);
        }
    }

    #[test]
    fn yaml_scalar_round_trips_quotes_and_backslashes() {
        // Each of these actually triggers the quoted path (a leading `"`, or
        // an embedded ": "), so this exercises escape + unescape together,
        // not just the "stayed bare because it didn't need quoting" no-op.
        for v in [
            "\"hi\" she said",          // leading quote
            "\"quoted\" and \\slashed", // leading quote + embedded backslash
            "a: b # c",                 // embedded ": "
        ] {
            let emitted = yaml_scalar(v);
            assert!(
                emitted.starts_with('"') && emitted.ends_with('"'),
                "expected {v:?} to be quoted, got {emitted:?}"
            );
            assert_eq!(unquote(&emitted), v, "round-trip failed for {v:?}");
        }
    }

    #[test]
    fn all_tags_merges_frontmatter_and_inline() {
        let note = "---\ntags:\n  - chat\n  - mcp\n---\n\n# Title\n\nbody with #inline and #project/corvux\n";
        let tags = all_tags(note);
        assert!(tags.contains(&"chat".to_string()));
        assert!(tags.contains(&"mcp".to_string()));
        assert!(tags.contains(&"inline".to_string()));
        assert!(tags.contains(&"project/corvux".to_string()));
    }

    #[test]
    fn all_tags_skips_headings_code_and_numeric() {
        // `## Heading` is not a tag; fenced code is skipped; `#2026` is numeric.
        let note = "# H1\n\n## Heading\n\n```\n#notacode_tag\n```\n\nreal #tag and #2026 here\n";
        let tags = all_tags(note);
        assert_eq!(tags, vec!["tag".to_string()]);
    }

    #[test]
    fn all_tags_handles_flow_list_and_scalar() {
        assert_eq!(all_tags("---\ntags: [a, b]\n---\nx"), vec!["a", "b"]);
        assert_eq!(all_tags("---\ntag: solo\n---\nx"), vec!["solo"]);
    }

    #[test]
    fn all_tags_empty_when_none() {
        assert!(all_tags("# Just prose\n\nno tags, no frontmatter").is_empty());
    }
}
