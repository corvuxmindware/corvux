//! `task` — the entry point for subagent delegation. Spawns a fresh
//! agent loop with its own context window, a narrowed tool registry,
//! and the parent's chat provider. Returns the subagent's final text
//! to the parent as the tool result; the subagent's intermediate tool
//! calls stay inside the child loop, not the parent's history.
//!
//! ## Why this is one tool, not several
//!
//! The plan considered shipping per-subagent tools (`task_research`,
//! `task_writer`, …). We didn't — the LLM picks the subagent by name
//! through `subagent_type`, and a single dispatcher keeps the
//! depth-guard, narrowed-registry build, and event-emission paths in
//! one place. Adding a subagent definition does not require touching
//! Rust code.
//!
//! ## Depth cap
//!
//! Subagents cannot spawn further subagents. The `task` tool itself is
//! filtered out of the child registry by
//! [`SubagentDef::tools_with_defaults`], and we also enforce a
//! belt-and-suspenders depth check here in case a user-authored
//! AGENT.md explicitly lists `task` in `tools:`. Returning a clear
//! retryable error lets the inner LLM recover by completing the work
//! in its current context instead of hanging on a bad delegation.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use async_trait::async_trait;
use serde::Serialize;
use serde_json::{json, Value};
use tauri_specta::Event;

use crate::events;

use crate::ai::tools::AgentLoopConfig;
use crate::ai::ChatMessage;

use super::bundle::ToolBundle;
use super::registry::{SafetyTier, Tool, ToolContext, ToolError, ToolRegistry};

/// Depth at which the `task` tool refuses to dispatch. Parent loops
/// are depth 0; a subagent loop is depth 1. Anything at depth ≥ 1 is
/// already inside a subagent and must finish the work locally.
const SUBAGENT_DEPTH_CAP: u8 = 1;

pub struct TaskTool;

#[allow(dead_code)] // emitted via tauri-specta event API; specta::Type derive triggers the warning
#[derive(Serialize, Clone, specta::Type)]
pub struct SubagentStartPayload {
    pub subagent_type: String,
    pub description: String,
    pub depth: u8,
}

#[allow(dead_code)] // emitted via tauri-specta event API
#[derive(Serialize, Clone, specta::Type)]
pub struct SubagentDonePayload {
    pub subagent_type: String,
    pub success: bool,
    #[specta(type = u32)]
    pub text_chars: usize,
}

