//! Read-side graph + backlink + tag queries. No mutations — all call
//! `ScriptMutability::Immutable`. Frontend renders everything returned
//! here directly, so types live in `super::types`.

use cozo::{DataValue, DbInstance, Num, ScriptMutability};
use std::collections::{BTreeMap, HashMap};

use super::types::{BacklinkEntry, FullGraph, GraphEdge, GraphNode, GraphStats, NoteLinkEntry};
use crate::parser::tags::{fold_tag, TagIdentities};

/// Graph node id of a tag: `tag:` + its identity key (LR-6). Stored `tags`
/// rows stay verbatim; the folded key is applied HERE, at read time, so
/// `#ML` / `#ml` / NFD-vs-NFC spellings — old rows and new rows alike — are one
/// node. The `tag:` prefix keeps tag ids from colliding with note ids.
fn tag_node_id(key: &str) -> String {
    format!("tag:{key}")
}

/// Get all notes that link TO the given note (backlinks).
pub fn get_backlinks(db: &DbInstance, note_id: &str) -> Result<Vec<BacklinkEntry>, String> {
    // AUDIT-023: callers must pass already-canonical (lowercased,
    // forward-slash, roost-relative) ids. See `queries::mod.rs`.
    debug_assert_eq!(
        note_id,
        note_id.to_lowercase(),
        "note_id must be lowercased at the canonicalization site, not here"
    );
    let mut params = BTreeMap::new();
    params.insert("note_id".to_string(), DataValue::Str(note_id.into()));

    let result = db
        .run_script(
            r#"?[title, path] := *links{ from_id, to_id },
                                  to_id == $note_id,
                                  *notes{ id: from_id, title, path }"#,
            params,
            ScriptMutability::Immutable,
        )
        .map_err(|e| format!("Backlinks query failed: {e}"))?;

    let entries = result
        .rows
        .iter()
        .filter_map(|row| {
            let title = row.first()?.get_str()?.to_string();
            let path = row.get(1)?.get_str()?.to_string();
            Some(BacklinkEntry { title, path })
        })
        .collect();

    Ok(entries)
}

/// Get all notes that the given note links to (forward links).
pub fn get_note_links(db: &DbInstance, note_id: &str) -> Result<Vec<NoteLinkEntry>, String> {
    // AUDIT-023: callers must pass already-canonical (lowercased,
    // forward-slash, roost-relative) ids. See `queries::mod.rs`.
    debug_assert_eq!(
        note_id,
        note_id.to_lowercase(),
        "note_id must be lowercased at the canonicalization site, not here"
    );
    let mut params = BTreeMap::new();
    params.insert("note_id".to_string(), DataValue::Str(note_id.into()));

    let result = db
        .run_script(
            r#"?[target, link_text] := *links{ from_id, to_id, link_text },
                                        from_id == $note_id,
                                        target = to_id"#,
            params,
            ScriptMutability::Immutable,
        )
        .map_err(|e| format!("Forward links query failed: {e}"))?;

    let entries = result
        .rows
        .iter()
        .filter_map(|row| {
            let target = row.first()?.get_str()?.to_string();
            let link_text = row.get(1)?.get_str()?.to_string();
            Some(NoteLinkEntry { target, link_text })
        })
        .collect();

    Ok(entries)
}

/// Get all tags for a given note. Spellings that differ only by case /
/// normalization (`#ML` and `#ml` in one note) are one tag: the first stored
/// spelling is returned, so the list matches what the graph shows (LR-6).
pub fn get_note_tags(db: &DbInstance, note_id: &str) -> Result<Vec<String>, String> {
    // AUDIT-023: callers must pass already-canonical (lowercased,
    // forward-slash, roost-relative) ids. See `queries::mod.rs`.
    debug_assert_eq!(
        note_id,
        note_id.to_lowercase(),
        "note_id must be lowercased at the canonicalization site, not here"
    );
    let mut params = BTreeMap::new();
    params.insert("note_id".to_string(), DataValue::Str(note_id.into()));

    let result = db
        .run_script(
            r#"?[tag] := *tags{ note_id, tag }, note_id == $note_id"#,
            params,
            ScriptMutability::Immutable,
        )
        .map_err(|e| format!("Tags query failed: {e}"))?;

    let mut seen = std::collections::HashSet::new();
    let tags = result
        .rows
        .iter()
        .filter_map(|row| row.first()?.get_str().map(|s| s.to_string()))
        .filter(|tag| seen.insert(fold_tag(tag)))
        .collect();

    Ok(tags)
}

/// Get graph-wide statistics (note / link / tag counts).
pub fn get_graph_stats(db: &DbInstance) -> Result<GraphStats, String> {
    let notes_result = db
        .run_script(
            r#"?[count(id)] := *notes{ id }"#,
            Default::default(),
            ScriptMutability::Immutable,
        )
        .map_err(|e| format!("Stats query failed: {e}"))?;

    let links_result = db
        .run_script(
            r#"?[count(from_id)] := *links{ from_id }"#,
            Default::default(),
            ScriptMutability::Immutable,
        )
        .map_err(|e| format!("Stats query failed: {e}"))?;

    let tags_result = db
        .run_script(
            r#"?[count(note_id)] := *tags{ note_id }"#,
            Default::default(),
            ScriptMutability::Immutable,
        )
        .map_err(|e| format!("Stats query failed: {e}"))?;

    Ok(GraphStats {
        notes: extract_count(&notes_result.rows),
        links: extract_count(&links_result.rows),
        tags: extract_count(&tags_result.rows),
    })
}

/// Pull a `count(*)` aggregate result from a Cozo row vector. `cozo::Num`
/// derives externally-tagged serde serialization (`{"Int": 5}`), so the
/// old serde_json round-trip lost the value. Pattern-match variants
/// directly instead.
fn extract_count(rows: &[Vec<DataValue>]) -> usize {
    rows.first()
        .and_then(|r| r.first())
        .and_then(|v| match v {
            DataValue::Num(Num::Int(i)) => Some(*i as usize),
            DataValue::Num(Num::Float(f)) => Some(*f as usize),
            _ => None,
        })
        .unwrap_or(0)
}

