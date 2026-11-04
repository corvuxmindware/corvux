//! `skill_list` — return every registered skill's `{name, description}`.
//! Lets the agent discover user-authored instruction packs before
//! committing to a multi-step plan.

use async_trait::async_trait;
use serde_json::{json, Value};

use super::registry::{SafetyTier, Tool, ToolContext, ToolError};

pub struct SkillListTool;

#[async_trait]
impl<R: tauri::Runtime + 'static> Tool<R> for SkillListTool {
    fn name(&self) -> &'static str {
        "skill_list"
    }

    fn description(&self) -> &'static str {
        "List available skills — markdown instruction packs for specific task patterns. \
         Each entry has a `name` and a `description`. Call `skill_load` with the name \
         to get the full body. Prefer calling this BEFORE designing a complex \
         multi-step task — there may already be a skill that matches."
    }

    fn input_schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }

    fn safety_tier(&self) -> SafetyTier {
        SafetyTier::Read
    }

    fn max_result_bytes(&self) -> usize {
        // Skill lists are tiny — name + one-liner description. 16 KB is
        // generous (allows ~50 skills before truncation kicks in).
        16 * 1024
    }

    async fn execute(&self, ctx: &ToolContext<R>, _args: Value) -> Result<Value, ToolError> {
        Ok(build_skill_list_response(ctx.skill_registry.as_deref()))
    }
}

/// Pure helper for the `skill_list` JSON shape. Pulled out of `execute`
/// so tests can pin the contract without going through the full
/// `Tool` dispatch (which would need an `AppHandle` and a working
/// `tauri::test::MockRuntime`).
pub(crate) fn build_skill_list_response(
    registry: Option<&crate::ai::skills::SkillRegistry>,
) -> Value {
    let Some(reg) = registry else {
        return json!({
            "skills": [],
            "note": "No skill registry attached. This usually means a roost is not open."
        });
    };
    let skills: Vec<Value> = reg
        .list()
        .into_iter()
        .map(|m| {
            json!({
                "name": m.name,
                "description": m.description,
            })
        })
        .collect();
    json!({ "skills": skills })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::skills::SkillRegistry;
    use tempfile::TempDir;

    fn write_skill(root: &std::path::Path, name: &str, description: &str) {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!(
                "---\nname: {name}\ndescription: {description}\n---\n\n# {name}\n\ninstructions\n"
            ),
        )
        .unwrap();
    }

    #[test]
    fn input_schema_shape() {
        let t = SkillListTool;
        let schema = <SkillListTool as Tool>::input_schema(&t);
        assert_eq!(schema["type"], "object");
        assert!(schema["properties"].is_object());
    }

    #[test]
    fn safety_tier_is_read() {
        let t = SkillListTool;
        assert_eq!(<SkillListTool as Tool>::safety_tier(&t), SafetyTier::Read);
    }

    #[test]
    fn empty_registry_returns_note() {
        let v = build_skill_list_response(None);
        assert_eq!(v["skills"].as_array().unwrap().len(), 0);
        assert!(v["note"].as_str().unwrap().contains("roost"));
    }

    #[test]
    fn populated_registry_returns_alpha_sorted_skills() {
        let dir = TempDir::new().unwrap();
        write_skill(dir.path(), "zeta", "z one");
        write_skill(dir.path(), "alpha", "a one");
        let reg = SkillRegistry::new();
        reg.refresh(Some(dir.path()), None);

        let v = build_skill_list_response(Some(&reg));
        let skills = v["skills"].as_array().unwrap();
        assert_eq!(skills.len(), 2);
        assert_eq!(skills[0]["name"], "alpha");
        assert_eq!(skills[0]["description"], "a one");
        assert_eq!(skills[1]["name"], "zeta");
        // No body in the list response — that's what skill_load is for.
        assert!(skills[0].get("body").is_none());
    }
}
