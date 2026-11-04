//! Tool bundles — group related tools so we can register / render /
//! reason about them as units instead of touching the registry per-tool.
//!
//! ## Why bundles
//!
//! At 8 tools the flat-list registry is fine. As we cross ~10–15 tools,
//! three pressures appear:
//!
//! 1. **Prompt bloat** — every tool's JSON schema is dumped into the
//!    system prompt on every chat turn. Bundles let us emit a grouped,
//!    scannable layout (headers per domain) and later let `AgentProfile`
//!    inject only the bundles the active agent needs.
//! 2. **Agent specialization** — a "research" agent doesn't need
//!    `roost_write`; a "writer" agent doesn't need `graph_query`.
//!    Bundles are the unit a profile turns on or off.
//! 3. **New-tool ergonomics** — a contributor adds a tool by appending
//!    it to a bundle's `tools()` list. No registration churn elsewhere.
//!
//! ## What a bundle is
//!
//! A `ToolBundle` is a trait object that returns:
//! - `name()` — short stable id (`"roost"`, `"graph"`, `"memory"`)
//! - `description()` — one-line summary; rendered as a section header
//! - `tools()` — the `Arc<dyn Tool>` instances this bundle owns
//!
//! Bundles are stateless and zero-cost — instantiating one is just
//! returning a fresh `Vec<Arc<dyn Tool>>` of pre-existing tool structs.

use std::sync::Arc;

use super::registry::{Tool, ToolRegistry};

/// A named group of tools that travel together.
pub trait ToolBundle: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    fn tools(&self) -> Vec<Arc<dyn Tool>>;
}

/// Convenience: assemble a registry from a list of bundles. Tools
/// register in the order their bundles appear, then within each bundle
/// in the order returned by `tools()`. Later registrations of the same
/// name overwrite earlier ones — caller decides whether that matters.
pub fn registry_from_bundles(bundles: &[Box<dyn ToolBundle>]) -> ToolRegistry {
    let mut reg = ToolRegistry::new();
    for bundle in bundles {
        for tool in bundle.tools() {
            reg.register(tool);
        }
    }
    reg
}