/// Get the full graph: **every** note in the roost as a node, including
/// orphans with no links at all, plus tag nodes and all edges (links +
/// note→tag). Tag nodes are synthesized from the `tags` relation with
/// ids prefixed `tag:` so they can't collide with note stems.
///
/// This is the query behind Graph View's "Full" mode. Unlike
/// [`get_graph_window`], it is NOT a BFS from a focus node — a window
/// anchored on any single node can only ever reach that node's connected
/// component, so disconnected notes (orphans, separate islands) silently
/// vanish. Full mode must show the whole roost, so it reads `*notes`
/// directly.
///
/// AUDIT-024 removed the original unbounded version because full-roost
/// payloads don't survive force-directed rendering past ~10K nodes. This
/// version restores the feature **with a hard `max_nodes` cap**: when the
/// roost has more notes than the cap, the highest-degree notes (hubs) are
/// kept and the long tail is dropped, with the truncation logged so it's
/// never silent. Edges are filtered to kept notes only. Tag nodes adjacent
/// to kept notes are added on top of the note budget — tags are few in
/// practice and the cap that mattered for AUDIT-024 was the note count.
pub fn get_full_graph(db: &DbInstance, max_nodes: usize) -> Result<FullGraph, String> {
    if max_nodes == 0 {
        return Ok(FullGraph {
            nodes: Vec::new(),
            edges: Vec::new(),
        });
    }

    let notes_result = db
        .run_script(
            r#"?[id, title, path] := *notes{ id, title, path }"#,
            Default::default(),
            ScriptMutability::Immutable,
        )
        .map_err(|e| format!("Full graph nodes query failed: {e}"))?;

    // Rank from relation-level aggregates rather than materializing every
    // edge/tag row in Rust before the cap is applied. A dense 50K-note graph
    // can contain millions of edges; aggregate output stays O(notes).
    let outgoing_degree = db
        .run_script(
            r#"?[id, count(other)] := *links{ from_id: id, to_id: other }"#,
            Default::default(),
            ScriptMutability::Immutable,
        )
        .map_err(|e| format!("Full graph outgoing-degree query failed: {e}"))?;

    let incoming_degree = db
        .run_script(
            r#"?[id, count(other)] := *links:by_to{ from_id: other, to_id: id }"#,
            Default::default(),
            ScriptMutability::Immutable,
        )
        .map_err(|e| format!("Full graph incoming-degree query failed: {e}"))?;

    let tag_degree = db
        .run_script(
            r#"?[id, count(tag)] := *tags{ note_id: id, tag }"#,
            Default::default(),
            ScriptMutability::Immutable,
        )
        .map_err(|e| format!("Full graph tag-degree query failed: {e}"))?;

    // All note rows, in DB order.
    let all_notes: Vec<(String, String, String)> = notes_result
        .rows
        .iter()
        .filter_map(|row| {
            let id = row.first()?.get_str()?.to_string();
            let title = row.get(1)?.get_str()?.to_string();
            let path = row.get(2)?.get_str()?.to_string();
            Some((id, title, path))
        })
        .collect();

    // Whole-graph degree, used only to RANK notes when we're over the cap.
    // Counts both link endpoints and tag adjacency so hubs (the nodes worth
    // keeping when we must drop some) rank highest.
    let mut rank_degree: HashMap<String, usize> = HashMap::new();
    for result in [&outgoing_degree, &incoming_degree, &tag_degree] {
        for row in &result.rows {
            let Some(id) = row.first().and_then(|v| v.get_str()) else {
                continue;
            };
            let count = match row.get(1) {
                Some(DataValue::Num(Num::Int(n))) if *n > 0 => *n as usize,
                Some(DataValue::Num(Num::Float(n))) if *n > 0.0 => *n as usize,
                _ => 0,
            };
            if count > 0 {
                *rank_degree.entry(id.to_string()).or_default() += count;
            }
        }
    }

    // Select which notes survive the cap. Under the cap → keep all (the
    // common case for real roosts; orphans included by construction).
    // Over the cap → keep the highest-degree notes, tiebreak by id for a
    // deterministic result.
    let mut selected = all_notes;
    if selected.len() > max_nodes {
        let dropped = selected.len() - max_nodes;
        selected.sort_by(|a, b| {
            let da = rank_degree.get(&a.0).copied().unwrap_or(0);
            let db_ = rank_degree.get(&b.0).copied().unwrap_or(0);
            db_.cmp(&da).then_with(|| a.0.cmp(&b.0))
        });
        selected.truncate(max_nodes);
        tracing::info!(
            kept = max_nodes,
            dropped,
            "full_graph: roost exceeds max_nodes; kept highest-degree notes"
        );
    }
    let selected_ids: std::collections::HashSet<String> =
        selected.iter().map(|(id, _, _)| id.clone()).collect();
    if selected_ids.is_empty() {
        return Ok(FullGraph {
            nodes: Vec::new(),
            edges: Vec::new(),
        });
    }

    let selected_rows: Vec<DataValue> = selected_ids
        .iter()
        .map(|id| DataValue::List(vec![DataValue::Str(id.clone().into())]))
        .collect();
    let mut link_params = BTreeMap::new();
    link_params.insert("ids".to_string(), DataValue::List(selected_rows.clone()));
    let links_result = db
        .run_script(
            r#"want[id] <- $ids
               ?[from_id, to_id] := *links{ from_id, to_id }, want[from_id], want[to_id]"#,
            link_params,
            ScriptMutability::Immutable,
        )
        .map_err(|e| format!("Full graph selected-edges query failed: {e}"))?;
    let mut tag_params = BTreeMap::new();
    tag_params.insert("ids".to_string(), DataValue::List(selected_rows));
    let tags_result = db
        .run_script(
            r#"want[id] <- $ids
               ?[note_id, tag] := *tags{ note_id, tag }, want[note_id]"#,
            tag_params,
            ScriptMutability::Immutable,
        )
        .map_err(|e| format!("Full graph selected-tags query failed: {e}"))?;

    // Build edges + display degree from the SELECTED set only, mirroring
    // the window query: link edges need both endpoints kept; tag edges
    // need the note kept.
    let mut degree: HashMap<String, usize> = HashMap::new();
    let mut edges: Vec<GraphEdge> = Vec::new();
    let max_edges = max_nodes.saturating_mul(20).max(max_nodes);

    for row in &links_result.rows {
        let Some(source) = row.first().and_then(|v| v.get_str()).map(String::from) else {
            continue;
        };
        let Some(target) = row.get(1).and_then(|v| v.get_str()).map(String::from) else {
            continue;
        };
        if !selected_ids.contains(&source) || !selected_ids.contains(&target) {
            continue;
        }
        if edges.len() >= max_edges {
            break;
        }
        *degree.entry(source.clone()).or_default() += 1;
        *degree.entry(target.clone()).or_default() += 1;
        edges.push(GraphEdge { source, target });
    }

    let mut tag_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    // LR-6: a tag node is an IDENTITY (folded key), not a stored spelling.
    // `seen_note_tags` keeps a note that carries `#ML` and `#ml` to ONE edge.
    let mut tag_identities = TagIdentities::default();
    let mut seen_note_tags: std::collections::HashSet<(String, String)> =
        std::collections::HashSet::new();
    let max_tag_nodes = (max_nodes / 4).max(1);
    for row in &tags_result.rows {
        let Some(note_id) = row.first().and_then(|v| v.get_str()).map(String::from) else {
            continue;
        };
        let Some(tag) = row.get(1).and_then(|v| v.get_str()).map(String::from) else {
            continue;
        };
        if tag.is_empty() || !selected_ids.contains(&note_id) {
            continue;
        }
        let key = tag_identities.add(&tag, 1);
        if !seen_note_tags.insert((note_id.clone(), key.clone())) {
            continue;
        }
        let tag_id = tag_node_id(&key);
        if !tag_ids.contains(&tag_id) && tag_ids.len() >= max_tag_nodes {
            continue;
        }
        if edges.len() >= max_edges {
            break;
        }
        tag_ids.insert(tag_id.clone());
        *degree.entry(note_id.clone()).or_default() += 1;
        *degree.entry(tag_id.clone()).or_default() += 1;
        edges.push(GraphEdge {
            source: note_id,
            target: tag_id,
        });
    }

    // Build note nodes (every selected note — orphans get link_count 0).
    let mut nodes: Vec<GraphNode> = selected
        .into_iter()
        .map(|(id, title, path)| {
            let link_count = degree.get(&id).copied().unwrap_or(0);
            GraphNode {
                id,
                title,
                path,
                kind: "note".to_string(),
                link_count,
            }
        })
        .collect();

    // Append tag nodes adjacent to kept notes.
    for tag_id in &tag_ids {
        let key = tag_id.strip_prefix("tag:").unwrap_or(tag_id);
        // Show a real stored spelling (most used among the kept notes), not
        // the lower-cased key, so `#Q4` still reads `#Q4`.
        let tag_name = tag_identities.display(key).unwrap_or(key);
        nodes.push(GraphNode {
            id: tag_id.clone(),
            title: format!("#{tag_name}"),
            path: String::new(),
            kind: "tag".to_string(),
            link_count: degree.get(tag_id).copied().unwrap_or(0),
        });
    }

    Ok(FullGraph { nodes, edges })
}

