//! Synthetic vault generator for scale testing.
//!
//! Usage:
//!   cargo run --release --bin gen_vault -- --count 10000 --out ./scale-test-vault [--seed 42]
//!
//! Generates a vault of N markdown notes with realistic structure:
//!   - Titles drawn from a curated topic pool
//!   - 2–5 paragraphs of synthetic prose with embedded `[[wikilinks]]`
//!   - 1–3 inline `#tags` from a hierarchical taxonomy
//!   - YAML frontmatter (type, domain, project, status, created)
//!   - Folder tree: 3–40 folders depending on count, ~100 notes/folder
//!
//! Reproducible: same seed → identical vault. Open the generated folder
//! as a vault in the app to find perf cliffs at 1K / 10K / 50K scale.

use rand::rngs::StdRng;
use rand::seq::SliceRandom;
use rand::{Rng, SeedableRng};
use std::env;
use std::fs;
use std::path::PathBuf;

const TOPICS: &[&str] = &[
    "ai",
    "ml",
    "graph",
    "rust",
    "tauri",
    "solidjs",
    "ux",
    "perf",
    "security",
    "sync",
    "search",
    "embed",
    "llm",
    "api",
    "vault",
    "note",
    "link",
    "tag",
    "concept",
    "idea",
    "plan",
    "review",
    "research",
    "design",
    "spec",
    "strategy",
    "roadmap",
    "experiment",
    "data",
    "db",
];

const TAGS: &[&str] = &[
    "domain/work",
    "domain/personal",
    "domain/research",
    "type/note",
    "type/spec",
    "type/review",
    "type/idea",
    "type/plan",
    "project/corvux",
    "project/learning",
    "status/active",
    "status/parked",
    "status/done",
    "topic/ai",
    "topic/db",
    "topic/ux",
];

const SAMPLE_WORDS: &[&str] = &[
    "the",
    "graph",
    "node",
    "edge",
    "link",
    "tag",
    "note",
    "vault",
    "concept",
    "idea",
    "search",
    "embedding",
    "vector",
    "query",
    "index",
    "data",
    "model",
    "system",
    "design",
    "feature",
    "user",
    "context",
    "memory",
    "store",
    "task",
    "build",
    "ship",
    "test",
    "review",
    "spec",
    "and",
    "of",
    "in",
    "for",
    "with",
    "to",
    "from",
    "this",
    "that",
    "into",
    "rust",
    "tokio",
    "async",
    "await",
    "future",
    "trait",
    "struct",
    "enum",
    "click",
    "tap",
    "open",
    "close",
    "save",
    "load",
    "render",
    "paint",
];

struct Args {
    count: usize,
    out_dir: PathBuf,
    seed: u64,
}

fn parse_args() -> Args {
    let mut count: Option<usize> = None;
    let mut out_dir: Option<PathBuf> = None;
    let mut seed: u64 = 42;
    let mut iter = env::args().skip(1);
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--count" => count = iter.next().and_then(|v| v.parse().ok()),
            "--out" => out_dir = iter.next().map(PathBuf::from),
            "--seed" => seed = iter.next().and_then(|v| v.parse().ok()).unwrap_or(42),
            "--help" | "-h" => {
                eprintln!("Usage: gen_vault --count N --out PATH [--seed N]");
                std::process::exit(0);
            }
            _ => {
                eprintln!("Unknown arg: {arg}");
                std::process::exit(2);
            }
        }
    }
    let count = count.unwrap_or_else(|| {
        eprintln!("--count is required");
        std::process::exit(2);
    });
    let out_dir = out_dir.unwrap_or_else(|| {
        eprintln!("--out is required");
        std::process::exit(2);
    });
    Args {
        count,
        out_dir,
        seed,
    }
}

