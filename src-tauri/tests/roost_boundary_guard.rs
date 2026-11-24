//! Source-level ratchet guards for the roost/IPC trust boundary — the
//! mechanical half of the 2026-11-11 sharp-edges audit
//! (`tob-security-audit-2026-11-11.md`, addendum F1–F12).
//!
//! Why these exist: that audit found the boundary itself sound ("the safe path
//! is mandatory inside `roostfs`") but everything AROUND it a convention —
//! `to_rel` is a string helper, `NativeFs::new` skips the lock, the truncate
//! helper wasn't the easy shape so six sites hand-rolled it, and the only raw-fs
//! lint covered one directory. Reviews catch what a reviewer remembers; these
//! catch what nobody does. They follow the `sync_invariants.rs` template:
//! per-file baselines, a count ABOVE baseline fails (you added a site), a count
//! BELOW baseline also fails (you removed one — lower the baseline so it cannot
//! creep back). Production code only: test-only `#[cfg(..)]` items
//! (`#[cfg(test)]`, `#[cfg(all(test, ..))]`; NOT `not(test)` or `any(test, ..)`,
//! which ship) and `//` comments are skipped (brace-depth tracked), so test
//! scaffolding is free to use raw `std::fs` on a `TempDir`.
//!
//! "Does this function call X?" checks (`WIRING`, `ORDERING`, F5) read a
//! code-only view of the source (`code_only`): a comment or a string that
//! mentions `X(` cannot satisfy them, a call whose result is thrown away
//! (`let _ = gate(..)`) does not count, and `ORDERING` pins that the gate comes
//! before the effect it guards (RF-9).
//!
//! Guards:
//! * **F6** `raw_fs_calls_in_commands_db_ai_are_pinned` — raw `std::fs` /
//!   `tokio::fs` across `src/commands`, `src/db`, `src/ai` (the old lint covered
//!   `src/ai/tools/*.rs` writes and `src/commands` reads only).
//!   `raw_fs_calls_in_app_root_and_sync_store_are_pinned` extends it to
//!   `src/*.rs` and `src/sync/store/*.rs` (RF-9).
//! * **F8** `native_fs_new_has_exactly_the_sanctioned_callers`.
//! * **F9** `no_hand_rolled_utf8_boundary_walks`.
//! * **F5** `installing_a_backend_wraps_it_in_validated_fs`.
//! * **F1/F2/F3/F10/F11** `renderer_trust_choke_points_stay_wired` — the
//!   commands that decide security-bearing state must keep calling the shared
//!   choke points (native confirmation, redaction, validation); deleting the
//!   call is the regression, and unit tests on the choke point alone cannot see
//!   it.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

fn src_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src")
}

fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rs_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// Path relative to `src/`, forward slashes.
fn rel_key(path: &Path) -> String {
    path.strip_prefix(src_dir())
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// Three-valued result of evaluating a `cfg` predicate in a NON-test build,
/// where every atom other than `test` (a target OS, a feature, `unix`) is
/// unknown because the guard does not know which build ships.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Cfg {
    False,
    True,
    Unknown,
}

fn cfg_not(v: Cfg) -> Cfg {
    match v {
        Cfg::False => Cfg::True,
        Cfg::True => Cfg::False,
        Cfg::Unknown => Cfg::Unknown,
    }
}

/// Evaluate the `cfg(...)` predicate starting at `s` (e.g. `all(test, unix)`),
/// returning the value and the rest of the text after the predicate.
fn eval_cfg(s: &str) -> (Cfg, &str) {
    let s = s.trim_start();
    let end = s
        .find(|c: char| !(c.is_alphanumeric() || c == '_'))
        .unwrap_or(s.len());
    let (ident, rest) = s.split_at(end);
    let rest_trim = rest.trim_start();
    if matches!(ident, "all" | "any" | "not") && rest_trim.starts_with('(') {
        let mut rest = &rest_trim[1..];
        let mut values = Vec::new();
        loop {
            rest = rest.trim_start();
            if rest.is_empty() || rest.starts_with(')') {
                rest = rest.strip_prefix(')').unwrap_or(rest);
                break;
            }
            let (v, after) = eval_cfg(rest);
            values.push(v);
            if after.len() == rest.len() {
                break; // no progress on unexpected input: never loop forever
            }
            rest = after.trim_start();
            rest = rest.strip_prefix(',').unwrap_or(rest);
        }
        let v = match ident {
            "all" => {
                if values.contains(&Cfg::False) {
                    Cfg::False
                } else if values.iter().all(|v| *v == Cfg::True) {
                    Cfg::True
                } else {
                    Cfg::Unknown
                }
            }
            "any" => {
                if values.contains(&Cfg::True) {
                    Cfg::True
                } else if values.iter().all(|v| *v == Cfg::False) {
                    Cfg::False
                } else {
                    Cfg::Unknown
                }
            }
            _ => cfg_not(values.first().copied().unwrap_or(Cfg::Unknown)),
        };
        return (v, rest);
    }
    // An atom, possibly `key = "value"`: skip the value if present.
    let mut rest = rest_trim;
    if let Some(after_eq) = rest.strip_prefix('=') {
        let after_eq = after_eq.trim_start();
        rest = match after_eq.strip_prefix('"') {
            Some(q) => q.find('"').map_or("", |i| &q[i + 1..]),
            None => after_eq,
        };
    }
    let v = if ident == "test" && rest_trim.len() == rest.len() {
        Cfg::False // `test` is off in a production build
    } else {
        Cfg::Unknown
    };
    (v, rest)
}

/// Is this line a `#[cfg(...)]` attribute whose item can never be compiled
/// into a production build? `#[cfg(test)]` and `#[cfg(all(test, ..))]` are
/// test-only. `#[cfg(not(test))]` and `#[cfg(any(test, target_os = "ios"))]`
/// are NOT: they ship (always, or on iOS), so a raw-fs call inside them is a
/// production call (RF-9: the old test was "starts with `#[cfg(` and contains
/// `test`", which hid both).
fn is_test_only_cfg(trimmed_line: &str) -> bool {
    let Some(inner) = trimmed_line.strip_prefix("#[cfg(") else {
        return false;
    };
    eval_cfg(inner).0 == Cfg::False
}

