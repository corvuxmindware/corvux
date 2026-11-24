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
//! creep back). Production code only: `#[cfg(...test...)]` items and `//`
//! comments are skipped (brace-depth tracked), so test scaffolding is free to
//! use raw `std::fs` on a `TempDir`.
//!
//! Guards:
//! * **F6** `raw_fs_calls_in_commands_db_ai_are_pinned` — raw `std::fs` /
//!   `tokio::fs` across `src/commands`, `src/db`, `src/ai` (the old lint covered
//!   `src/ai/tools/*.rs` writes and `src/commands` reads only).
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

/// Count PRODUCTION lines containing any of `needles`: skips any line inside a
/// `#[cfg(...test...)]`-gated item (brace-depth tracked, so a gated `fn`
/// mid-`impl`, a gated `mod`, and `#[cfg(all(test, ..))]` are all excluded) and
/// strips `//` comments first. Same semantics as `sync_invariants.rs`; kept as
/// a copy because integration tests cannot share a module without a `common/`
/// crate, and that guard is not worth refactoring for it.
///
/// Caveats (benign — drift shows up as a count that no longer matches the
/// audited baseline, a loud failure rather than a silent pass): braces inside
/// string literals and block comments are not handled.
fn count_production_lines(path: &Path, needles: &[&str]) -> usize {
    let Ok(text) = fs::read_to_string(path) else {
        return 0;
    };
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

        let is_test_cfg = trimmed.starts_with("#[cfg(") && code.contains("test");
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
    ("ai/skills/manifest.rs", 1, "ROOST (F7, tracked): reads a manifest the registry scan found; native-only, SAF uses refresh_with_roost_contents"),
    ("ai/skills/registry.rs", 1, "ROOST (F7, tracked): native scan of <roost>/.corvux/skills and ~/.corvux/skills"),
    ("ai/skills/watcher.rs", 1, "ROOST (F7, tracked): creates the watched <roost>/.corvux/skills dir so notify can attach"),
    ("ai/subagents/manifest.rs", 1, "ROOST (F7, tracked): same shape as skills/manifest.rs"),
    ("ai/subagents/registry.rs", 1, "ROOST (F7, tracked): native scan of <roost>/.corvux/agents and the global dir"),
    ("ai/subagents/watcher.rs", 1, "ROOST (F7, tracked): creates the watched agents dir"),
    ("ai/tools/trace.rs", 4, "ROOST, dev-only: opt-in CORVUX_TRACE_AGENT=1 JSONL recorder under <roost>/.corvux/traces; never on by default"),
    ("commands/ai/consent.rs", 3, "APP-PRIVATE: ai_consent.json next to settings.json in app-data"),
    ("commands/ai/settings.rs", 3, "APP-PRIVATE: settings.json temp+atomic replace in app-data"),
    ("commands/graph.rs", 1, "APP-PRIVATE: rebuild_database deletes derived graph.db/search.db files in the per-roost app-data index dir"),
    ("commands/roost.rs", 15,"ROOST bootstrap + app-data: creates/backs up app-created roost dirs before any backend exists, native tree scanner with AUDIT-076 symlink filtering (MR-B4), roosts.json manifest, index-dir delete"),
    ("commands/roost_tree_cache.rs", 3, "APP-PRIVATE: derived file-tree cache in app-data"),
    ("commands/sync.rs", 6, "APP-PRIVATE: sync prefs + TLS identity in app-data (also ratcheted by sync_invariants.rs)"),
    ("commands/task_notify.rs", 3, "APP-PRIVATE: missed-notification ledger in app-data"),
    ("db/queries/notes.rs", 1, "ROOST (F7, OPEN): file_mtime_secs stats a DB-stored absolute path; metadata only, no content read, never returned to the webview. Real fix = RoostFs mtime + store RoostRel"),
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

#[test]
fn installing_a_backend_wraps_it_in_validated_fs() {
    let text = read_src("commands/files.rs");
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
];

#[test]
fn renderer_trust_choke_points_stay_wired() {
    let mut problems = Vec::new();
    for (file, func, needle, why) in WIRING {
        let text = read_src(file);
        match fn_body(&text, func) {
            None => problems.push(format!("{file}: `fn {func}(` not found ({why})")),
            Some(body) if !body.contains(needle) => problems.push(format!(
                "{file}: `{func}` no longer calls `{needle}` — {why}"
            )),
            Some(_) => {}
        }
    }
    assert!(problems.is_empty(), "\n{}\n", problems.join("\n"));
}

/// `get_settings` must not be able to regain a key by accident: the only
/// `Settings` clone it may return is the redacted view.
#[test]
fn get_settings_never_returns_the_raw_guard() {
    let text = read_src("commands/ai/settings.rs");
    let body = fn_body(&text, "get_settings").expect("get_settings exists");
    assert!(
        !body.contains("Ok(guard.clone())"),
        "get_settings returns the live struct again (provider keys leak to the webview — F11)"
    );
}
