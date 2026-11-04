//! Source-level ratchet guards for the sync subsystem (Area S).
//!
//! These make two `cx-area-sync` invariants fail the build instead of relying
//! on review memory:
//!   SYN-I1 — RoostFs-only roost access: no NEW raw `std::fs` / `tokio::fs`
//!            call sites on the sync surface (route through `RoostFs` /
//!            `resolve_fs` so `safe_resolve` rejects symlink escapes).
//!   SYN-I4 — structured lifecycle: no NEW untracked `tokio::spawn` sites
//!            (every task must be owned, cancellable, and drained by stop()).
//!            Roost transitions also preserve `init → lifecycle` lock order.
//!
//! ## Production-only counting (fixed 2026-07-11)
//!
//! The first cut counted EVERY textual `std::fs::` line, including
//! `#[cfg(test)]` modules and comments. A code-map showed that made the
//! baselines meaningless — e.g. `peer.rs`'s "33" were 100% `TempDir` test
//! scaffolding; its production raw-fs is zero. A ratchet that counts test
//! noise trains people to ignore it, which is worse than no ratchet.
//!
//! `count_production_lines` now skips any line inside a `#[cfg(...test...)]`
//! block — tracked by brace depth, so an inline `#[cfg(test)] fn` mid-`impl`
//! and a `#[cfg(all(test, feature = "…"))] mod` are both excluded — and strips
//! `//` comments before matching. The baselines below are the REAL production
//! surface: ~13 raw-fs and ~17 spawn sites, not the ~91 / ~40 the blunt count
//! reported.
//!
//! Ratchet semantics (strict, both directions): a count above baseline fails
//! (you added a site — route it correctly, or, for legitimate app-private
//! state, raise the baseline WITH a justifying comment); a count below baseline
//! also fails (you removed sites — lower the baseline so it can't creep back).
//! Baselines audited @ 77f79fc. Each entry is tagged ROOST-PATH (must reach 0)
//! or APP-PRIVATE (legitimate, pinned).

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Recursively collect `.rs` files under `dir`.
fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
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

/// Path relative to the manifest dir, normalized to forward slashes.
fn rel_key(path: &Path) -> String {
    path.strip_prefix(manifest_dir())
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// Count PRODUCTION lines in `path` containing any of `needles`, excluding any
/// line inside a `#[cfg(...test...)]`-gated item. This codebase gates test code
/// inline in several forms — `#[cfg(test)] mod tests` at file end, `#[cfg(test)]`
/// on a single method mid-`impl`, and `#[cfg(all(test, feature = "…"))] mod` —
/// so we track brace depth to skip exactly the gated block, not "everything
/// after the first cfg". `//` line-comments are stripped before both brace
/// counting and needle matching (so a stale doc-comment mentioning `std::fs`
/// or a comment brace can't skew the result).
///
/// Heuristic caveats (all benign here, and any drift shows up as a count that
/// no longer matches the audited baseline — a loud failure, not a silent pass):
/// naive brace counting doesn't strip string/char literals, and block comments
/// aren't handled.
fn count_production_lines(path: &Path, needles: &[&str]) -> usize {
    let Ok(text) = fs::read_to_string(path) else {
        return 0;
    };
    let mut count = 0i64;
    let mut depth = 0i64; // running brace depth
    let mut skip_below = -1i64; // if >=0, we're inside a test block; exit when depth < this
    let mut pending_test = false; // saw a test-cfg attr, awaiting its block/`;`

    for raw in text.lines() {
        // Strip a line-comment tail so `//` braces and `// std::fs` mentions
        // don't count. (Doesn't handle `//` inside a string literal, but no
        // such line in the sync tree carries a brace-imbalance or a needle.)
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
                // attr + block open on one line
                let before = depth;
                depth += opens - closes;
                if depth > before {
                    skip_below = before + 1;
                }
            } else if !trimmed.contains(';') {
                pending_test = true; // block opens on a later line
            }
            depth += if opens > 0 { 0 } else { opens - closes };
            continue;
        }

        if pending_test {
            let before = depth;
            depth += opens - closes;
            if opens > 0 {
                skip_below = before + 1; // gated block just opened
                pending_test = false;
            } else if trimmed.contains(';') {
                pending_test = false; // single-line gated item (use/const)
            }
            continue;
        }

        depth += opens - closes;
        if !trimmed.is_empty() && needles.iter().any(|n| code.contains(n)) {
            count += 1;
        }
    }
    count.max(0) as usize
}

/// Compare per-file production counts of `needles` across the sync surface
/// (`src/sync/**` + `src/commands/sync.rs`) against `baseline`, reporting every
/// deviation with remediation. `src/roostfs/` is the hardened backend and lives
/// OUTSIDE `src/sync/`, so it is never in scope — no exclusion needed.
fn run_ratchet(
    invariant: &str,
    needles: &[&str],
    baseline: &BTreeMap<&str, usize>,
    remediation: &str,
) {
    let root = manifest_dir();
    let mut files = Vec::new();
    collect_rs_files(&root.join("src/sync"), &mut files);
    let commands_sync = root.join("src/commands/sync.rs");
    if commands_sync.exists() {
        files.push(commands_sync);
    }

    let mut actual: BTreeMap<String, usize> = BTreeMap::new();
    for f in &files {
        let n = count_production_lines(f, needles);
        if n > 0 {
            actual.insert(rel_key(f), n);
        }
    }

    let mut violations = Vec::new();
    for (file, count) in &actual {
        match baseline.get(file.as_str()) {
            None => violations.push(format!(
                "NEW production {invariant} site(s): {file} ({count}) — classify and route correctly, \
                 or add a baseline entry WITH a justifying comment"
            )),
            Some(base) if count > base => violations.push(format!(
                "{file}: {count}, baseline {base} — site(s) ADDED"
            )),
            Some(base) if count < base => violations.push(format!(
                "{file}: {count}, baseline {base} — sites removed (good): lower the baseline to {count}"
            )),
            _ => {}
        }
    }
    for (file, base) in baseline {
        if !actual.contains_key(*file) {
            violations.push(format!(
                "{file}: 0, baseline {base} — file clean or moved: remove/update its baseline entry"
            ));
        }
    }

    assert!(
        violations.is_empty(),
        "\n== {invariant} ratchet violated ==\n{}\n\n{remediation}\n\
         Baselines: src-tauri/tests/sync_invariants.rs\n",
        violations.join("\n")
    );
}