/// Count PRODUCTION lines containing any of `needles`: skips every line inside
/// a TEST-ONLY `#[cfg(..)]` item (`#[cfg(test)]`, `#[cfg(all(test, ..))]`; see
/// [`is_test_only_cfg`]) — brace-depth tracked, so a gated `fn` mid-`impl` and
/// a gated `mod` are both excluded — and strips `//` comments first. Same
/// semantics as `sync_invariants.rs` except for that cfg rule; kept as a copy
/// because integration tests cannot share a module without a `common/` crate,
/// and that guard is not worth refactoring for it.
///
/// Caveats (benign — drift shows up as a count that no longer matches the
/// audited baseline, a loud failure rather than a silent pass): braces inside
/// string literals and block comments are not handled, and `//` inside a string
/// literal is treated as a comment start (never put such a string before a `{`
/// on one line).
fn count_production_lines(path: &Path, needles: &[&str]) -> usize {
    let Ok(text) = fs::read_to_string(path) else {
        return 0;
    };
    count_lines_in_text(&text, needles)
}

/// [`count_production_lines`] on source text (so the counter itself is testable).
fn count_lines_in_text(text: &str, needles: &[&str]) -> usize {
    let mut count = 0usize;
    let mut depth = 0i64;
    let mut skip_below = -1i64;
    let mut pending_test = false;

    for raw in text.lines() {
        let code = match raw.find("//") {
            Some(i) => &raw[..i],
            None => raw,
        };
        let opens = code.matches('{').count() as i64;
        let closes = code.matches('}').count() as i64;
        let trimmed = code.trim_start();

        if skip_below >= 0 {
            depth += opens - closes;
            if depth < skip_below {
                skip_below = -1;
            }
            continue;
        }

        let is_test_cfg = is_test_only_cfg(trimmed);
        if is_test_cfg {
            if opens > 0 {
                let before = depth;
                depth += opens - closes;
                if depth > before {
                    skip_below = before + 1;
                }
            } else if !trimmed.contains(';') {
                pending_test = true;
            }
            depth += if opens > 0 { 0 } else { opens - closes };
            continue;
        }

        if pending_test {
            let before = depth;
            depth += opens - closes;
            if opens > 0 {
                skip_below = before + 1;
                pending_test = false;
            } else if trimmed.contains(';') {
                pending_test = false;
            }
            continue;
        }

        depth += opens - closes;
        if !trimmed.is_empty() && needles.iter().any(|n| code.contains(n)) {
            count += 1;
        }
    }
    count
}

/// Compare actual per-file counts against `baseline` (files absent from the
/// baseline have an implicit baseline of 0). Returns human-readable problems.
fn ratchet_problems(
    what: &str,
    actual: &BTreeMap<String, usize>,
    baseline: &[(&str, usize, &str)],
) -> Vec<String> {
    let mut problems = Vec::new();
    let base: BTreeMap<&str, usize> = baseline.iter().map(|(f, n, _)| (*f, *n)).collect();

    for (file, &n) in actual {
        let allowed = base.get(file.as_str()).copied().unwrap_or(0);
        if n > allowed {
            problems.push(format!(
                "{what}: {file} has {n} site(s), baseline is {allowed}. A NEW site was added — \
                 route it through the sanctioned path, or (if it is genuinely app-private / \
                 legitimate) raise the baseline WITH a reason."
            ));
        } else if n < allowed {
            problems.push(format!(
                "{what}: {file} has {n} site(s), baseline is {allowed}. A site was REMOVED — \
                 lower the baseline so it cannot creep back."
            ));
        }
    }
    for (file, n, _) in baseline {
        if !actual.contains_key(*file) && *n != 0 {
            problems.push(format!(
                "{what}: {file} no longer has any site (baseline {n}) — lower the baseline to 0 / \
                 remove the entry."
            ));
        }
    }
    problems
}

fn scan(trees: &[&str], needles: &[&str]) -> BTreeMap<String, usize> {
    let mut files = Vec::new();
    for tree in trees {
        collect_rs_files(&src_dir().join(tree), &mut files);
    }
    files
        .iter()
        .filter_map(|f| {
            let n = count_production_lines(f, needles);
            (n > 0).then(|| (rel_key(f), n))
        })
        .collect()
}

// ───────────────────────────── F6 ──────────────────────────────────────────

/// Raw filesystem access. A roost path must go through `RoostFs` (symlink-safe
/// containment, SAF support, per-note locks, sync notify). Matching is textual
/// on purpose: it must still fire for the next author who types `std::fs::` in
/// a place nobody thought to lint.
const RAW_FS: &[&str] = &[
    "std::fs::",
    "tokio::fs::",
    "fs::read(",
    "fs::read_to_string(",
    "fs::write(",
    "fs::read_dir(",
    "fs::metadata(",
    "fs::symlink_metadata(",
    "fs::canonicalize(",
    "fs::rename(",
    "fs::remove_file(",
    "fs::remove_dir(",
    "fs::remove_dir_all(",
    "fs::create_dir(",
    "fs::create_dir_all(",
    "fs::copy(",
    "File::open(",
    "File::create(",
    "OpenOptions::new(",
];

