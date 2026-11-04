//! `Tool` trait, registry, safety classification, error types, and the
//! shared execution context.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::Serialize;
use tauri::{AppHandle, Manager, Runtime, Wry};

use std::sync::atomic::AtomicBool;

use crate::ai::skills::SkillRegistry;
use crate::ai::subagents::SubagentRegistry;
use crate::ai::LLMProvider;
use crate::commands::ai::types::AgentAutonomy;

use super::truncate::MAX_TOOL_RESULT_BYTES;

/// Safety classification for tool authorization.
///
/// Today the agent loop runs every safety tier without prompting (Phase 1
/// — same UX as the legacy single-shot `create_note` exec). The frontend
/// receives a `ai:tool-result` event for every call so the user can see
/// what happened. A real approval gate (block-on tool-card accept) lands
/// in a follow-up once the UI exists.
#[allow(dead_code)] // Write/Destructive surface is wired in Phase 2 (tool-card UI)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize, specta::Type)]
#[serde(rename_all = "lowercase")]
pub enum SafetyTier {
    /// Read-only — no confirmation needed.
    Read,
    /// Creates or modifies data. Future: requires user approval.
    Write,
    /// Destructive (delete, overwrite). Future: requires explicit
    /// confirmation with undo. Phase 2 only — no Tier-1 tool returns
    /// this today.
    Destructive,
}

/// Tool execution error — surfaced to the LLM as a structured message
/// so it can choose to retry with adjusted args.
#[derive(Debug, Clone)]
pub struct ToolError {
    pub message: String,
    /// True when the LLM should NOT retry (e.g. invalid roost path,
    /// permission denied). False when a retry with different args might
    /// succeed (e.g. note title not found — try a similar one).
    pub fatal: bool,
}

impl ToolError {
    pub fn retryable(msg: impl Into<String>) -> Self {
        Self {
            message: msg.into(),
            fatal: false,
        }
    }

    pub fn fatal(msg: impl Into<String>) -> Self {
        Self {
            message: msg.into(),
            fatal: true,
        }
    }
}

impl std::fmt::Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl From<String> for ToolError {
    fn from(s: String) -> Self {
        ToolError::retryable(s)
    }
}

impl From<&str> for ToolError {
    fn from(s: &str) -> Self {
        ToolError::retryable(s.to_string())
    }
}

/// Shared state every tool execution gets. Cheap to clone: `AppHandle`
/// is internally `Arc`'d, the rest are owned strings or `None`.
///
/// Generic over Tauri runtime `R` (defaults to `Wry`). Production code
/// reads `ToolContext` unchanged; tests construct `ToolContext<MockRuntime>`
/// from `tauri::test::mock_app()` so the agent loop can run end-to-end
/// without a real Tauri app.
#[derive(Clone)]
pub struct ToolContext<R: Runtime + 'static = Wry> {
    pub app: AppHandle<R>,
    /// Absolute path to the currently-open roost. Required by every
    /// roost-touching tool — execute() returns a fatal error when the
    /// caller didn't open a roost first.
    pub roost_path: Option<String>,
    /// Generation of the installed `RoostFs` captured with `roost_path`.
    /// Every roost-touching tool verifies this before it runs so an agent
    /// started in roost A cannot continue reading or writing after the user
    /// switches to roost B.
    pub roost_generation: Option<u64>,
    /// Currently-open note path (if any). Used by `backlinks` /
    /// `note_outline` when the LLM omits the path arg, and as the
    /// `context_note_id` field for `ai_signals` rows.
    #[allow(dead_code)] // wired by ai_signals consumer (Phase 2 tool-card UI)
    pub open_note_path: Option<String>,
    /// Skill registry — populated by `lib.rs` setup so the `skill_list`
    /// and `skill_load` tools can resolve user-authored SKILL.md files.
    /// `None` in tests and on cold start before a roost is opened; both
    /// skill tools surface a friendly "no skills available" message in
    /// that case rather than crashing.
    pub skill_registry: Option<Arc<SkillRegistry>>,
    /// ─── Subagent state (Phase 2.C) ───────────────────────────────
    ///
    /// All five fields below are `None` outside the chat agent loop
    /// (tests, IPC commands that aren't `chat_send`) and `Some(_)`
    /// inside it. The `task` tool reads them to spin up a child loop;
    /// every other tool ignores them.
    ///
    /// Depth in the subagent chain. `0` for the top-level conversation,
    /// `1` inside a `task`-spawned subagent. The `task` tool refuses to
    /// dispatch when this is already >= 1 — depth-2 grandchildren are
    /// almost always runaway loops, not real orchestration.
    pub subagent_depth: u8,
    /// Snapshot of the parent's tool registry — the `task` tool filters
    /// this to build the child's narrowed registry (parent tools minus
    /// `task` itself minus Destructive tier by default). `None` when
    /// constructed outside the chat loop.
    pub parent_registry: Option<ToolRegistry<R>>,
    /// Map of subagent name → SubagentDef. Populated by lib.rs setup;
    /// the `task` tool reads it to look up the requested `subagent_type`.
    pub subagent_registry: Option<Arc<SubagentRegistry>>,
    /// Shared chat provider — the same `Arc<dyn LLMProvider>` the
    /// parent loop is using. Cloning is free (Arc bump); subagent
    /// inherits the parent's model and configuration unless AGENT.md
    /// overrides it (model override is reserved for a future change).
    pub parent_chat_provider: Option<Arc<dyn LLMProvider<R>>>,
    /// Parent's iteration / token / wall-clock budget. The child's
    /// effective budget is `SubagentDef::budget_or(parent_budget)` so
    /// AGENT.md can tighten or loosen any individual axis.
    pub parent_budget: Option<super::agent_loop::AgentBudget>,
    /// Parent's autonomy snapshot. The child gets a *clone* — its
    /// "always approve this tool" decisions are local to its loop and
    /// do not propagate back to the parent.
    pub parent_autonomy: Option<AgentAutonomy>,
    /// Shared cancel flag — pressing Stop in the UI must kill the
    /// parent AND every in-flight subagent. The `task` tool clones
    /// this Arc into the child loop's `cancel` argument so both
    /// loops poll the same atomic.
    pub cancel: Option<Arc<AtomicBool>>,
}

