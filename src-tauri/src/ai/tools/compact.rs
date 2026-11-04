//! Template-based chat-history compaction.
//!
//! As the agent loop iterates, every tool call appends two messages:
//!
//! 1. A `Model` turn carrying the LLM's prose plus any `<tool_call>...</tool_call>`
//!    blocks (re-fed verbatim so the model sees what it just said).
//! 2. A synthetic `User` turn carrying the rendered tool results, often
//!    including the FULL JSON body of each tool's output. Multi-iteration
//!    research sessions accumulate hundreds of KB of these JSON blobs.
//!
//! Without compaction, a 12-iteration `Research` profile session can push
//! 200KB+ of tool-result JSON into the context, crowding the actually-useful
//! retrieved chunks. This module:
//!
//! - **Keeps the last [`KEEP_RECENT_ITERATIONS`] iterations untouched** —
//!   the model needs full context for its current reasoning.
//! - **Replaces older tool-result turns** with a one-line stub naming
//!   which tools ran. If the LLM needs the data again, it can call the
//!   tool again — calling is idempotent at the read tier.
//! - **Leaves model turns intact** — those carry the LLM's reasoning,
//!   which dropping would break conversation flow.
//!
//! No LLM call. Pure string transformation. Idempotent — running the
//! compaction twice on the same buffer is a no-op on the second pass.
//!
//! The doc-spec'd "summarize old turns into a single Context-so-far
//! message via lightweight LLM call" is left for a follow-up; the
//! template version captures most of the win at zero token cost.

use crate::ai::{ChatMessage, ChatRole};

/// Iterations of the agent loop to keep at full fidelity. Each iteration
/// is two messages (model + synthetic tool-result user turn). Anything
/// older has its tool-result body replaced with a stub.
///
/// 4 is a balance: long enough to span a typical multi-step task's
/// active reasoning window, short enough that the heaviest tool blobs
/// from older iterations get compacted before crowding the context.
pub const KEEP_RECENT_ITERATIONS: usize = 4;

/// Marker the agent loop prepends to every synthetic tool-result user
/// turn. We pattern-match on it to decide what's compactable. Keep in
/// sync with `agent_loop.rs::run_agent_loop`.
const TOOL_RESULT_MARKER: &str = "Tool results from your previous response";

/// Marker placed on a turn we've already compacted. Idempotency hinge:
/// running compaction twice on the same buffer skips already-stubbed
/// turns, never re-stubbing or growing them.
const COMPACTED_MARKER: &str = "[Compacted earlier tool calls";

/// Compact the chat history in-place. No-op when:
/// - `messages.len() <= KEEP_RECENT_ITERATIONS * 2` (nothing old to compact).
/// - All compactable turns are already compacted (idempotent).
///
/// Returns the number of messages whose content was rewritten — useful
/// for tracing / metrics. The caller doesn't need to act on the count.
pub fn compact_history(messages: &mut [ChatMessage]) -> usize {
    let keep = KEEP_RECENT_ITERATIONS * 2;
    if messages.len() <= keep {
        return 0;
    }
    let cutoff = messages.len() - keep;
    let mut rewritten = 0;
    for msg in messages.iter_mut().take(cutoff) {
        if !matches!(msg.role, ChatRole::User) {
            continue;
        }
        if !msg.content.starts_with(TOOL_RESULT_MARKER) {
            continue;
        }
        if msg.content.starts_with(COMPACTED_MARKER) {
            continue;
        }
        msg.content = compact_tool_section(&msg.content);
        rewritten += 1;
    }
    rewritten
}

