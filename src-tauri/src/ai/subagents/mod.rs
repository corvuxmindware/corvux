//! Subagents — isolated-context delegation. The parent agent can call
//! the `task` tool to spawn a fresh agent-loop with its own context
//! window, its own tool allowlist, and (later) its own model.
//!
//! ## Layout
//! - `<roost>/.corvux/agents/<name>/AGENT.md` (roost-local, top precedence)
//! - `~/.corvux/agents/<name>/AGENT.md` (global, mid precedence)
//! - compiled-in built-ins (always available — guarantees `general`)
//!
//! Subagents are capped at depth 1: a subagent's `task` tool is not in
//! its inherited registry, so it can't spawn further nested loops.

mod builtin;
mod manifest;
mod registry;
mod watcher;

#[allow(unused_imports)] // first consumers are TaskTool + IPC commands
pub use manifest::{is_valid_subagent_name, parse_agent_md, SubagentDef, SubagentParseError};
#[allow(unused_imports)]
pub use registry::{
    global_agents_dir, roost_agents_dir, RefreshOutcome, SubagentEntry, SubagentRegistry,
    SubagentSource,
};
#[allow(unused_imports)]
pub use watcher::{spawn_default_watcher, spawn_watcher, SubagentWatcher};