impl<R: Runtime + 'static> ToolContext<R> {
    pub fn require_roost(&self, tool: &str) -> Result<&str, ToolError> {
        let roost = self
            .roost_path
            .as_deref()
            .ok_or_else(|| ToolError::fatal(format!("{tool}: no roost open")))?;
        let expected = self
            .roost_generation
            .ok_or_else(|| ToolError::fatal(format!("{tool}: roost scope is not bound")))?;
        let state = self
            .app
            .try_state::<crate::commands::files::RoostFsState>()
            .ok_or_else(|| ToolError::fatal(format!("{tool}: roost filesystem unavailable")))?;
        if !state.is_generation_current(expected) {
            return Err(ToolError::fatal(format!(
                "{tool}: roost changed while the agent was running"
            )));
        }
        Ok(roost)
    }
}

/// Production tool trait. Every tool implements this and registers via
/// [`ToolRegistry::register`].
///
/// Generic over Tauri runtime `R` (defaults to `Wry`). Most prod impls
/// use `impl<R: Runtime + 'static> Tool<R> for FooTool` so a single
/// declaration serves both Wry and MockRuntime — every method the
/// existing tool bodies call on `AppHandle<R>` comes through the
/// `Manager<R>` trait, which is impl'd for any `R`.
#[async_trait]
pub trait Tool<R: Runtime + 'static = Wry>: Send + Sync {
    /// Unique identifier matching `tool_call.name` (e.g. `roost_search`).
    fn name(&self) -> &'static str;

    /// One-line description injected into the system prompt so the LLM
    /// knows when to use this tool.
    fn description(&self) -> &'static str;

    /// JSON Schema fragment describing input args. Today this only feeds
    /// the system-prompt blurb; runtime validation is left to each
    /// `execute` impl. Wiring up `jsonschema` validation is straightforward
    /// once the LLM starts emitting malformed args at scale.
    fn input_schema(&self) -> serde_json::Value;

    #[allow(dead_code)] // wired by Phase 2 tool-card approval flow
    fn safety_tier(&self) -> SafetyTier;

    /// Maximum bytes this tool may return after [`super::truncate_tool_result`].
    /// Default is the global cap; override for tools that need less
    /// (e.g. note_outline returns a few hundred bytes per note).
    fn max_result_bytes(&self) -> usize {
        MAX_TOOL_RESULT_BYTES
    }

    /// Per-call timeout. Read tools default to 30s, write tools should
    /// be quicker — override on the impl if needed.
    fn timeout(&self) -> Duration {
        Duration::from_secs(30)
    }

    /// Execute the tool. Receives validated args + shared context.
    /// Returns structured JSON the agent loop appends to the conversation.
    async fn execute(
        &self,
        ctx: &ToolContext<R>,
        args: serde_json::Value,
    ) -> Result<serde_json::Value, ToolError>;
}

/// Registry mapping `name` → `Tool` impl. Construction is sync; dispatch
/// is async.
///
/// Generic over Tauri runtime `R`. A single `ToolRegistry<MockRuntime>`
/// is the test entry point — register mock tools with whatever schemas
/// the test needs, then drive the agent loop against it.
pub struct ToolRegistry<R: Runtime + 'static = Wry> {
    tools: HashMap<&'static str, Arc<dyn Tool<R>>>,
}

impl<R: Runtime + 'static> Default for ToolRegistry<R> {
    fn default() -> Self {
        Self {
            tools: HashMap::new(),
        }
    }
}