/// Replace a synthetic tool-result user turn with a one-line stub
/// listing which tools ran. The full JSON bodies are dropped; the LLM
/// can re-call the tool if it needs the data.
///
/// We extract tool names from `### Tool: \`<name>\`` markers — the
/// agent loop emits one per dispatched tool. Empty list (no markers
/// found) gets a generic stub so the conversation still has a turn
/// where one was expected.
fn compact_tool_section(full: &str) -> String {
    let names: Vec<&str> = full
        .lines()
        .filter_map(|line| {
            let line = line.trim_start();
            line.strip_prefix("### Tool: `")
                .and_then(|r| r.strip_suffix('`').or_else(|| r.split('`').next()))
        })
        .collect();
    if names.is_empty() {
        return format!("{COMPACTED_MARKER}: details no longer in context. Re-call any tool whose result you need.]");
    }
    format!(
        "{COMPACTED_MARKER}: {}. Results no longer in context. Re-call any tool whose result you need.]",
        names.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user_tool_result(tools: &[&str]) -> ChatMessage {
        let mut s = String::from(TOOL_RESULT_MARKER);
        s.push_str(" (most recent first). Use these to continue.\n\n");
        for t in tools {
            s.push_str(&format!("### Tool: `{t}`\n```json\n{{\"summary\": \"…\", \"data\": {{\"big\": \"blob\"}}}}\n```\n\n"));
        }
        ChatMessage {
            role: ChatRole::User,
            content: s,
            ..Default::default()
        }
    }

    fn model_msg(text: &str) -> ChatMessage {
        ChatMessage {
            role: ChatRole::Model,
            content: text.to_string(),
            ..Default::default()
        }
    }

    fn user_plain(text: &str) -> ChatMessage {
        ChatMessage {
            role: ChatRole::User,
            content: text.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn no_op_when_history_short() {
        let mut msgs = vec![
            user_plain("hi"),
            model_msg("hello <tool_call>{}</tool_call>"),
            user_tool_result(&["roost_search"]),
        ];
        let before = msgs.clone();
        let count = compact_history(&mut msgs);
        assert_eq!(count, 0);
        assert_eq!(msgs.len(), before.len());
        // Content unchanged.
        for (a, b) in msgs.iter().zip(before.iter()) {
            assert_eq!(a.content, b.content);
        }
    }

    #[test]
    fn compacts_old_tool_results_when_history_long() {
        // Build 6 iterations = 12 messages. KEEP_RECENT_ITERATIONS = 4
        // means iterations 1-2 (messages 0-3) get compacted; 3-6 stay.
        let mut msgs: Vec<ChatMessage> = Vec::new();
        for i in 0..6 {
            msgs.push(model_msg(&format!("turn {i} <tool_call>{{}}</tool_call>")));
            msgs.push(user_tool_result(&[&format!("tool_{i}")]));
        }
        let count = compact_history(&mut msgs);
        // 2 old iterations = 2 user-tool-result turns rewritten.
        assert_eq!(count, 2);
        // The first two compacted user turns now match the marker.
        assert!(msgs[1].content.starts_with(COMPACTED_MARKER));
        assert!(msgs[1].content.contains("tool_0"));
        assert!(msgs[3].content.starts_with(COMPACTED_MARKER));
        assert!(msgs[3].content.contains("tool_1"));
        // Recent iterations untouched (still carry the JSON blob).
        assert!(msgs[5].content.contains("```json"));
        assert!(msgs[11].content.contains("```json"));
    }

    #[test]
    fn idempotent_second_pass_does_nothing() {
        let mut msgs: Vec<ChatMessage> = Vec::new();
        for i in 0..6 {
            msgs.push(model_msg(&format!("turn {i}")));
            msgs.push(user_tool_result(&[&format!("tool_{i}")]));
        }
        let first = compact_history(&mut msgs);
        assert_eq!(first, 2);
        let second = compact_history(&mut msgs);
        assert_eq!(second, 0, "second pass should be a no-op");
    }

    #[test]
    fn model_turns_never_compacted_even_if_old() {
        let mut msgs: Vec<ChatMessage> = Vec::new();
        for i in 0..6 {
            msgs.push(model_msg(&format!("model turn {i} body")));
            msgs.push(user_tool_result(&["t"]));
        }
        compact_history(&mut msgs);
        for (idx, m) in msgs.iter().enumerate() {
            if matches!(m.role, ChatRole::Model) {
                assert!(
                    m.content.starts_with("model turn"),
                    "model turn at {idx} got rewritten"
                );
            }
        }
    }

    #[test]
    fn user_turns_that_arent_tool_results_left_alone() {
        // First two messages are a real user prompt + model reply. Then
        // 4 iterations of tool-result accumulation. KEEP=4 means the
        // first iteration's tool result IS old and gets compacted, but
        // the original user prompt at msgs[0] must not be touched.
        let mut msgs: Vec<ChatMessage> = vec![
            user_plain("Original user question"),
            model_msg("First model reply"),
        ];
        for i in 0..4 {
            msgs.push(model_msg(&format!("iter {i} <tool_call>{{}}</tool_call>")));
            msgs.push(user_tool_result(&[&format!("t{i}")]));
        }
        compact_history(&mut msgs);
        assert_eq!(
            msgs[0].content, "Original user question",
            "non-tool user turn must not be compacted"
        );
    }

    #[test]
    fn extracts_multiple_tool_names_into_stub() {
        let section = format!(
            "{TOOL_RESULT_MARKER} (most recent first).\n\n\
             ### Tool: `roost_search`\n```json\n{{}}\n```\n\n\
             ### Tool: `backlinks`\n```json\n{{}}\n```\n\n\
             ### Tool: `note_outline`\n```json\n{{}}\n```\n"
        );
        let stub = compact_tool_section(&section);
        assert!(stub.contains("roost_search"));
        assert!(stub.contains("backlinks"));
        assert!(stub.contains("note_outline"));
        assert!(!stub.contains("```json"));
    }

    #[test]
    fn compact_tool_section_with_no_markers_emits_generic_stub() {
        let section = format!("{TOOL_RESULT_MARKER}\nbut no `### Tool:` lines\n");
        let stub = compact_tool_section(&section);
        assert!(stub.starts_with(COMPACTED_MARKER));
        assert!(stub.contains("Re-call any tool"));
    }

    #[test]
    fn compaction_actually_shrinks_bytes() {
        let mut msgs: Vec<ChatMessage> = Vec::new();
        for i in 0..6 {
            msgs.push(model_msg(&format!("turn {i}")));
            // Beef up tool result with a large payload to make the
            // before/after byte delta unmistakable.
            let mut s = String::from(TOOL_RESULT_MARKER);
            s.push_str(" (most recent first).\n\n### Tool: `big`\n```json\n{");
            for _ in 0..200 {
                s.push_str("\"k\":\"v\",");
            }
            s.push_str("}\n```\n");
            msgs.push(ChatMessage {
                role: ChatRole::User,
                content: s,
                ..Default::default()
            });
        }
        let before: usize = msgs.iter().map(|m| m.content.len()).sum();
        compact_history(&mut msgs);
        let after: usize = msgs.iter().map(|m| m.content.len()).sum();
        assert!(
            after < before,
            "expected size to shrink; before={before}, after={after}"
        );
    }
}
