//! Budget + configuration types for the agent loop.
//!
//! Extracted from the monolithic `agent_loop.rs` as part of AUDIT-035.
//! Kept deliberately tiny + side-effect-free so the surrounding
//! orchestrator can refactor around it without churning these types.

use tauri::{Runtime, Wry};

use crate::ai::tools::registry::ToolRegistry;

/// Token / iteration budget for a single agent loop invocation.
#[derive(Debug, Clone, Copy)]
pub struct AgentBudget {
    /// Hard ceiling on tool-call iterations. Once hit, the loop stops
    /// even if the LLM is still asking for tools.
    pub max_iterations: usize,
    /// Soft ceiling on cumulative tool-result bytes across the loop.
    /// When approached, the loop returns a budget warning instead of
    /// re-streaming. Tracks bytes (rough proxy for tokens) — replace
    /// with a real tokenizer once a Claude/OpenAI provider lands.
    pub max_tool_result_bytes: usize,
    /// Wall-clock ceiling for the entire loop. Defends against a
    /// stuck-streaming + tool-dispatch cycle that the iteration cap
    /// alone doesn't bound (LLM stream stalls don't count as an
    /// iteration). Once exceeded, the loop forces a final no-tool
    /// pass with whatever context has accumulated.
    pub max_wall_clock_secs: u64,
    /// Cap on the number of `<tool_call>` blocks executed in a single
    /// iteration. Each LLM response can legitimately ask for multiple
    /// tools in parallel; this stops a single response from dispatching
    /// hundreds before the iteration counter advances. Excess calls
    /// are dropped with a structured note back to the LLM.
    pub max_tool_calls_per_iter: usize,
}

impl Default for AgentBudget {
    fn default() -> Self {
        Self {
            max_iterations: 8,
            // 100K bytes ≈ 25K tokens — leaves room for system prompt
            // + history within a 32K-token window. Generous enough that
            // a 5-iteration loop with average tool results never trips
            // it; tight enough that pathological cases don't blow the
            // window silently.
            max_tool_result_bytes: 100_000,
            // 5 minutes — more than any normal Research-profile session
            // needs, well short of "I forgot the laptop on a stuck loop
            // and now my Gemini bill is $40." Per-tool timeout (30s
            // default) is unchanged.
            max_wall_clock_secs: 300,
            // 8 calls is double the worst-case observed legitimate
            // parallel fanout (Research profile cycling through 4
            // searches at once). Past this, the LLM is almost certainly
            // looping or got prompt-injected; let the iteration cap
            // catch it instead.
            max_tool_calls_per_iter: 8,
        }
    }
}

/// Knobs for the agent loop. Generic over Tauri runtime so the registry
/// can carry the same `R` the loop is parameterized on.
pub struct AgentLoopConfig<R: Runtime + 'static = Wry> {
    pub budget: AgentBudget,
    pub registry: ToolRegistry<R>,
}

impl<R: Runtime + 'static> Clone for AgentLoopConfig<R> {
    fn clone(&self) -> Self {
        Self {
            budget: self.budget,
            registry: self.registry.clone(),
        }
    }
}

impl<R: Runtime + 'static> AgentLoopConfig<R> {
    #[allow(dead_code)] // direct constructor; chat.rs goes through AgentProfile::build()
    pub fn new(registry: ToolRegistry<R>) -> Self {
        Self {
            budget: AgentBudget::default(),
            registry,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_budget_has_reasonable_caps() {
        let b = AgentBudget::default();
        // Lock down the published defaults; bumping them is a deliberate
        // decision, not a silent drift.
        assert_eq!(b.max_iterations, 8);
        assert_eq!(b.max_tool_result_bytes, 100_000);
        assert_eq!(b.max_wall_clock_secs, 300);
        assert_eq!(b.max_tool_calls_per_iter, 8);
    }

    #[test]
    fn budget_is_copy_and_clone_friendly() {
        let b = AgentBudget::default();
        let c = b;
        // If this compiles, AgentBudget is still Copy — the loop
        // assumes copy semantics in multiple branches.
        assert_eq!(b.max_iterations, c.max_iterations);
    }
}
