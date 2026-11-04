//! Built-in CozoDB graph algorithms over the `links` relation.
//!
//! These are the moat features that justified picking CozoDB over plain
//! SQLite. None of them existed in the codebase before Phase 1 — the
//! infrastructure was ready, the queries were not. They share two
//! conventions:
//!
//! - Edges are derived from `*links` via a small auxiliary rule, since
//!   Cozo's fixed-rule algorithms expect a `[from, to, weight?]` shape
//!   and our base relation has a `link_text` column we don't need.
//! - Empty-graph defense: if `links` is empty, Cozo errors when the
//!   inner edge rule produces no rows. We catch that and return an
//!   empty result rather than propagate an opaque error.

use cozo::{DataValue, DbInstance, Num, ScriptMutability};
use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Debug, Serialize, Clone, specta::Type)]
pub struct PageRankEntry {
    pub note_id: String,
    pub rank: f64,
}

#[derive(Debug, Serialize, Clone, specta::Type)]
pub struct CommunityEntry {
    pub note_id: String,
    #[specta(type = i32)]
    pub community: i64,
}

#[derive(Debug, Serialize, Clone, specta::Type)]
pub struct CentralityEntry {
    pub note_id: String,
    pub score: f64,
}

#[derive(Debug, Serialize, Clone, specta::Type)]
pub struct ShortestPathResult {
    pub from_id: String,
    pub to_id: String,
    pub cost: f64,
    pub path: Vec<String>,
}

/// PageRank over the wiki-link graph. Returns up to `limit` notes
/// ordered by rank descending — the "hub" notes everything connects
/// through.
///
/// Directed: an incoming link is a vote of relevance, matching the
/// classic PageRank semantics. Tags are NOT included — they'd dominate
/// the ranking for any reasonably-tagged roost.
pub fn page_rank(db: &DbInstance, limit: usize) -> Result<Vec<PageRankEntry>, String> {
    if !has_any_links(db)? {
        return Ok(Vec::new());
    }
    let mut params = BTreeMap::new();
    params.insert("limit".to_string(), DataValue::Num(Num::Int(limit as i64)));
    // Project links to (from, to) — Cozo's weighted-edge algorithms read
    // the third column as a numeric weight, and our `link_text` is a
    // String. PageRank ignores weights but the aux rule keeps all four
    // queries in this file consistent.
    let result = db
        .run_script(
            r#"edges[a, b] := *links{ from_id: a, to_id: b }
               ?[note_id, rank] <~ PageRank(edges[], theta: 0.85, epsilon: 0.0001, iterations: 20)
               :order -rank
               :limit $limit"#,
            params,
            ScriptMutability::Immutable,
        )
        .map_err(|e| format!("PageRank failed: {e}"))?;

    let mut out = Vec::with_capacity(result.rows.len());
    for row in &result.rows {
        let Some(id) = row.first().and_then(|v| v.get_str()).map(String::from) else {
            continue;
        };
        let rank = match row.get(1) {
            Some(DataValue::Num(Num::Float(f))) => *f,
            Some(DataValue::Num(Num::Int(i))) => *i as f64,
            _ => continue,
        };
        out.push(PageRankEntry { note_id: id, rank });
    }
    Ok(out)
}

/// Louvain community detection — assigns each connected note a
/// community label. Treated as undirected because two notes linking in
/// either direction share a topic; insisting on edge direction would
/// over-fragment communities.
///
/// `keep_depth: 1` flattens Cozo's hierarchical output (Louvain produces
/// labels at every level of the agglomeration) down to a single label
/// per node. We only render a flat coloring in the graph view; the
/// hierarchy isn't surfaced.
///
/// Returns one row per (note_id → community label).
///
/// AUDIT-039: community ids are NOT stable across recomputes. Cozo's
/// Louvain is stochastic with no exposed seed parameter, so the integer
/// label for any given note can change every time this runs. Use the
/// label only as a same-cluster grouping key within one result set —
/// never persist it outside the cache, never display it as an
/// identifier, never use it to diff "the community moved" between runs.
pub fn community_detection(db: &DbInstance) -> Result<Vec<CommunityEntry>, String> {
    if !has_any_links(db)? {
        return Ok(Vec::new());
    }
    let result = db
        .run_script(
            r#"edges[a, b] := *links{ from_id: a, to_id: b }
               ?[labels, note_id] <~ CommunityDetectionLouvain(edges[], undirected: true, keep_depth: 1)"#,
            Default::default(),
            ScriptMutability::Immutable,
        )
        .map_err(|e| format!("CommunityDetection failed: {e}"))?;

    let mut out = Vec::with_capacity(result.rows.len());
    for row in &result.rows {
        // Cozo returns `(labels_list, node)`. `labels` is a List<Int>
        // of community ids — one per kept hierarchy level. With
        // `keep_depth: 1` it has exactly one element.
        let community = match row.first() {
            Some(DataValue::List(items)) => match items.first() {
                Some(DataValue::Num(Num::Int(i))) => *i,
                _ => continue,
            },
            _ => continue,
        };
        let id = match row.get(1) {
            Some(DataValue::Str(s)) => s.to_string(),
            _ => continue,
        };
        out.push(CommunityEntry {
            note_id: id,
            community,
        });
    }
    Ok(out)
}