/// `(file under src/, production raw-fs lines, why this is legitimate)`.
/// APP-PRIVATE = app-data / settings / caches / model files — never a roost
/// path. ROOST = touches a roost or a DB-stored roost path; each carries the
/// finding that tracks removing it. The goal is for every ROOST entry to reach 0.
const RAW_FS_BASELINE: &[(&str, usize, &str)] = &[
    ("ai/models.rs", 10, "APP-PRIVATE: local GGUF model download/verify under app-data/models"),
    ("ai/skills/manifest.rs", 1, "APP-GLOBAL (AT-5): reads a SKILL.md from the user's ~/.corvux/skills folder only — not a roost; roost-local manifests are read by ai/skills/roost_scan.rs through RoostFs"),
    ("ai/skills/registry.rs", 1, "APP-GLOBAL (AT-5): collect_dir scans ~/.corvux/skills only; the roost directory goes through roost_scan (symlink-safe)"),
    ("ai/subagents/manifest.rs", 1, "APP-GLOBAL (AT-5): same shape as skills/manifest.rs, for ~/.corvux/agents"),
    ("ai/subagents/registry.rs", 1, "APP-GLOBAL (AT-5): collect_dir scans ~/.corvux/agents only; the roost directory goes through skills/roost_scan"),
    ("ai/tools/trace.rs", 4, "ROOST, dev-only: opt-in CORVUX_TRACE_AGENT=1 JSONL recorder under <roost>/.corvux/traces; never on by default"),
    ("commands/ai/consent.rs", 3, "APP-PRIVATE: ai_consent.json next to settings.json in app-data"),
    ("commands/ai/settings.rs", 4, "APP-PRIVATE: settings.json temp+atomic replace in app-data; settings.json read to learn which keys the file holds, F2/N3"),
    ("commands/graph.rs", 1, "APP-PRIVATE: rebuild_database deletes derived graph.db/search.db files in the per-roost app-data index dir"),
    ("commands/roost.rs", 18,"ROOST bootstrap + app-data: creates/backs up app-created roost dirs before any backend exists, native tree scanner with AUDIT-076 symlink filtering (MR-B4), roosts.json manifest, index-dir delete, confirmed_roost_roots.json first-open record (R2 — APP-PRIVATE)"),
    ("commands/roost_tree_cache.rs", 3, "APP-PRIVATE: derived file-tree cache in app-data"),
    ("commands/sync.rs", 6, "APP-PRIVATE: sync prefs + TLS identity in app-data (also ratcheted by sync_invariants.rs)"),
    ("commands/sync_endpoint.rs", 4, "APP-PRIVATE: confirmed_sync_peers.json first-pair record in app-data (R1) + RF-3 read-only listing of <app_data>/vaults to find the paired-peer registries (marked index dirs only)"),
    ("commands/task_notify.rs", 3, "APP-PRIVATE: missed-notification ledger in app-data"),
    ("db/queries/notes.rs", 1, "ROOST (F7, hardened): file_mtime_secs stats a DB-stored absolute path with symlink_metadata (no follow); metadata only, no content read, never returned to the webview. Real fix = RoostFs mtime + store RoostRel"),
    ("db/schema.rs", 5, "APP-PRIVATE: pre-migration snapshot/rollback of graph.db in the app-data index dir"),
    ("db/search_db.rs", 2, "APP-PRIVATE: search.db dir creation and corrupt-DB cleanup in app-data"),
];

