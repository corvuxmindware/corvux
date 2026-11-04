//! Agent profiles — preset combinations of `ToolBundle`s, budgets, and
//! prompt addenda that define what kind of agent runs for a given turn.
//!
//! ## Why profiles
//!
//! Different conversations want different agents:
//!
//! - A **quick lookup** ("what's in [[note]]?") wants ≤3 iterations,
//!   a small tool set, a terse prompt — all about latency.
//! - A **research session** ("what do I know about X across my whole
//!   roost?") wants 12+ iterations, every tool available, a generous
//!   token budget — accuracy over speed.
//! - A **writer** assistant doesn't need graph algorithms, but does
//!   need write access to the roost.
//!
//! A profile is the unit the user (or future routing logic) picks. The
//! `chat_send` command resolves a profile name from settings, builds an
//! `AgentLoopConfig` from it, and feeds that to the loop. New profiles =
//! new variants here, not a refactor of the loop.
//!
//! Profiles are intentionally string-stable across the wire (serde
//! `lowercase`) so the frontend / settings file can carry them without
//! touching Rust enums.

use serde::{Deserialize, Serialize};

use super::agent_loop::{AgentBudget, AgentLoopConfig};
use super::bundle::{
    registry_from_bundles, GraphBundle, LegacyBundle, MemoryBundle, RoostBundle, SkillsBundle,
    TasksBundle, ToolBundle,
};
// SubagentBundle import kept so unhide is a one-line restore in the
// match below — see the 2026-05-20 deferral note in `bundles()`.
#[allow(unused_imports)]
use super::task::SubagentBundle;

/// Preset agent shapes. Add a variant when a new use case earns its
/// own latency / capability tradeoff — not for every minor variation.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AgentProfile {
    /// Balanced default — every bundle, balanced budget. Used unless
    /// the user explicitly picks a specialized profile in settings.
    #[default]
    Default,
    /// Latency-first lookup. Roost + Memory only, max 3 iterations,
    /// smaller token budget. For "what's in [[X]]?", "find Y", etc.
    QuickLookup,
    /// Accuracy-first deep dive. Every bundle, 12 iterations, big
    /// budget. For multi-hop research over the whole roost.
    Research,
    /// Note-writing assistant. Roost + Memory; no graph distractions.
    /// Encourages structured drafts + frontmatter.
    Writer,
}