/// Shortest path from `from_id` to `to_id` over the wiki-link graph,
/// treated as undirected (the user usually means "how are these
/// connected" without caring about edge direction). Returns `None` if
/// the two notes aren't reachable from each other.
pub fn shortest_path(
    db: &DbInstance,
    from_id: &str,
    to_id: &str,
) -> Result<Option<ShortestPathResult>, String> {
    if !has_any_links(db)? {
        return Ok(None);
    }
    let mut params = BTreeMap::new();
    params.insert(
        "from".to_string(),
        DataValue::Str(from_id.to_string().into()),
    );
    params.insert("to".to_string(), DataValue::Str(to_id.to_string().into()));

    let result = db
        .run_script(
            r#"edges[a, b] := *links{ from_id: a, to_id: b }
               starting[] <- [[$from]]
               goals[] <- [[$to]]
               ?[from_id, to_id, cost, path] <~ ShortestPathDijkstra(edges[], starting[], goals[], undirected: true)"#,
            params,
            ScriptMutability::Immutable,
        )
        .map_err(|e| format!("ShortestPathDijkstra failed: {e}"))?;

    let row = match result.rows.into_iter().next() {
        Some(r) => r,
        None => return Ok(None),
    };

    let from_out = row
        .first()
        .and_then(|v| v.get_str())
        .map(String::from)
        .unwrap_or_default();
    let to_out = row
        .get(1)
        .and_then(|v| v.get_str())
        .map(String::from)
        .unwrap_or_default();
    let cost = match row.get(2) {
        Some(DataValue::Num(Num::Float(f))) => *f,
        Some(DataValue::Num(Num::Int(i))) => *i as f64,
        _ => return Ok(None),
    };
    // Cozo's Dijkstra emits a row even for unreachable goals, with
    // cost = +Infinity and an empty path. The user-meaningful answer in
    // that case is "no path" — surface it as None so callers don't
    // render an Inf-cost route.
    if !cost.is_finite() {
        return Ok(None);
    }
    let path = match row.get(3) {
        Some(DataValue::List(items)) => items
            .iter()
            .filter_map(|v| v.get_str().map(String::from))
            .collect(),
        _ => Vec::new(),
    };

    Ok(Some(ShortestPathResult {
        from_id: from_out,
        to_id: to_out,
        cost,
        path,
    }))
}

/// Betweenness centrality — finds "bridge" notes that sit on the
/// shortest paths between many other pairs of notes. High score = a
/// note that, if removed, would split the graph into more components.
///
/// Cozo's docs explicitly call this expensive on medium/large graphs.
/// Callers must clamp via `limit` and only run this on idle (or on a
/// capped subgraph). Not safe to call on every save in a 50K-note roost.
pub fn betweenness_centrality(
    db: &DbInstance,
    limit: usize,
) -> Result<Vec<CentralityEntry>, String> {
    if !has_any_links(db)? {
        return Ok(Vec::new());
    }
    let mut params = BTreeMap::new();
    params.insert("limit".to_string(), DataValue::Num(Num::Int(limit as i64)));
    let result = db
        .run_script(
            r#"edges[a, b] := *links{ from_id: a, to_id: b }
               ?[note_id, score] <~ BetweennessCentrality(edges[], undirected: true)
               :order -score
               :limit $limit"#,
            params,
            ScriptMutability::Immutable,
        )
        .map_err(|e| format!("BetweennessCentrality failed: {e}"))?;

    let mut out = Vec::with_capacity(result.rows.len());
    for row in &result.rows {
        let Some(id) = row.first().and_then(|v| v.get_str()).map(String::from) else {
            continue;
        };
        let score = match row.get(1) {
            Some(DataValue::Num(Num::Float(f))) => *f,
            Some(DataValue::Num(Num::Int(i))) => *i as f64,
            _ => continue,
        };
        out.push(CentralityEntry { note_id: id, score });
    }
    Ok(out)
}