/// Viewport-bounded graph fetch — BFS up to `depth` hops out from
/// `focus_id`, capped at `max_nodes`. Replaces `get_full_graph` for
/// visualization at scale: at 50K+ notes the full graph has too many
/// nodes for any force-directed layout to render.
///
/// Edges traversed (treated as undirected for reachability):
///   * note→note `links`
///   * note→tag adjacency (a note and a tag are 1 hop apart)
///
/// Returned edges are filtered to those between visited nodes only —
/// dangling edges to nodes outside the window are dropped, so the
/// frontend doesn't have to filter again.
///
/// `depth = 0` returns just the focus node. Most graph-view callers
/// will want `depth = 2` (focus + neighbors + neighbors-of-neighbors)
/// with `max_nodes` around 200–500.
pub fn get_graph_window(
    db: &DbInstance,
    focus_id: &str,
    depth: usize,
    max_nodes: usize,
) -> Result<FullGraph, String> {
    if max_nodes == 0 {
        return Ok(FullGraph {
            nodes: Vec::new(),
            edges: Vec::new(),
        });
    }

    // LR-6: tag node ids carry the folded identity key. A focus id remembered
    // from before (e.g. a stale `tag:ML`) is folded onto the same key so it
    // still names the node the edges below produce.
    let focus_id = match focus_id.strip_prefix("tag:") {
        Some(rest) => tag_node_id(&fold_tag(rest)),
        None => focus_id.to_string(),
    };
    let focus_id = focus_id.as_str();

    // BFS state. `visited[id] = first_hop_seen` so we never revisit and
    // closer-by nodes always win ties when we hit the cap.
    let mut visited: HashMap<String, usize> = HashMap::new();
    visited.insert(focus_id.to_string(), 0);
    let mut frontier: Vec<String> = vec![focus_id.to_string()];

    'outer: for hop in 1..=depth {
        if visited.len() >= max_nodes {
            break;
        }
        // ONE batched expansion per hop (≤4 Datalog queries total) instead
        // of the old 3 queries × every node in the frontier. A 200-node
        // local view was ~600 scripts; this is bounded by depth.
        let remaining = max_nodes.saturating_sub(visited.len());
        let neighbors = neighbors_of_frontier(db, &frontier, remaining, visited.len())?;
        let mut next_frontier: Vec<String> = Vec::new();
        for neighbor in neighbors {
            if visited.contains_key(&neighbor) {
                continue;
            }
            visited.insert(neighbor.clone(), hop);
            next_frontier.push(neighbor);
            if visited.len() >= max_nodes {
                break 'outer;
            }
        }
        if next_frontier.is_empty() {
            break;
        }
        frontier = next_frontier;
    }

    let visited_set: std::collections::HashSet<String> = visited.keys().cloned().collect();

    // Collect note-node metadata for visited note ids (skip tag-prefixed ids).
    let note_ids: Vec<String> = visited
        .keys()
        .filter(|id| !id.starts_with("tag:"))
        .cloned()
        .collect();
    let mut note_meta: HashMap<String, (String, String)> = HashMap::new();
    if !note_ids.is_empty() {
        let rows: Vec<DataValue> = note_ids
            .iter()
            .map(|id| DataValue::List(vec![DataValue::Str(id.clone().into())]))
            .collect();
        let mut params = BTreeMap::new();
        params.insert("ids".to_string(), DataValue::List(rows));
        // Cozo wants a separate auxiliary rule defining `want` from the
        // parameter, then the main `?[...]` head joins against it. Inline
        // `want[id] <- $ids` inside the head is a parse error.
        let result = db
            .run_script(
                r#"want[id] <- $ids
                   ?[id, title, path] := *notes{ id, title, path }, want[id]"#,
                params,
                ScriptMutability::Immutable,
            )
            .map_err(|e| format!("Window note metadata query failed: {e}"))?;
        for row in &result.rows {
            let Some(id) = row.first().and_then(|v| v.get_str()).map(String::from) else {
                continue;
            };
            let title = row
                .get(1)
                .and_then(|v| v.get_str())
                .map(String::from)
                .unwrap_or_default();
            let path = row
                .get(2)
                .and_then(|v| v.get_str())
                .map(String::from)
                .unwrap_or_default();
            note_meta.insert(id, (title, path));
        }
    }

    // Edges between visited nodes only — links and note→tag.
    //
    // Push the `visited_set` filter into Datalog so Cozo never materializes
    // the full `*links` / `*tags` relations. At 50K notes with 200K edges
    // the old "select-all then filter in Rust" path allocated ~10 MB per
    // window query; the parameterized join here visits only the rows whose
    // endpoints are inside the window.
    let mut degree: HashMap<String, usize> = HashMap::new();
    let mut edges: Vec<GraphEdge> = Vec::new();

    // Note ids in the window — used to filter `*links` by both endpoints.
    let note_id_rows: Vec<DataValue> = note_ids
        .iter()
        .map(|id| DataValue::List(vec![DataValue::Str(id.clone().into())]))
        .collect();

    if !note_id_rows.is_empty() {
        let mut link_params = BTreeMap::new();
        link_params.insert(
            "note_ids".to_string(),
            DataValue::List(note_id_rows.clone()),
        );
        let links_result = db
            .run_script(
                r#"want[id] <- $note_ids
                   ?[from_id, to_id] := *links{ from_id, to_id },
                                       want[from_id], want[to_id]"#,
                link_params,
                ScriptMutability::Immutable,
            )
            .map_err(|e| format!("Window edges query failed: {e}"))?;
        for row in &links_result.rows {
            let Some(source) = row.first().and_then(|v| v.get_str()).map(String::from) else {
                continue;
            };
            let Some(target) = row.get(1).and_then(|v| v.get_str()).map(String::from) else {
                continue;
            };
            *degree.entry(source.clone()).or_default() += 1;
            *degree.entry(target.clone()).or_default() += 1;
            edges.push(GraphEdge { source, target });
        }
    }

    // Tag-edge filter: the source must be a visited note, and the
    // resulting `tag:<tag>` id must also be in the visited set. The note
    // side is filtered in Datalog; the tag-side membership check still
    // happens in Rust because `visited_set` includes the synthetic
    // `tag:<key>` ids that the BFS added (those don't exist in `*tags`).
    let mut tag_identities = TagIdentities::default();
    let mut seen_note_tags: std::collections::HashSet<(String, String)> =
        std::collections::HashSet::new();
    let mut tag_titles: HashMap<String, String> = HashMap::new();
    if !note_id_rows.is_empty() {
        let mut tag_params = BTreeMap::new();
        tag_params.insert("note_ids".to_string(), DataValue::List(note_id_rows));
        let tags_result = db
            .run_script(
                r#"want[id] <- $note_ids
                   ?[note_id, tag] := *tags{ note_id, tag }, want[note_id]"#,
                tag_params,
                ScriptMutability::Immutable,
            )
            .map_err(|e| format!("Window tag edges query failed: {e}"))?;
        for row in &tags_result.rows {
            let Some(note_id) = row.first().and_then(|v| v.get_str()).map(String::from) else {
                continue;
            };
            let Some(tag) = row.get(1).and_then(|v| v.get_str()).map(String::from) else {
                continue;
            };
            if tag.is_empty() {
                continue;
            }
            let key = fold_tag(&tag);
            let tag_id = tag_node_id(&key);
            if !visited_set.contains(&tag_id) {
                continue;
            }
            tag_identities.add(&tag, 1);
            // A note carrying `#ML` and `#ml` gets ONE edge to the one node.
            if !seen_note_tags.insert((note_id.clone(), key)) {
                continue;
            }
            *degree.entry(note_id.clone()).or_default() += 1;
            *degree.entry(tag_id.clone()).or_default() += 1;
            edges.push(GraphEdge {
                source: note_id,
                target: tag_id,
            });
        }
        // Title = a real stored spelling (most used in this window), so
        // `#Q4` still reads `#Q4`; the node id carries the folded key.
        for id in &visited_set {
            if let Some(key) = id.strip_prefix("tag:") {
                if let Some(display) = tag_identities.display(key) {
                    tag_titles.insert(id.clone(), format!("#{display}"));
                }
            }
        }
    }

    // Build node list. Note nodes first (with metadata), then tag nodes.
    let mut nodes: Vec<GraphNode> = Vec::with_capacity(visited.len());
    for id in visited.keys() {
        if id.starts_with("tag:") {
            let title = tag_titles.get(id).cloned().unwrap_or_else(|| {
                id.strip_prefix("tag:")
                    .map(String::from)
                    .unwrap_or_default()
            });
            nodes.push(GraphNode {
                id: id.clone(),
                title,
                path: String::new(),
                kind: "tag".to_string(),
                link_count: degree.get(id).copied().unwrap_or(0),
            });
        } else if let Some((title, path)) = note_meta.get(id) {
            nodes.push(GraphNode {
                id: id.clone(),
                title: title.clone(),
                path: path.clone(),
                kind: "note".to_string(),
                link_count: degree.get(id).copied().unwrap_or(0),
            });
        } else {
            // Visited but not in notes table — happens for the focus id when
            // it doesn't exist (caller passed a bad id). Surface it as an
            // empty-titled note so the frontend can show "missing".
            nodes.push(GraphNode {
                id: id.clone(),
                title: id.clone(),
                path: String::new(),
                kind: "note".to_string(),
                link_count: degree.get(id).copied().unwrap_or(0),
            });
        }
    }

    Ok(FullGraph { nodes, edges })
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod window_tests {
    use super::*;
    use cozo::DbInstance;

    /// Spin up an in-memory DB with `notes`, `links`, `tags` (no HNSW —
    /// the window query never touches embeddings).
    fn mem_db_with_graph() -> DbInstance {
        let db = DbInstance::new("mem", "", Default::default()).expect("mem db");
        db.run_script(
            r#":create notes { id: String => title: String, path: String, updated_at: Float }"#,
            Default::default(),
            ScriptMutability::Mutable,
        )
        .expect("create notes");
        db.run_script(
            r#":create links { from_id: String, to_id: String => link_text: String }"#,
            Default::default(),
            ScriptMutability::Mutable,
        )
        .expect("create links");
        db.run_script(
            r#":create tags { note_id: String, tag: String }"#,
            Default::default(),
            ScriptMutability::Mutable,
        )
        .expect("create tags");
        db.run_script(
            r#"::index create links:by_to {to_id}"#,
            Default::default(),
            ScriptMutability::Mutable,
        )
        .expect("create reverse-link index");
        db
    }

    fn add_note(db: &DbInstance, id: &str) {
        let mut params = BTreeMap::new();
        params.insert("id".to_string(), DataValue::Str(id.to_string().into()));
        params.insert("title".to_string(), DataValue::Str(id.to_string().into()));
        params.insert(
            "path".to_string(),
            DataValue::Str(format!("/roost/{id}.md").into()),
        );
        params.insert("updated_at".to_string(), DataValue::from(0.0_f64));
        db.run_script(
            r#"?[id, title, path, updated_at] <- [[$id, $title, $path, $updated_at]]
               :put notes { id => title, path, updated_at }"#,
            params,
            ScriptMutability::Mutable,
        )
        .expect("put note");
    }

    fn add_link(db: &DbInstance, from: &str, to: &str) {
        let mut params = BTreeMap::new();
        params.insert("from".to_string(), DataValue::Str(from.to_string().into()));
        params.insert("to".to_string(), DataValue::Str(to.to_string().into()));
        params.insert("text".to_string(), DataValue::Str(to.to_string().into()));
        db.run_script(
            r#"?[from_id, to_id, link_text] <- [[$from, $to, $text]]
               :put links { from_id, to_id => link_text }"#,
            params,
            ScriptMutability::Mutable,
        )
        .expect("put link");
    }

    fn add_tag(db: &DbInstance, note_id: &str, tag: &str) {
        let mut params = BTreeMap::new();
        params.insert(
            "note_id".to_string(),
            DataValue::Str(note_id.to_string().into()),
        );
        params.insert("tag".to_string(), DataValue::Str(tag.to_string().into()));
        db.run_script(
            r#"?[note_id, tag] <- [[$note_id, $tag]]
               :put tags { note_id, tag }"#,
            params,
            ScriptMutability::Mutable,
        )
        .expect("put tag");
    }

    #[test]
    fn window_depth_zero_returns_focus_only() {
        let db = mem_db_with_graph();
        add_note(&db, "a");
        add_note(&db, "b");
        add_link(&db, "a", "b");

        let g = get_graph_window(&db, "a", 0, 100).expect("window");
        assert_eq!(g.nodes.len(), 1);
        assert_eq!(g.nodes[0].id, "a");
        assert!(g.edges.is_empty(), "no edges should leave the focus");
    }

    #[test]
    fn window_one_hop_includes_direct_neighbors() {
        let db = mem_db_with_graph();
        for n in &["a", "b", "c", "d"] {
            add_note(&db, n);
        }
        add_link(&db, "a", "b");
        add_link(&db, "c", "a"); // incoming counts
        add_link(&db, "b", "d"); // 2 hops out — must NOT appear at depth 1

        let g = get_graph_window(&db, "a", 1, 100).expect("window");
        let ids: std::collections::HashSet<_> = g.nodes.iter().map(|n| n.id.as_str()).collect();
        assert!(ids.contains("a"));
        assert!(ids.contains("b"));
        assert!(ids.contains("c"));
        assert!(!ids.contains("d"), "d is 2 hops away — must be excluded");
    }

    #[test]
    fn window_max_nodes_cap_bounds_result() {
        let db = mem_db_with_graph();
        add_note(&db, "hub");
        for i in 0..50 {
            let id = format!("n{i}");
            add_note(&db, &id);
            add_link(&db, "hub", &id);
        }
        let g = get_graph_window(&db, "hub", 1, 10).expect("window");
        assert!(g.nodes.len() <= 10, "must respect max_nodes");
    }

    #[test]
    fn window_drops_edges_to_unvisited_nodes() {
        // a → b → c. Window depth=1 from a includes {a, b}. The b→c edge
        // must NOT appear in the result, because c is outside the window.
        let db = mem_db_with_graph();
        for n in &["a", "b", "c"] {
            add_note(&db, n);
        }
        add_link(&db, "a", "b");
        add_link(&db, "b", "c");

        let g = get_graph_window(&db, "a", 1, 100).expect("window");
        let ids: std::collections::HashSet<_> = g.nodes.iter().map(|n| n.id.as_str()).collect();
        assert!(ids.contains("a") && ids.contains("b"));
        assert!(!ids.contains("c"));
        for e in &g.edges {
            assert!(
                ids.contains(e.source.as_str()) && ids.contains(e.target.as_str()),
                "edge endpoints must be within the visited set"
            );
        }
    }

    #[test]
    fn window_includes_tag_neighbors() {
        let db = mem_db_with_graph();
        add_note(&db, "a");
        add_tag(&db, "a", "ml");

        let g = get_graph_window(&db, "a", 1, 100).expect("window");
        let ids: std::collections::HashSet<_> = g.nodes.iter().map(|n| n.id.as_str()).collect();
        assert!(ids.contains("tag:ml"));
        assert_eq!(g.edges.len(), 1, "exactly one note→tag edge");
    }

    #[test]
    fn popular_tag_expansion_respects_window_budget() {
        let db = mem_db_with_graph();
        for i in 0..1_000 {
            let id = format!("n{i}");
            add_note(&db, &id);
            add_tag(&db, &id, "popular");
        }

        let g = get_graph_window(&db, "n0", 2, 25).expect("window");
        assert!(g.nodes.len() <= 25);
        assert!(g.nodes.iter().any(|n| n.id == "tag:popular"));
    }

    /// Batched-frontier BFS must produce the same visited set the old
    /// per-node expansion would: a 2-hop window over a branching graph,
    /// including a tag reached only at hop 2. Pins the rewrite.
    #[test]
    fn window_two_hop_batched_expansion_matches_expected() {
        let db = mem_db_with_graph();
        for n in &["a", "b", "c", "d", "e"] {
            add_note(&db, n);
        }
        add_link(&db, "a", "b"); // hop 1
        add_link(&db, "a", "e"); // hop 1
        add_link(&db, "b", "c"); // hop 2 (via b)
        add_link(&db, "c", "d"); // hop 3 — excluded at depth 2
        add_tag(&db, "b", "ml"); // tag:ml reached at hop 2

        let g = get_graph_window(&db, "a", 2, 100).expect("window");
        let ids: std::collections::HashSet<_> = g.nodes.iter().map(|n| n.id.as_str()).collect();
        assert!(ids.contains("a"));
        assert!(ids.contains("b"));
        assert!(ids.contains("e"));
        assert!(ids.contains("c"));
        assert!(ids.contains("tag:ml"), "tag reached at hop 2 must appear");
        assert!(!ids.contains("d"), "d is 3 hops away — must be excluded");
    }

    /// Incoming links expand the frontier too (directed graph traversed as
    /// undirected for reachability), through the batched path.
    #[test]
    fn window_batched_follows_incoming_links() {
        let db = mem_db_with_graph();
        for n in &["focus", "x", "y"] {
            add_note(&db, n);
        }
        add_link(&db, "x", "focus"); // incoming to focus → x at hop 1
        add_link(&db, "y", "x"); // incoming to x → y at hop 2

        let g = get_graph_window(&db, "focus", 2, 100).expect("window");
        let ids: std::collections::HashSet<_> = g.nodes.iter().map(|n| n.id.as_str()).collect();
        assert!(ids.contains("focus") && ids.contains("x") && ids.contains("y"));
    }

    /// Regression for the "Full graph shows only 1 node" bug: a roost of
    /// disconnected notes. `get_graph_window` (BFS) can only see the focus
    /// node's component — here, just the focus. `get_full_graph` must
    /// return every note regardless of links.
    #[test]
    fn full_graph_includes_disconnected_orphans() {
        let db = mem_db_with_graph();
        for n in &["welcome", "e-idstack", "corvux-skill"] {
            add_note(&db, n);
        }
        // A self-link on "welcome" (mirrors the real Welcome note) — 1 node,
        // 1 edge is exactly what the window returned in the bug report.
        add_link(&db, "welcome", "welcome");

        let window = get_graph_window(&db, "welcome", 20, 1000).expect("window");
        assert_eq!(
            window.nodes.len(),
            1,
            "window from welcome can't reach the other islands"
        );

        let full = get_full_graph(&db, 1000).expect("full");
        let ids: std::collections::HashSet<_> = full.nodes.iter().map(|n| n.id.as_str()).collect();
        assert!(ids.contains("welcome"));
        assert!(ids.contains("e-idstack"));
        assert!(ids.contains("corvux-skill"));
        assert_eq!(full.nodes.len(), 3, "every note is a node, links or not");
    }

    #[test]
    fn full_graph_includes_links_and_tag_nodes() {
        let db = mem_db_with_graph();
        add_note(&db, "a");
        add_note(&db, "b");
        add_link(&db, "a", "b");
        add_tag(&db, "a", "ml");

        let full = get_full_graph(&db, 1000).expect("full");
        let ids: std::collections::HashSet<_> = full.nodes.iter().map(|n| n.id.as_str()).collect();
        assert!(ids.contains("a") && ids.contains("b") && ids.contains("tag:ml"));
        // one a→b link edge + one a→tag edge
        assert_eq!(full.edges.len(), 2);
    }

    #[test]
    fn full_graph_caps_synthetic_tag_nodes() {
        let db = mem_db_with_graph();
        add_note(&db, "a");
        for i in 0..20 {
            add_tag(&db, "a", &format!("tag-{i}"));
        }

        let full = get_full_graph(&db, 4).expect("full");
        assert_eq!(full.nodes.iter().filter(|n| n.kind == "note").count(), 1);
        assert!(
            full.nodes.iter().filter(|n| n.kind == "tag").count() <= 1,
            "tag nodes are capped to one quarter of the note budget"
        );
        assert!(full.edges.len() <= 4 * 20, "edge output is hard-capped");
    }

    #[test]
    fn full_graph_caps_and_keeps_highest_degree() {
        let db = mem_db_with_graph();
        // "hub" is linked to by many; orphans have degree 0.
        add_note(&db, "hub");
        for i in 0..20 {
            let id = format!("n{i}");
            add_note(&db, &id);
            add_link(&db, &id, "hub");
        }
        for i in 0..20 {
            add_note(&db, &format!("orphan{i}"));
        }

        let full = get_full_graph(&db, 5).expect("full");
        let note_nodes = full.nodes.iter().filter(|n| n.kind == "note").count();
        assert!(note_nodes <= 5, "must respect max_nodes");
        let ids: std::collections::HashSet<_> = full.nodes.iter().map(|n| n.id.as_str()).collect();
        assert!(
            ids.contains("hub"),
            "the highest-degree node must survive truncation"
        );
    }

    /// AUDIT-023: passing a mixed-case `note_id` to a read query must
    /// fire the debug_assert in debug builds (catching the violator at
    /// the call site instead of letting the query silently return empty).
    /// Release builds skip the assert by design.
    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "note_id must be lowercased")]
    fn mixed_case_note_id_panics_debug() {
        let db = mem_db_with_graph();
        add_note(&db, "a");
        let _ = get_backlinks(&db, "Mixed/Case");
    }

    #[cfg(debug_assertions)]
    #[test]
    fn canonical_note_id_works_debug() {
        let db = mem_db_with_graph();
        add_note(&db, "lowercase/id");
        assert!(get_backlinks(&db, "lowercase/id").is_ok());
    }

    // ── LR-6 / LR-7: tag identity in the graph ───────────────────────────
    // Stored rows are VERBATIM (`add_tag` below writes them exactly as the
    // indexer would); every read folds them onto one identity.

    fn tag_nodes(g: &FullGraph) -> Vec<&GraphNode> {
        g.nodes.iter().filter(|n| n.kind == "tag").collect()
    }

    #[test]
    fn lr6_full_graph_merges_case_variants_into_one_tag_node() {
        let db = mem_db_with_graph();
        for n in ["a", "b", "c"] {
            add_note(&db, n);
        }
        add_tag(&db, "a", "ML");
        add_tag(&db, "a", "ml"); // same note, two spellings
        add_tag(&db, "b", "ml");
        add_tag(&db, "c", "Ml");

        let full = get_full_graph(&db, 1000).expect("full");
        let tags = tag_nodes(&full);
        assert_eq!(tags.len(), 1, "ML / ml / Ml are ONE tag node: {tags:?}");
        assert_eq!(tags[0].id, "tag:ml");
        assert_eq!(tags[0].title, "#ml", "most-used stored spelling is shown");
        assert_eq!(
            full.edges.len(),
            3,
            "one edge per NOTE, not per stored spelling"
        );
        assert_eq!(tags[0].link_count, 3);
        // Storage is untouched, so the stats cache (row counts) stays exact.
        assert_eq!(get_graph_stats(&db).unwrap().tags, 4);
    }

    #[test]
    fn lr6_tag_node_keeps_a_real_spelling_in_its_title() {
        let db = mem_db_with_graph();
        add_note(&db, "a");
        add_tag(&db, "a", "Q4");

        let full = get_full_graph(&db, 1000).expect("full");
        let tags = tag_nodes(&full);
        assert_eq!(
            (tags[0].id.as_str(), tags[0].title.as_str()),
            ("tag:q4", "#Q4")
        );
        let win = get_graph_window(&db, "a", 1, 100).expect("window");
        let wtags = tag_nodes(&win);
        assert_eq!(
            (wtags[0].id.as_str(), wtags[0].title.as_str()),
            ("tag:q4", "#Q4")
        );
    }

    #[test]
    fn lr6_window_scalar_frontier_merges_case_variants() {
        let db = mem_db_with_graph();
        add_note(&db, "a");
        add_note(&db, "b");
        add_link(&db, "a", "b");
        add_tag(&db, "a", "ML");
        add_tag(&db, "b", "ml");

        let g = get_graph_window(&db, "a", 1, 100).expect("window");
        assert_eq!(tag_nodes(&g).len(), 1);
        assert_eq!(tag_nodes(&g)[0].id, "tag:ml");
        // a→b, a→tag, b→tag
        assert_eq!(g.edges.len(), 3);
    }

    #[test]
    fn lr6_window_batched_frontier_merges_case_variants() {
        // > 32 frontier notes at hop 2 takes the batched (list-rule) path.
        let db = mem_db_with_graph();
        add_note(&db, "a");
        for i in 0..40 {
            let id = format!("n{i}");
            add_note(&db, &id);
            add_link(&db, "a", &id);
            add_tag(&db, &id, if i % 2 == 0 { "ML" } else { "ml" });
        }
        let g = get_graph_window(&db, "a", 2, 200).expect("window");
        let tags = tag_nodes(&g);
        assert_eq!(tags.len(), 1, "both spellings reached via the batched path");
        assert_eq!(tags[0].id, "tag:ml");
        assert_eq!(
            g.edges.iter().filter(|e| e.target == "tag:ml").count(),
            40,
            "one edge per note"
        );
    }

    #[test]
    fn lr6_window_focus_on_a_stale_tag_id_is_folded() {
        let db = mem_db_with_graph();
        add_note(&db, "a");
        add_tag(&db, "a", "ML");
        let g = get_graph_window(&db, "tag:ML", 0, 10).expect("window");
        assert_eq!(g.nodes.len(), 1);
        assert_eq!(g.nodes[0].id, "tag:ml");
    }

    #[test]
    fn lr6_get_note_tags_lists_one_entry_per_identity() {
        let db = mem_db_with_graph();
        add_note(&db, "a");
        add_tag(&db, "a", "ML");
        add_tag(&db, "a", "ml");
        add_tag(&db, "a", "Other");
        let tags = get_note_tags(&db, "a").unwrap();
        assert_eq!(tags.len(), 2, "{tags:?}");
        assert!(tags.contains(&"Other".to_string()));
    }

    #[test]
    fn lr7_nfc_and_nfd_spellings_are_one_graph_tag() {
        let db = mem_db_with_graph();
        add_note(&db, "a");
        add_note(&db, "b");
        add_tag(&db, "a", "Café");
        add_tag(&db, "b", "cafe\u{301}"); // decomposed
        let full = get_full_graph(&db, 1000).expect("full");
        let tags = tag_nodes(&full);
        assert_eq!(tags.len(), 1);
        assert_eq!(tags[0].id, "tag:café");
    }

    /// Old graph.db compat: rows written before this change (verbatim,
    /// mixed case, a truncated `caf` from the ASCII-only parser) fold with
    /// rows written after it — no migration, no reindex.
    #[test]
    fn lr6_rows_from_before_and_after_the_change_share_identities() {
        let db = mem_db_with_graph();
        add_note(&db, "old");
        add_note(&db, "new");
        add_tag(&db, "old", "Ideas"); // indexed by the old parser
        add_tag(&db, "old", "caf"); // old parser truncated `#café`
        add_tag(&db, "new", "ideas"); // indexed by the new parser
        add_tag(&db, "new", "café");
        let full = get_full_graph(&db, 1000).expect("full");
        let ids: std::collections::BTreeSet<_> =
            tag_nodes(&full).iter().map(|n| n.id.clone()).collect();
        assert_eq!(
            ids,
            ["tag:caf", "tag:café", "tag:ideas"]
                .into_iter()
                .map(String::from)
                .collect(),
            "`Ideas`/`ideas` merge; the stale truncated `caf` stays a separate \
             (known, self-healing on re-save) residual"
        );
    }
}

