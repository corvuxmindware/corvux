//! Compiled-in built-in subagents — guarantees that even on a fresh
//! roost with no `.corvux/agents/` directory, the parent can still
//! delegate to a sane default. Today there's only `general`; more can
//! ship over time without forcing users to copy AGENT.md files.
//!
//! Precedence at registry-build time: roost-local > global > builtin.
//! User-defined `general/AGENT.md` overrides the bundled one verbatim.

use std::path::PathBuf;

use super::manifest::SubagentDef;

const GENERAL_BODY: &str =
    "You are the `general` subagent. The parent agent delegated this task to \
you so its own context window can stay focused. You have your own fresh \
context — you will NOT see the parent's prior conversation. \n\n\
Steps:\n\
1. Read the prompt carefully. The parent has already framed the task; \
do not re-interpret it.\n\
2. Use the tools available to you to gather what you need. Cite \
[[wiki-links]] for any roost content you reference.\n\
3. Produce a concise, single-message answer. The parent will see your \
final text as the tool result; intermediate tool calls stay inside \
your context.\n\n\
Do NOT:\n\
- Ask the parent clarifying questions (you can't — the parent isn't \
listening). Make the best judgment call you can with the prompt \
provided.\n\
- Spawn further subagents (the `task` tool is intentionally not in \
your registry). Complete the work in this context.\n\
- Return long-form reasoning. Just the synthesized answer.\n";

/// Return all built-in subagent definitions. Today: just `general`.
/// Returned as full `SubagentDef`s so the registry's lookup path is
/// uniform — roost/global/builtin all produce the same struct.
pub fn builtins() -> Vec<SubagentDef> {
    vec![SubagentDef {
        name: "general".to_string(),
        description: "Broad delegation for tasks that would flood the parent's context with intermediate results. Use when the user asks something that requires 3+ tool calls of exploration but the parent agent shouldn't have all of those round-trips in its history.".to_string(),
        body: GENERAL_BODY.to_string(),
        model: None,
        tools: None, // inherits parent's tools minus task/destructive
        max_iterations: None,
        max_wall_clock_secs: None,
        max_tool_result_bytes: None,
        source_path: PathBuf::from("<builtin>"),
    }]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ships_general_subagent() {
        let all = builtins();
        assert!(all.iter().any(|d| d.name == "general"));
        let gen = all.iter().find(|d| d.name == "general").unwrap();
        assert!(!gen.description.is_empty());
        assert!(gen.body.contains("subagent"));
        // builtin marker so the registry can detect overrides
        assert_eq!(gen.source_path, PathBuf::from("<builtin>"));
        // never inherits a tool list — uses parent-minus-task default
        assert!(gen.tools.is_none());
    }

    #[test]
    fn general_body_discourages_clarifying_questions() {
        let gen = builtins()
            .into_iter()
            .find(|d| d.name == "general")
            .unwrap();
        let lower = gen.body.to_lowercase();
        assert!(
            lower.contains("clarifying") || lower.contains("can't ask"),
            "general subagent must instruct itself not to ask clarifying questions"
        );
    }
}