// ── Cache layer for expensive algorithms ──────────────────────────────
//
// PageRank/CommunityDetectionLouvain/BetweennessCentrality all run over
// the full `links` relation. On a 50K-note roost that's hundreds of ms
// at minimum per algo (BetweennessCentrality is much worse). Recomputing
// on every graph-view paint would freeze the UI.
//
// Strategy: persist results in `graph_algo_cache`. Recompute only when:
//   1. The cache is empty for that algo (first call after roost open).
//   2. A note save invalidates the cache (any link change can shift
//      hub ranks and community boundaries).

const ALGO_PAGERANK: &str = "pagerank";
const ALGO_COMMUNITY: &str = "community";
const ALGO_BETWEENNESS: &str = "betweenness";

/// Read cached PageRank results, ordered by rank descending.
pub fn cached_page_rank(db: &DbInstance, limit: usize) -> Result<Vec<PageRankEntry>, String> {
    read_cache(db, ALGO_PAGERANK, limit, true).map(|rows| {
        rows.into_iter()
            .map(|(note_id, value)| PageRankEntry {
                note_id,
                rank: value,
            })
            .collect()
    })
}

/// Read cached community labels for every note.
pub fn cached_community_detection(db: &DbInstance) -> Result<Vec<CommunityEntry>, String> {
    read_cache(db, ALGO_COMMUNITY, i64::MAX as usize, false).map(|rows| {
        rows.into_iter()
            .map(|(note_id, value)| CommunityEntry {
                note_id,
                community: value as i64,
            })
            .collect()
    })
}

/// Read cached BetweennessCentrality results, ordered by score descending.
pub fn cached_betweenness_centrality(
    db: &DbInstance,
    limit: usize,
) -> Result<Vec<CentralityEntry>, String> {
    read_cache(db, ALGO_BETWEENNESS, limit, true).map(|rows| {
        rows.into_iter()
            .map(|(note_id, value)| CentralityEntry {
                note_id,
                score: value,
            })
            .collect()
    })
}

/// Compute PageRank fresh and persist into the cache. Returns the
/// freshly-computed entries. Callers (graph view) should prefer
/// `cached_page_rank` and only fall through to this when the cache is
/// empty (e.g. after invalidation).
pub fn recompute_page_rank(db: &DbInstance, limit: usize) -> Result<Vec<PageRankEntry>, String> {
    // Compute over the FULL graph (no limit) so we cache everything,
    // then return the top `limit` to the caller.
    let all = page_rank(db, i64::MAX as usize)?;
    write_cache(
        db,
        ALGO_PAGERANK,
        all.iter().map(|e| (e.note_id.as_str(), e.rank)),
    )?;
    let mut top = all;
    top.truncate(limit);
    Ok(top)
}

/// Compute Community detection fresh and persist into the cache.
pub fn recompute_community_detection(db: &DbInstance) -> Result<Vec<CommunityEntry>, String> {
    let all = community_detection(db)?;
    write_cache(
        db,
        ALGO_COMMUNITY,
        all.iter().map(|e| (e.note_id.as_str(), e.community as f64)),
    )?;
    Ok(all)
}

/// Compute BetweennessCentrality fresh and persist into the cache.
pub fn recompute_betweenness_centrality(
    db: &DbInstance,
    limit: usize,
) -> Result<Vec<CentralityEntry>, String> {
    let all = betweenness_centrality(db, i64::MAX as usize)?;
    write_cache(
        db,
        ALGO_BETWEENNESS,
        all.iter().map(|e| (e.note_id.as_str(), e.score)),
    )?;
    let mut top = all;
    top.truncate(limit);
    Ok(top)
}