/// SYN-I1: no new raw filesystem access on the production sync surface.
#[test]
fn syn_i1_no_new_raw_fs_on_sync_surface() {
    // Audited @ 77f79fc; SYN-B2 CLOSED 2026-07-11 — vault_id now routes through
    // NativeFs's symlink-safe resolve, so ZERO production ROOST-PATH raw-fs
    // remains on the sync surface. Every roost read/write/delete goes through
    // resolve_fs → RoostFs. The entries below are legitimate APP-PRIVATE state
    // (app_data_dir, never the roost).
    let baseline: BTreeMap<&str, usize> = BTreeMap::from([
        ("src/commands/sync.rs", 8), // APP-PRIVATE — prefs.json + tls.bin under app_data_dir/sync
        ("src/sync/engine/engine.rs", 1), // APP-PRIVATE — DocStore/PeerStore state_dir (never the roost)
    ]);
    run_ratchet(
        "SYN-I1 raw fs (std::fs / tokio::fs)",
        &["std::fs::", "tokio::fs::"],
        &baseline,
        "ROOST-PATH (notes/attachments/.corvux/trash): route through the RoostFs backend \
         (resolve_fs / NativeFs) — never a raw-fs fallback. APP-PRIVATE state or test code: \
         adjust the baseline WITH a justifying comment.",
    );
}

/// SYN-I4: zero raw `tokio::spawn` in the engine-owned sync surface. W4 landed
/// the engine-owned `CancellationToken` + `TaskTracker` (`SyncEngine::spawn_tracked`),
/// so EVERY engine task is now owned, cancel-aware, and drained by `stop()`.
/// The only remaining raw spawns are the two tasks with a lifecycle SEPARATE
/// from the engine (below) — they are not `stop()`'s to drain.
#[test]
fn syn_i4_no_new_untracked_spawns_in_sync() {
    // Post-W4 baseline: engine-owned files are at ZERO raw `tokio::spawn` — the
    // ratchet fails LOUD if any reappears (a new task that isn't `spawn_tracked`
    // is by definition undrained by `stop()`). The two survivors are separate
    // lifecycles, not engine tasks:
    let baseline: BTreeMap<&str, usize> = BTreeMap::from([
        // Discovery mDNS browse loop — owned by the `Discovery` daemon (shut
        // down via `discovery.shutdown()` in `sync_stop`), not the engine.
        ("src/sync/discovery/mdns.rs", 1),
    ]);
    run_ratchet(
        "SYN-I4 untracked tokio::spawn",
        &["tokio::spawn"],
        &baseline,
        "Engine sync tasks MUST go through `SyncEngine::spawn_tracked` (tracked + cancel-aware, \
         drained by stop()). A raw `tokio::spawn` in an engine-owned file is a SYN-B7 regression: \
         answer 'who cancels this on stop()/reset?' — the answer is spawn_tracked. If you truly \
         added a separate-lifecycle task, add a baseline entry WITH a justifying comment.",
    );
}

/// SYN-I4: roost switching and sync init share one lock order. This is a
/// source-level guard because the old AB/BA cycle required a rare interleaving
/// and ordinary async tests could pass thousands of times without scheduling
/// it. The transition must never call the public shutdown wrapper (which takes
/// `init`) while already holding `lifecycle`.
#[test]
fn syn_i4_roost_transition_uses_init_then_lifecycle() {
    let source =
        fs::read_to_string(manifest_dir().join("src/commands/roost.rs")).expect("read roost.rs");
    let install = source
        .split("async fn install_roost_backend")
        .nth(1)
        .and_then(|s| s.split("fn clear_backend_scoped_state").next())
        .expect("install_roost_backend body");
    let init = install
        .find("sync_state.init.lock().await")
        .expect("init lock");
    let lifecycle = install
        .find("sync_state.lifecycle.lock().await")
        .expect("lifecycle lock");
    assert!(
        init < lifecycle,
        "roost switch lock order must be init → lifecycle"
    );
    let stop = source
        .split("async fn stop_sync_for_roost_switch")
        .nth(1)
        .and_then(|s| s.split("async fn scan_directory_blocking").next())
        .expect("stop_sync_for_roost_switch body");
    assert!(
        stop.contains("sync_shutdown_without_disabling_init_locked"),
        "roost switch must call the shutdown leaf that assumes init is held"
    );
    assert!(
        !stop.contains("sync_shutdown_without_disabling(app"),
        "public shutdown reacquires init and would deadlock under lifecycle"
    );
}
