//! Integration test for the graph-indexing pipeline.
//!
//! Coverage: parser (links + tags) → `bulk_index_pass1` → `LinkResolver`
//! → `bulk_index_pass2` → query layer (`get_backlinks`, `get_note_links`,
//! `get_note_tags`). Intentionally avoids HNSW so the test runs against
//! cozo's `mem` storage and stays fast (<5ms per case).
//!
//! Each test builds its own DB so cases can run in parallel without
//! polluting each other's state.

use std::collections::BTreeMap;

use cozo::{DbInstance, ScriptMutability};

use corvux_lib::db::queries::{
    bulk_index_pass1, bulk_index_pass2, get_backlinks, get_note_links, get_note_tags, LinkResolver,
};

const VAULT: &str = "/vault";

fn mem_db_with_schema() -> DbInstance {
    let db = DbInstance::new("mem", "", Default::default()).expect("mem db");
    for script in [
        r#":create notes { id: String => title: String, path: String, updated_at: Float }"#,
        r#":create links { from_id: String, to_id: String => link_text: String }"#,
        r#":create tags { note_id: String, tag: String }"#,
        // pass1 bulk-clears tasks for the indexed notes; without this
        // relation the index call fails before any link/tag rows land.
        r#":create task {
            note_id: String,
            source_line: Int,
            =>
            text: String,
            content_anchor: String,
            due_at: String,
            completed: Bool,
            completed_at: String,
            linked_person: String,
            linked_project: String,
            updated_at: Float
        }"#,
        // pass2 bulk-clears dangling-link rows for the indexed notes; without
        // this relation the call fails before any link/tag rows land.
        r#":create dangling {
            from_id: String,
            target: String
            =>
            link_text: String,
            target_base: String
        }"#,
    ] {
        db.run_script(script, BTreeMap::new(), ScriptMutability::Mutable)
            .expect("create relation");
    }
    db
}

/// Run the full two-pass index over `files` with the same shape the live
/// `init_graph` command does: pass1, then build a resolver from the now-
/// populated `notes` relation, then pass2.
fn run_full_index(db: &DbInstance, files: &[(String, String)]) {
    bulk_index_pass1(db, VAULT, files).expect("pass1");
    let resolver = LinkResolver::build(db).expect("build resolver");
    bulk_index_pass2(db, VAULT, files, &resolver).expect("pass2");
}

fn collect_ids(db: &DbInstance) -> Vec<String> {
    let result = db
        .run_script(
            r#"?[id] := *notes{ id }"#,
            BTreeMap::new(),
            ScriptMutability::Immutable,
        )
        .expect("scan ids");
    result
        .rows
        .iter()
        .filter_map(|row| row.first()?.get_str().map(String::from))
        .collect()
}

#[test]
fn wiki_link_creates_forward_and_backlink_edges() {
    let db = mem_db_with_schema();
    let files = vec![
        (
            format!("{VAULT}/source.md"),
            "Body links to [[Target]] here.".to_string(),
        ),
        (format!("{VAULT}/target.md"), "Target body.".to_string()),
    ];
    run_full_index(&db, &files);

    // Forward link: source → target
    let forward = get_note_links(&db, "source").expect("forward links");
    assert_eq!(forward.len(), 1, "source should have exactly one outlink");
    assert_eq!(forward[0].target, "target");

    // Backlink: target ← source
    let backlinks = get_backlinks(&db, "target").expect("backlinks");
    assert_eq!(backlinks.len(), 1, "target should have one backlink");
    assert!(
        backlinks[0].path.ends_with("source.md"),
        "backlink should resolve to source.md, got {}",
        backlinks[0].path
    );
}

#[test]
fn unresolvable_wiki_link_is_silently_dropped() {
    // Links to a note that doesn't exist in the vault must NOT create a
    // ghost row; the live system relies on this so PageRank/community
    // queries don't trip over orphan ids.
    let db = mem_db_with_schema();
    let files = vec![(
        format!("{VAULT}/orphan.md"),
        "Refers to [[Nonexistent Note]].".to_string(),
    )];
    run_full_index(&db, &files);

    assert_eq!(
        get_note_links(&db, "orphan").expect("forward").len(),
        0,
        "unresolved links must not write a row"
    );
}

#[test]
fn tags_indexed_and_queryable_per_note() {
    let db = mem_db_with_schema();
    let files = vec![(
        format!("{VAULT}/note.md"),
        "Body has #foo and #bar tags, and #foo again (dedup).".to_string(),
    )];
    run_full_index(&db, &files);

    let mut tags = get_note_tags(&db, "note").expect("tags");
    tags.sort();
    assert_eq!(tags, vec!["bar".to_string(), "foo".to_string()]);
}

#[test]
fn rebuild_clears_stale_tags() {
    // bulk_index_pass1 wipes the entire tags relation before re-inserting
    // — verify a deleted tag really disappears from the second run.
    let db = mem_db_with_schema();
    let v1 = vec![(format!("{VAULT}/n.md"), "#alpha #beta".to_string())];
    run_full_index(&db, &v1);
    assert!(
        get_note_tags(&db, "n")
            .unwrap()
            .contains(&"beta".to_string()),
        "v1 must have #beta"
    );

    let v2 = vec![(format!("{VAULT}/n.md"), "#alpha".to_string())];
    run_full_index(&db, &v2);
    let tags = get_note_tags(&db, "n").unwrap();
    assert_eq!(tags, vec!["alpha".to_string()], "stale #beta must be gone");
}

#[test]
fn folder_aware_resolver_picks_nearest_sibling() {
    // Two `puli-fitness.md` files in different daily folders; a link
    // from one of them should land in the same-folder sibling, not the
    // other one. Mirrors the behavior the resolver unit tests assert,
    // but here through the full bulk-index pipeline.
    let db = mem_db_with_schema();
    let files = vec![
        (
            format!("{VAULT}/daily/2026-w10/puli-fitness.md"),
            "See [[notes]].".to_string(),
        ),
        (
            format!("{VAULT}/daily/2026-w10/notes.md"),
            "W10 notes.".to_string(),
        ),
        (
            format!("{VAULT}/daily/2026-w11/notes.md"),
            "W11 notes.".to_string(),
        ),
    ];
    run_full_index(&db, &files);

    let forward = get_note_links(&db, "daily/2026-w10/puli-fitness").expect("forward");
    assert_eq!(forward.len(), 1);
    assert_eq!(
        forward[0].target, "daily/2026-w10/notes",
        "resolver should pick the same-folder notes.md, not the W11 one"
    );
}

#[test]
fn empty_input_is_a_no_op() {
    let db = mem_db_with_schema();
    let files: Vec<(String, String)> = Vec::new();
    run_full_index(&db, &files);
    assert_eq!(collect_ids(&db).len(), 0);
}
