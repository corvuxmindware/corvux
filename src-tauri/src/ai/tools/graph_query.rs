//! `graph_query` — exposes the CozoDB graph algorithms (PageRank,
//! Louvain communities, shortest path, neighbor BFS) to the agent.
//! These are the moat features that justify the graph DB.
//!
//! Every algorithm hits the cached path first. PageRank/communities are
//! cached at indexing time; betweenness is intentionally NOT exposed here
//! because cold-cache betweenness on 50K notes is too slow to run inside
//! a chat turn (per spec §5.3).

use async_trait::async_trait;
use serde_json::{json, Value};
use tauri::Manager;

use crate::commands::graph::GraphDb;
use crate::db::queries;

use super::registry::{SafetyTier, Tool, ToolContext, ToolError};

pub struct GraphQueryTool;

#[async_trait]
impl Tool for GraphQueryTool {
    fn name(&self) -> &'static str {
        "graph_query"
    }

    fn description(&self) -> &'static str {
        "Run a graph algorithm over the wiki-link graph. Algorithms: \
         `page_rank` (top hub notes), `communities` (topic clusters via \
         Louvain), `shortest_path` (between two notes), `neighbors` (BFS \
         out from a note). Use this when the user asks about structure: \
         'what are my hub notes', 'what topics are emerging', 'how is X \
         connected to Y'. None of these are possible in plain Obsidian — \
         this is the differentiating feature."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "algorithm": {
                    "type": "string",
                    "enum": ["page_rank", "communities", "shortest_path", "neighbors"]
                },
                "limit": { "type": "integer", "default": 20, "maximum": 100 },
                "from_id": { "type": "string", "description": "shortest_path: source note id" },
                "to_id": { "type": "string", "description": "shortest_path: target note id" },
                "note_id": { "type": "string", "description": "neighbors: focus note id" },
                "depth": { "type": "integer", "default": 2, "maximum": 4, "description": "neighbors: BFS depth" }
            },
            "required": ["algorithm"]
        })
    }

    fn safety_tier(&self) -> SafetyTier {
        SafetyTier::Read
    }

    async fn execute(&self, ctx: &ToolContext, args: Value) -> Result<Value, ToolError> {
        let algo = args
            .get("algorithm")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::retryable("missing 'algorithm'"))?
            .to_string();
        let limit = args
            .get("limit")
            .and_then(|v| v.as_i64())
            .unwrap_or(20)
            .clamp(1, 100) as usize;

        tokio::task::block_in_place(|| {
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

            // Serve fresh hubs/communities: if the graph changed since the last
            // read, wipe the algo cache so the `cached_*` calls below recompute.
            // (No-op for shortest_path/neighbors, which don't read the cache.)
            graph
                .refresh_algo_cache_if_dirty(db)
                .map_err(ToolError::retryable)?;

            match algo.as_str() {
            "page_rank" => {
                let mut entries =
                    queries::cached_page_rank(db, limit).map_err(ToolError::retryable)?;
                if entries.is_empty() {
                    // Cold cache — recompute once.
                    entries = queries::recompute_page_rank(db, limit)
                        .map_err(ToolError::retryable)?;
                }
                Ok(json!({
                    "summary": format!("top {} hub notes by PageRank", entries.len()),
                    "algorithm": "page_rank",
                    "results": entries
                        .iter()
                        .map(|e| json!({ "note_id": e.note_id, "rank": e.rank }))
                        .collect::<Vec<_>>(),
                }))
            }
            "communities" => {
                let mut entries =
                    queries::cached_community_detection(db).map_err(ToolError::retryable)?;
                if entries.is_empty() {
                    let stats = queries::get_graph_stats(db).map_err(ToolError::retryable)?;
                    if stats.notes > crate::commands::graph::MAX_AUTO_COMMUNITY_NOTES {
                        return Err(ToolError::retryable(format!(
                            "community detection is disabled above {} notes to keep the local graph responsive (this roost has {})",
                            crate::commands::graph::MAX_AUTO_COMMUNITY_NOTES,
                            stats.notes
                        )));
                    }
                    entries = queries::recompute_community_detection(db)
                        .map_err(ToolError::retryable)?;
                }
                // Group by community id for readability.
                use std::collections::BTreeMap;
                let mut grouped: BTreeMap<i64, Vec<String>> = BTreeMap::new();
                for e in entries {
                    grouped.entry(e.community).or_default().push(e.note_id);
                }
                let communities: Vec<Value> = grouped
                    .into_iter()
                    .map(|(id, members)| {
                        json!({
                            "community": id,
                            "size": members.len(),
                            "members": members.iter().take(10).cloned().collect::<Vec<_>>(),
                        })
                    })
                    .collect();
                Ok(json!({
                    "summary": format!("{} community cluster(s) detected", communities.len()),
                    "algorithm": "communities",
                    "results": communities,
                }))
            }
            "shortest_path" => {
                let from_id = args
                    .get("from_id")
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
                    .ok_or_else(|| ToolError::retryable("shortest_path: missing 'from_id'"))?;
                let to_id = args
                    .get("to_id")
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
                    .ok_or_else(|| ToolError::retryable("shortest_path: missing 'to_id'"))?;
                // The LLM is shown note TITLES (mixed case) in its prompt, but
                // the graph is keyed by canonical lowercased ids. Resolve the
                // args the same way `backlinks` does, or a perfectly valid pair
                // silently reports "no path". Falls back to a lowercase of the
                // raw arg when the resolver can't place it.
                let resolver = queries::LinkResolver::build(db).map_err(ToolError::retryable)?;
                let from = resolver
                    .resolve(&from_id, "")
                    .unwrap_or_else(|| from_id.to_lowercase());
                let to = resolver
                    .resolve(&to_id, "")
                    .unwrap_or_else(|| to_id.to_lowercase());
                let result =
                    queries::shortest_path(db, &from, &to).map_err(ToolError::retryable)?;
                match result {
                    Some(p) => Ok(json!({
                        "summary": format!("path of {} hop(s) (cost {:.2})", p.path.len().saturating_sub(1), p.cost),
                        "algorithm": "shortest_path",
                        "from_id": p.from_id,
                        "to_id": p.to_id,
                        "cost": p.cost,
                        "path": p.path,
                    })),
                    None => Ok(json!({
                        "summary": "no path found",
                        "algorithm": "shortest_path",
                        "from_id": from_id,
                        "to_id": to_id,
                        "path": Value::Null,
                    })),
                }
            }
            "neighbors" => {
                let note_id = args
                    .get("note_id")
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
                    .ok_or_else(|| ToolError::retryable("neighbors: missing 'note_id'"))?;
                let depth = args
                    .get("depth")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(2)
                    .clamp(0, 4) as usize;
                let max_nodes = limit.max(20);
                // Resolve a title / mixed-case id to the canonical graph id
                // (see shortest_path above) so neighbors isn't an empty window.
                let resolver = queries::LinkResolver::build(db).map_err(ToolError::retryable)?;
                let focus = resolver
                    .resolve(&note_id, "")
                    .unwrap_or_else(|| note_id.to_lowercase());
                let window = queries::get_graph_window(db, &focus, depth, max_nodes)
                    .map_err(ToolError::retryable)?;
                Ok(json!({
                    "summary": format!("{} node(s) within {} hop(s) of `{}`", window.nodes.len(), depth, focus),
                    "algorithm": "neighbors",
                    "focus": focus,
                    "depth": depth,
                    "nodes": window.nodes
                        .iter()
                        .map(|n| json!({
                            "id": n.id,
                            "title": n.title,
                            "kind": n.kind,
                            "link_count": n.link_count,
                        }))
                        .collect::<Vec<_>>(),
                    "edges": window.edges
                        .iter()
                        .map(|e| json!({ "source": e.source, "target": e.target }))
                        .collect::<Vec<_>>(),
                }))
            }
            other => Err(ToolError::retryable(format!(
                "unknown algorithm '{other}' — pick one of page_rank, communities, shortest_path, neighbors"
            ))),
            }
        })
    }
}