#[test]
fn raw_fs_calls_in_commands_db_ai_are_pinned() {
    let actual = scan(&["commands", "db", "ai"], RAW_FS);
    let problems = ratchet_problems("raw fs (F6)", &actual, RAW_FS_BASELINE);
    assert!(
        problems.is_empty(),
        "\n{}\n\nActual per-file production counts (paste into RAW_FS_BASELINE with a reason \
         each):\n{}\n",
        problems.join("\n"),
        actual
            .iter()
            .map(|(f, n)| format!("    (\"{f}\", {n}, \"\"),"))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

// ─────────────────────── F6, top level (RF-9) ──────────────────────────────

/// Like [`scan`], but only the `.rs` files directly inside each directory
/// (`""` = `src/` itself). The top-level files and `sync/store` sit beside
/// trees that other guards own, so they are listed one by one.
fn scan_flat(dirs: &[&str], needles: &[&str]) -> BTreeMap<String, usize> {
    let mut files = Vec::new();
    for dir in dirs {
        let Ok(entries) = fs::read_dir(src_dir().join(dir)) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() && path.extension().is_some_and(|e| e == "rs") {
                files.push(path);
            }
        }
    }
    files
        .iter()
        .filter_map(|f| {
            let n = count_production_lines(f, needles);
            (n > 0).then(|| (rel_key(f), n))
        })
        .collect()
}

/// RF-9: the F6 scan covered `commands`, `db` and `ai` only, so `paths.rs` (the
/// per-roost dirs and GC), `lib.rs`, `diagnostics.rs` and `sync/store/*` could
/// grow raw-fs sites unseen. Same rules, same counter, separate table so the
/// original baseline above stays exactly as audited. Every entry is app-private
/// state; none may touch a roost path.
///
/// Counted with the precise test-only cfg rule, so `secret_store.rs` (items
/// gated `any(android, ios, test)`: they ship on mobile) shows up here; the old
/// "any cfg mentioning test" rule hid its 9 sites.
const RAW_FS_BASELINE_TOP: &[(&str, usize, &str)] = &[
    ("diagnostics.rs", 9, "APP-PRIVATE: session marker + crash.log tail under app-data (read, stat, rotate); never a roost path"),
    ("ios_backup.rs", 1, "APP-PRIVATE: creates app-data/secrets and app-data/sync so the iCloud-backup-exclusion flag can be set"),
    ("lib.rs", 7, "APP-PRIVATE: crash.log rotation next to the panic hook, and settings.json read at startup, both in app-data"),
    ("paths.rs", 21, "APP-PRIVATE: app-data index dirs (roost_db_dir, marker, legacy migration, orphan GC) and the legacy model/crash-log/secret migrations; a roost folder is touched only through sync/vault_id.rs (read-only lookup for GC, RF-6)"),
    ("secret_store.rs", 9, "APP-PRIVATE: mobile encrypted-secrets file + seed under app-data/secrets (atomic tmp+rename, fsync); items gated any(android, ios, test)"),
    ("secure_file.rs", 1, "APP-PRIVATE: chmod 0600 helper for app-data key files (identity.bin, tls.bin)"),
    ("sync/store/doc_store.rs", 25, "APP-PRIVATE: the per-roost sync DocStore (Automerge snapshots, WAL, actor generation) under roost_db_dir/sync; also ratcheted by sync_invariants.rs"),
    ("sync/store/outbox_store.rs", 4, "APP-PRIVATE: durable outbox file (tmp+rename) under roost_db_dir/sync"),
    ("sync/store/peer_store.rs", 6, "APP-PRIVATE: peers.bin registry (tmp+rename, secrets in the keychain) under roost_db_dir/sync"),
    ("sync/store/pending_trash_store.rs", 4, "APP-PRIVATE: pending-trash retry ledger (tmp+rename) under roost_db_dir/sync"),
];

#[test]
fn raw_fs_calls_in_app_root_and_sync_store_are_pinned() {
    let actual = scan_flat(&["", "sync/store"], RAW_FS);
    let problems = ratchet_problems("raw fs (F6, top level)", &actual, RAW_FS_BASELINE_TOP);
    assert!(
        problems.is_empty(),
        "\n{}\n\nActual per-file production counts (paste into RAW_FS_BASELINE_TOP with a reason \
         each):\n{}\n",
        problems.join("\n"),
        actual
            .iter()
            .map(|(f, n)| format!("    (\"{f}\", {n}, \"\"),"))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

// ───────────────────────────── F8 ──────────────────────────────────────────

/// `NativeFs::new` returns an UNLOCKED handle. Production code may only use it
/// for a secondary view of a roost whose lock is already held. The open roost is
/// `open_locked`. (Production sites only — tests use `new` freely.)
const NATIVE_FS_NEW_BASELINE: &[(&str, usize, &str)] = &[
    (
        "sync/engine/peer.rs",
        1,
        "on-demand fallback view in resolve_fs; the open roost's lock is held by RoostFsState",
    ),
    (
        "sync/vault_id.rs",
        1,
        "symlink-safe read/write of .corvux/vault-id.bin via the sync twins; lock held elsewhere",
    ),
    (
        "ai/skills/roost_scan.rs",
        1,
        "AT-5: on-demand secondary view so the skill/subagent watcher threads (no async context, no backend handle) read <roost>/.corvux/skills|agents symlink-safely; the lock is held by RoostFsState",
    ),
];

#[test]
fn native_fs_new_has_exactly_the_sanctioned_callers() {
    let actual = scan(&["."], &["NativeFs::new("]);
    let problems = ratchet_problems("NativeFs::new (F8)", &actual, NATIVE_FS_NEW_BASELINE);
    assert!(
        problems.is_empty(),
        "\n{}\n\nA new production `NativeFs::new` skips the roost lock — use `open_locked` for the \
         roost the app opens.\n",
        problems.join("\n")
    );
}

// ───────────────────────────── F9 ──────────────────────────────────────────

/// A hand-rolled `is_char_boundary` walk is how the STRSLICE panics happened
/// (and how six sites ended up reimplementing the same loop). Use
/// `ai::tools::{utf8_prefix, truncate_utf8, truncate_utf8_with}`.
const UTF8_WALK_BASELINE: &[(&str, usize, &str)] = &[(
    "ai/tools/truncate.rs",
    1,
    "the one implementation: utf8_prefix",
)];

#[test]
fn no_hand_rolled_utf8_boundary_walks() {
    let actual = scan(&["."], &["is_char_boundary("]);
    let problems = ratchet_problems("utf8 boundary walk (F9)", &actual, UTF8_WALK_BASELINE);
    assert!(
        problems.is_empty(),
        "\n{}\n\nUse crate::ai::tools::{{utf8_prefix, truncate_utf8, truncate_utf8_with}}.\n",
        problems.join("\n")
    );
}

// ───────────────────────────── F5 ──────────────────────────────────────────

/// Slice the body of `fn name(`: from its signature to the first line that is
/// a lone `}` at the SAME indentation as the signature line (column 0 for a free
/// fn, 4 for an `impl` method), so inner blocks do not end the slice early.
fn fn_body<'a>(text: &'a str, name: &str) -> Option<&'a str> {
    let start = text
        .find(&format!("fn {name}("))
        .or_else(|| text.find(&format!("fn {name}<")))?;
    let line_start = text[..start].rfind('\n').map_or(0, |i| i + 1);
    let indent = text[line_start..].chars().take_while(|c| *c == ' ').count();
    let rest = &text[start..];
    let closer = format!("\n{}}}\n", " ".repeat(indent));
    let end = rest.find(&closer)?;
    Some(&rest[..end])
}

fn read_src(rel: &str) -> String {
    fs::read_to_string(src_dir().join(rel))
        .unwrap_or_else(|e| panic!("read src/{rel}: {e}"))
        .replace("\r\n", "\n")
}

/// [`read_src`] as [`code_only`]: what a "does this function call X?" check may
/// look at.
fn read_code(rel: &str) -> String {
    code_only(&read_src(rel))
}

/// `src` with everything that is not code blanked out: `//` and nested `/* */`
/// comments, and the CONTENT of string and char literals (the delimiters stay,
/// so `"abc"` becomes `"   "`). Newlines and columns are preserved, so
/// [`fn_body`]'s indentation logic still works on the result.
///
/// A guard that asks "does this function call `X(`?" must ask this view. On the
/// raw text a comment, or a log message, that merely mentions `X(` satisfies it
/// and the guard stays green after the real call is deleted (RF-9).
fn code_only(src: &str) -> String {
    let c: Vec<char> = src.chars().collect();
    let n = c.len();
    let at = |k: usize| c.get(k).copied();
    let blank = |ch: char| if ch == '\n' { '\n' } else { ' ' };
    let mut out = String::with_capacity(src.len());
    let mut i = 0;
    while i < n {
        let ch = c[i];
        // Line comment (also `///` and `//!`).
        if ch == '/' && at(i + 1) == Some('/') {
            while i < n && c[i] != '\n' {
                out.push(' ');
                i += 1;
            }
            continue;
        }
        // Block comment; Rust nests them.
        if ch == '/' && at(i + 1) == Some('*') {
            let mut depth = 0i32;
            while i < n {
                if c[i] == '/' && at(i + 1) == Some('*') {
                    depth += 1;
                    out.push_str("  ");
                    i += 2;
                } else if c[i] == '*' && at(i + 1) == Some('/') {
                    depth -= 1;
                    out.push_str("  ");
                    i += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    out.push(blank(c[i]));
                    i += 1;
                }
            }
            continue;
        }
        // Raw string: r"..", r#".."#, br"..". Only at an identifier boundary.
        if (ch == 'r' || ch == 'b') && (i == 0 || !(c[i - 1].is_alphanumeric() || c[i - 1] == '_'))
        {
            let mut j = i;
            if c[j] == 'b' {
                j += 1;
            }
            if at(j) == Some('r') {
                j += 1;
                let mut hashes = 0usize;
                while at(j) == Some('#') {
                    hashes += 1;
                    j += 1;
                }
                if at(j) == Some('"') {
                    for &kept in &c[i..=j] {
                        out.push(kept);
                    }
                    i = j + 1;
                    while i < n {
                        if c[i] == '"' && (0..hashes).all(|h| at(i + 1 + h) == Some('#')) {
                            out.push('"');
                            for _ in 0..hashes {
                                out.push('#');
                            }
                            i += 1 + hashes;
                            break;
                        }
                        out.push(blank(c[i]));
                        i += 1;
                    }
                    continue;
                }
            }
        }
        // Ordinary string (also the tail of `b"..."`).
        if ch == '"' {
            out.push('"');
            i += 1;
            while i < n {
                if c[i] == '\\' {
                    out.push(' ');
                    i += 1;
                    if i < n {
                        out.push(blank(c[i]));
                        i += 1;
                    }
                } else if c[i] == '"' {
                    out.push('"');
                    i += 1;
                    break;
                } else {
                    out.push(blank(c[i]));
                    i += 1;
                }
            }
            continue;
        }
        // Char literal (`'a'`, `'"'`, `'\n'`, `'\u{41}'`) versus a lifetime or
        // loop label (`'a`, `'static`), which is code.
        if ch == '\'' {
            if at(i + 1) == Some('\\') {
                let mut j = i + 3; // past the backslash and the escaped character
                while j < n && c[j] != '\'' {
                    j += 1;
                }
                out.push('\'');
                for &inner in &c[i + 1..j.min(n)] {
                    out.push(blank(inner));
                }
                if j < n {
                    out.push('\'');
                }
                i = j + 1;
                continue;
            }
            if at(i + 2) == Some('\'') && at(i + 1) != Some('\'') {
                out.push_str("' '");
                i += 3;
                continue;
            }
        }
        out.push(ch);
        i += 1;
    }
    out
}

/// The line of `body` holding byte offset `pos`.
fn line_at(body: &str, pos: usize) -> &str {
    let start = body[..pos].rfind('\n').map_or(0, |i| i + 1);
    let end = body[pos..].find('\n').map_or(body.len(), |i| pos + i);
    &body[start..end]
}

/// A statement that evaluates a gate and throws the answer away
/// (`let _ = authorize(..);`) calls it without enforcing it.
fn discards_result(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("let _ =") || t.starts_with("let _:") || t.starts_with("_ =")
}

/// Byte offset of the first occurrence of `needle` in `body` whose result is
/// used (see [`discards_result`]).
fn first_enforced(body: &str, needle: &str) -> Option<usize> {
    body.match_indices(needle)
        .map(|(pos, _)| pos)
        .find(|pos| !discards_result(line_at(body, *pos)))
}

/// Why the code-only `body` does not satisfy "calls `needle` and uses the
/// result", or `None` if it does.
fn wiring_problem(body: &str, needle: &str) -> Option<&'static str> {
    if !body.contains(needle) {
        Some("no longer calls")
    } else if first_enforced(body, needle).is_none() {
        Some("calls it but throws the result away:")
    } else {
        None
    }
}