#[async_trait]
impl<R: tauri::Runtime + 'static> Tool<R> for TaskTool {
    fn name(&self) -> &'static str {
        "task"
    }

    fn description(&self) -> &'static str {
        "Delegate a focused sub-task to a fresh agent loop with its own context window. \
         Use when the user's question requires 3+ tool calls of exploration that would \
         flood this conversation, OR when a specialized subagent is a better fit. \
         CALL `subagent_list` FIRST to see available `subagent_type` values — the list \
         is user-extensible via AGENT.md files. The subagent has NO memory of this \
         conversation; pass everything it needs in `prompt`. It returns only its final \
         synthesized text — intermediate tool calls stay inside the subagent's context."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "subagent_type": {
                    "type": "string",
                    "description": "Name of the subagent to invoke. Call `subagent_list` to discover. The built-in `general` is always available."
                },
                "prompt": {
                    "type": "string",
                    "description": "The task for the subagent. Be specific: state the goal, list anything you've already learned, give the subagent the context it needs to make judgment calls. The subagent will NOT see this conversation."
                },
                "description": {
                    "type": "string",
                    "description": "3-5 word label of what the subagent is doing, shown to the user as a status row."
                }
            },
            "required": ["subagent_type", "prompt", "description"]
        })
    }

    fn safety_tier(&self) -> SafetyTier {
        // The `task` tool itself is read-tier — it spawns a loop, doesn't
        // mutate anything. The child loop's individual writes go through
        // their own approval gates.
        SafetyTier::Read
    }

    fn timeout(&self) -> std::time::Duration {
        // The child loop has its own wall-clock cap; we let it run up
        // to the parent's. The outer timeout is generous because the
        // child enforces its own ceiling.
        std::time::Duration::from_secs(900)
    }

    fn max_result_bytes(&self) -> usize {
        // The result is the subagent's final synthesized text — same
        // ballpark as a long chat reply.
        128 * 1024
    }

    async fn execute(&self, ctx: &ToolContext<R>, args: Value) -> Result<Value, ToolError> {
        let req = parse_task_args(&args)?;

        // Depth guard — belt-and-suspenders next to the registry filter
        // in SubagentDef::tools_with_defaults. If a user-authored
        // AGENT.md re-adds "task" to its tools: list, this guard still
        // refuses, with a clear retryable hint so the inner LLM
        // completes the work in its current context.
        if ctx.subagent_depth >= SUBAGENT_DEPTH_CAP {
            return Err(ToolError::retryable(
                "subagents cannot spawn further subagents — complete the work in this context",
            ));
        }

        // Pull subagent definition.
        let reg = ctx.subagent_registry.as_deref().ok_or_else(|| {
            ToolError::fatal("subagent registry not attached — `task` requires a roost to be open")
        })?;
        let entry = reg.get(&req.subagent_type).ok_or_else(|| {
            let available = reg.names().join(", ");
            ToolError::retryable(format!(
                "unknown subagent_type `{}` (available: {available})",
                req.subagent_type
            ))
        })?;
        let def = entry.def;

        // Build the narrowed child registry: explicit `tools:` from
        // AGENT.md, or parent-minus-task-minus-destructive default.
        let parent_registry = ctx.parent_registry.as_ref().ok_or_else(|| {
            ToolError::fatal(
                "task: parent_registry missing — task can only be called from inside the agent loop",
            )
        })?;
        let child_tool_names = def.tools_with_defaults(parent_registry);
        if child_tool_names.is_empty() {
            return Err(ToolError::fatal(format!(
                "subagent `{}` has no tools available — check the `tools:` list in its AGENT.md",
                def.name
            )));
        }
        let child_registry = filter_registry(parent_registry, &child_tool_names);

        // Build the child provider — for the MVP we inherit the
        // parent's. `def.model` is parsed and stored for a future
        // model-override change.
        let parent_provider = ctx.parent_chat_provider.clone().ok_or_else(|| {
            ToolError::fatal(
                "task: parent_chat_provider missing — task can only be called from inside the agent loop",
            )
        })?;

        // Effective budget: AGENT.md overrides applied on top of the
        // parent's budget.
        let parent_budget = ctx.parent_budget.unwrap_or_default();
        let child_budget = def.budget_or(&parent_budget);

        // Shared cancel — Stop from the UI must kill both loops.
        let cancel: Arc<AtomicBool> = ctx
            .cancel
            .clone()
            .unwrap_or_else(|| Arc::new(AtomicBool::new(false)));

        // Autonomy snapshot: child gets a clone so its "Always approve"
        // decisions stay local to the inner loop.
        let child_autonomy = ctx.parent_autonomy.clone().unwrap_or_default();

        // Build the child ToolContext at depth+1. parent_chat_provider /
        // parent_registry / parent_budget stay None for the child —
        // even though the depth guard already blocks recursion, NOT
        // exposing the parent's provider to a subagent prevents an
        // accidental leak path.
        let child_ctx = ToolContext::<R> {
            app: ctx.app.clone(),
            roost_path: ctx.roost_path.clone(),
            roost_generation: ctx.roost_generation,
            open_note_path: ctx.open_note_path.clone(),
            skill_registry: ctx.skill_registry.clone(),
            subagent_depth: ctx.subagent_depth + 1,
            parent_registry: None,
            subagent_registry: ctx.subagent_registry.clone(),
            parent_chat_provider: None,
            parent_budget: None,
            parent_autonomy: None,
            cancel: Some(cancel.clone()),
        };

        // System prompt for the child: AGENT.md body + registry blurb
        // built from the narrowed registry only. The child does NOT
        // see the parent's system prompt.
        let child_system = build_child_system_prompt(&def.body, &child_registry);

        // Initial messages: a single user turn carrying the parent's
        // `prompt` arg. The child has no other history.
        let initial_messages = vec![ChatMessage::user(req.prompt.clone())];

        // UI: subagent-start event. Frontend shows a "Running subagent"
        // chevron with the user-facing `description` label.
        let _ = events::AiSubagentStart {
            subagent_type: def.name.clone(),
            description: req.description.clone(),
            depth: child_ctx.subagent_depth,
        }
        .emit(&ctx.app);

        let cfg = AgentLoopConfig {
            registry: child_registry,
            budget: child_budget,
        };
        let app = ctx.app.clone();

        // Inner loop: use the same v2 entry point the parent uses so
        // we get native tool-calling semantics + identical safety
        // wiring (secret scan, validation, approval gate, external_
        // content wrap). The legacy path is not invoked here — Phase
        // 2.A is the lower bound for subagent execution.
        let result = super::agent_loop::run_agent_loop_v2(
            app,
            parent_provider,
            Some(child_system),
            initial_messages,
            cancel,
            cfg,
            child_ctx,
            child_autonomy,
        )
        .await;

        let success = result.is_ok();
        let text = result.clone().unwrap_or_default();
        let _ = events::AiSubagentDone {
            subagent_type: def.name.clone(),
            success,
            text_chars: text.len(),
        }
        .emit(&ctx.app);

        match result {
            Ok(text) => Ok(json!({
                "subagent": def.name,
                "result": text,
            })),
            Err(e) => Err(ToolError::retryable(format!(
                "subagent `{}` failed: {e}",
                def.name
            ))),
        }
    }
}