/// Render a bundle-grouped tool blurb for system-prompt injection.
/// Same protocol explanation as `ToolRegistry::render_for_system_prompt`
/// but tools are grouped under their bundle headers — easier for the
/// LLM to scan and (more importantly) easier for *us* to scan when
/// debugging. Trailing `addendum` is appended verbatim if non-empty;
/// `AgentProfile` uses this to inject profile-specific instructions.
///
/// `low_confidence_tools` (agent-hardening § 4.4) is a list of tool
/// names whose recent rejection rate exceeds the configured threshold.
/// Each matching tool's description is prefixed with a "(low
/// confidence — user often rejects this)" qualifier so the LLM treats
/// it as a softer recommendation. Empty slice = no softening, matches
/// the pre-scorecard render exactly.
///
/// Skips bundles whose `name() == "legacy"` from the rendered blurb so
/// deprecated aliases don't waste prompt tokens — the alias still
/// dispatches via the registry, the LLM just isn't taught its name.
pub fn render_for_system_prompt(
    bundles: &[Box<dyn ToolBundle>],
    addendum: &str,
    low_confidence_tools: &[String],
    use_native_tools: bool,
) -> String {
    let mut s = String::from("## Available tools\n\n");
    if use_native_tools {
        s.push_str(
            "Call tools using your provider's native function-calling capability. After every \
             call, the tool runs and the result is appended to the conversation as a tool \
             message. You may then continue your reply, call another tool, or write the final \
             answer. Do not explain that you are about to call a tool — just call it. Do NOT \
             emit legacy XML tool-call blocks in your prose; the wire format is native function \
             calls, and any XML you write will be shown verbatim to the user.\n\n",
        );
    } else {
        s.push_str(
            "Call a tool by emitting one or more `<tool_call>...</tool_call>` blocks in your response. \
             The block must contain valid JSON with `name` and `args`. After every block you emit, the \
             tool runs and the result is appended to the conversation as a `tool_result` system message. \
             You may then continue your reply, call another tool, or write the final answer. Do not \
             explain that you are about to call a tool — just emit the block.\n\n",
        );
    }
    s.push_str(
        "### Critical tool-selection rules\n\n\
         1. **Don't read-before-write for auto-creating tools.** When the user asks to create / add / \
         append / insert something at a known target (especially phrases like \"today's daily note\", \
         \"my daily note\", or any dated note), call the matching write tool DIRECTLY from the list \
         below. Do NOT use `roost_read` or `roost_search` to verify the target exists first — write \
         tools designed for daily-note workflows accept a `\"daily\"` shorthand on their `note_path` \
         arg and auto-create today's note if it doesn't exist yet. There is nothing to verify.\n\
         2. **Only call tools that appear in the list below.** If the user asks for an action and the \
         right tool isn't listed in your bundles, tell the user (e.g. \"this profile doesn't include \
         task tools — switch to Default in Settings → AI\"). Do NOT invent tool names or chain reads \
         to simulate a missing write. \n\
         3. **Prefer ONE tool call over a chain.** When the user's intent maps cleanly to one tool, \
         use it. Reserve `roost_search` for ambiguous discovery queries where you genuinely don't know \
         which note to act on. A read-then-write chain on a clearly-targeted write request wastes a \
         turn and risks the path-not-found dead end.\n\
         4. **Never fabricate tool history.** If the user says \"delete that task you just created\" \
         or \"undo what you just did\" and you do NOT see a corresponding successful tool call in \
         THIS conversation's history (the visible turns above), say so honestly: \"I don't have a \
         record of creating a task in this chat — could you tell me which note and task you mean?\" \
         Do NOT invent a story (\"the previous attempt errored\", \"the task was never created due \
         to X\") to explain absence — you may be in a fresh chat session and the user may be \
         referring to work from a prior session.\n\
         5. **Verify before claiming non-existence.** Before telling the user \"the task / note / \
         file does not exist\", \"was never created\", or \"there's nothing to delete\", you MUST \
         first confirm by calling `roost_read` (for note content) or `query_tasks` (for task \
         existence). A rejected tool call is NOT proof of non-existence — the user may have \
         created the item manually or in a prior session, and the rejection just means they \
         didn't want you to act on it at that moment.\n\
         6. **Disambiguation responses preserve the verb.** When you have just asked the user to \
         clarify WHICH note or task they meant — especially after \"delete that task\", \"update \
         the X\", \"complete the Y\", etc. — the user's next short reply is the IDENTIFIER for \
         the action you were already about to take. It is NOT a new instruction. \
         Example flow: user says \"delete that task you just created\", you reply \"I don't have \
         a record — which task did you mean?\", user replies \"going hyd\" → call `delete_task` \
         (or `query_tasks` first if the anchor is fuzzy) with `text_anchor: \"going hyd\"`. Do \
         NOT call `create_task` with that text — the action is still DELETE; the user just gave \
         you the target. Same principle for update / complete / archive / reschedule: the verb \
         from your clarification question carries forward. The user only switches verbs with \
         explicit override language (\"actually, never mind, add this instead\").\n\
         7. **Refer to notes the way users think of them — never as filesystem paths.** When \
         you tell the user where you saved or edited a note, use `[[Title]]` (preferred for \
         clickable references) or the roost-relative folder path WITHOUT the `.md` extension \
         (e.g. `Brain-Dump/Corvux Priority Tasks`). Do NOT echo absolute filesystem paths like \
         `W:\\Roost\\Brain-Dump\\Foo.md` — they leak machine details, look unprofessional, and \
         confuse users who think in note titles, not file paths. The tool results you receive \
         already use the roost-relative form; just quote them as-is.\n\
         8. **Reuse existing tags — do NOT invent new ones.** When `tags` is in a write tool's \
         args, pick ONLY from the `## Roost tag inventory` section in this system prompt. Prefer \
         tags with higher use counts — they thread the new note into established graph clusters. \
         A new thematic note about an existing topic should land on existing tags (e.g. \
         `#fitness #puli` not `#weekly-fitness-summary-puli-w20`). The write tool will REJECT \
         any tag not in the inventory; that's a feature, not a bug — synonym pollution \
         (`#fitness` vs `#workout` vs `#exercise`) and singleton tags fragment the knowledge \
         graph. Cap `tags` at ≤3 — more dilutes signal. If the inventory has no fitting tag, \
         omit `tags` entirely rather than inventing one; the user can add the tag manually in \
         their next note.\n\
         9. **Never pretend a note was \"provided\" when it wasn't.** If the user says \
         \"summarize this note\" / \"what are the action items here\" / \"give me the takeaways\" \
         and NO open-note context appears in the system prompt (no `## Notes mentioned by name \
         in the question:` section, no current-note body inlined, no obvious `[[wiki-link]]` in \
         the user's message), **ask the user which note they mean** instead of running \
         `roost_search` and picking whichever vaguely-relevant note shows up. Phrases like \
         \"based on the note content provided\" or \"the note above\" are a LIE when nothing was \
         actually provided to you. If the user explicitly named a note via a `[[wiki-link]]` or \
         a roost-relative path in their message, treat THAT as the target and call `roost_read` \
         on it. Otherwise: clarify before acting. Guessing burns tool calls AND erodes user \
         trust when they realize you summarized a random note.\n\n",
    );

    for bundle in bundles {
        if bundle.name() == "legacy" {
            continue;
        }
        let tools = bundle.tools();
        if tools.is_empty() {
            continue;
        }
        s.push_str(&format!(
            "### {} — {}\n\n",
            capitalize(bundle.name()),
            bundle.description()
        ));
        for tool in &tools {
            // Soft-warn the LLM when usage data shows the user
            // frequently rejects this specific tool. Doesn't remove
            // the tool from the registry — only adjusts the prompt
            // recommendation. § 4.4 of agent-hardening.
            let low_conf = low_confidence_tools.iter().any(|n| n == tool.name());
            let prefix = if low_conf {
                "(low confidence — user often rejects this) "
            } else {
                ""
            };
            s.push_str(&format!(
                "- **`{}`** — {}{}\n",
                tool.name(),
                prefix,
                tool.description()
            ));
            let schema = serde_json::to_string(&tool.input_schema()).unwrap_or_default();
            s.push_str(&format!("  Args: `{schema}`\n"));
        }
        s.push('\n');
    }

    if !use_native_tools {
        s.push_str(
            "## Tool-call format\n\n\
             ```\n\
             <tool_call>\n\
             {\"name\": \"<tool>\", \"args\": { ... }}\n\
             </tool_call>\n\
             ```\n\n\
             Output ONE JSON object per `<tool_call>` block. The JSON must be valid — escape \
             newlines inside string fields as \\n. Multiple tool calls in the same response run \
             in order; their results all arrive before your next chance to speak.\n",
        );
    }

    if !addendum.trim().is_empty() {
        s.push('\n');
        s.push_str(addendum);
        if !addendum.ends_with('\n') {
            s.push('\n');
        }
    }

    s
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

// ── Concrete bundles ───────────────────────────────────────────────
// Each bundle is a thin wrapper that lists the tool structs it owns.
// Adding a new tool = bump the relevant bundle's tools() list. Adding
// a new domain = add a new bundle struct here and add it to the
// AgentProfile::bundles() of any profile that should expose it.

use super::backlinks::BacklinksTool;
use super::graph_query::GraphQueryTool;
use super::memory::{ForgetTool, RememberTool};
use super::note_outline::NoteOutlineTool;
use super::roost_edit::RoostEditTool;
use super::roost_read::RoostReadTool;
use super::roost_search::RoostSearchTool;
use super::roost_write::{CreateNoteAliasTool, RoostWriteTool};
use super::scratchpad::{ScratchpadReadTool, ScratchpadWriteTool};
use super::skill_list::SkillListTool;
use super::skill_load::SkillLoadTool;
use super::tasks_crud::{
    ArchiveTaskTool, CompleteTaskTool, CreateTaskTool, DeleteTaskTool, QueryTasksTool,
    RescheduleTaskTool, UpdateTaskTool,
};

/// Roost tools — the primitives every agent needs to interact with the
/// user's notes: discover, read, outline, create, edit, and a working-
/// memory scratchpad to track state across long multi-step tasks.
pub struct RoostBundle;
impl ToolBundle for RoostBundle {
    fn name(&self) -> &'static str {
        "roost"
    }
    fn description(&self) -> &'static str {
        "Read, search, summarize, create, and edit notes in the roost. Scratchpad tools track state across long multi-step tasks."
    }
    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        vec![
            Arc::new(RoostSearchTool),
            Arc::new(RoostReadTool),
            Arc::new(NoteOutlineTool),
            Arc::new(RoostWriteTool),
            Arc::new(RoostEditTool),
            Arc::new(ScratchpadReadTool),
            Arc::new(ScratchpadWriteTool),
        ]
    }
}