/// Why `gate` is not evaluated (result used) before `effect` in `body`, or
/// `None` if it is. Textual order: it catches the regression where the gated
/// action is moved above its gate, which a presence check cannot see.
fn order_problem(body: &str, gate: &str, effect: &str) -> Option<String> {
    let Some(effect_pos) = body.find(effect) else {
        return Some(format!(
            "the guarded effect `{effect}` is not in the body (stale entry?)"
        ));
    };
    match first_enforced(body, gate) {
        None => Some(format!("`{gate}` is not called with its result used")),
        Some(gate_pos) if gate_pos < effect_pos => None,
        Some(_) => Some(format!("`{effect}` now runs BEFORE `{gate}`")),
    }
}

#[test]
fn installing_a_backend_wraps_it_in_validated_fs() {
    // Code-only view: a comment that says "ValidatedFs::new(" must not count.
    let text = read_code("commands/files.rs");
    let body = fn_body(&text, "install").expect("RoostFsState::install exists");
    assert!(
        body.contains("ValidatedFs::new("),
        "RoostFsState::install must wrap every backend in roostfs::validated::ValidatedFs — it is \
         the single place the path contract (no `..`/absolute/drive/NUL) is enforced for SAF as \
         well as native (F5).\n--- install body ---\n{body}"
    );
    // And nothing else may assign the backend slot directly.
    let direct = text.matches("*guard = Some(").count();
    assert_eq!(
        direct, 1,
        "only `install` may put a backend into RoostFsState (found {direct} `*guard = Some(`)"
    );
}

// ──────────────────── F1 / F2 / F3 / F10 / F11 ─────────────────────────────

