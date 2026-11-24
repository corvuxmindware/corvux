//! `<tool_call>...</tool_call>` regex parser.
//!
//! Provider-agnostic plain-text protocol — survives Gemini, llama.cpp,
//! anything that can emit JSON between literal angle-bracket tags. Native
//! function-calling APIs (Gemini's `functionCall`, Claude's `tool_use`)
//! are Phase B.

use serde::Deserialize;

/// Parsed tool call from LLM output.
#[derive(Debug, Clone, Deserialize)]
pub struct ToolCall {
    pub name: String,
    #[serde(default)]
    pub args: serde_json::Value,
}

/// Extract every `<tool_call>...JSON...</tool_call>` block from `text`.
/// Tolerates surrounding prose and `` ```json ... ``` `` fences inside
/// the block. Returns an empty vec if none found.
pub fn extract_tool_calls(text: &str) -> Vec<ToolCall> {
    let mut calls = Vec::new();
    let mut cursor = 0;

    while let Some(start) = text[cursor..].find("<tool_call>") {
        let abs_start = cursor + start + "<tool_call>".len();
        let Some(end_rel) = text[abs_start..].find("</tool_call>") else {
            break;
        };
        let abs_end = abs_start + end_rel;
        let json_str = text[abs_start..abs_end].trim();
        let cleaned = strip_code_fence(json_str);

        if let Ok(call) = serde_json::from_str::<ToolCall>(cleaned) {
            calls.push(call);
        }
        cursor = abs_end + "</tool_call>".len();
    }

    calls
}

/// Strip ``` or ```json fences from around a JSON string.
fn strip_code_fence(s: &str) -> &str {
    let s = s.trim();
    let s = s
        .strip_prefix("```json")
        .or_else(|| s.strip_prefix("```"))
        .unwrap_or(s);
    let s = s.strip_suffix("```").unwrap_or(s);
    s.trim()
}

/// Strip every `<tool_call>...</tool_call>` block from `text`, returning
/// the surrounding prose. Used when re-feeding model history into the LLM
/// so the agent doesn't see its own tool-call markup as conversational
/// content (it should see the parsed result instead).
///
/// Runs to a fixed point (AT-7): removing a block can splice its neighbours
/// into a new one (`<tool_c<tool_call>x</tool_call>all>y</tool_call>`), so a
/// single pass is not enough. Bounded; input still changing after the bound is
/// adversarially nested and has its remaining markers defanged instead.
pub fn strip_tool_call_blocks(text: &str) -> String {
    const MAX_PASSES: usize = 16;
    let mut cur = strip_tool_call_blocks_once(text);
    for _ in 0..MAX_PASSES {
        let next = strip_tool_call_blocks_once(&cur);
        // A pass only ever removes bytes: equal length == nothing changed.
        if next.len() == cur.len() {
            return cur;
        }
        cur = next;
    }
    cur.replace("<tool_call>", "&lt;tool_call>")
        .replace("</tool_call>", "&lt;/tool_call>")
}

/// One left-to-right pass of [`strip_tool_call_blocks`].
fn strip_tool_call_blocks_once(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0;
    while let Some(start) = text[cursor..].find("<tool_call>") {
        out.push_str(&text[cursor..cursor + start]);
        let abs_start = cursor + start + "<tool_call>".len();
        let Some(end_rel) = text[abs_start..].find("</tool_call>") else {
            // Unterminated — keep the rest as-is and bail.
            out.push_str(&text[cursor + start..]);
            return out;
        };
        cursor = abs_start + end_rel + "</tool_call>".len();
    }
    out.push_str(&text[cursor..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_single_tool_call() {
        let text = r#"Sure, I'll create that note.

<tool_call>
{"name": "create_note", "args": {"title": "Sunday Plans", "body": "Relax and read"}}
</tool_call>

Done!"#;
        let calls = extract_tool_calls(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "create_note");
    }

    #[test]
    fn extract_multiple_tool_calls() {
        let text = r#"<tool_call>{"name":"roost_search","args":{"query":"a"}}</tool_call>
Some text
<tool_call>{"name":"roost_search","args":{"query":"b"}}</tool_call>"#;
        let calls = extract_tool_calls(text);
        assert_eq!(calls.len(), 2);
    }

    #[test]
    fn strips_code_fence() {
        let text = r#"<tool_call>```json
{"name": "roost_search", "args": {"query": "x"}}
```</tool_call>"#;
        let calls = extract_tool_calls(text);
        assert_eq!(calls.len(), 1);
    }

    #[test]
    fn no_calls_when_missing_tags() {
        let text = "Just a normal message with no tools.";
        assert!(extract_tool_calls(text).is_empty());
    }

    #[test]
    fn strip_blocks_keeps_prose() {
        let text = r#"Hello <tool_call>{"name":"x","args":{}}</tool_call> world"#;
        assert_eq!(strip_tool_call_blocks(text), "Hello  world");
    }

    #[test]
    fn strip_blocks_handles_unterminated() {
        let text = "Hello <tool_call>oops never closed";
        // Unterminated block: keep the literal text so the LLM sees its own output.
        assert_eq!(
            strip_tool_call_blocks(text),
            "Hello <tool_call>oops never closed"
        );
    }

    /// AT-7: removing a block must not splice its neighbours into a new one.
    #[test]
    fn strip_blocks_reaches_a_fixed_point() {
        // One pass turns this into `<tool_call>y</tool_call>`; the fixed point
        // removes that too.
        let text = "a<tool_c<tool_call>x</tool_call>all>y</tool_call>b";
        let out = strip_tool_call_blocks(text);
        assert_eq!(out, "ab");
        assert!(!out.contains("tool_call"));
    }

    #[test]
    fn strip_blocks_is_bounded_and_defangs_deep_nesting() {
        // Each wrapper level re-forms one block when its inner block is
        // removed, so 40 levels need 40 passes — more than the bound.
        let mut text = "<tool_call>x</tool_call>".to_string();
        for _ in 0..40 {
            text = format!("<tool_c{text}all>x</tool_call>");
        }
        let out = strip_tool_call_blocks(&text);
        assert!(!out.contains("<tool_call>"), "{out}");
        assert!(!out.contains("</tool_call>"), "{out}");
    }

    #[test]
    fn strip_blocks_leaves_clean_text_alone() {
        let text = "no markup, <b>html</b>, and a < sign";
        assert_eq!(strip_tool_call_blocks(text), text);
    }
}