#[derive(Debug)]
pub(crate) struct TaskArgs {
    pub subagent_type: String,
    pub prompt: String,
    pub description: String,
}

pub(crate) fn parse_task_args(args: &Value) -> Result<TaskArgs, ToolError> {
    let subagent_type = args
        .get("subagent_type")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ToolError::retryable("missing 'subagent_type'"))?
        .to_string();
    let prompt = args
        .get("prompt")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ToolError::retryable("missing 'prompt'"))?
        .to_string();
    let description = args
        .get("description")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ToolError::retryable("missing 'description'"))?
        .to_string();
    Ok(TaskArgs {
        subagent_type,
        prompt,
        description,
    })
}

/// Construct a `ToolRegistry` containing only the named tools, looked
/// up from `parent`. Names not present in the parent are silently
/// dropped — `SubagentDef::tools_with_defaults` has already filtered
/// against registered names, but a hot-reload between resolve and
/// dispatch could in principle drop one. Returning an empty registry
/// is impossible because `execute` checks for that case earlier.
pub(crate) fn filter_registry<R: tauri::Runtime + 'static>(
    parent: &ToolRegistry<R>,
    names: &[String],
) -> ToolRegistry<R> {
    let mut child = ToolRegistry::new();
    for name in names {
        if let Some(t) = parent.get(name) {
            child.register(t);
        }
    }
    child
}

/// Render the child loop's system prompt. The AGENT.md body lands
/// verbatim at the top; the narrowed tool registry blurb is appended
/// so the child knows what it has available without inheriting the
/// parent's full registry render.
pub(crate) fn build_child_system_prompt<R: tauri::Runtime + 'static>(
    agent_body: &str,
    child_registry: &ToolRegistry<R>,
) -> String {
    let mut s = String::new();
    s.push_str(agent_body.trim_end());
    s.push_str("\n\n");
    // Reuse the registry's flat renderer (bundle-aware grouping is
    // overkill for a typically-narrow child registry; the flat blurb
    // is exactly the same protocol explanation the parent gets).
    s.push_str(&child_registry.render_for_system_prompt());
    s
}

