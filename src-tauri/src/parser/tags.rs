use std::collections::HashMap;

use serde::Serialize;
use unicode_normalization::{char::is_combining_mark, UnicodeNormalization};

#[derive(Debug, Serialize, Clone, specta::Type)]
pub struct InlineTag {
    pub tag: String,
    #[specta(type = u32)]
    pub start: usize,
    #[specta(type = u32)]
    pub end: usize,
}

// ── Tag identity (LR-6 / LR-7) ───────────────────────────────────────────
//
// ONE rule for "what is a tag character" and "when are two tags the same
// tag", shared by every parser/comparer in the app (inline body scan,
// frontmatter scan, task-line scan, graph tag nodes, tag inventory, the agent
// tag-guard) and mirrored verbatim by the standalone `corvux-mcp` crate
// (`corvux-mcp/src/frontmatter.rs`, "tag identity" section). The two copies
// are kept in lock-step by the shared fixture
// `corvux-mcp/tests/fixtures/tag_identity.json`, asserted by a test on each
// side.
//
// * Characters: Unicode letters, numbers and combining marks (Telugu/Hindi
//   vowel signs and viramas are marks), plus `_`, `-` and `/` (nesting).
//   Anything else — whitespace, punctuation (ASCII or CJK), emoji — ends the
//   tag. Before LR-7 this was ASCII-only, so `#café` indexed as `caf` and
//   `#日本語` was dropped.
// * Identity: Unicode lowercase of the NFC form, applied at QUERY / GROUP
//   time only. Stored `tags` rows stay VERBATIM (Locked Decision #12), so a
//   graph.db written before this change folds to exactly the same keys as one
//   written after it — no migration, no reindex, no mixed identities.

/// May `c` appear inside a tag?
pub fn is_tag_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '-' | '/') || is_combining_mark(c)
}

/// Obsidian rule: a tag needs at least one non-numeric character, so `#1`,
/// `#5`, `#2024` (list markers, issue refs) are not tags. Combining marks do
/// not count as content (a keycap emoji `1\u{FE0F}\u{20E3}` is not a tag).
/// Empty text is never a tag.
pub fn is_valid_tag_text(tag: &str) -> bool {
    tag.chars()
        .any(|c| !c.is_numeric() && !is_combining_mark(c))
}

/// Tag identity key: Unicode lowercase of the NFC form. Two spellings are the
/// same tag iff their keys are equal (`#ML` == `#ml`, NFC `café` == NFD
/// `café`). The input is never stored in this form — only compared/grouped.
pub fn fold_tag(tag: &str) -> String {
    if tag.is_ascii() {
        return tag.to_ascii_lowercase();
    }
    let nfc: String = tag.nfc().collect();
    nfc.to_lowercase().nfc().collect()
}

/// The `candidates` that are NOT in `known`, compared by tag IDENTITY
/// ([`fold_tag`]) — `known` holds stored (verbatim) spellings, so a roost with
/// `#ml` knows `#ML`. The agent tag-guards (`roost_write`, `roost_edit`) use
/// this instead of exact-string `contains`, which was the LR-6 gap.
pub fn tags_not_in<'a>(
    candidates: impl IntoIterator<Item = &'a str>,
    known: &std::collections::HashSet<String>,
) -> Vec<&'a str> {
    let keys: std::collections::HashSet<String> = known.iter().map(|t| fold_tag(t)).collect();
    candidates
        .into_iter()
        .filter(|t| !keys.contains(&fold_tag(t)))
        .collect()
}

/// If the `#` at byte `hash` of `line` starts a tag, the byte offset just past
/// the tag text (so the text is `line[hash + 1..end]`); otherwise `None`.
///
/// The `#` must open the line or follow whitespace (any Unicode whitespace —
/// the editor's highlighter already agrees, and CJK text uses U+3000), and the
/// scanned text must pass [`is_valid_tag_text`]. `hash` must index a `#` byte
/// (so it is a char boundary). This is the single scanner behind both
/// [`parse_tags`] and the task-line extractor in `parser::checkboxes`.
pub(crate) fn scan_inline_tag(line: &str, hash: usize) -> Option<usize> {
    let preceded_ok = hash == 0
        || line[..hash]
            .chars()
            .next_back()
            .is_some_and(char::is_whitespace);
    if !preceded_ok {
        return None;
    }
    let start = hash + 1;
    let end = start
        + line[start..]
            .char_indices()
            .find(|&(_, c)| !is_tag_char(c))
            .map_or(line.len() - start, |(i, _)| i);
    is_valid_tag_text(&line[start..end]).then_some(end)
}

/// Groups verbatim tag spellings under their [`fold_tag`] identity and picks
/// one spelling to DISPLAY per identity (most-used wins; ties go to the
/// byte-wise smallest, so the choice is deterministic). Used wherever the app
/// folds stored rows for display: the tag inventory and the graph's tag nodes.
#[derive(Debug, Default)]
pub struct TagIdentities {
    /// identity key → (verbatim spelling → weight)
    by_key: HashMap<String, HashMap<String, usize>>,
}

