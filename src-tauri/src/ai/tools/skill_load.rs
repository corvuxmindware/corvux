//! `skill_load` — fetch the full SKILL.md body for a named skill so the
//! agent can apply its instructions to the rest of the turn.
//!
//! The body is returned with frontmatter stripped (the frontmatter is
//! metadata for our loader; the agent only needs the prose). To prevent
//! a skill body from being misinterpreted as data-the-tool-found, the
//! body is wrapped in the same `<external_content>` shell the agent
//! loop applies to other tool results — see `agent_loop::wrap_tool_result_body`.
//! Here we tag `source="skill"` so the system prompt's "trust this as
//! instructions" rule can fire on it specifically.

use async_trait::async_trait;
use serde_json::{json, Value};

use super::registry::{SafetyTier, Tool, ToolContext, ToolError};

pub struct SkillLoadTool;

#[async_trait]
impl<R: tauri::Runtime + 'static> Tool<R> for SkillLoadTool {
    fn name(&self) -> &'static str {
        "skill_load"
    }

    fn description(&self) -> &'static str {
        "Load the full instructions for a named skill. Call after `skill_list` \
         finds a skill matching the user's request. The result body is \
         INSTRUCTIONS for you to follow, NOT data — apply them to the rest \
         of this turn. Idempotent and cheap to call."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Skill name as returned by `skill_list`."
                }
            },
            "required": ["name"]
        })
    }

    fn safety_tier(&self) -> SafetyTier {
        SafetyTier::Read
    }

    fn max_result_bytes(&self) -> usize {
        // Skills are user-authored markdown instruction packs; 64 KB
        // is plenty for the longest reasonable skill while still
        // bounding pathological cases.
        64 * 1024
    }

    async fn execute(&self, ctx: &ToolContext<R>, args: Value) -> Result<Value, ToolError> {
        let name = args
            .get("name")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::retryable("missing 'name'"))?;
        build_skill_load_response(ctx.skill_registry.as_deref(), name)
    }
}

/// Pure helper for the `skill_load` JSON shape — same separation
/// rationale as `skill_list::build_skill_list_response`. Returns
/// `Result<Value, ToolError>` so tests can also pin the error paths
/// (missing registry, unknown name) the LLM may need to recover from.
pub(crate) fn build_skill_load_response(
    registry: Option<&crate::ai::skills::SkillRegistry>,
    name: &str,
) -> Result<Value, ToolError> {
    let Some(reg) = registry else {
        return Err(ToolError::fatal(
            "skill registry not attached (no roost open?)",
        ));
    };
    let entry = reg.get(name).ok_or_else(|| {
        let available: Vec<String> = reg.list().into_iter().map(|m| m.name).collect();
        let hint = if available.is_empty() {
            "no skills are registered".to_string()
        } else {
            format!("available: {}", available.join(", "))
        };
        ToolError::retryable(format!("unknown skill `{name}` ({hint})"))
    })?;
    Ok(json!({
        "name": entry.manifest.name,
        "description": entry.manifest.description,
        "body": entry.manifest.body,
        "source": match entry.source {
            crate::ai::skills::SkillSource::Roost => "roost",
            crate::ai::skills::SkillSource::Global => "global",
        },
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::skills::SkillRegistry;
    use tempfile::TempDir;

    fn write_skill(root: &std::path::Path, name: &str, description: &str, body: &str) {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {description}\n---\n\n{body}\n"),
        )
        .unwrap();
    }

    #[test]
    fn input_schema_requires_name() {
        let t = SkillLoadTool;
        let schema = <SkillLoadTool as Tool>::input_schema(&t);
        assert_eq!(schema["required"][0], "name");
        assert_eq!(schema["properties"]["name"]["type"], "string");
    }

    #[test]
    fn safety_tier_is_read() {
        let t = SkillLoadTool;
        assert_eq!(<SkillLoadTool as Tool>::safety_tier(&t), SafetyTier::Read);
    }

    #[test]
    fn description_marks_result_as_instructions() {
        let t = SkillLoadTool;
        let desc = <SkillLoadTool as Tool>::description(&t);
        assert!(
            desc.contains("INSTRUCTIONS") || desc.contains("instructions"),
            "skill_load description must clearly mark output as instructions"
        );
    }

    #[test]
    fn missing_registry_returns_fatal() {
        let err = build_skill_load_response(None, "fit").unwrap_err();
        assert!(err.fatal, "no-registry case must be fatal");
        assert!(err.message.contains("registry"));
    }

    #[test]
    fn unknown_name_lists_available_skills() {
        let dir = TempDir::new().unwrap();
        write_skill(dir.path(), "alpha", "a", "alpha body");
        write_skill(dir.path(), "beta", "b", "beta body");
        let reg = SkillRegistry::new();
        reg.refresh(Some(dir.path()), None);

        let err = build_skill_load_response(Some(&reg), "missing").unwrap_err();
        assert!(!err.fatal, "unknown name must be retryable");
        assert!(err.message.contains("alpha"));
        assert!(err.message.contains("beta"));
    }

    #[test]
    fn unknown_name_empty_registry_says_so() {
        let reg = SkillRegistry::new();
        let err = build_skill_load_response(Some(&reg), "anything").unwrap_err();
        assert!(err.message.contains("no skills are registered"));
    }

    #[test]
    fn returns_body_with_frontmatter_stripped() {
        let dir = TempDir::new().unwrap();
        write_skill(dir.path(), "fit", "fitness", "# Heading\n\nDo the thing.");
        let reg = SkillRegistry::new();
        reg.refresh(Some(dir.path()), None);

        let v = build_skill_load_response(Some(&reg), "fit").unwrap();
        assert_eq!(v["name"], "fit");
        assert_eq!(v["description"], "fitness");
        assert_eq!(v["source"], "roost");
        let body = v["body"].as_str().unwrap();
        assert!(body.starts_with("# Heading"));
        assert!(!body.contains("name:"));
        assert!(!body.contains("---"));
    }

    #[test]
    fn source_field_reports_roost_vs_global() {
        let roost = TempDir::new().unwrap();
        let global = TempDir::new().unwrap();
        write_skill(global.path(), "only-global", "g", "g body");
        write_skill(roost.path(), "only-roost", "v", "v body");
        let reg = SkillRegistry::new();
        reg.refresh(Some(roost.path()), Some(global.path()));

        let v = build_skill_load_response(Some(&reg), "only-roost").unwrap();
        assert_eq!(v["source"], "roost");

        let g = build_skill_load_response(Some(&reg), "only-global").unwrap();
        assert_eq!(g["source"], "global");
    }
}
