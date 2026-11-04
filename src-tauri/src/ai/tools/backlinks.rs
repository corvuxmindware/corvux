//! `backlinks` — incoming + outgoing direct wiki-link neighbors of a
//! note. Concept-edge expansion (Phase 2 of graph extraction) is not
//! wired in yet; the placeholder for it is in this tool's response so
//! the LLM can be steered toward concept-aware mode once the relation
//! is populated at scale.

use async_trait::async_trait;
use serde_json::{json, Value};
use tauri::Manager;

use crate::commands::graph::GraphDb;
use crate::db::queries;

use super::registry::{SafetyTier, Tool, ToolContext, ToolError};

pub struct BacklinksTool;

#[async_trait]
impl Tool for BacklinksTool {
    fn name(&self) -> &'static str {
        "backlinks"
    }

    fn description(&self) -> &'static str {
        "Get the wiki-link neighborhood of a note: backlinks (notes that \
         link TO it), forward links (notes it links TO), and tags. Use \
         this to answer 'what's connected to X', 'who references this \
         note', or to map a note's local context before reading it."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "note_id": { "type": "string", "description": "Note id (roost-relative path without extension, lowercase) or a [[wiki-link]] title." },
                "include_forward": { "type": "boolean", "default": true },
                "include_tags": { "type": "boolean", "default": true },
                "limit": { "type": "integer", "default": 20, "maximum": 50 }
            },
            "required": ["note_id"]
        })
    }

    fn safety_tier(&self) -> SafetyTier {
        SafetyTier::Read
    }

    async fn execute(&self, ctx: &ToolContext, args: Value) -> Result<Value, ToolError> {
        let raw_id = args
            .get("note_id")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::retryable("missing 'note_id'"))?
            .trim_start_matches("[[")
            .trim_end_matches("]]")
            .to_string();
        let include_forward = args
            .get("include_forward")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        let include_tags = args
            .get("include_tags")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        let limit = args
            .get("limit")
            .and_then(|v| v.as_i64())
            .unwrap_or(20)
            .clamp(1, 50) as usize;

        let graph = ctx
            .app
            .try_state::<GraphDb>()
            .ok_or_else(|| ToolError::fatal("graph DB not initialized"))?;
        let db_guard = graph
            .db
            .lock()
            .map_err(|e| ToolError::retryable(format!("graph lock: {e}")))?;
        let db = db_guard
            .as_ref()
            .ok_or_else(|| ToolError::fatal("graph DB not initialized"))?;

        // Resolve [[Title]] → canonical id via LinkResolver.
        let resolver = queries::LinkResolver::build(db).map_err(ToolError::retryable)?;
        let id = resolver
            .resolve(&raw_id, "")
            .unwrap_or_else(|| raw_id.to_lowercase());

        let backlinks = queries::get_backlinks(db, &id).map_err(ToolError::retryable)?;
        let forward = if include_forward {
            queries::get_note_links(db, &id).map_err(ToolError::retryable)?
        } else {
            Vec::new()
        };
        let tags = if include_tags {
            queries::get_note_tags(db, &id).map_err(ToolError::retryable)?
        } else {
            Vec::new()
        };

        let backlinks_v: Vec<Value> = backlinks
            .into_iter()
            .take(limit)
            .map(|b| json!({ "title": b.title, "path": b.path }))
            .collect();
        let forward_v: Vec<Value> = forward
            .into_iter()
            .take(limit)
            .map(|f| json!({ "target": f.target, "link_text": f.link_text }))
            .collect();

        Ok(json!({
            "summary": format!("{} backlink(s), {} forward link(s), {} tag(s)", backlinks_v.len(), forward_v.len(), tags.len()),
            "note_id": id,
            "backlinks": backlinks_v,
            "forward_links": forward_v,
            "tags": tags,
        }))
    }
}