impl TagIdentities {
    /// Record `weight` uses of `spelling`; returns its identity key.
    pub fn add(&mut self, spelling: &str, weight: usize) -> String {
        let key = fold_tag(spelling);
        *self
            .by_key
            .entry(key.clone())
            .or_default()
            .entry(spelling.to_string())
            .or_default() += weight;
        key
    }

    /// The spelling to show for `key` (`None` if the key was never added).
    pub fn display(&self, key: &str) -> Option<&str> {
        self.by_key.get(key).and_then(|spellings| {
            spellings
                .iter()
                .max_by(|a, b| a.1.cmp(b.1).then_with(|| b.0.cmp(a.0)))
                .map(|(s, _)| s.as_str())
        })
    }
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
                // `scan_inline_tag` owns the whole rule: the `#` must open the
                // line or follow whitespace, tag characters are Unicode-aware
                // (LR-7), and the Obsidian "at least one non-numeric char" rule
                // keeps `#1`, `#5`, `#2024` (list markers / issue refs) out of
                // the graph.
                match scan_inline_tag(line, i) {
                    Some(end) => {
                        tags.push(InlineTag {
                            tag: line[i + 1..end].to_string(),
                            start: offset + i,
                            end: offset + end,
                        });
                        i = end;
                    }
                    None => i += 1,
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
/// Applies the SAME rule as inline tags (`is_tag_char` / `is_valid_tag_text`):
///  - must contain at least one non-numeric character
///  - chars restricted to Unicode letters/numbers/marks, `-`, `_`, `/`
fn push_tag(out: &mut Vec<InlineTag>, raw: &str) {
    let stripped = raw.trim().trim_matches(['"', '\'']).trim_start_matches('#'); // tolerate `- #tag` in YAML
    if stripped.is_empty() {
        return;
    }
    // Reject if any disallowed char.
    if !stripped.chars().all(is_tag_char) {
        return;
    }
    // Reject purely-numeric tags (matches Obsidian rule).
    if !is_valid_tag_text(stripped) {
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

    // ── LR-7: Unicode tag characters ─────────────────────────────────

    fn names(content: &str) -> Vec<String> {
        parse_tags(content.to_string())
            .into_iter()
            .map(|t| t.tag)
            .collect()
    }

    #[test]
    fn lr7_accented_tag_is_not_truncated() {
        // Regression: the ASCII-only loop broke at `é` and still pushed the
        // prefix, so `#café` was indexed as `caf`.
        assert_eq!(names("Le #café ici"), vec!["café"]);
    }

    #[test]
    fn lr7_non_latin_scripts_are_tags() {
        // Regression: `#日本語` produced an empty prefix and was dropped.
        assert_eq!(names("#日本語 と #タグ。"), vec!["日本語", "タグ"]);
        // Telugu / Devanagari: vowel signs and viramas are combining MARKS,
        // not letters — they must not end the tag.
        assert_eq!(names("#ప్రాజెక్ట్ #తెలుగు"), vec!["ప్రాజెక్ట్", "తెలుగు"]);
        assert_eq!(names("#हिंदी"), vec!["हिंदी"]);
    }

    #[test]
    fn lr7_decomposed_accent_stays_one_tag() {
        assert_eq!(names("#cafe\u{301} ok"), vec!["cafe\u{301}"]);
    }

    #[test]
    fn lr7_offsets_are_byte_offsets_into_the_original() {
        let content = "あ #café x";
        let tags = parse_tags(content.to_string());
        assert_eq!(tags.len(), 1);
        assert_eq!(&content[tags[0].start..tags[0].end], "#café");
    }

    #[test]
    fn lr7_emoji_and_trailing_punctuation_end_a_tag() {
        assert_eq!(
            names("ship #🚀 and #launch🚀 and #ok❤️"),
            vec!["launch", "ok"]
        );
        assert_eq!(
            names("#end. #comma, #bang! #q? #semi; #colon: #x) (#paren)"),
            vec!["end", "comma", "bang", "q", "semi", "colon", "x"]
        );
    }

    #[test]
    fn lr7_numeric_rule_is_unicode_aware() {
        // Arabic-Indic digits are numbers too: `#١٢٣` is not a tag, `#2nd` is.
        assert_eq!(names("#١٢٣ #2024 #2nd"), vec!["2nd"]);
        // A keycap emoji (digit + marks) must not smuggle a numeric tag in.
        assert!(names("#1\u{FE0F}\u{20E3}").is_empty());
    }

    #[test]
    fn lr7_url_fragments_and_headings_and_code_stay_excluded() {
        assert_eq!(
            names("see https://example.com/page#section and http://x.y/#top but #real"),
            vec!["real"]
        );
        assert_eq!(names("# Titel\n\n## Über #notatag\n\n#café"), vec!["café"]);
        assert_eq!(names("```\n#café\n```\n`#日本語` #タグ"), vec!["タグ"]);
    }

    #[test]
    fn lr7_unicode_whitespace_may_precede_a_hash() {
        // The editor's highlighter (`\s`) already treats these as boundaries;
        // CJK text uses U+3000 between words.
        assert_eq!(
            names("nbsp\u{a0}#nbsp wide\u{3000}#wide"),
            vec!["nbsp", "wide"]
        );
    }

    #[test]
    fn lr7_frontmatter_accepts_the_same_characters_as_inline() {
        // Regression: `[café]` was rejected by the ASCII-only frontmatter
        // check while the MCP accepted it — two answers for one note.
        let content = "---\ntags: [café, 日本語, ML, 2024, two words]\n---\n\n#café";
        assert_eq!(names(content), vec!["café", "日本語", "ML", "café"]);
        let block = "---\ntags:\n  - Ünïcode\n  - \"#日本語\"\n---\n";
        assert_eq!(names(block), vec!["Ünïcode", "日本語"]);
        assert_eq!(
            frontmatter_tag_names("---\ntags: [café, ML]\n---\nbody #inline"),
            vec!["café", "ML"]
        );
    }

    // ── LR-6: identity (case + normalization) ─────────────────────────

    #[test]
    fn lr6_fold_is_case_and_normalization_insensitive() {
        assert_eq!(fold_tag("ML"), fold_tag("ml"));
        assert_eq!(fold_tag("Project/Corvux"), "project/corvux");
        assert_eq!(fold_tag("CAFÉ"), "café");
        // NFD and NFC spellings of the same word are ONE tag.
        assert_eq!(fold_tag("cafe\u{301}"), fold_tag("café"));
        assert_eq!(fold_tag("CAFE\u{301}"), "café");
        // Folding never merges genuinely different tags.
        assert_ne!(fold_tag("ml"), fold_tag("ml2"));
        assert_ne!(fold_tag("a/b"), fold_tag("a-b"));
    }

    #[test]
    fn lr6_fold_is_idempotent() {
        for s in [
            "ML",
            "CAFÉ",
            "cafe\u{301}",
            "İstanbul",
            "ΣΑΣ",
            "日本語",
            "x/Y_z-9",
        ] {
            let once = fold_tag(s);
            assert_eq!(fold_tag(&once), once, "fold must be idempotent for {s:?}");
        }
    }

    #[test]
    fn lr6_tags_not_in_compares_by_identity_not_exact_string() {
        // The `roost_write` / `roost_edit` guards' core: stored spellings are
        // verbatim (`ml`, `Café`), candidates may use any spelling.
        let known: std::collections::HashSet<String> = ["ml", "Café", "project/corvux"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert!(tags_not_in(["ML", "café", "cafe\u{301}", "Project/Corvux"], &known).is_empty());
        assert_eq!(
            tags_not_in(["ML", "invented", "日本語"], &known),
            vec!["invented", "日本語"]
        );
        // Identity is exact modulo case/NFC — a different tag is still unknown.
        assert_eq!(tags_not_in(["ml2"], &known), vec!["ml2"]);
    }

    #[test]
    fn lr6_display_picks_most_used_spelling_then_smallest() {
        let mut ids = TagIdentities::default();
        let k = ids.add("ML", 1);
        assert_eq!(ids.add("ml", 3), k);
        assert_eq!(ids.add("Ml", 1), k);
        assert_eq!(k, "ml");
        assert_eq!(ids.display("ml"), Some("ml"), "most-used spelling wins");
        // Tie → byte-wise smallest, so the choice is deterministic.
        let mut tie = TagIdentities::default();
        tie.add("ml", 2);
        tie.add("ML", 2);
        assert_eq!(tie.display("ml"), Some("ML"));
        assert_eq!(tie.display("absent"), None);
    }

    /// Shared contract — the SAME fixture is asserted against the standalone
    /// MCP parser (`corvux-mcp/src/frontmatter.rs`) and the frontend
    /// (`tests/unit/tags.test.ts`). If any of the three drifts, one fails.
    #[test]
    fn shared_tag_identity_fixture_matches() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("corvux-mcp")
            .join("tests")
            .join("fixtures")
            .join("tag_identity.json");
        let raw = std::fs::read_to_string(&path).expect("read tag_identity.json");
        let fixture: serde_json::Value = serde_json::from_str(&raw).expect("parse fixture");

        let parse_cases = fixture["parse"].as_array().expect("parse array");
        assert!(parse_cases.len() >= 20, "fixture looks truncated");
        for case in parse_cases {
            let name = case["name"].as_str().unwrap();
            let content = case["content"].as_str().unwrap();
            let want: Vec<String> = case["tags"]
                .as_array()
                .unwrap()
                .iter()
                .map(|t| t.as_str().unwrap().to_string())
                .collect();
            assert_eq!(names(content), want, "parse case {name:?}");
        }

        for pair in fixture["fold"].as_array().expect("fold array") {
            let input = pair[0].as_str().unwrap();
            let key = pair[1].as_str().unwrap();
            assert_eq!(fold_tag(input), key, "fold case {input:?}");
        }
    }
}
