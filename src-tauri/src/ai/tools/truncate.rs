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

/// Truncate `result` to at most `max_bytes`, breaking on the nearest
/// preceding newline so we don't cut mid-sentence. Appends a one-line
/// hint so the LLM knows it can request the rest with a more targeted
/// follow-up call.
pub fn truncate_tool_result(result: &str, max_bytes: usize) -> String {
    if result.len() <= max_bytes {
        return result.to_string();
    }
    // Find the last UTF-8 char boundary at or before max_bytes — slicing
    // without one panics on multi-byte content.
    let mut cut = max_bytes;
    while cut > 0 && !result.is_char_boundary(cut) {
        cut -= 1;
    }
    let head = &result[..cut];
    let break_at = head.rfind('\n').unwrap_or(cut);
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