/// Drop every cached result. Called from `index_note` after a save —
/// any link change can shift PageRank ranks, community boundaries, and
/// betweenness scores. Cheap (one bulk `:rm`).
pub fn invalidate_algo_cache(db: &DbInstance) -> Result<(), String> {
    db.run_script(
        r#"?[algo, note_id] := *graph_algo_cache{ algo, note_id }
           :rm graph_algo_cache { algo, note_id }"#,
        Default::default(),
        ScriptMutability::Mutable,
    )
    .map(|_| ())
    .map_err(|e| format!("graph_algo_cache invalidation failed: {e}"))
}

fn read_cache(
    db: &DbInstance,
    algo: &str,
    limit: usize,
    order_desc: bool,
) -> Result<Vec<(String, f64)>, String> {
    let mut params = BTreeMap::new();
    params.insert("algo".to_string(), DataValue::Str(algo.to_string().into()));
    params.insert("limit".to_string(), DataValue::Num(Num::Int(limit as i64)));
    let order_clause = if order_desc { "-value" } else { "value" };
    let script = format!(
        r#"?[note_id, value] := *graph_algo_cache{{ algo, note_id, value }}, algo == $algo
           :order {order_clause}
           :limit $limit"#
    );
    let result = db
        .run_script(&script, params, ScriptMutability::Immutable)
        .map_err(|e| format!("graph_algo_cache read failed: {e}"))?;
    let mut out = Vec::with_capacity(result.rows.len());
    for row in &result.rows {
        let Some(id) = row.first().and_then(|v| v.get_str()).map(String::from) else {
            continue;
        };
        let value = match row.get(1) {
            Some(DataValue::Num(Num::Float(f))) => *f,
            Some(DataValue::Num(Num::Int(i))) => *i as f64,
            _ => continue,
        };
        out.push((id, value));
    }
    Ok(out)
}