fn main() {
    let args = parse_args();
    let mut rng = StdRng::seed_from_u64(args.seed);
    fs::create_dir_all(&args.out_dir).expect("create out dir");

    let n_folders = (args.count / 200).clamp(3, 40);
    let folders: Vec<String> = (0..n_folders).map(|i| format!("topic-{i:02}")).collect();
    for f in &folders {
        fs::create_dir_all(args.out_dir.join(f)).expect("mkdir folder");
    }

    // Pre-generate titles so wikilinks can target the full set.
    let titles: Vec<String> = (0..args.count)
        .map(|i| {
            let topic = TOPICS.choose(&mut rng).copied().unwrap_or("note");
            format!("{topic}-{i:05}")
        })
        .collect();

    println!(
        "Generating {} notes across {} folders into {}…",
        args.count,
        folders.len(),
        args.out_dir.display()
    );
    let start = std::time::Instant::now();
    for i in 0..args.count {
        let folder = &folders[(i / 100) % folders.len()];
        let path = args.out_dir.join(folder).join(format!("{}.md", titles[i]));
        let body = render_note(&mut rng, i, &titles);
        fs::write(&path, body).expect("write note");
        if i > 0 && i % 1000 == 0 {
            eprintln!("  ... {i} / {}", args.count);
        }
    }
    let elapsed = start.elapsed();

    println!("\nDone:");
    println!("  Notes:    {}", args.count);
    println!("  Folders:  {}", folders.len());
    println!("  Output:   {}", args.out_dir.display());
    println!("  Elapsed:  {:.2}s", elapsed.as_secs_f64());
    println!(
        "  Rate:     {:.0} notes/sec",
        args.count as f64 / elapsed.as_secs_f64()
    );
    println!("\nOpen this folder as a vault in the app to test scale behavior.");
}

fn render_note(rng: &mut StdRng, idx: usize, all_titles: &[String]) -> String {
    let mut s = String::with_capacity(2048);

    // Frontmatter
    let fm_type = ["note", "spec", "review", "idea", "plan"]
        .choose(rng)
        .copied()
        .unwrap_or("note");
    let fm_domain = ["work", "personal", "research"]
        .choose(rng)
        .copied()
        .unwrap_or("work");
    let fm_status = ["active", "parked", "done"]
        .choose(rng)
        .copied()
        .unwrap_or("active");
    s.push_str("---\n");
    s.push_str(&format!("type: {fm_type}\n"));
    s.push_str(&format!("domain: {fm_domain}\n"));
    s.push_str("project: corvux\n");
    s.push_str(&format!("status: {fm_status}\n"));
    s.push_str(&format!("created: 2026-04-{:02}\n", (idx % 28) + 1));
    s.push_str("---\n\n");

    s.push_str(&format!("# {}\n\n", all_titles[idx]));

    // Inline tags
    let n_tags = rng.gen_range(1..=3);
    for _ in 0..n_tags {
        let t = TAGS.choose(rng).copied().unwrap_or("topic/general");
        s.push_str(&format!("#{t} "));
    }
    s.push_str("\n\n");

    // 2–5 paragraphs of prose with sprinkled wikilinks. Bias toward
    // nearby indices so the resulting graph has small-world structure
    // rather than uniform random connectivity (matches real vaults).
    let n_para = rng.gen_range(2..=5);
    for _ in 0..n_para {
        let n_words = rng.gen_range(20..=80);
        let mut para = String::new();
        for w in 0..n_words {
            if w > 0 && rng.gen_range(0..15) == 0 {
                let target_idx = if rng.gen_bool(0.7) {
                    let lo = idx.saturating_sub(20);
                    let hi = (idx + 20).min(all_titles.len().saturating_sub(1));
                    if hi > lo {
                        rng.gen_range(lo..=hi)
                    } else {
                        idx
                    }
                } else {
                    rng.gen_range(0..all_titles.len())
                };
                if target_idx != idx {
                    para.push_str(&format!("[[{}]] ", all_titles[target_idx]));
                    continue;
                }
            }
            let word = SAMPLE_WORDS.choose(rng).copied().unwrap_or("note");
            para.push_str(word);
            para.push(' ');
        }
        s.push_str(para.trim_end());
        s.push_str("\n\n");
    }

    s
}