/// Graph tools — algorithms that operate on the wiki-link graph.
/// These are the moat capabilities; they don't exist in plain Obsidian.
pub struct GraphBundle;
impl ToolBundle for GraphBundle {
    fn name(&self) -> &'static str {
        "graph"
    }
    fn description(&self) -> &'static str {
        "Run graph algorithms (PageRank, communities, shortest path, neighbors) and inspect a note's link neighborhood."
    }
    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        vec![Arc::new(GraphQueryTool), Arc::new(BacklinksTool)]
    }
}

/// Task CRUD tools — Phase 2.2 Week 2. Every write tool records an
/// `:edit_event` with `Actor::Agent` so the History pane can show and
/// revert agent edits. Edit History (Week 1) is the trust gate that
/// makes shipping these tools safe.
pub struct TasksBundle;
impl ToolBundle for TasksBundle {
    fn name(&self) -> &'static str {
        "tasks"
    }
    fn description(&self) -> &'static str {
        "Create, update, complete, reschedule, archive, delete, and query tasks. Every write is logged in the Edit History pane."
    }
    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        vec![
            Arc::new(QueryTasksTool),
            Arc::new(CreateTaskTool),
            Arc::new(UpdateTaskTool),
            Arc::new(CompleteTaskTool),
            Arc::new(RescheduleTaskTool),
            Arc::new(ArchiveTaskTool),
            Arc::new(DeleteTaskTool),
        ]
    }
}