/// `(file, fn, substring its body must contain, what it protects)`.
const WIRING: &[(&str, &str, &str, &str)] = &[
    (
        "commands/ai/settings.rs",
        "get_settings",
        "redacted_for_renderer(",
        "F11: get_settings must never return live provider keys",
    ),
    (
        "commands/ai/settings.rs",
        "save_settings",
        "save_settings_core(",
        "F1/F2/F11: save_settings must go through plan → native confirm → apply",
    ),
    (
        "commands/ai/settings.rs",
        "save_settings_core",
        "plan_settings_update(",
        "F1/F2/F11: the planner decides what escalates and what a None key means",
    ),
    (
        "commands/ai/settings.rs",
        "save_settings_core",
        ".confirm(",
        "F1/F2: an escalating save must ask out-of-band",
    ),
    (
        "commands/ai/chat.rs",
        "build_chat_provider",
        "validate_provider_base_url(",
        "F2: the Ollama base URL is re-validated on every chat",
    ),
    (
        "commands/ai/ollama_models.rs",
        "list_ollama_models",
        "resolve_ollama_probe(",
        "F2/F11: probes validate the URL and scope the saved token",
    ),
    (
        "commands/ai/connection_test.rs",
        "test_ollama_connection",
        "resolve_ollama_probe(",
        "F2/F11: probes validate the URL and scope the saved token",
    ),
    (
        "commands/ai/ollama_models.rs",
        "list_ollama_models",
        "authorize_ollama_probe(",
        "AK-1: a probe to a non-saved internet host needs a native confirmation",
    ),
    (
        "commands/ai/connection_test.rs",
        "test_ollama_connection",
        "authorize_ollama_probe(",
        "AK-1: a probe to a non-saved internet host needs a native confirmation",
    ),
    (
        "commands/ai/ollama_models.rs",
        "list_ollama_models",
        "no_redirect_client(",
        "AK-1: a user-supplied address is fetched without following redirects",
    ),
    (
        "commands/ai/connection_test.rs",
        "test_ollama_connection",
        "no_redirect_client(",
        "AK-1: a user-supplied address is fetched without following redirects",
    ),
    (
        "ai/ollama.rs",
        "new",
        "no_redirect_client(",
        "AK-1: Ollama chat (user-chosen base URL) never follows redirects",
    ),
    (
        "commands/sync.rs",
        "sync_pin_custom_queue_host",
        "authorize_pin_change(",
        "F3: pinning a new queue host needs a native confirmation",
    ),
    (
        "commands/roost.rs",
        "write_roost_manifest",
        "authorize_manifest_write(",
        "F10: dropping a roost from the manifest (or replacing an unreadable one) needs a native confirmation",
    ),
    (
        "commands/roost.rs",
        "write_roost_manifest",
        "MANIFEST_WRITE.lock()",
        "F10/#7: read-previous → confirm → write is ONE lane, held across the dialog",
    ),
    (
        "ai/tools/approval.rs",
        "request",
        "authorize_always_approve(",
        "F1/#2: decide_tool(AlwaysApprove) pins a tool only after a native yes",
    ),
    (
        "commands/roost.rs",
        "write_roost_manifest",
        "manifest_paths_checked(",
        "F10: the manifest is validated before it can authorize GC",
    ),
    (
        "commands/roost.rs",
        "delete_roost_index",
        "authorize_removals(",
        "F10: deleting a roost's index + sync credentials needs a native confirmation",
    ),
    // RF-3 moved the R2 gate out of `open_roost` into `gate_roost_open` so the
    // whole gate (check → dialog → record) is unit-tested; the chain below pins
    // open_roost → gate → decision + dialog + lane.
    (
        "commands/roost.rs",
        "open_roost",
        "gate_roost_open(",
        "R2: opening a root the backend never confirmed must hit the native first-open gate",
    ),
    (
        "commands/roost.rs",
        "gate_roost_open",
        "roost_open_needs_confirm(",
        "R2: the gate decides from the backend-owned confirmed set",
    ),
    (
        "commands/roost.rs",
        "gate_roost_open",
        "authorize_roost_open(",
        "R2: an unconfirmed root needs the native first-open dialog",
    ),
    (
        "commands/roost.rs",
        "gate_roost_open",
        "lock_or_recover(&CONFIRMED_ROOTS_WRITE)",
        "R2: the confirmed-roots read-modify-write is one lane",
    ),
    (
        "commands/sync.rs",
        "sync_connect",
        "authorize_first_pair(",
        "R1: pairing with an unconfirmed peer needs a native confirmation",
    ),
    (
        "commands/sync.rs",
        "sync_connect_iroh",
        "gate_iroh_first_pair(",
        "R1/SY-1: sync_connect_iroh must go through the one pre-dial function (PIN parse, ticket decode, native gate, THEN the enrollment window)",
    ),
    (
        "commands/sync.rs",
        "gate_iroh_first_pair",
        "authorize_first_pair(",
        "R1: pairing with an unconfirmed NodeId needs a native confirmation",
    ),
    (
        "commands/sync.rs",
        "gate_iroh_first_pair",
        "PairAuth::UserConfirmed",
        "SA-2: the enrollment window opens only when the user answered the dialog just now, never for an already-known peer",
    ),
    (
        "commands/sync.rs",
        "sync_listen_iroh",
        "authorize_pairing_reveal(",
        "R1-in: revealing the pairing code / opening the enrollment window needs a native confirmation",
    ),
    (
        "commands/sync.rs",
        "sync_start_listening",
        "authorize_pairing_reveal(",
        "R1-in: starting the inbound listener + enrollment window needs a native confirmation",
    ),
    (
        "commands/sync.rs",
        "sync_init",
        "sync_init_inner(",
        "RF-1: sync_init starts the engine only through the gated sync_init_inner",
    ),
    (
        "commands/sync.rs",
        "sync_try_autostart",
        "gated_sync_roost_backend(",
        "RF-1: autostart refuses a roost that is not the open (R2-confirmed) one before stopping or starting anything",
    ),
    (
        "commands/sync.rs",
        "sync_try_autostart",
        "sync_init_inner(",
        "RF-1: autostart starts the engine only through the gated sync_init_inner",
    ),
    (
        "commands/sync.rs",
        "sync_init_inner",
        "gate_then_resolve_sync_state_dir(",
        "RF-1/R2: sync_init_inner gates on the installed backend BEFORE roost_db_dir writes vault-id.bin",
    ),
    (
        "commands/sync.rs",
        "sync_init_inner",
        "check_sync_roost_backend(",
        "RF-1: the install-commit re-check refuses a closed (None) or switched backend",
    ),
    (
        "commands/sync.rs",
        "gate_then_resolve_sync_state_dir",
        "gated_sync_roost_backend(",
        "RF-1: the state-dir resolver runs only after the backend gate",
    ),
    (
        "commands/sync.rs",
        "gated_sync_roost_backend",
        "check_sync_roost_backend(",
        "RF-1: no installed backend (e.g. after close_roost) or another roost's backend is refused",
    ),
    (
        "commands/mcp.rs",
        "claude_desktop_mcp",
        ".confirm(",
        "R5: giving Claude Desktop persistent roost access needs a native confirmation",
    ),
];

#[test]
fn renderer_trust_choke_points_stay_wired() {
    let mut problems = Vec::new();
    for (file, func, needle, why) in WIRING {
        // Code-only view (RF-9): a comment or a string that mentions `needle`
        // must not satisfy the check, and a call whose result is discarded
        // (`let _ = gate(..)`) is not a gate.
        let text = read_code(file);
        match fn_body(&text, func) {
            None => problems.push(format!("{file}: `fn {func}(` not found ({why})")),
            Some(body) => {
                if let Some(problem) = wiring_problem(body, needle) {
                    problems.push(format!("{file}: `{func}` {problem} `{needle}` — {why}"));
                }
            }
        }
    }
    assert!(problems.is_empty(), "\n{}\n", problems.join("\n"));
}

/// RF-9, order: `(file, fn, gate, effect, why)` — inside `fn`, the gate must be
/// evaluated (result used) BEFORE the effect it guards. `WIRING` proves a gate
/// is present; this proves it still comes first, which is the other way a
/// refactor silently breaks one (the action moved above its check).
///
/// Scoped to the roost/first-open/manifest surface. A sync or AI entry belongs
/// in the table only if its owner pins the effect name too.
const ORDERING: &[(&str, &str, &str, &str, &str)] = &[
    (
        "commands/roost.rs",
        "open_roost",
        "gate_roost_open(",
        "open_roost_impl(",
        "R2: the first-open dialog comes before anything is opened or installed",
    ),
    (
        "commands/roost.rs",
        "gate_roost_open",
        "authorize_roost_open(",
        "record_confirmed_root_locked(",
        "R2: a root is recorded as confirmed only AFTER the user said yes",
    ),
    (
        "commands/roost.rs",
        "write_roost_manifest",
        "manifest_paths_checked(",
        "authorize_manifest_write(",
        "F10: the manifest is validated before it can reach the confirmation",
    ),
    (
        "commands/roost.rs",
        "write_roost_manifest",
        "MANIFEST_WRITE.lock()",
        "authorize_manifest_write(",
        "F10/#7: the lane is taken before the previous manifest is read and the dialog runs",
    ),
    (
        "commands/roost.rs",
        "write_roost_manifest",
        "authorize_manifest_write(",
        "fs::write(",
        "F10: nothing is written to roosts.json before the confirmation",
    ),
    (
        "commands/roost.rs",
        "delete_roost_index",
        "authorize_removals(",
        "existing_index_dirs(",
        "F10: the removal dialog comes before any index dir is even looked up",
    ),
    (
        "commands/roost.rs",
        "delete_roost_index",
        "authorize_removals(",
        "remove_dir_all(",
        "F10: nothing is deleted before the confirmation",
    ),
];