fn write_cache<'a, I>(db: &DbInstance, algo: &str, entries: I) -> Result<(), String>
where
    I: IntoIterator<Item = (&'a str, f64)>,
{
    // Clear existing rows for this algo first — semantics are "replace".
    // Use distinct local names (`a`, `n`) for the bindings so the
    // `algo == $algo` filter is unambiguous: column-name `algo` would
    // otherwise overlap with the rule binding and Cozo's filter
    // resolution wasn't narrowing the rows correctly.
    let mut params = BTreeMap::new();
    params.insert(
        "wanted_algo".to_string(),
        DataValue::Str(algo.to_string().into()),
    );
    db.run_script(
        r#"?[algo, note_id] := *graph_algo_cache{ algo: stored_algo, note_id },
                                  stored_algo == $wanted_algo,
                                  algo = stored_algo
           :rm graph_algo_cache { algo, note_id }"#,
        params,
        ScriptMutability::Mutable,
    )
    .map_err(|e| format!("graph_algo_cache clear failed for {algo}: {e}"))?;

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64();

    let rows: Vec<DataValue> = entries
        .into_iter()
        .map(|(id, value)| {
            DataValue::List(vec![
                DataValue::Str(algo.to_string().into()),
                DataValue::Str(id.to_string().into()),
                DataValue::from(value),
                DataValue::from(now),
            ])
        })
        .collect();
    if rows.is_empty() {
        return Ok(());
    }
    let mut params = BTreeMap::new();
    params.insert("rows".to_string(), DataValue::List(rows));
    db.run_script(
        r#"?[algo, note_id, value, computed_at] <- $rows
           :put graph_algo_cache { algo, note_id => value, computed_at }"#,
        params,
        ScriptMutability::Mutable,
    )
    .map_err(|e| format!("graph_algo_cache write failed: {e}"))?;
    Ok(())
}

/// Cheap pre-check: do we have any links at all? Cozo's fixed-rule
/// algorithms reject empty edge rules with an opaque error; this lets
/// us short-circuit to an empty-result Ok before we hit that path.
fn has_any_links(db: &DbInstance) -> Result<bool, String> {
    let result = db
        .run_script(
            r#"?[count(from_id)] := *links{ from_id }"#,
            Default::default(),
            ScriptMutability::Immutable,
        )
        .map_err(|e| format!("links count failed: {e}"))?;
    let n = result
        .rows
        .first()
        .and_then(|r| r.first())
        .and_then(|v| match v {
            DataValue::Num(Num::Int(i)) => Some(*i),
            _ => None,
        })
        .unwrap_or(0);
    Ok(n > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db_with_links() -> DbInstance {
        let db = DbInstance::new("mem", "", Default::default()).expect("mem db");
        db.run_script(
            r#":create links { from_id: String, to_id: String => link_text: String }"#,
            Default::default(),
            ScriptMutability::Mutable,
        )
        .expect("create links");
        db
    }

    fn db_with_links_and_cache() -> DbInstance {
        let db = db_with_links();
        db.run_script(
            r#":create graph_algo_cache {
                algo: String, note_id: String
                => value: Float, computed_at: Float
            }"#,
            Default::default(),
            ScriptMutability::Mutable,
        )
        .expect("create cache");
        db
    }

    fn add_link(db: &DbInstance, from: &str, to: &str) {
        let mut params = BTreeMap::new();
        params.insert("f".to_string(), DataValue::Str(from.to_string().into()));
        params.insert("t".to_string(), DataValue::Str(to.to_string().into()));
        params.insert("x".to_string(), DataValue::Str("link".to_string().into()));
        db.run_script(
            r#"?[from_id, to_id, link_text] <- [[$f, $t, $x]]
               :put links { from_id, to_id => link_text }"#,
            params,
            ScriptMutability::Mutable,
        )
        .expect("put link");
    }

    #[test]
    fn page_rank_returns_higher_rank_for_hub_node() {
        // Hub gets links from many notes — should outrank the leaves.
        let db = db_with_links();
        for i in 0..5 {
            add_link(&db, &format!("leaf{i}"), "hub");
        }
        let r = page_rank(&db, 10).expect("pagerank");
        assert!(!r.is_empty());
        assert_eq!(r[0].note_id, "hub", "hub must be highest-rank node");
    }

    #[test]
    fn page_rank_empty_graph_returns_empty() {
        let db = db_with_links();
        let r = page_rank(&db, 10).expect("pagerank empty");
        assert!(r.is_empty());
    }

    #[test]
    fn community_detection_groups_connected_components() {
        // Two disjoint triangles. Louvain should split them into ≥2 communities.
        let db = db_with_links();
        for (a, b) in [("a1", "a2"), ("a2", "a3"), ("a3", "a1")] {
            add_link(&db, a, b);
        }
        for (a, b) in [("b1", "b2"), ("b2", "b3"), ("b3", "b1")] {
            add_link(&db, a, b);
        }
        let r = community_detection(&db).expect("communities");
        let communities: std::collections::HashSet<_> = r.iter().map(|e| e.community).collect();
        assert!(
            communities.len() >= 2,
            "two disconnected triangles must yield 2+ communities, got {communities:?}"
        );
    }

    #[test]
    fn community_detection_empty_graph_returns_empty() {
        let db = db_with_links();
        let r = community_detection(&db).expect("communities empty");
        assert!(r.is_empty());
    }

    #[test]
    fn shortest_path_finds_two_hop_route() {
        let db = db_with_links();
        // a → b → c
        add_link(&db, "a", "b");
        add_link(&db, "b", "c");
        let p = shortest_path(&db, "a", "c").expect("path").expect("some");
        assert_eq!(p.from_id, "a");
        assert_eq!(p.to_id, "c");
        assert!(
            p.path.iter().any(|p| p == "b"),
            "path must traverse b: {:?}",
            p.path
        );
    }

    #[test]
    fn shortest_path_unreachable_returns_none() {
        let db = db_with_links();
        // Two disconnected components.
        add_link(&db, "a", "b");
        add_link(&db, "c", "d");
        let p = shortest_path(&db, "a", "d").expect("query");
        assert!(p.is_none());
    }

    #[test]
    fn betweenness_centrality_picks_bridge_node() {
        // a — b — c — d — e. b, c, d are bridges (c most so).
        let db = db_with_links();
        for (a, b) in [("a", "b"), ("b", "c"), ("c", "d"), ("d", "e")] {
            add_link(&db, a, b);
        }
        let r = betweenness_centrality(&db, 10).expect("centrality");
        assert!(!r.is_empty());
        // c is the middle of the chain — must be highest.
        assert_eq!(r[0].note_id, "c");
    }

    #[test]
    fn betweenness_centrality_empty_returns_empty() {
        let db = db_with_links();
        assert!(betweenness_centrality(&db, 10).unwrap().is_empty());
    }

    #[test]
    fn cache_layer_round_trips_pagerank() {
        let db = db_with_links_and_cache();
        for i in 0..5 {
            add_link(&db, &format!("leaf{i}"), "hub");
        }
        // Cache miss → recompute writes the result.
        let cached_before = cached_page_rank(&db, 10).unwrap();
        assert!(cached_before.is_empty(), "cache starts empty");
        let recomputed = recompute_page_rank(&db, 10).unwrap();
        assert!(!recomputed.is_empty());
        // Cache hit returns the same data without recomputing.
        let cached_after = cached_page_rank(&db, 10).unwrap();
        assert_eq!(cached_after.len(), recomputed.len());
        assert_eq!(cached_after[0].note_id, "hub");
    }

    #[test]
    fn cache_invalidation_clears_all_algos() {
        let db = db_with_links_and_cache();
        add_link(&db, "a", "b");
        let _ = recompute_page_rank(&db, 10).unwrap();
        let _ = recompute_community_detection(&db).unwrap();

        invalidate_algo_cache(&db).unwrap();

        assert!(cached_page_rank(&db, 10).unwrap().is_empty());
        assert!(cached_community_detection(&db).unwrap().is_empty());
    }

    #[test]
    fn cache_invalidation_surfaces_missing_relation() {
        let db = db_with_links();
        let err = invalidate_algo_cache(&db).expect_err("missing cache relation must fail");
        assert!(err.contains("invalidation failed"));
    }

    #[test]
    fn purge_note_invalidates_algo_cache() {
        // Regression for the audit's C2: deleting / renaming a note left its
        // cached PageRank + community rows behind, so a deleted hub kept
        // surfacing in `cached_page_rank` (UI hub list + agent). purge_note
        // now wipes the algo cache; the next read recomputes from the live
        // graph.
        let db = db_with_links_and_cache();
        add_link(&db, "a", "hub");
        add_link(&db, "b", "hub");
        let _ = recompute_page_rank(&db, 10).unwrap();
        assert!(
            !cached_page_rank(&db, 10).unwrap().is_empty(),
            "test setup: cache must be seeded"
        );
        // Ignore the Result: this fixture has no `notes`/`tags`/`task`
        // relations, so those :rm steps error harmlessly; the cache
        // invalidation runs unconditionally after the relation sweep.
        let _ = crate::db::queries::purge_note(&db, "hub");
        assert!(
            cached_page_rank(&db, 10).unwrap().is_empty(),
            "purge_note must invalidate the algo cache"
        );
    }

    #[test]
    fn recompute_overwrites_stale_cache_for_one_algo_only() {
        // Louvain returns nothing for trivial graphs (2 nodes, 1 edge),
        // so seed two triangles to guarantee non-empty community output.
        let db = db_with_links_and_cache();
        for (a, b) in [
            ("a1", "a2"),
            ("a2", "a3"),
            ("a3", "a1"),
            ("b1", "b2"),
            ("b2", "b3"),
            ("b3", "b1"),
        ] {
            add_link(&db, a, b);
        }

        let _ = recompute_page_rank(&db, 100).unwrap();
        let comm_before = recompute_community_detection(&db).unwrap();
        assert!(
            !comm_before.is_empty(),
            "test setup: community must be non-empty"
        );

        // Recomputing pagerank must NOT clear community results.
        add_link(&db, "a3", "b1"); // bridge — changes pagerank, not test invariant
        let _ = recompute_page_rank(&db, 100).unwrap();

        let comm_after = cached_community_detection(&db).unwrap();
        assert!(
            !comm_after.is_empty(),
            "per-algo recompute must not stomp other algos' caches"
        );
    }

    #[test]
    fn cache_value_round_trips_community_label_int() {
        let db = db_with_links_and_cache();
        add_link(&db, "a", "b");
        let recomputed = recompute_community_detection(&db).unwrap();
        let cached = cached_community_detection(&db).unwrap();
        // Same number of rows; community labels survive the f64 round-trip.
        assert_eq!(cached.len(), recomputed.len());
        let recomp_set: std::collections::HashSet<_> = recomputed
            .iter()
            .map(|e| (e.note_id.clone(), e.community))
            .collect();
        let cache_set: std::collections::HashSet<_> = cached
            .iter()
            .map(|e| (e.note_id.clone(), e.community))
            .collect();
        assert_eq!(recomp_set, cache_set);
    }
}