/// Memory tools — long-term durable facts the AI carries across all
/// chat sessions via `{roost}/.corvux/memory.md`.
pub struct MemoryBundle;
impl ToolBundle for MemoryBundle {
    fn name(&self) -> &'static str {
        "memory"
    }
    fn description(&self) -> &'static str {
        "Persist or remove durable facts about the user. Visible to the AI in every future chat."
    }
    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        vec![Arc::new(RememberTool), Arc::new(ForgetTool)]
    }
}

/// Skill tools — on-demand markdown instruction packs loaded from
/// `<roost>/.corvux/skills/<name>/SKILL.md` or `~/.corvux/skills/`.
/// Both tools are Read-tier (no approval gate). Bundle is always
/// registered, even when no skills are installed yet — the empty list
/// is a valid result that helps the agent skip the discovery hop.
pub struct SkillsBundle;
impl ToolBundle for SkillsBundle {
    fn name(&self) -> &'static str {
        "skills"
    }
    fn description(&self) -> &'static str {
        "Discover and load user-authored markdown instruction packs (\"skills\") for repeated task patterns."
    }
    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        vec![Arc::new(SkillListTool), Arc::new(SkillLoadTool)]
    }
}

/// Legacy aliases — keep deprecated tool names dispatching to current
/// implementations so older system prompts and saved chat transcripts
/// don't break. Drop after a few releases.
pub struct LegacyBundle;
impl ToolBundle for LegacyBundle {
    fn name(&self) -> &'static str {
        "legacy"
    }
    fn description(&self) -> &'static str {
        "Backwards-compat aliases — do not use directly; prefer the canonical names in other bundles."
    }
    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        vec![Arc::new(CreateNoteAliasTool)]
    }
}