/// Batched neighbor expansion for an entire BFS frontier — note→note
/// links (both directions) and note→tag adjacency — in at most THREE
/// Datalog queries total (regardless of frontier size),
/// versus the old `neighbors_of` which ran 3 queries PER node PER hop.
/// The frontier is pushed into Datalog as a parameter list and joined
/// against, so Cozo never materializes the full `*links` / `*tags`
/// relations. Returns connected ids — note ids (no prefix) and tag ids
/// (`tag:` prefix); duplicates are fine (the BFS `visited` set dedups).
fn neighbors_of_frontier(
    db: &DbInstance,
    frontier: &[String],
    remaining: usize,
    visited_len: usize,
) -> Result<Vec<String>, String> {
    if remaining == 0 {
        return Ok(Vec::new());
    }
    let mut out: Vec<String> = Vec::new();
    // Each relation can overlap with another (an id may be both incoming and
    // outgoing), so allow 2× headroom before the BFS visited-set deduplicates.
    // The old unbounded tag→note query returned every note carrying a popular
    // tag even when only a few window slots remained.
    let query_limit = remaining
        .saturating_add(visited_len)
        .saturating_mul(2)
        .max(remaining);

    // Tag nodes are leaves for BFS. Expanding tag→note turns a common tag
    // into an enormous implicit clique (`#work` can touch 10K notes), making a
    // local window both arbitrary and O(roost). Notes still expose their tag
    // neighbors, so tags remain visible without becoming traversal hubs.
    let note_ids: Vec<&str> = frontier
        .iter()
        .filter(|id| !id.starts_with("tag:"))
        .map(String::as_str)
        .collect();
    let note_id_rows: Vec<DataValue> = note_ids
        .iter()
        .map(|id| DataValue::List(vec![DataValue::Str((*id).to_string().into())]))
        .collect();

    // A small frontier is the normal local-view case. Binding a scalar key
    // lets Cozo seek the primary/reverse indices directly; the list-rule join
    // below can choose a relation scan on the SQLite backend, which made a
    // sparse 50K-note window scale linearly with the entire roost. Keep the
    // batched path for broad frontiers so a hub never causes hundreds of IPC-
    // internal scripts.
    if !note_ids.is_empty() && note_ids.len() <= 32 {
        for id in note_ids {
            let mut params = BTreeMap::new();
            params.insert("id".to_string(), DataValue::Str(id.to_string().into()));
            params.insert(
                "limit".to_string(),
                DataValue::Num(Num::Int(query_limit.min(i64::MAX as usize) as i64)),
            );
            for (script, label, prefix_tag) in [
                (
                    r#"?[neighbor] := *links{ from_id: $id, to_id: neighbor }
                       :limit $limit"#,
                    "outgoing",
                    false,
                ),
                (
                    r#"?[neighbor] := *links:by_to{ to_id: $id, from_id: neighbor }
                       :limit $limit"#,
                    "incoming",
                    false,
                ),
                (
                    r#"?[neighbor] := *tags{ note_id: $id, tag: neighbor }
                       :limit $limit"#,
                    "tags",
                    true,
                ),
            ] {
                let result = db
                    .run_script(script, params.clone(), ScriptMutability::Immutable)
                    .map_err(|e| format!("Frontier scalar {label} query failed: {e}"))?;
                for row in &result.rows {
                    if let Some(value) = row.first().and_then(|v| v.get_str()) {
                        if !value.is_empty() {
                            out.push(if prefix_tag {
                                tag_node_id(&fold_tag(value))
                            } else {
                                value.to_string()
                            });
                        }
                    }
                }
            }
        }
        return Ok(out);
    }
    if !note_id_rows.is_empty() {
        let mut params = BTreeMap::new();
        params.insert("ids".to_string(), DataValue::List(note_id_rows));
        params.insert(
            "limit".to_string(),
            DataValue::Num(Num::Int(query_limit.min(i64::MAX as usize) as i64)),
        );

        // Outgoing note→note (frontier node is the source).
        let outgoing = db
            .run_script(
                r#"want[id] <- $ids
                   ?[to_id] := *links{ from_id, to_id }, want[from_id]
                   :limit $limit"#,
                params.clone(),
                ScriptMutability::Immutable,
            )
            .map_err(|e| format!("Frontier outgoing links query failed: {e}"))?;
        for row in &outgoing.rows {
            if let Some(s) = row.first().and_then(|v| v.get_str()) {
                out.push(s.to_string());
            }
        }
        // Incoming note→note (frontier node is the target).
        let incoming = db
            .run_script(
                r#"want[id] <- $ids
                   ?[from_id] := *links:by_to{ from_id, to_id }, want[to_id]
                   :limit $limit"#,
                params.clone(),
                ScriptMutability::Immutable,
            )
            .map_err(|e| format!("Frontier incoming links query failed: {e}"))?;
        for row in &incoming.rows {
            if let Some(s) = row.first().and_then(|v| v.get_str()) {
                out.push(s.to_string());
            }
        }
        // Tags carried by frontier notes.
        let note_tags = db
            .run_script(
                r#"want[id] <- $ids
                   ?[tag] := *tags{ note_id, tag }, want[note_id]
                   :limit $limit"#,
                params,
                ScriptMutability::Immutable,
            )
            .map_err(|e| format!("Frontier note-tags query failed: {e}"))?;
        for row in &note_tags.rows {
            if let Some(s) = row.first().and_then(|v| v.get_str()) {
                if !s.is_empty() {
                    out.push(tag_node_id(&fold_tag(s)));
                }
            }
        }
    }

    Ok(out)
}