#[test]
fn guarded_effects_come_after_their_gate() {
    let mut problems = Vec::new();
    for (file, func, gate, effect, why) in ORDERING {
        let text = read_code(file);
        match fn_body(&text, func) {
            None => problems.push(format!("{file}: `fn {func}(` not found ({why})")),
            Some(body) => {
                if let Some(problem) = order_problem(body, gate, effect) {
                    problems.push(format!("{file}: `{func}`: {problem} — {why}"));
                }
            }
        }
    }
    assert!(problems.is_empty(), "\n{}\n", problems.join("\n"));
}

// ── the guard's own machinery (RF-9) ─────────────────────────────────────────

#[test]
fn code_only_blanks_comments_and_literal_contents() {
    let src = concat!(
        "fn f<'a>(x: &'a str) {\n",
        "    // gate(1)\n",
        "    /// gate(2)\n",
        "    /* gate(3) /* nested gate(4) */ still comment gate(5) */\n",
        "    let s = \"gate(6)\";\n",
        "    let e = \"esc \\\" gate(7)\";\n",
        "    let r = r#\"gate(8) \" still raw\"#;\n",
        "    let b = br\"gate(9)\";\n",
        "    let q = '\"'; let n = '\\n'; let u = '\\u{41}';\n",
        "    real_call(x);\n",
        "}\n",
    );
    let code = code_only(src);
    assert_eq!(code.len(), src.len(), "layout preserved");
    assert_eq!(code.matches('\n').count(), src.matches('\n').count());
    for n in 1..=9 {
        assert!(
            !code.contains(&format!("gate({n})")),
            "gate({n}) survived:\n{code}"
        );
    }
    // Code after a char literal holding a quote, and after a lifetime, is kept.
    assert!(code.contains("real_call(x);"), "{code}");
    assert!(code.contains("fn f<'a>(x: &'a str)"), "{code}");
}

#[test]
fn fn_body_ignores_a_fn_that_only_a_comment_mentions() {
    let code = code_only(concat!(
        "// fn target( is described here\n",
        "fn other() {\n    x();\n}\n",
        "fn target() {\n    real();\n}\n",
    ));
    let body = fn_body(&code, "target").unwrap();
    assert!(body.contains("real();") && !body.contains("x();"), "{body}");
}

#[test]
fn a_comment_or_string_cannot_satisfy_the_wiring_check() {
    let src = concat!(
        "fn guarded() {\n",
        "    // authorize(thing) goes here\n",
        "    tracing::info!(\"authorize(thing) skipped\");\n",
        "    do_it();\n",
        "}\n",
    );
    let code = code_only(src);
    let body = fn_body(&code, "guarded").unwrap();
    assert!(wiring_problem(body, "authorize(").is_some());
    // The raw text WOULD have passed — that was the hole.
    assert!(fn_body(src, "guarded").unwrap().contains("authorize("));
}

#[test]
fn a_call_whose_result_is_thrown_away_is_not_a_gate() {
    let ignored = "fn g() {\n    let _ = authorize(x);\n    do_it();\n}\n";
    let underscore = "fn g() {\n    _ = authorize(x);\n    do_it();\n}\n";
    let used = "fn g() {\n    let ok = authorize(x)?;\n    do_it(ok);\n}\n";
    let named = "fn g() {\n    let _lane = authorize(x);\n    do_it();\n}\n";
    for src in [ignored, underscore] {
        let code = code_only(src);
        let body = fn_body(&code, "g").unwrap();
        assert!(wiring_problem(body, "authorize(").is_some(), "{src}");
    }
    for src in [used, named] {
        let code = code_only(src);
        let body = fn_body(&code, "g").unwrap();
        assert!(wiring_problem(body, "authorize(").is_none(), "{src}");
    }
}

#[test]
fn ordering_check_catches_an_effect_moved_above_its_gate() {
    let good = code_only("fn g() {\n    gate()?;\n    effect();\n}\n");
    let moved = code_only("fn g() {\n    effect();\n    gate()?;\n}\n");
    let ignored = code_only("fn g() {\n    let _ = gate();\n    effect();\n}\n");
    let in_comment = code_only("fn g() {\n    effect();\n    // gate() is checked later\n}\n");
    let ok = |code: &str| order_problem(fn_body(code, "g").unwrap(), "gate(", "effect(");
    assert_eq!(ok(&good), None);
    assert!(ok(&moved).unwrap().contains("BEFORE"));
    assert!(ok(&ignored).is_some(), "a discarded gate is not a gate");
    assert!(ok(&in_comment).is_some(), "a comment is not a gate");
    assert!(
        order_problem(fn_body(&good, "g").unwrap(), "gate(", "missing(")
            .unwrap()
            .contains("stale")
    );
}

#[test]
fn only_test_only_cfg_items_are_skipped_by_the_counter() {
    for test_only in [
        "#[cfg(test)]",
        "#[cfg(test)] mod tests {",
        "#[cfg(all(test, feature = \"queue-server\"))]",
        "#[cfg(all(test, unix))]",
        "#[cfg(all(unix, test))]",
    ] {
        assert!(is_test_only_cfg(test_only), "{test_only}");
    }
    for production in [
        "#[cfg(not(test))]",
        "#[cfg(any(test, target_os = \"ios\"))]",
        "#[cfg(any(target_os = \"android\", target_os = \"ios\", test))]",
        "#[cfg(all(unix, not(test)))]",
        "#[cfg(unix)]",
        "#[cfg(feature = \"queue-server\")]",
        "#[cfg(target_os = \"test\")]",
        "#[cfg_attr(test, allow(dead_code))]",
        "#[derive(Debug)]",
        "fn test() {",
    ] {
        assert!(!is_test_only_cfg(production), "{production}");
    }
}

