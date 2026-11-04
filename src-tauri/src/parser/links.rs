use serde::Serialize;

#[derive(Debug, Serialize, Clone, specta::Type)]
pub struct WikiLink {
    pub target: String,
    pub display: Option<String>,
    #[specta(type = u32)]
    pub start: usize,
    #[specta(type = u32)]
    pub end: usize,
}

/// Extract all [[wiki-links]] from markdown content.
/// Supports `[[Target]]` and `[[Target|Display Text]]` syntax.
///
/// AUDIT-017: the prior `bytes[i] as char` byte-by-byte loop silently
/// garbled every multi-byte UTF-8 target. The character-iterator form
/// here preserves CJK / accented / emoji link targets verbatim.
///
/// LR-5: a `[[link]]` inside a fenced code block or inline `code` span is
/// documentation, not a real link — it must NOT become a graph edge (which
/// showed up as a phantom backlink and a false "broken link" warning).
/// `parse_tags` already excluded both; `parse_links` didn't. We now scan
/// line-by-line, toggling on ```/~~~ fence lines and on inline backticks,
/// exactly mirroring the tag parser so the two agree on what "code" is. A
/// wiki-link never spans lines, so line-at-a-time scanning loses nothing.
#[tauri::command]
#[specta::specta]
pub fn parse_links(content: String) -> Vec<WikiLink> {
    let mut links = Vec::new();
    let mut in_code_block = false;

    for line in content.lines() {
        // Byte offset of this line within `content` (pointer arithmetic, same
        // technique as `parse_tags`), so returned offsets stay absolute.
        let line_start = line.as_ptr() as usize - content.as_ptr() as usize;
        let trimmed = line.trim();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_code_block = !in_code_block;
            continue;
        }
        if in_code_block {
            continue;
        }
        collect_wikilinks_in_line(line, line_start, &mut links);
    }

    links
}

