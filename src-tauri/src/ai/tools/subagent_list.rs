//! `subagent_list` — discover available subagents (built-in, global,
//! roost-local) before calling `task`. Same Read-tier shape as
//! `skill_list`.

use async_trait::async_trait;
use serde_json::{json, Value};

use super::registry::{SafetyTier, Tool, ToolContext, ToolError};

pub struct SubagentListTool;

#[async_trait]
impl<R: tauri::Runtime + 'static> Tool<R> for SubagentListTool {
    fn name(&self) -> &'static str {
        "subagent_list"
    }

    fn description(&self) -> &'static str {
        "List available subagents — preset agent roles for delegating focused sub-tasks. \
         Each entry has a `name` and `description`. Call `task` with the chosen `name` \
         as `subagent_type` to delegate. ALWAYS call this before `task` if you don't \
         know what subagents are available — the list changes when the user adds \
         AGENT.md files."
    }

    fn input_schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }

    fn safety_tier(&self) -> SafetyTier {
        SafetyTier::Read
    }

    fn max_result_bytes(&self) -> usize {
        16 * 1024
    }

    async fn execute(&self, ctx: &ToolContext<R>, _args: Value) -> Result<Value, ToolError> {
        Ok(build_subagent_list_response(
            ctx.subagent_registry.as_deref(),
        ))
    }
}

pub(crate) fn build_subagent_list_response(
    registry: Option<&crate::ai::subagents::SubagentRegistry>,
) -> Value {
    let Some(reg) = registry else {
        return json!({
            "subagents": [],
            "note": "No subagent registry attached. This usually means a roost is not open."
        });
    };
    let subagents: Vec<Value> = reg
        .list()
        .into_iter()
        .map(|d| {
            json!({
                "name": d.name,
                "description": d.description,
            })
        })
        .collect();
    json!({ "subagents": subagents })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::subagents::SubagentRegistry;

    #[test]
    fn input_schema_shape() {
        let t = SubagentListTool;
        let s = <SubagentListTool as Tool>::input_schema(&t);
        assert_eq!(s["type"], "object");
    }

    #[test]
    fn safety_tier_is_read() {
        let t = SubagentListTool;
        assert_eq!(
            <SubagentListTool as Tool>::safety_tier(&t),
            SafetyTier::Read
        );
    }

    #[test]
    fn empty_registry_returns_note() {
        let v = build_subagent_list_response(None);
        assert_eq!(v["subagents"].as_array().unwrap().len(), 0);
        assert!(v["note"].as_str().unwrap().contains("roost"));
    }

    #[test]
    fn registry_with_only_builtin_returns_general() {
        let reg = SubagentRegistry::new();
        reg.refresh(None, None);
        let v = build_subagent_list_response(Some(&reg));
        let list = v["subagents"].as_array().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0]["name"], "general");
        assert!(list[0]["description"].as_str().unwrap().len() > 10);
    }
}