impl<R: Runtime + 'static> Clone for ToolRegistry<R> {
    fn clone(&self) -> Self {
        Self {
            tools: self.tools.clone(),
        }
    }
}

impl<R: Runtime + 'static> ToolRegistry<R> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, tool: Arc<dyn Tool<R>>) {
        self.tools.insert(tool.name(), tool);
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool<R>>> {
        self.tools.get(name).cloned()
    }

    #[allow(dead_code)] // first consumer is the test harness
    pub fn names(&self) -> Vec<&'static str> {
        self.tools.keys().copied().collect()
    }

    /// Snapshot the registry as a list of [`ToolDef`]s for the native
    /// function-calling path. Built once per agent-loop invocation and
    /// passed to the provider so the API can enforce the input schema
    /// before responses come back. Order matches `names()`.
    pub fn to_tool_defs(&self) -> Vec<crate::ai::tool_protocol::ToolDef> {
        self.tools
            .values()
            .map(|t| crate::ai::tool_protocol::ToolDef {
                name: t.name().to_string(),
                description: t.description().to_string(),
                input_schema: t.input_schema(),
            })
            .collect()
    }

    /// Render a markdown blurb listing every tool with its description and
    /// input schema. Goes straight into the system prompt so the LLM has
    /// the inventory it can call from. Bundle-aware grouped rendering
    /// (recommended) lives at `bundle::render_for_system_prompt`; this
    /// flat one stays available for tests and ad-hoc registries.
    #[allow(dead_code)] // bundle-grouped rendering preferred; this path is for tests
    pub fn render_for_system_prompt(&self) -> String {
        let mut s = String::from("## Available tools\n\n");
        s.push_str(
            "Call a tool by emitting one or more `<tool_call>...</tool_call>` blocks in your response. \
             The block must contain valid JSON with `name` and `args`. After every block you emit, the \
             tool runs and the result is appended to the conversation as a `tool_result` system message. \
             You may then continue your reply, call another tool, or write the final answer. Do not \
             explain that you are about to call a tool — just emit the block.\n\n",
        );
        let mut names: Vec<_> = self.tools.keys().copied().collect();
        names.sort();
        for name in names {
            let Some(t) = self.tools.get(name) else {
                continue;
            };
            s.push_str(&format!("### `{}`\n", t.name()));
            s.push_str(&format!("{}\n\n", t.description()));
            let schema = serde_json::to_string_pretty(&t.input_schema()).unwrap_or_default();
            s.push_str(&format!("Args schema:\n```json\n{schema}\n```\n\n"));
        }
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
        s
    }
}

#[cfg(test)]
mod tool_defs_tests {
    use super::*;
    use async_trait::async_trait;
    use serde_json::json;
    use tauri::Wry;

    /// Minimal `Tool` impl for asserting the registry → ToolDef
    /// pipeline shape. Returns hard-coded metadata; `execute` panics
    /// because these tests never dispatch.
    struct StubTool {
        name: &'static str,
        description: &'static str,
        schema: serde_json::Value,
    }

    #[async_trait]
    impl Tool<Wry> for StubTool {
        fn name(&self) -> &'static str {
            self.name
        }
        fn description(&self) -> &'static str {
            self.description
        }
        fn input_schema(&self) -> serde_json::Value {
            self.schema.clone()
        }
        fn safety_tier(&self) -> SafetyTier {
            SafetyTier::Read
        }
        async fn execute(
            &self,
            _ctx: &ToolContext<Wry>,
            _args: serde_json::Value,
        ) -> Result<serde_json::Value, ToolError> {
            unreachable!("StubTool::execute is not used in tool_defs_tests")
        }
    }

    #[test]
    fn to_tool_defs_emits_one_def_per_registered_tool() {
        let mut reg: ToolRegistry<Wry> = ToolRegistry::new();
        reg.register(Arc::new(StubTool {
            name: "roost_read",
            description: "Read notes",
            schema: json!({"type":"object","required":["paths"]}),
        }));
        reg.register(Arc::new(StubTool {
            name: "roost_search",
            description: "Search notes",
            schema: json!({"type":"object","required":["query"]}),
        }));
        let mut defs = reg.to_tool_defs();
        defs.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(defs.len(), 2);
        assert_eq!(defs[0].name, "roost_read");
        assert_eq!(defs[0].description, "Read notes");
        assert_eq!(defs[0].input_schema["required"][0], "paths");
        assert_eq!(defs[1].name, "roost_search");
        assert_eq!(defs[1].input_schema["required"][0], "query");
    }

    #[test]
    fn to_tool_defs_empty_registry_returns_empty_vec() {
        let reg: ToolRegistry<Wry> = ToolRegistry::new();
        assert!(reg.to_tool_defs().is_empty());
    }
}
