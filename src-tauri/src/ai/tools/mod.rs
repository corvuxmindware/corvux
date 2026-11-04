//! Agentic tool registry — Tier-1 capabilities + bundle / profile factory.
//!
//! See [`docs/agentic-capabilities.md`](../../../docs/agentic-capabilities.md)
//! for the architecture spec. The agent loop streams an LLM response, parses
//! `<tool_call>...</tool_call>` blocks, dispatches each through a registered
//! [`Tool`] impl, appends results to the conversation, and re-streams until
//! the LLM emits no more tool calls or the iteration budget is exhausted.
//!
//! ## Layout
//!
//! - `registry.rs` — `Tool` trait, `ToolRegistry`, `SafetyTier`, `ToolError`,
//!   `ToolContext`. The pure plumbing.
//! - `agent_loop/` — `AgentBudget`, `AgentLoopConfig` (in `budget.rs`)
//!   plus `run_agent_loop` (V1, deprecated) and `run_agent_loop_v2`
//!   (in `mod.rs`). Split is mid-flight per AUDIT-035; see the
//!   tracking comment at the top of `agent_loop/mod.rs`.
//! - `bundle.rs` — `ToolBundle` trait + concrete bundles (`RoostBundle`,
//!   `GraphBundle`, `MemoryBundle`, `LegacyBundle`). Group tools by domain
//!   so adding a new capability is an append, not a registry edit.
//! - `profile.rs` — `AgentProfile` (Default / QuickLookup / Research /
//!   Writer). Each profile selects bundles + budget + prompt addendum.
//!   This is the unit the user / settings picks.
//! - Per-tool files (`roost_search.rs`, `memory.rs`, …) — concrete `Tool`
//!   impls. Stay flat for now; if individual modules grow, group them in
//!   sub-folders.
//!
//! ## Adding a new capability
//!
//! 1. Implement the tool in `ai/tools/<name>.rs`.
//! 2. Add it to the `tools()` list of the appropriate bundle in
//!    `bundle.rs`. (New domain → add a new `ToolBundle` impl, then list
//!    it on whichever `AgentProfile` should expose it.)
//! 3. Register `pub mod <name>;` here.
//!
//! That's it — no other files touch.
//!
//! Why a homegrown text protocol vs. provider-native function calling: the
//! same `<tool_call>` shape works on Gemini, llama.cpp, and any future
//! provider that can output structured JSON in plain text. Phase B (per
//! the spec) will switch Gemini to native function-calling once a Claude /
//! OpenAI provider lands and the round-trip cost of the regex parser
//! starts to matter.

mod agent_loop;
pub mod approval;
mod bundle;
mod compact;
mod glob;
// pub(crate): the path sanitizers (sanitize_for_llm / scrub_roost_paths) are
// exercised by the AI-8 privacy-boundary tests in `ai::tool_translator`.
pub(crate) mod note_ref;
mod parse;
mod profile;
mod registry;
mod result;
mod schema;
pub mod secret_scanner;
pub mod trace;
mod truncate;

pub mod backlinks;
pub mod graph_query;
pub mod memory;
pub mod note_outline;
pub mod roost_edit;
pub mod roost_read;
pub mod roost_search;
pub mod roost_write;
pub mod scratchpad;
pub mod skill_list;
pub mod skill_load;
pub mod subagent_list;
pub mod task;
pub mod tasks_crud;

// AUDIT-035: `run_agent_loop` (V1) is `#[deprecated]` at its
// definition. The re-export here is intentional — the chat IPC still
// dispatches to V1 when `settings.use_native_tools = false`. Drop the
// allow when that flag is removed.
#[allow(unused_imports)] // public surface — AUDIT-035 acceptance criterion
pub use agent_loop::clean_prior_text;
#[allow(deprecated)]
pub use agent_loop::{run_agent_loop, run_agent_loop_v2};
#[allow(unused_imports)] // public surface — first consumers are tests + future UI
pub use agent_loop::{AgentBudget, AgentLoopConfig};
#[allow(unused_imports)]
pub use bundle::{
    registry_from_bundles, GraphBundle, LegacyBundle, MemoryBundle, RoostBundle, SkillsBundle,
    TasksBundle, ToolBundle,
};
#[allow(unused_imports)]
pub use parse::{extract_tool_calls, ToolCall};
pub use profile::AgentProfile;
pub use registry::ToolContext;
#[allow(unused_imports)]
pub use registry::{SafetyTier, Tool, ToolError, ToolRegistry};
#[allow(unused_imports)]
pub use result::{emit_tool_result, SecretWarningEvent, ToolResult};
#[allow(unused_imports)] // first consumers are the Default profile + chat.rs wiring
pub use task::SubagentBundle;
#[allow(unused_imports)]
pub use truncate::{truncate_tool_result, MAX_TOOL_RESULT_BYTES};