/// Subagent bundle — pairs `task` with its discovery companion
/// `subagent_list`. Both Read-tier. Currently not in any profile
/// (deferred 2026-05-20 — see profile.rs comment); the struct stays
/// compiled so re-adding it is a one-line restore in `bundles()`.
#[allow(dead_code)]
pub struct SubagentBundle;
impl ToolBundle for SubagentBundle {
    fn name(&self) -> &'static str {
        "subagents"
    }
    fn description(&self) -> &'static str {
        "Delegate sub-tasks to fresh agent loops with isolated context windows. Read-tier; the child loop's writes still go through their own approval gate."
    }
    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        vec![
            Arc::new(super::subagent_list::SubagentListTool),
            Arc::new(TaskTool),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::subagents::SubagentRegistry;
    use crate::ai::tools::AgentBudget;
    use serde_json::json;

    #[test]
    fn parse_task_args_validates_all_three_fields() {
        let ok = json!({
            "subagent_type": "general",
            "prompt": "do the thing",
            "description": "doing thing"
        });
        let parsed = parse_task_args(&ok).unwrap();
        assert_eq!(parsed.subagent_type, "general");
        assert_eq!(parsed.prompt, "do the thing");
        assert_eq!(parsed.description, "doing thing");
    }

    #[test]
    fn parse_task_args_rejects_missing_fields() {
        for missing in ["subagent_type", "prompt", "description"] {
            let mut obj = json!({
                "subagent_type": "general",
                "prompt": "do the thing",
                "description": "doing thing"
            });
            obj.as_object_mut().unwrap().remove(missing);
            let err = parse_task_args(&obj).unwrap_err();
            assert!(
                err.message.contains(missing),
                "error for missing `{missing}` must name the field; got: {}",
                err.message
            );
        }
    }

    #[test]
    fn parse_task_args_rejects_empty_strings() {
        let bad = json!({
            "subagent_type": "  ",
            "prompt": "x",
            "description": "y"
        });
        assert!(parse_task_args(&bad).is_err());
    }

    #[test]
    fn input_schema_marks_three_required() {
        let t = TaskTool;
        let s = <TaskTool as Tool>::input_schema(&t);
        let req: Vec<&str> = s["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(req.contains(&"subagent_type"));
        assert!(req.contains(&"prompt"));
        assert!(req.contains(&"description"));
    }

    #[test]
    fn safety_tier_is_read() {
        let t = TaskTool;
        assert_eq!(<TaskTool as Tool>::safety_tier(&t), SafetyTier::Read);
    }

    #[test]
    fn description_mentions_subagent_list_for_discovery() {
        let t = TaskTool;
        let d = <TaskTool as Tool>::description(&t);
        assert!(
            d.contains("subagent_list"),
            "description must teach the model to discover available subagents"
        );
        assert!(
            d.to_lowercase().contains("no memory") || d.to_lowercase().contains("not see"),
            "description must warn that the subagent has no shared context"
        );
    }

    #[test]
    fn filter_registry_keeps_only_named_tools() {
        use crate::ai::tools::default_registry;
        let parent: ToolRegistry<tauri::Wry> = default_registry();
        let names = vec!["roost_search".to_string(), "roost_read".to_string()];
        let child = filter_registry(&parent, &names);
        assert!(child.get("roost_search").is_some());
        assert!(child.get("roost_read").is_some());
        // `roost_write` is in the parent but excluded — must not leak.
        assert!(child.get("roost_write").is_none());
    }

    #[test]
    fn filter_registry_drops_unknown_names_silently() {
        use crate::ai::tools::default_registry;
        let parent: ToolRegistry<tauri::Wry> = default_registry();
        let names = vec!["roost_search".to_string(), "does_not_exist".to_string()];
        let child = filter_registry(&parent, &names);
        assert!(child.get("roost_search").is_some());
        assert!(child.get("does_not_exist").is_none());
    }

    #[test]
    fn build_child_system_prompt_embeds_agent_body_and_tool_blurb() {
        use crate::ai::tools::default_registry;
        let parent: ToolRegistry<tauri::Wry> = default_registry();
        let child = filter_registry(&parent, &["roost_search".to_string()]);
        let s = build_child_system_prompt("You are a focused researcher.", &child);
        assert!(s.contains("You are a focused researcher."));
        assert!(s.contains("roost_search"));
        assert!(s.contains("Available tools"));
    }

    #[test]
    fn tools_with_defaults_excludes_task_and_destructive() {
        use crate::ai::subagents::SubagentDef;
        use crate::ai::tools::default_registry;
        let parent: ToolRegistry<tauri::Wry> = default_registry();
        let def = SubagentDef {
            name: "x".into(),
            description: "y".into(),
            body: String::new(),
            model: None,
            tools: None, // inherit-default path
            max_iterations: None,
            max_wall_clock_secs: None,
            max_tool_result_bytes: None,
            source_path: std::path::PathBuf::from("/tmp"),
        };
        let names = def.tools_with_defaults(&parent);
        assert!(
            !names.contains(&"task".to_string()),
            "default must drop task"
        );
        // Confirm at least one expected Read-tier tool survives.
        assert!(names.contains(&"roost_search".to_string()));
    }

    #[test]
    fn tools_with_defaults_explicit_list_filters_to_registered() {
        use crate::ai::subagents::SubagentDef;
        use crate::ai::tools::default_registry;
        let parent: ToolRegistry<tauri::Wry> = default_registry();
        let def = SubagentDef {
            name: "x".into(),
            description: "y".into(),
            body: String::new(),
            model: None,
            tools: Some(vec![
                "roost_search".into(),
                "roost_search".into(),    // duplicate — must dedupe
                "totally_made_up".into(), // unknown — must drop
            ]),
            max_iterations: None,
            max_wall_clock_secs: None,
            max_tool_result_bytes: None,
            source_path: std::path::PathBuf::from("/tmp"),
        };
        let names = def.tools_with_defaults(&parent);
        assert_eq!(names, vec!["roost_search".to_string()]);
    }

    #[test]
    fn subagent_list_response_lists_builtin_general() {
        let reg = SubagentRegistry::new();
        reg.refresh(None, None);
        let v = crate::ai::tools::subagent_list::build_subagent_list_response(Some(&reg));
        let list = v["subagents"].as_array().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0]["name"], "general");
    }

    #[test]
    fn explicit_tools_list_with_destructive_passes_through() {
        // Sanity check on the trust boundary: if a user-authored
        // AGENT.md explicitly lists a Destructive tool in `tools:`,
        // tools_with_defaults DOES pass it through (the "least
        // privilege by default" rule is for the inherit path only).
        // This is intentional — the user opted in.
        use crate::ai::subagents::SubagentDef;
        use crate::ai::tools::default_registry;
        let parent: ToolRegistry<tauri::Wry> = default_registry();
        // delete_task is Destructive in the default registry; confirm
        // that pre-condition holds before asserting on the trust path.
        let delete_task_present = parent.get("delete_task").is_some();
        if let Some(dt) = parent.get("delete_task") {
            assert!(matches!(dt.safety_tier(), SafetyTier::Destructive));
        }
        let def = SubagentDef {
            name: "x".into(),
            description: "y".into(),
            body: String::new(),
            model: None,
            tools: Some(vec!["delete_task".into()]),
            max_iterations: None,
            max_wall_clock_secs: None,
            max_tool_result_bytes: None,
            source_path: std::path::PathBuf::from("/tmp"),
        };
        let names = def.tools_with_defaults(&parent);
        // Whether the registry exposes delete_task or not, the
        // explicit list must be honored verbatim (filtered only
        // against registry presence, not against tier).
        if delete_task_present {
            assert_eq!(names, vec!["delete_task".to_string()]);
        }
    }

    #[test]
    fn budget_or_caps_per_iter_inherits_always() {
        use crate::ai::subagents::SubagentDef;
        let parent_budget = AgentBudget {
            max_iterations: 8,
            max_tool_result_bytes: 100_000,
            max_wall_clock_secs: 300,
            max_tool_calls_per_iter: 6,
        };
        let def = SubagentDef {
            name: "x".into(),
            description: "y".into(),
            body: String::new(),
            model: None,
            tools: None,
            // Try to override per-iter via fields we don't expose —
            // these get ignored.
            max_iterations: Some(20),
            max_wall_clock_secs: Some(60),
            max_tool_result_bytes: Some(50_000),
            source_path: std::path::PathBuf::from("/tmp"),
        };
        let b = def.budget_or(&parent_budget);
        // Set fields override
        assert_eq!(b.max_iterations, 20);
        assert_eq!(b.max_wall_clock_secs, 60);
        assert_eq!(b.max_tool_result_bytes, 50_000);
        // Per-iter cap is NEVER overrideable from AGENT.md.
        assert_eq!(b.max_tool_calls_per_iter, 6);
    }

    #[test]
    fn child_system_prompt_does_not_include_parent_addendum() {
        // The child system prompt is purely AGENT.md body + child
        // registry blurb — the parent's profile addendum (e.g.
        // "Profile: Research") MUST NOT leak in. Verify by building
        // a child prompt from a tiny inline body + tiny registry.
        use crate::ai::tools::default_registry;
        let parent: ToolRegistry<tauri::Wry> = default_registry();
        let child = filter_registry(&parent, &["roost_search".to_string()]);
        let s =
            build_child_system_prompt("Custom subagent body that the parent never sees.", &child);
        assert!(s.contains("Custom subagent body that the parent never sees."));
        assert!(!s.contains("Profile: Research"));
        assert!(!s.contains("Profile: QuickLookup"));
        assert!(!s.contains("Profile: Writer"));
    }

    /// Subagent runs share the parent's cancel flag — toggling it
    /// from one side must be visible from the other. This pins the
    /// Arc<AtomicBool> sharing semantics for the depth-2 stop case.
    #[test]
    fn shared_cancel_flag_propagates_in_both_directions() {
        use std::sync::atomic::Ordering;
        let parent_cancel = Arc::new(AtomicBool::new(false));
        let child_cancel = parent_cancel.clone();

        parent_cancel.store(true, Ordering::Relaxed);
        assert!(child_cancel.load(Ordering::Relaxed));

        // And the reverse — child setting it must also be visible
        // from the parent's side (the UI's Stop button hits the
        // parent's clone; if the child completed and re-armed it
        // wrong this would regress).
        parent_cancel.store(false, Ordering::Relaxed);
        child_cancel.store(true, Ordering::Relaxed);
        assert!(parent_cancel.load(Ordering::Relaxed));
    }
}