#[test]
fn counter_counts_cfg_not_test_and_any_test_items_as_production() {
    let src = r#"
fn real() {
    std::fs::read("a");
}
#[cfg(test)]
mod tests {
    fn t() {
        std::fs::read("in-test-module");
    }
}
#[cfg(all(test, unix))]
fn only_in_tests() {
    std::fs::read("in-all-test");
}
#[cfg(not(test))]
fn shipped() {
    std::fs::read("not-test");
}
#[cfg(any(test, target_os = "ios"))]
fn ios_too() {
    std::fs::read("ios-or-test");
}
fn after() {
    std::fs::read("after");
}
"#;
    // real + not(test) + any(test, ios) + after = 4. The old counter skipped
    // all of `not(test)` and `any(test, ..)` and reported 2.
    assert_eq!(count_lines_in_text(src, &["std::fs::"]), 4);
}

// ───────────────────────────── RF-6 ────────────────────────────────────────

/// The roost-id RESOLVER THAT CREATES: it writes `<roost>/.corvux/vault-id.bin`
/// into the folder it is given. Only code that already holds a confirmed,
/// installed roost may reach it. Anything that merely needs to FIND an existing
/// index (startup GC over the webview-writable manifest, deleting a roost's
/// index) uses the read-only twin (`lookup_vault_id_sync` /
/// `existing_index_dirs` / `live_index_dir_names`).
const ENSURE_VAULT_ID_BASELINE: &[(&str, usize, &str)] = &[
    (
        "paths.rs",
        1,
        "stable_roost_dir_name, reached only through roost_db_dir (pinned below)",
    ),
    (
        "sync/engine/engine.rs",
        1,
        "SyncEngine::new: constructed only by sync_init_inner after the installed-backend gate (RF-1)",
    ),
    ("sync/vault_id.rs", 1, "the definition"),
];

/// `roost_db_dir` = the creating resolver + the index dir + its marker. Every
/// production caller, and why it is behind a confirmation.
const ROOST_DB_DIR_BASELINE: &[(&str, usize, &str)] = &[
    ("paths.rs", 1, "the definition"),
    (
        "commands/graph.rs",
        2,
        "init_graph and rebuild_database: both compare the path with the INSTALLED backend (same_roost_root) first",
    ),
    (
        "commands/search.rs",
        1,
        "init_search: compares the path with the installed backend first",
    ),
    (
        "commands/roost_tree_cache.rs",
        1,
        "cache_file: callers pass the OPEN roost (open_roost_impl after the first-open gate; invalidate/revalidate with the installed backend's root)",
    ),
    (
        "commands/sync.rs",
        1,
        "the resolver closure of sync_init_inner, run by gate_then_resolve_sync_state_dir only AFTER the installed-backend gate (RF-1)",
    ),
];

#[test]
fn only_gated_code_may_create_a_roost_id() {
    let ensure = scan(&["."], &["ensure_vault_id_sync("]);
    let problems = ratchet_problems(
        "ensure_vault_id_sync (RF-6)",
        &ensure,
        ENSURE_VAULT_ID_BASELINE,
    );
    assert!(
        problems.is_empty(),
        "\n{}\n\nThis call WRITES vault-id.bin into the folder it is given. A new caller must be \
         behind the first-open gate / installed-backend check, or use lookup_vault_id_sync.\n",
        problems.join("\n")
    );
    let db_dir = scan(&["."], &["roost_db_dir("]);
    let problems = ratchet_problems("roost_db_dir (RF-6)", &db_dir, ROOST_DB_DIR_BASELINE);
    assert!(
        problems.is_empty(),
        "\n{}\n\nroost_db_dir creates the roost id. A caller that only needs to FIND an index must \
         use paths::existing_index_dirs / live_index_dir_names.\n",
        problems.join("\n")
    );
}

/// The read-only paths must stay read-only: GC, the live-name computation, the
/// index lookup and `delete_roost_index` may not call any resolver that
/// creates (code-only view, so a comment cannot hide a call and cannot fake one).
#[test]
fn gc_and_index_deletion_never_use_a_creating_resolver() {
    const CREATING: &[&str] = &[
        "ensure_vault_id_sync(",
        "stable_roost_dir_name(",
        "roost_db_dir(",
    ];
    let paths_code = read_code("paths.rs");
    let roost_code = read_code("commands/roost.rs");
    let mut problems = Vec::new();
    for (code, file, func) in [
        (&paths_code, "paths.rs", "gc_orphaned_index_dirs"),
        (&paths_code, "paths.rs", "gc_orphaned_index_dirs_in"),
        (&paths_code, "paths.rs", "live_index_dir_names"),
        (&paths_code, "paths.rs", "existing_roost_dir_name"),
        (&paths_code, "paths.rs", "existing_index_dirs"),
        (&roost_code, "commands/roost.rs", "delete_roost_index"),
    ] {
        let Some(body) = fn_body(code, func) else {
            problems.push(format!("{file}: `fn {func}(` not found"));
            continue;
        };
        for creating in CREATING {
            if body.contains(creating) {
                problems.push(format!(
                    "{file}: `{func}` calls `{creating}` — it would write vault-id.bin into a folder nobody confirmed"
                ));
            }
        }
    }
    // And each one still reaches the read-only lookup it is meant to use.
    for (code, file, func, needle) in [
        (
            &paths_code,
            "paths.rs",
            "gc_orphaned_index_dirs_in",
            "live_index_dir_names(",
        ),
        (
            &paths_code,
            "paths.rs",
            "live_index_dir_names",
            "existing_roost_dir_name(",
        ),
        (
            &paths_code,
            "paths.rs",
            "existing_roost_dir_name",
            "lookup_vault_id_sync(",
        ),
        (
            &paths_code,
            "paths.rs",
            "existing_index_dirs",
            "existing_roost_dir_name(",
        ),
        (
            &roost_code,
            "commands/roost.rs",
            "delete_roost_index",
            "existing_index_dirs(",
        ),
    ] {
        match fn_body(code, func) {
            None => problems.push(format!("{file}: `fn {func}(` not found")),
            Some(body) => {
                if let Some(problem) = wiring_problem(body, needle) {
                    problems.push(format!("{file}: `{func}` {problem} `{needle}`"));
                }
            }
        }
    }
    assert!(problems.is_empty(), "\n{}\n", problems.join("\n"));
}

/// `get_settings` must not be able to regain a key by accident: the only
/// `Settings` clone it may return is the redacted view.
#[test]
fn get_settings_never_returns_the_raw_guard() {
    let text = read_code("commands/ai/settings.rs");
    let body = fn_body(&text, "get_settings").expect("get_settings exists");
    assert!(
        !body.contains("Ok(guard.clone())"),
        "get_settings returns the live struct again (provider keys leak to the webview — F11)"
    );
}