/// Default registry — kept for callers that want every tool without
/// caring about profiles. Internally just `AgentProfile::Default`'s
/// bundle list, so it stays in sync as bundles are added.
#[allow(dead_code)] // call site retired in favor of AgentProfile::build(); kept for tests
pub fn default_registry() -> ToolRegistry {
    AgentProfile::Default.build().registry
}

#[cfg(test)]
mod no_raw_fs_writes_test {
    //! AUDIT-033 regression: prevent new roost-write sites from sneaking
    //! `std::fs::write` / `rename` / `remove_file` back into agent tools.
    //! Every such call bypasses `RoostFs` and the `roost://tree-changed`
    //! sync chain — the 2026-05-16 `tasks_crud` incident is exactly this.
    //!
    //! Scan every `.rs` in this directory (non-recursively), strip
    //! `#[cfg(test)]` blocks and any `mod tests { ... }` block (tests
    //! freely use raw std::fs for fixtures — that's fine), then fail if
    //! any forbidden call remains outside the allowlist below.
    //!
    //! Allowlist entries must be a short comment explaining why the call
    //! is legitimate (e.g. writes to the app-data directory, not the
    //! roost).
    use std::fs;
    use std::path::Path;
    // PR-C: the forbidden set is matched as a plain substring, so the bare
    // forms below also catch the fully-qualified `std::fs::write(` etc. It also
    // now includes `.write_atomic(` — a direct RoostFs write bypasses the sync
    // notify + `roost://tree-changed` chain (RoostFs alone touches only disk),
    // so every agent write must route through
    // the shared create/mutate helpers in `commands::files` instead.
    const FORBIDDEN: &[&str] = &[
        "fs::write(",
        "fs::rename(",
        "fs::remove_file(",
        ".write_atomic(",
    ];

    /// Files in `ai/tools/` that may keep raw filesystem writes. Each entry
    /// is the bare filename; the reason explains why the call doesn't
    /// belong to the roost sync chain.
    const ALLOWLIST: &[(&str, &str)] = &[
        // `approval.rs` persists agent-autonomy decisions into the app
        // data dir (settings.json), not the user's roost — sync is not
        // and should not be involved.
        (
            "approval.rs",
            "writes settings.json in app-data, not the roost",
        ),
    ];

    fn strip_balanced_block(src: &str, header: &str) -> String {
        let mut out = String::with_capacity(src.len());
        let mut rest = src;
        while let Some(idx) = rest.find(header) {
            out.push_str(&rest[..idx]);
            // Find the opening brace after `header`.
            let after = &rest[idx + header.len()..];
            let Some(brace_off) = after.find('{') else {
                // Header without a body — keep it as-is and move on.
                out.push_str(&rest[idx..idx + header.len()]);
                rest = after;
                continue;
            };
            // Walk braces to find the matching close.
            let body_start = idx + header.len() + brace_off + 1;
            let bytes = rest.as_bytes();
            let mut depth: i32 = 1;
            let mut pos = body_start;
            while pos < bytes.len() && depth > 0 {
                match bytes[pos] {
                    b'{' => depth += 1,
                    b'}' => depth -= 1,
                    _ => {}
                }
                pos += 1;
            }
            // Drop the `header { ... }` slice entirely.
            rest = &rest[pos..];
        }
        out.push_str(rest);
        out
    }

    #[test]
    fn no_raw_fs_writes_outside_allowlist() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src")
            .join("ai")
            .join("tools");
        let entries =
            fs::read_dir(&dir).unwrap_or_else(|e| panic!("read_dir {}: {e}", dir.display()));

        let mut violations: Vec<String> = Vec::new();
        for entry in entries {
            let entry = entry.expect("dir entry");
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("rs") {
                continue;
            }
            let name = path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();
            if ALLOWLIST.iter().any(|(f, _)| *f == name) {
                continue;
            }
            let src = fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
            // Strip `#[cfg(test)] mod ...` and bare `mod tests { ... }`
            // blocks. Both shapes appear across this folder; either form
            // is allowed to use raw std::fs for fixtures.
            let stripped = strip_balanced_block(&src, "#[cfg(test)]\nmod");
            let stripped = strip_balanced_block(&stripped, "mod tests");

            for forbidden in FORBIDDEN {
                if stripped.contains(forbidden) {
                    violations.push(format!("{name}: contains `{forbidden}`"));
                }
            }
        }

        assert!(
            violations.is_empty(),
            "AUDIT-033: agent tools must route writes through RoostFs.\n\
             Violations:\n  {}\n\
             If a write is legitimate (e.g. app-data, not roost), add it \
             to ALLOWLIST with a short justification.",
            violations.join("\n  ")
        );
    }
}
