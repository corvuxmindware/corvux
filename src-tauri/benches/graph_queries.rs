//! Microbenchmarks for hot graph queries.
//!
//! Run with `cargo bench --bench graph_queries`. Results land under
//! `target/criterion/`. The fixtures here are deliberately small (100
//! and 1000 notes) — the goal is to detect regressions per-commit, not
//! to model real-vault scale (which lives in the synthetic vault tooling
//! per the readiness audit).
//!
//! The bench builds an in-memory CozoDB ("mem" storage) so we don't pay
//! SQLite open cost in every iteration. Same backend the unit tests use.

use std::collections::BTreeMap;

use cozo::{DataValue, DbInstance, ScriptMutability};
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};

use corvux_lib::db::queries;

/// Spin up an empty in-memory DB with the three relations the graph
/// queries touch. Mirrors the setup used by the `graph::window_tests`
/// module in the main crate.
fn mem_db() -> DbInstance {
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

fn sqlite_db() -> (tempfile::TempDir, DbInstance) {
    let dir = tempfile::TempDir::new().expect("temp graph dir");
    let db = corvux_lib::db::schema::init_db(dir.path()).expect("SQLite-backed graph DB");
    (dir, db)
}

/// Populate a synthetic graph: `n` notes, each linked to its 3 nearest
/// neighbors (mod n), and tagged with one of 5 rotating tags. Average
/// out-degree = 3, density tracks a real Obsidian-style vault more
/// closely than uniform random.
fn populate(db: &DbInstance, n: usize) {
    let note_rows: Vec<DataValue> = (0..n)
        .map(|i| {
            let id = format!("note-{i:05}");
            DataValue::List(vec![
                DataValue::Str(id.into()),
                DataValue::Str(format!("Note {i}").into()),
                DataValue::Str(format!("/vault/note-{i:05}.md").into()),
                DataValue::from(0.0_f64),
            ])
        })
        .collect();
    let mut params = BTreeMap::new();
    params.insert("rows".to_string(), DataValue::List(note_rows));
    db.run_script(
        r#"?[id, title, path, updated_at] <- $rows
           :put notes { id => title, path, updated_at }"#,
        params,
        ScriptMutability::Mutable,
    )
    .expect("put notes");

    let link_rows: Vec<DataValue> = (0..n)
        .flat_map(|i| {
            (1..=3).map(move |offset| {
                let from = format!("note-{i:05}");
                let to = format!("note-{:05}", (i + offset) % n);
                DataValue::List(vec![
                    DataValue::Str(from.into()),
                    DataValue::Str(to.clone().into()),
                    DataValue::Str(to.into()),
                ])
            })
        })
        .collect();
    let mut params = BTreeMap::new();
    params.insert("rows".to_string(), DataValue::List(link_rows));
    db.run_script(
        r#"?[from_id, to_id, link_text] <- $rows
           :put links { from_id, to_id => link_text }"#,
        params,
        ScriptMutability::Mutable,
    )
    .expect("put links");

    let tag_rows: Vec<DataValue> = (0..n)
        .map(|i| {
            DataValue::List(vec![
                DataValue::Str(format!("note-{i:05}").into()),
                DataValue::Str(format!("tag-{}", i % 5).into()),
            ])
        })
        .collect();
    let mut params = BTreeMap::new();
    params.insert("rows".to_string(), DataValue::List(tag_rows));
    db.run_script(
        r#"?[note_id, tag] <- $rows
           :put tags { note_id, tag }"#,
        params,
        ScriptMutability::Mutable,
    )
    .expect("put tags");
}

fn bench_get_backlinks(c: &mut Criterion) {
    let mut group = c.benchmark_group("get_backlinks");
    for size in [100usize, 1_000, 10_000].iter() {
        let db = mem_db();
        populate(&db, *size);
        // Pick a deterministic mid-graph note as the query target so
        // every iteration hits the same workload.
        let target = format!("note-{:05}", size / 2);
        group.bench_with_input(BenchmarkId::from_parameter(size), size, |b, _| {
            b.iter(|| queries::get_backlinks(&db, &target).expect("backlinks"));
        });
    }
    group.finish();
}

fn bench_get_graph_window(c: &mut Criterion) {
    let mut group = c.benchmark_group("get_graph_window");
    for size in [100usize, 1_000, 10_000].iter() {
        let db = mem_db();
        populate(&db, *size);
        let focus = format!("note-{:05}", size / 2);
        group.bench_with_input(BenchmarkId::from_parameter(size), size, |b, _| {
            b.iter(|| queries::get_graph_window(&db, &focus, 2, 200).expect("window"));
        });
    }
    group.finish();
}

fn bench_full_graph(c: &mut Criterion) {
    let mut group = c.benchmark_group("get_full_graph");
    for size in [1_000usize, 10_000].iter() {
        let db = mem_db();
        populate(&db, *size);
        group.bench_with_input(BenchmarkId::from_parameter(size), size, |b, _| {
            b.iter(|| queries::get_full_graph(&db, 1_000).expect("full graph"));
        });
    }
    group.finish();
}

fn bench_algorithms(c: &mut Criterion) {
    let mut group = c.benchmark_group("graph_algorithms");
    group.sample_size(20);
    for size in [1_000usize, 10_000].iter() {
        let db = mem_db();
        populate(&db, *size);
        group.bench_with_input(BenchmarkId::new("page_rank", size), size, |b, _| {
            b.iter(|| queries::page_rank(&db, 1_000).expect("PageRank"));
        });
        group.bench_with_input(BenchmarkId::new("communities", size), size, |b, _| {
            b.iter(|| queries::community_detection(&db).expect("communities"));
        });
    }
    group.finish();
}

fn bench_sqlite_10k(c: &mut Criterion) {
    let mut group = c.benchmark_group("sqlite_10k");
    group.sample_size(20);
    let (_dir, db) = sqlite_db();
    populate(&db, 10_000);
    // Production callers read the persisted algorithm cache first and only
    // recompute after indexing invalidates it. Benchmark both paths so the
    // user-visible hot path is not hidden behind the noisier full algorithm.
    queries::recompute_page_rank(&db, 1_000).expect("seed PageRank cache");
    group.bench_function("window", |b| {
        b.iter(|| queries::get_graph_window(&db, "note-05000", 2, 200).expect("window"));
    });
    group.bench_function("full_graph", |b| {
        b.iter(|| queries::get_full_graph(&db, 1_000).expect("full graph"));
    });
    group.bench_function("page_rank", |b| {
        b.iter(|| queries::page_rank(&db, 1_000).expect("PageRank"));
    });
    group.bench_function("cached_page_rank", |b| {
        b.iter(|| queries::cached_page_rank(&db, 1_000).expect("cached PageRank"));
    });
    group.finish();
}

fn bench_sqlite_50k(c: &mut Criterion) {
    let mut group = c.benchmark_group("sqlite_50k");
    group.sample_size(10);
    let (_dir, db) = sqlite_db();
    populate(&db, 50_000);
    queries::recompute_page_rank(&db, 1_000).expect("seed PageRank cache");
    group.bench_function("window", |b| {
        b.iter(|| queries::get_graph_window(&db, "note-25000", 2, 200).expect("window"));
    });
    group.bench_function("full_graph", |b| {
        b.iter(|| queries::get_full_graph(&db, 1_000).expect("full graph"));
    });
    group.bench_function("page_rank", |b| {
        b.iter(|| queries::page_rank(&db, 1_000).expect("PageRank"));
    });
    group.bench_function("cached_page_rank", |b| {
        b.iter(|| queries::cached_page_rank(&db, 1_000).expect("cached PageRank"));
    });
    group.finish();
}

criterion_group!(
    benches,
    bench_get_backlinks,
    bench_get_graph_window,
    bench_full_graph,
    bench_algorithms,
    bench_sqlite_10k,
    bench_sqlite_50k
);
criterion_main!(benches);