/// Scan one line for `[[wiki-links]]`, skipping inline `code` spans. `line`
/// contains no newline; offsets are reported relative to `content` by adding
/// `line_start`.
fn collect_wikilinks_in_line(line: &str, line_start: usize, links: &mut Vec<WikiLink>) {
    let mut iter = line.char_indices().peekable();
    let mut in_inline_code = false;

    while let Some(&(i, c)) = iter.peek() {
        // LR-5: inline code span — `[[x]]` between backticks isn't a link.
        if c == '`' {
            in_inline_code = !in_inline_code;
            iter.next();
            continue;
        }
        if in_inline_code {
            iter.next();
            continue;
        }
        if c == '[' {
            // Look ahead for a second '['. Two clones — cheap because
            // CharIndices iterators are just (slice, offset).
            let mut look = iter.clone();
            look.next();
            if look.peek().map(|(_, c)| *c) != Some('[') {
                iter.next();
                continue;
            }
            let start = i;
            iter.next();
            iter.next();

            let mut target = String::new();
            let mut display: Option<String> = None;
            let mut end_offset: Option<usize> = None;

            while let Some(&(j, c)) = iter.peek() {
                if c == ']' {
                    let mut look = iter.clone();
                    look.next();
                    if look.peek().map(|(_, c)| *c) == Some(']') {
                        iter.next();
                        iter.next();
                        // Byte offset just past the closing `]]` (or line end).
                        end_offset = Some(iter.peek().map(|(k, _)| *k).unwrap_or(line.len()));
                        break;
                    }
                    // Single `]` is allowed inside the target.
                    if let Some(ref mut d) = display {
                        d.push(c);
                    } else {
                        target.push(c);
                    }
                    iter.next();
                    continue;
                }
                if c == '|' && display.is_none() {
                    display = Some(String::new());
                    iter.next();
                    continue;
                }
                if c == '`' {
                    // A backtick opens a code span mid-`[[`; the link is
                    // malformed. Abort it and let the outer loop resume so the
                    // backtick correctly toggles inline-code accounting.
                    break;
                }
                if let Some(ref mut d) = display {
                    d.push(c);
                } else {
                    target.push(c);
                }
                iter.next();
                let _ = j; // j is the next-byte index; unused directly
            }

            if let Some(end) = end_offset {
                let trimmed_target = target.trim().to_string();
                if !trimmed_target.is_empty() {
                    links.push(WikiLink {
                        target: trimmed_target,
                        display: display.map(|d| d.trim().to_string()),
                        start: line_start + start,
                        end: line_start + end,
                    });
                }
            }
        } else {
            iter.next();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_simple_link() {
        let links = parse_links("Hello [[World]] there".to_string());
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].target, "World");
        assert_eq!(links[0].display, None);
    }

    #[test]
    fn test_pipe_display() {
        let links = parse_links("See [[Target|shown text]] here".to_string());
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].target, "Target");
        assert_eq!(links[0].display, Some("shown text".to_string()));
    }

    #[test]
    fn test_multiple_links() {
        let links = parse_links("A [[First]] and [[Second]] note".to_string());
        assert_eq!(links.len(), 2);
        assert_eq!(links[0].target, "First");
        assert_eq!(links[1].target, "Second");
    }

    #[test]
    fn test_no_newline_in_link() {
        let links = parse_links("[[broken\nlink]]".to_string());
        assert_eq!(links.len(), 0);
    }

    #[test]
    fn test_empty_link_ignored() {
        let links = parse_links("[[]] text".to_string());
        assert_eq!(links.len(), 0);
    }

    // ── AUDIT-017: UTF-8 round-trip ───────────────────────────────────

    #[test]
    fn parses_cjk_target() {
        let links = parse_links("link to [[资料]] here".to_string());
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].target, "资料");
        assert_eq!(links[0].display, None);
    }

    #[test]
    fn parses_accented_target() {
        let links = parse_links("see [[café]] for context".to_string());
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].target, "café");
    }

    #[test]
    fn parses_emoji_target() {
        let links = parse_links("project [[🦀]] mascot".to_string());
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].target, "🦀");
    }

    #[test]
    fn parses_cjk_alias() {
        let links = parse_links("see [[foo|显示]] here".to_string());
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].target, "foo");
        assert_eq!(links[0].display, Some("显示".to_string()));
    }

    #[test]
    fn start_end_indices_are_byte_offsets() {
        let content = "x [[资料]] y".to_string();
        let links = parse_links(content.clone());
        assert_eq!(links.len(), 1);
        // The slice `start..end` must round-trip back to the literal `[[资料]]`
        // — proving the offsets are valid char boundaries.
        assert_eq!(&content[links[0].start..links[0].end], "[[资料]]");
    }

    // ── LR-5: code exclusion (parity with parse_tags) ─────────────────

    #[test]
    fn fenced_code_block_links_are_ignored() {
        let content = "before [[Real]]\n```\n[[NotALink]]\nmore [[AlsoNot]]\n```\nafter [[Also]]";
        let links = parse_links(content.to_string());
        let targets: Vec<_> = links.iter().map(|l| l.target.as_str()).collect();
        assert_eq!(targets, vec!["Real", "Also"]);
    }

    #[test]
    fn tilde_fence_links_are_ignored() {
        let content = "~~~\n[[Nope]]\n~~~\n[[Yes]]";
        let links = parse_links(content.to_string());
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].target, "Yes");
    }

    #[test]
    fn inline_code_links_are_ignored() {
        let content = "use `[[NotALink]]` but [[RealLink]] works";
        let links = parse_links(content.to_string());
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].target, "RealLink");
    }

    #[test]
    fn offsets_stay_absolute_across_lines() {
        // A link on a later line must report offsets into the whole content.
        let content = "line one\nsee [[Target]] here".to_string();
        let links = parse_links(content.clone());
        assert_eq!(links.len(), 1);
        assert_eq!(&content[links[0].start..links[0].end], "[[Target]]");
    }
}
