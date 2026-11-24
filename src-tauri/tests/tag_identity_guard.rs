//! Source-level guard for the tag-identity rule (LR-6 / LR-7).
//!
//! The rule — Unicode tag characters, identity = Unicode lowercase of the NFC
//! form, stored rows verbatim, folding at read time — lives in ONE place per
//! language (`src/parser/tags.rs`, `corvux-mcp/src/frontmatter.rs`,
//! `src/lib/tags.ts`). The shared fixture
//! (`corvux-mcp/tests/fixtures/tag_identity.json`) proves the three agree.
//! These guards freeze the CLASS mechanically, so a future second scanner or a
//! new reader of the `tags` relation cannot quietly reintroduce the three
//! different answers for one note:
//!
//!   tg1 — nothing re-implements the old ASCII-only tag character test;
//!   tg2 — only the two modules that fold may read the `tags` relation, and
//!         the graph builds tag node ids in exactly one place;
//!   tg3 — the agent tag-guards compare by identity, never by exact string.
//!
//! Text-only (no crate linking), so it is fast and runs on every platform.

use std::fs;
use std::path::{Path, PathBuf};

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn collect_files(dir: &Path, exts: &[&str], out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name == "node_modules" || name == "target" || name == "dist" {
                continue;
            }
            collect_files(&path, exts, out);
        } else if path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| exts.contains(&e))
        {
            out.push(path);
        }
    }
}

fn rel(path: &Path, base: &Path) -> String {
    path.strip_prefix(base)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// The pre-LR-7 ASCII-only tag-character tests, assembled from pieces so this
/// file does not match itself.
fn ascii_tag_char_signatures() -> Vec<String> {
    let bytes_form = format!(
        "is_ascii_alphanumeric() || {c} == b'-' || {c} == b'_' || {c} == b'/'",
        c = "c"
    );
    let chars_form = format!(
        "is_ascii_alphanumeric() || {c} == '-' || {c} == '_' || {c} == '/'",
        c = "c"
    );
    vec![bytes_form, chars_form]
}

#[test]
fn tg1_no_second_ascii_only_tag_scanner() {
    let manifest = manifest_dir();
    let mut offenders = Vec::new();

    // Rust: the app and the standalone MCP crate.
    let mut rs = Vec::new();
    collect_files(&manifest.join("src"), &["rs"], &mut rs);
    collect_files(
        &manifest.join("..").join("corvux-mcp").join("src"),
        &["rs"],
        &mut rs,
    );
    let sigs = ascii_tag_char_signatures();
    for file in &rs {
        let text = fs::read_to_string(file).unwrap_or_default();
        if sigs.iter().any(|s| text.contains(s.as_str())) {
            offenders.push(rel(file, &manifest));
        }
    }

    // TypeScript: the ASCII-only `\w` tag regex (editor, hover card, mock).
    let mut ts = Vec::new();
    collect_files(&manifest.join("..").join("src"), &["ts", "tsx"], &mut ts);
    let ts_sigs = [
        format!("#([{}][{}", r"\w", r"\w"),
        format!("#([A-Za-z0-9_][{}", r"\w"),
    ];
    for file in &ts {
        let text = fs::read_to_string(file).unwrap_or_default();
        if ts_sigs.iter().any(|s| text.contains(s.as_str())) {
            offenders.push(rel(file, &manifest));
        }
    }

    assert!(
        offenders.is_empty(),
        "tg1: ASCII-only tag character rule re-implemented in {offenders:?}. A tag is \
         Unicode letters/numbers/marks + `_ - /` (see parser/tags.rs `is_tag_char`, \
         corvux-mcp frontmatter.rs, src/lib/tags.ts) — call the shared scanner instead."
    );
}

#[test]
fn tg2_only_folding_modules_read_the_tags_relation() {
    let manifest = manifest_dir();
    let src = manifest.join("src");
    let mut files = Vec::new();
    collect_files(&src, &["rs"], &mut files);

    // The modules that fold stored spellings onto identities at read time.
    let allowed = ["db/queries/notes.rs", "db/queries/graph.rs"];
    let mut offenders = Vec::new();
    for file in &files {
        let r = rel(file, &src);
        let text = fs::read_to_string(file).unwrap_or_default();
        let reads_tags = text.contains("*tags{") || text.contains("*tags {");
        if reads_tags && !allowed.contains(&r.as_str()) {
            offenders.push(r);
        }
    }
    assert!(
        offenders.is_empty(),
        "tg2: {offenders:?} read the `tags` relation but do not fold. Stored tag rows are \
         VERBATIM (`#ML` and `#ml` are two rows); every reader must group/compare through \
         `parser::tags::fold_tag`. Add the query to db/queries/{{notes,graph}}.rs, or fold \
         there and extend this allowlist deliberately."
    );

    // Graph tag node ids: exactly one construction site (`tag_node_id`).
    let graph = fs::read_to_string(src.join("db/queries/graph.rs")).expect("read graph.rs");
    let constructions = graph.matches("format!(\"tag:").count();
    assert_eq!(
        constructions, 1,
        "tg2: graph.rs builds `tag:` ids in {constructions} places — build them ONLY via \
         `tag_node_id(&fold_tag(..))` so one tag is one node"
    );
    assert!(
        graph.contains("tag_node_id(&fold_tag("),
        "tg2: graph.rs must fold tag ids"
    );

    // The inventory groups by identity.
    let notes = fs::read_to_string(src.join("db/queries/notes.rs")).expect("read notes.rs");
    assert!(
        notes.contains("TagIdentities::default()"),
        "tg2: top_tags must group spellings by identity (TagIdentities)"
    );
}

#[test]
fn tg3_agent_tag_guards_compare_by_identity() {
    let tools = manifest_dir().join("src").join("ai").join("tools");
    for name in ["roost_write.rs", "roost_edit.rs"] {
        let text = fs::read_to_string(tools.join(name)).expect("read guard source");
        // Production part only: drop the test module (it may build raw sets).
        let prod = text.split("#[cfg(test)]").next().unwrap_or(&text);
        assert!(
            !prod.contains("known.contains("),
            "tg3: {name} compares tags to the inventory by exact string — use \
             `parser::tags::tags_not_in` (identity), or `#ML` is rejected as invented \
             while `#ml` exists"
        );
    }
    let write = fs::read_to_string(tools.join("roost_write.rs")).expect("read roost_write.rs");
    assert!(
        write.contains("tags_not_in("),
        "tg3: roost_write must use tags_not_in"
    );
    let edit = fs::read_to_string(tools.join("roost_edit.rs")).expect("read roost_edit.rs");
    assert!(
        edit.contains("tags_not_in("),
        "tg3: roost_edit must use tags_not_in"
    );
}