impl AgentProfile {
    /// All profiles, in display order — useful for the settings UI.
    pub fn all() -> &'static [AgentProfile] {
        &[
            AgentProfile::Default,
            AgentProfile::QuickLookup,
            AgentProfile::Research,
            AgentProfile::Writer,
        ]
    }

    /// Stable string id used in settings + serde.
    pub fn as_str(self) -> &'static str {
        match self {
            AgentProfile::Default => "default",
            AgentProfile::QuickLookup => "quicklookup",
            AgentProfile::Research => "research",
            AgentProfile::Writer => "writer",
        }
    }

    /// Human-readable label for UI.
    pub fn label(self) -> &'static str {
        match self {
            AgentProfile::Default => "Default",
            AgentProfile::QuickLookup => "Quick Lookup",
            AgentProfile::Research => "Research",
            AgentProfile::Writer => "Writer",
        }
    }

    /// One-line summary shown next to the profile name in pickers.
    pub fn summary(self) -> &'static str {
        match self {
            AgentProfile::Default => "Balanced — all tools, normal budget.",
            AgentProfile::QuickLookup => "Fast — roost + memory only, 3 iterations.",
            AgentProfile::Research => "Thorough — all tools, 12 iterations, large budget.",
            AgentProfile::Writer => "Drafting — roost + memory, no graph noise.",
        }
    }

    /// Wire-friendly tuple (`id`, `label`, `summary`) — used by the
    /// `list_agent_profiles` Tauri command so the frontend can render a
    /// picker without duplicating the metadata.
    pub fn descriptor(self) -> (&'static str, &'static str, &'static str) {
        (self.as_str(), self.label(), self.summary())
    }

    /// The bundles that should be registered for this profile. New
    /// bundles get wired in here per-profile so each can opt in or out.
    fn bundles(self) -> Vec<Box<dyn ToolBundle>> {
        // SkillsBundle is wired into every profile — the cost of two
        // extra schema lines in the prompt blurb is negligible, and
        // surfacing skills consistently lets a user-authored SKILL.md
        // work the same in every chat mode.
        //
        // SubagentBundle removed from every profile on 2026-05-20.
        // Reason: only the builtin `general` subagent ships and it
        // duplicates the parent's model + tools, so the LLM correctly
        // never delegated to it (zero `task` invocations across all
        // persisted chat sessions). The bundle was ~600 chars of pure
        // prompt overhead. The registry, manifest parser, builtin, and
        // task tool stay compiled so the unhide is one-line: add
        // `Box::new(SubagentBundle)` back to the profile(s) where you
        // want delegation available, ideally only AFTER shipping at
        // least one specialized AGENT.md so the parent has somewhere
        // worth routing to (e.g. weekly-summarizer, task-triager).
        match self {
            AgentProfile::Default => vec![
                Box::new(RoostBundle),
                Box::new(TasksBundle),
                Box::new(GraphBundle),
                Box::new(MemoryBundle),
                Box::new(SkillsBundle),
                Box::new(LegacyBundle),
            ],
            AgentProfile::QuickLookup => vec![
                Box::new(RoostBundle),
                Box::new(TasksBundle),
                Box::new(MemoryBundle),
                Box::new(SkillsBundle),
                Box::new(LegacyBundle),
            ],
            AgentProfile::Research => vec![
                Box::new(RoostBundle),
                Box::new(TasksBundle),
                Box::new(GraphBundle),
                Box::new(MemoryBundle),
                Box::new(SkillsBundle),
                Box::new(LegacyBundle),
            ],
            AgentProfile::Writer => vec![
                Box::new(RoostBundle),
                Box::new(TasksBundle),
                Box::new(MemoryBundle),
                Box::new(SkillsBundle),
                Box::new(LegacyBundle),
            ],
        }
    }

    /// Per-profile iteration / token budget. Values are intentionally
    /// distinct so the latency/accuracy tradeoff is visible at a glance.
    fn budget(self) -> AgentBudget {
        match self {
            AgentProfile::Default => AgentBudget::default(), // 8 iter, 100KB
            AgentProfile::QuickLookup => AgentBudget {
                max_iterations: 3,
                max_tool_result_bytes: 32_000,
                max_wall_clock_secs: 60,
                max_tool_calls_per_iter: 4,
            },
            AgentProfile::Research => AgentBudget {
                max_iterations: 12,
                max_tool_result_bytes: 200_000,
                max_wall_clock_secs: 600,
                max_tool_calls_per_iter: 8,
            },
            AgentProfile::Writer => AgentBudget {
                max_iterations: 6,
                max_tool_result_bytes: 80_000,
                max_wall_clock_secs: 300,
                max_tool_calls_per_iter: 6,
            },
        }
    }

    /// Optional system-prompt addendum that nudges the LLM toward this
    /// profile's behavior. Returned as a markdown block; chat.rs appends
    /// it after the registry blurb. Empty string = no addendum.
    pub fn system_prompt_addendum(self) -> &'static str {
        match self {
            AgentProfile::Default => "",
            AgentProfile::QuickLookup => {
                "## Profile: Quick Lookup\n\
                 Optimize for latency. Aim for 1–2 tool calls maximum, then \
                 produce the final answer. Skip preamble; jump to the result.\n"
            }
            AgentProfile::Research => {
                "## Profile: Research\n\
                 Optimize for accuracy. Use multiple tool calls if needed: \
                 roost_search to discover, graph_query (page_rank / communities) \
                 to map structure, roost_read to confirm. Cite each [[note]] in \
                 the final answer so the user can verify.\n"
            }
            AgentProfile::Writer => {
                "## Profile: Writer\n\
                 You are helping the user draft new notes. Prefer concrete \
                 markdown output (headings, bullets, frontmatter) over \
                 long-form prose. Use roost_search before writing to avoid \
                 contradicting existing notes; use roost_write to create \
                 drafts when the user asks.\n"
            }
        }
    }

    /// Build the full `AgentLoopConfig` for this profile — registry
    /// assembled from the profile's bundles, budget set per profile.
    pub fn build(self) -> AgentLoopConfig {
        AgentLoopConfig {
            registry: registry_from_bundles(&self.bundles()),
            budget: self.budget(),
        }
    }

    /// Render the system-prompt blurb (tool list grouped by bundle, plus
    /// this profile's addendum). Replaces the legacy flat-list renderer
    /// for chat.rs callers — bundles surface in headed sections so a
    /// 10+ tool inventory stays scannable.
    ///
    /// `low_confidence_tools` is forwarded to the bundle renderer so
    /// tools with high recent rejection rates get a "(low confidence)"
    /// qualifier inline with their description. Pass `&[]` to get the
    /// pre-scorecard render unchanged.
    pub fn render_system_prompt_blurb(
        self,
        low_confidence_tools: &[String],
        use_native_tools: bool,
    ) -> String {
        super::bundle::render_for_system_prompt(
            &self.bundles(),
            self.system_prompt_addendum(),
            low_confidence_tools,
            use_native_tools,
        )
    }

    /// Resolve a profile from its stable string id. Falls back to
    /// `Default` for unknown values so a typo in settings can't break
    /// chat.
    pub fn from_str(s: &str) -> AgentProfile {
        match s {
            "quicklookup" => AgentProfile::QuickLookup,
            "research" => AgentProfile::Research,
            "writer" => AgentProfile::Writer,
            _ => AgentProfile::Default,
        }
    }
}