#[cfg(test)]
mod tests {
    //! Coverage for the system-prompt render layer. The bundle/tool
    //! impls themselves are exercised elsewhere; these tests pin the
    //! *output shape* the LLM sees — specifically the agent-hardening
    //! § 4.4 low-confidence prefix, and the legacy-bundle filter so
    //! deprecated aliases don't leak back into the prompt.
    use super::*;

    fn bundles() -> Vec<Box<dyn ToolBundle>> {
        vec![Box::new(RoostBundle), Box::new(LegacyBundle)]
    }

    #[test]
    fn low_confidence_prefix_only_on_listed_tools() {
        let low = vec!["edit_note".to_string()];
        let s = render_for_system_prompt(&bundles(), "", &low, false);
        assert!(
            s.contains("`edit_note`** — (low confidence — user often rejects this) "),
            "edit_note description must carry the soft-warn prefix; got:\n{s}"
        );
        // Sibling tools in the same bundle MUST NOT be softened.
        assert!(
            !s.contains("`roost_read`** — (low confidence"),
            "roost_read must NOT carry the prefix when not listed"
        );
    }

    #[test]
    fn empty_low_confidence_list_matches_unsoftened_render() {
        let s = render_for_system_prompt(&bundles(), "", &[], false);
        assert!(!s.contains("(low confidence"));
    }

    #[test]
    fn legacy_bundle_is_excluded_from_blurb() {
        // Legacy bundle tools still dispatch via the registry, but the
        // system prompt must not advertise them. The header `### Legacy`
        // would only appear if the bundle were rendered.
        let s = render_for_system_prompt(&bundles(), "", &[], false);
        assert!(!s.contains("### Legacy"));
        assert!(!s.contains("Backwards-compat aliases"));
    }

    #[test]
    fn addendum_appended_when_non_empty() {
        let s = render_for_system_prompt(
            &bundles(),
            "## Profile note\nResearch profile prefers graph_query.",
            &[],
            false,
        );
        assert!(s.contains("## Profile note"));
        assert!(s.ends_with("\n"), "addendum must end with newline");
    }

    /// Regression — native-tools branch must NOT instruct the model to
    /// emit `<tool_call>` XML blocks. Gemini observed using the XML
    /// format on the v2 path because the legacy opener was still in the
    /// system prompt; the agent loop then saw zero native calls,
    /// declared the iter FINAL, and the frontend stripped the XML
    /// leaving a blank model bubble.
    #[test]
    fn native_tools_branch_omits_xml_instructions() {
        let s = render_for_system_prompt(&bundles(), "", &[], true);
        assert!(
            !s.contains("<tool_call>"),
            "native branch must not mention <tool_call> markup; got:\n{s}"
        );
        assert!(
            !s.contains("## Tool-call format"),
            "native branch must not render the XML format trailer; got:\n{s}"
        );
        assert!(
            s.contains("native function-calling"),
            "native branch should name the wire format so the model knows which to use"
        );
        // Tool-selection rules are wire-format-agnostic and must still
        // render in the native branch — they're how we keep the agent
        // honest about not fabricating tool history, not chaining
        // reads-before-writes, etc.
        assert!(
            s.contains("### Critical tool-selection rules"),
            "rules section must render regardless of wire format"
        );
    }

    /// Regression — the tag-reuse rule must render in BOTH wire-format
    /// branches. Without it, the agent had no incentive to reuse existing
    /// tags and was inventing one-shot tags (`#weekly-fitness-summary-puli`)
    /// that fragmented the knowledge graph with singleton clusters.
    #[test]
    fn tag_reuse_rule_present_in_both_branches() {
        for native in [false, true] {
            let s = render_for_system_prompt(&bundles(), "", &[], native);
            assert!(
                s.contains("Reuse existing tags"),
                "tag-reuse rule must render (native={native}); got:\n{s}"
            );
            assert!(
                s.contains("Roost tag inventory"),
                "rule must reference the inventory section (native={native})"
            );
        }
    }

    #[test]
    fn legacy_tools_branch_keeps_xml_instructions() {
        let s = render_for_system_prompt(&bundles(), "", &[], false);
        assert!(s.contains("<tool_call>"));
        assert!(s.contains("## Tool-call format"));
    }
}
