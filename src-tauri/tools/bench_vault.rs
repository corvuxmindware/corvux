//! End-to-end vault bench.
//!
//! Usage:
//!   cargo run --release --bin bench_vault -- --vault ./scale-test-vault
//!
//! Walks an existing vault on disk and times the operations that drive
//! cold-start cost: directory scan, file reads, link/tag parsing, and an
//! in-memory CozoDB build. Prints a markdown timing table.
//!
//! The numbers are pure-Rust local timings — no Tauri IPC, no WebView,
//! no SQLite. They represent the floor for what the cold-path could be
//! if those layers were free, useful for "is the bottleneck disk or
//! code?" answers.

use cozo::{DataValue, DbInstance, ScriptMutability};
use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use corvux_lib::parser::{links::parse_links, tags::parse_tags};

struct Args {
    vault: PathBuf,
}

fn parse_args() -> Args {
    let mut vault: Option<PathBuf> = None;
    let mut iter = env::args().skip(1);
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--vault" => vault = iter.next().map(PathBuf::from),
            "--help" | "-h" => {
                eprintln!("Usage: bench_vault --vault PATH");
                std::process::exit(0);
            }
            _ => {
                eprintln!("Unknown arg: {arg}");
                std::process::exit(2);
            }
        }
    }
    let vault = vault.unwrap_or_else(|| {
        eprintln!("--vault is required");
        std::process::exit(2);
    });
    Args { vault }
}

fn main() {
    let args = parse_args();
    if !args.vault.is_dir() {
        eprintln!("Not a directory: {}", args.vault.display());
        std::process::exit(1);
    }

    println!("Benching vault: {}\n", args.vault.display());
    let mut rows: Vec<(String, f64, String)> = Vec::new();

    // 1. Directory walk: list every .md file path. Mirrors what
    //    `commands::vault::scan_directory` does.
    let t = Instant::now();
    let paths = collect_markdown(&args.vault);
    let scan_ms = t.elapsed().as_secs_f64() * 1000.0;
    rows.push((
        "Directory walk".into(),
        scan_ms,
        format!("{} files", paths.len()),
    ));

    if paths.is_empty() {
        eprintln!("No markdown files found.");
        return;
    }

    // 2. Read all files into memory.
    let t = Instant::now();
    let contents: Vec<(PathBuf, String)> = paths
        .iter()
        .filter_map(|p| fs::read_to_string(p).ok().map(|c| (p.clone(), c)))
        .collect();
    let read_ms = t.elapsed().as_secs_f64() * 1000.0;
    let total_bytes: usize = contents.iter().map(|(_, c)| c.len()).sum();
    rows.push((
        "Read all files".into(),
        read_ms,
        format!("{:.1} MB", total_bytes as f64 / 1024.0 / 1024.0),
    ));

    // 3. Parse links + tags from every file.
    let t = Instant::now();
    let mut total_links = 0usize;
    let mut total_tags = 0usize;
    for (_, content) in &contents {
        total_links += parse_links(content.clone()).len();
        total_tags += parse_tags(content.clone()).len();
    }
    let parse_ms = t.elapsed().as_secs_f64() * 1000.0;
    rows.push((
        "Parse links + tags".into(),
        parse_ms,
        format!("{total_links} links, {total_tags} tags"),
    ));

    // 4. Build an in-memory CozoDB and insert every note + link + tag.
    //    Approximates the index-pass cost without paying SQLite open/fsync.
    let t = Instant::now();
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

    for (i, (path, _content)) in contents.iter().enumerate() {
        let id = format!("n-{i:06}");
        let title = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("note")
            .to_string();
        let mut params = BTreeMap::new();
        params.insert("id".into(), DataValue::Str(id.into()));
        params.insert("title".into(), DataValue::Str(title.into()));
        params.insert(
            "path".into(),
            DataValue::Str(path.to_string_lossy().to_string().into()),
        );
        params.insert("updated_at".into(), DataValue::from(0.0));
        db.run_script(
            "?[id, title, path, updated_at] <- [[$id, $title, $path, $updated_at]] :put notes { id => title, path, updated_at }",
            params,
            ScriptMutability::Mutable,
        )
        .expect("put note");
    }
    let index_ms = t.elapsed().as_secs_f64() * 1000.0;
    rows.push((
        "In-memory note index".into(),
        index_ms,
        format!("{} rows", contents.len()),
    ));

    // 5. Sample query: count rows. Trivial but exercises the query path.
    let t = Instant::now();
    let _ = db
        .run_script(
            "?[count(id)] := *notes{ id }",
            Default::default(),
            ScriptMutability::Immutable,
        )
        .expect("count");
    let query_ms = t.elapsed().as_secs_f64() * 1000.0;
    rows.push(("Count query".into(), query_ms, "count(id)".into()));

    // Print results
    println!("| Step | Time | Notes |");
    println!("|------|-----:|-------|");
    let mut total = 0.0;
    for (name, ms, note) in &rows {
        println!("| {name} | {ms:.1} ms | {note} |");
        total += ms;
    }
    println!("| **Total** | **{total:.1} ms** | |");
    println!(
        "\nThroughput: {:.0} notes/sec end-to-end",
        contents.len() as f64 / (total / 1000.0)
    );
}

fn collect_markdown(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = fs::read_dir(&dir) else { continue };
        for entry in rd.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') || name == ".corvux-trash" {
                continue;
            }
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "md") {
                out.push(path);
            }
        }
    }
    out
}
