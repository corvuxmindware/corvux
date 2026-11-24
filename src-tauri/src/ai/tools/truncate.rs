//! Tool-result size capping.
//!
//! Every tool result is fed back to the LLM as the next iteration's
//! context. Without a cap, a single `roost_read` on a folder of 20 large
//! notes (200KB+) would blow the context window. The cap is **non-
//! negotiable for 50K+ roosts**.

/// Hard ceiling for a single tool result. Tools can override per-call
/// via [`super::Tool::max_result_bytes`] if their semantics require more,
/// but anything above this gets truncated regardless.
pub const MAX_TOOL_RESULT_BYTES: usize = 32 * 1024;

/// Return the longest prefix within a byte budget without splitting a codepoint.
pub(crate) fn utf8_prefix(text: &str, max_bytes: usize) -> &str {
    let mut cut = max_bytes.min(text.len());
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    &text[..cut]
}

/// Cap `s` IN PLACE at `max_bytes` without splitting a codepoint. Returns
/// `true` when it cut anything.
///
/// This is the shape most call sites actually need ("cap this `String`, maybe
/// say so"), and `utf8_prefix` alone doesn't serve it — which is why six sites
/// hand-rolled the `is_char_boundary` walk, and why the next one was a
/// `String::truncate(n)` away from a UTF-8 panic that surfaces as a 500 on the
/// IPC (sharp-edges F9). `tests/roost_boundary_guard.rs` fails the build on a
/// new hand-rolled walk outside this file.
pub(crate) fn truncate_utf8(s: &mut String, max_bytes: usize) -> bool {
    if s.len() <= max_bytes {
        return false;
    }
    let cut = utf8_prefix(s, max_bytes).len();
    s.truncate(cut);
    true
}

/// [`truncate_utf8`], appending `suffix` (an ellipsis, a "[truncated]" marker)
/// only when something was cut. `max_bytes` bounds the kept text; the suffix is
/// extra.
pub(crate) fn truncate_utf8_with(s: &mut String, max_bytes: usize, suffix: &str) -> bool {
    let cut = truncate_utf8(s, max_bytes);
    if cut {
        s.push_str(suffix);
    }
    cut
}

/// Truncate `result` to at most `max_bytes`, breaking on the nearest
/// preceding newline so we don't cut mid-sentence. Appends a one-line
/// hint so the LLM knows it can request the rest with a more targeted
/// follow-up call.
pub fn truncate_tool_result(result: &str, max_bytes: usize) -> String {
    if result.len() <= max_bytes {
        return result.to_string();
    }
    let head = utf8_prefix(result, max_bytes);
    let break_at = head.rfind('\n').unwrap_or(head.len());
    format!(
        "{}\n\n[truncated — {} bytes omitted. Use a narrower query or ask roost_read for specific note paths.]",
        &result[..break_at],
        result.len() - break_at
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_prefix_respects_byte_budgets() {
        let text = "aé界😀";
        for (budget, expected) in [
            (0, ""),
            (1, "a"),
            (2, "a"),
            (3, "aé"),
            (5, "aé"),
            (6, "aé界"),
            (9, "aé界"),
            (10, text),
            (usize::MAX, text),
        ] {
            assert_eq!(utf8_prefix(text, budget), expected, "budget {budget}");
        }
        assert_eq!(utf8_prefix("", usize::MAX), "");
    }

    #[test]
    fn truncate_utf8_caps_in_place_on_a_boundary() {
        let original = "aé界😀";
        for (budget, expected, cut) in [
            (0, "", true),
            (1, "a", true),
            (2, "a", true),
            (3, "aé", true),
            (5, "aé", true),
            (6, "aé界", true),
            (9, "aé界", true),
            (10, original, false),
            (usize::MAX, original, false),
        ] {
            let mut s = original.to_string();
            assert_eq!(truncate_utf8(&mut s, budget), cut, "budget {budget}");
            assert_eq!(s, expected, "budget {budget}");
        }
    }

    #[test]
    fn truncate_utf8_with_appends_only_when_it_cut() {
        let mut short = "hi".to_string();
        assert!(!truncate_utf8_with(&mut short, 10, "…"));
        assert_eq!(short, "hi");

        let mut long = "界".repeat(10);
        assert!(truncate_utf8_with(&mut long, 7, "…"));
        assert_eq!(long, "界界…", "7 bytes keeps two 3-byte chars");

        // The panic the helper exists to prevent: naive `truncate(1)` on "é".
        let mut e = "é".to_string();
        assert!(truncate_utf8_with(&mut e, 1, "…"));
        assert_eq!(e, "…");
    }

    #[test]
    fn passthrough_when_under_cap() {
        let s = "abc";
        assert_eq!(truncate_tool_result(s, 100), "abc");
    }

    #[test]
    fn breaks_on_newline() {
        let input = "line one\nline two\nline three\nline four";
        let out = truncate_tool_result(input, 18);
        // Cut should fall back to the last newline before byte 18.
        assert!(out.starts_with("line one\nline two"));
        assert!(out.contains("[truncated"));
    }

    #[test]
    fn handles_multibyte_safely() {
        let input = "héllo wörld ".repeat(10);
        let out = truncate_tool_result(&input, 20);
        // Just shouldn't panic. Output is non-empty.
        assert!(!out.is_empty());
        assert!(out.contains("[truncated"));
    }
}
