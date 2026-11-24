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
//!   SYN-I11 — per-peer state is roost-scoped: keychain keys for paired-peer
//!            secrets are built ONLY by `PeerSecret` in `peer_store.rs`
//!            (scoped to one roost's registry), anywhere in the crate.
//!   SY-1   — the first-pair enrollment window is opened only after the
//!            native-dialog gate, in every pairing command (review 2026-11-12).
//!   SY-3   — prefs flows that need no PIN cannot reach the secret store
//!            (`sy3_*`); SY-4 — unpair forgets the first-pair confirmation
//!            (`sy4_*`); SY-5 — removed roosts lose their PIN slots, mobile
//!            secret writes are fsynced (`sy5_*`); SY-7 — the pairing trust
//!            record file is inside the raw-fs / spawn ratchets and has its
//!            own lock rules (`sy7_*`).
//!   SY-Q   — the only HTTP client on the sync surface is the queue client's, and
//!            it never follows redirects (`syq_*`).
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
    production_code_lines(&text)
        .iter()
        .filter(|code| !code.trim_start().is_empty() && needles.iter().any(|n| code.contains(n)))
        .count()
}

/// The production lines of `text`, in order, with `//` comment tails stripped
/// and every `#[cfg(...test...)]`-gated block removed (see
/// [`count_production_lines`] for the rules and caveats). Leading whitespace
/// is kept, so callers can still tell a column-0 item from an indented one.
fn production_code_lines(text: &str) -> Vec<String> {
    let mut out = Vec::new();
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
        out.push(code.to_string());
    }
    out
}

/// The sync surface outside `src/sync/`: the IPC layer. `sync_endpoint.rs`
/// holds the pairing trust records (raw `std::fs`, a static tokio lane + a static
/// std mutex), so it is a sync file for every ratchet below — it used to sit
/// outside all of them (review 2026-11-12, SY-7).
const SYNC_COMMAND_FILES: [&str; 2] = ["src/commands/sync.rs", "src/commands/sync_endpoint.rs"];

/// Compare per-file production counts of `needles` across the sync surface
/// (`src/sync/**` + [`SYNC_COMMAND_FILES`]) against `baseline`, reporting every
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
    for rel in SYNC_COMMAND_FILES {
        let file = root.join(rel);
        assert!(file.exists(), "sync surface file moved or renamed: {rel}");
        files.push(file);
    }
    check_ratchet(invariant, &files, needles, baseline, remediation);
}

/// The ratchet comparison itself, over an explicit file set (so an invariant
/// whose class can appear anywhere in the crate can scan all of `src/`).
fn check_ratchet(
    invariant: &str,
    files: &[PathBuf],
    needles: &[&str],
    baseline: &BTreeMap<&str, usize>,
    remediation: &str,
) {
    let mut actual: BTreeMap<String, usize> = BTreeMap::new();
    for f in files {
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
        // Shared atomic preference persistence removed the local write + rename.
        ("src/commands/sync.rs", 6), // APP-PRIVATE — prefs.json + tls.bin under app_data_dir/sync
        // APP-PRIVATE — the pairing trust record `app_data_dir/confirmed_sync_peers.json`
        // (read, write-tmp, rename) + a read-only listing of `<app_data>/vaults` to find the
        // paired-peer registries (marked, exactly-named index dirs only, SA-1). Never the
        // roost. SY-7: this file was outside the ratchet until 2026-11-12.
        ("src/commands/sync_endpoint.rs", 4),
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

/// SYN-I11: per-peer sync secrets are scoped to ONE roost's peer registry.
/// The keychain is one store per install while `peers.bin` is per roost, so a
/// key built from the peer device id alone lets the same device paired in two
/// roosts overwrite (and on unpair, delete) the other roost's credentials. The
/// only place allowed to spell a secret kind is `PeerSecret::kind()`, which
/// `scoped_key` / `legacy_key` wrap. Scans the WHOLE crate, not just
/// `src/sync`, because a stray `format!("sync_pin:{id}")` in any command
/// module would reintroduce the bug.
#[test]
fn syn_i11_peer_secret_keys_only_via_peer_secret() {
    let mut files = Vec::new();
    collect_rs_files(&manifest_dir().join("src"), &mut files);
    let baseline: BTreeMap<&str, usize> = BTreeMap::from([
        // The four `PeerSecret::kind()` match arms — the single choke point.
        ("src/sync/store/peer_store.rs", 4),
    ]);
    check_ratchet(
        "SYN-I11 paired-peer secret key",
        &files,
        &[
            "\"sync_pin",
            "\"sync_queue_key",
            "\"sync_resumption_token",
            "\"sync_peer_vault_id",
        ],
        &baseline,
        "Build paired-peer keychain keys ONLY via `PeerSecret::scoped_key` in \
         src/sync/store/peer_store.rs. A key from the device id alone collides across roosts \
         (the same phone paired in Work and Personal) — see cx-area-sync SYN-I11. A new secret \
         kind = a new `PeerSecret` variant (and raise this baseline by one, with a comment).",
    );
}

/// SY-1 (review 2026-11-12): the first-pair enrollment window admits INBOUND
/// first pairs from any NodeId, and every IPC argument comes from the webview.
/// So every production call that opens it must sit AFTER the native-dialog
/// gate in the same function. `sync_connect_iroh` used to open it before
/// parsing the PIN and before `authorize_first_pair`, so a hijacked renderer
/// looping the command with garbage kept the window open with no dialog.
///
/// Two checks, both textual (production lines only — tests and comments are
/// stripped by `production_code_lines`):
/// 1. Ratchet: the set of `open_enrollment_window(` call sites is pinned, so
///    a new opener anywhere in the crate fails until someone gates it and
///    adds it here.
/// 2. Order: in `commands/sync.rs`, each such call is preceded, inside its
///    enclosing top-level fn (header line excluded), by a gate call
///    (`authorize_pairing_reveal(` / `authorize_first_pair(` /
///    `gate_iroh_first_pair(`, which runs `authorize_first_pair` first). In
///    `sync_connect_iroh` the call must be the deferred `||` closure handed to
///    the gate, and inside `gate_iroh_first_pair` the order must be PIN parse
///    → ticket decode → gate → window. Limits: it sees text order, not
///    control flow (an `if` that skips the gate would pass); the behaviour is
///    proven by `commands::sync::iroh_pre_dial_gate_tests`.
#[test]
fn sy1_enrollment_window_opens_only_after_native_gate() {
    const OPEN: &str = "open_enrollment_window(";

    // 1. Ratchet over the whole crate.
    let mut files = Vec::new();
    collect_rs_files(&manifest_dir().join("src"), &mut files);
    let baseline: BTreeMap<&str, usize> = BTreeMap::from([
        // sync_start_listening, sync_listen_iroh, the `||` closure in
        // sync_connect_iroh, and the call inside gate_iroh_first_pair.
        ("src/commands/sync.rs", 4),
        // The definition `pub fn open_enrollment_window(&self)` itself.
        ("src/sync/engine/engine.rs", 1),
    ]);
    check_ratchet(
        "SY-1 enrollment-window opener",
        &files,
        &[OPEN],
        &baseline,
        "Open the first-pair enrollment window only AFTER the native gate \
         (`authorize_pairing_reveal` / `authorize_first_pair`) returned Ok and the inputs \
         parsed. A new opener must be gated, then added to this baseline with a comment.",
    );

    // 1b. SA-4: `open_after_confirm` RESETS the guess counter of an open window.
    // That is only safe for code that just received a real human "yes", so it
    // may be reached from exactly one place: `SyncEngine::open_enrollment_window`
    // (whose own callers are pinned above). A direct call from anywhere else —
    // an accept path, a reconnect, a new command — would hand out a fresh guess
    // budget with no dialog.
    check_ratchet(
        "SA-4 counter-resetting window opener",
        &files,
        &["open_after_confirm("],
        &BTreeMap::from([
            // The single call inside `SyncEngine::open_enrollment_window`.
            ("src/sync/engine/engine.rs", 1),
            // The definition `pub fn open_after_confirm(` itself.
            ("src/sync/crypto/pairing.rs", 1),
        ]),
        "`EnrollmentWindow::open_after_confirm` resets the failure counter and may only be \
         called from `SyncEngine::open_enrollment_window`, which is itself reachable only \
         after a native confirmation. Route new openers through that function.",
    );

    // 2. Source order inside commands/sync.rs.
    let text = fs::read_to_string(manifest_dir().join("src/commands/sync.rs"))
        .expect("read commands/sync.rs");
    let lines = production_code_lines(&text);
    let is_fn_header =
        |l: &str| !l.starts_with(char::is_whitespace) && l.contains("fn ") && !l.trim().is_empty();
    const GATES: [&str; 3] = [
        "authorize_pairing_reveal(",
        "authorize_first_pair(",
        "gate_iroh_first_pair(",
    ];
    let mut checked = 0;
    for (i, line) in lines.iter().enumerate() {
        if !line.contains(OPEN) {
            continue;
        }
        let start = (0..i)
            .rev()
            .find(|&j| is_fn_header(&lines[j]))
            .expect("enclosing fn for an enrollment-window opener");
        let header = &lines[start];
        let body_before = &lines[start + 1..i];
        let gated = body_before
            .iter()
            .any(|l| GATES.iter().any(|g| l.contains(g)));
        assert!(
            gated,
            "SY-1: `{}` opens the enrollment window without a preceding native gate in `{}`",
            line.trim(),
            header.trim()
        );
        if header.contains("fn sync_connect_iroh(") {
            assert!(
                line.contains("||"),
                "SY-1: sync_connect_iroh must open the window only through the closure \
                 handed to gate_iroh_first_pair, found `{}`",
                line.trim()
            );
        }
        if header.contains("fn gate_iroh_first_pair(") {
            let pos = |needle: &str| {
                body_before
                    .iter()
                    .position(|l| l.contains(needle))
                    .unwrap_or_else(|| panic!("gate_iroh_first_pair: `{needle}` missing"))
            };
            let (pin, ticket, gate) = (
                pos("Pin::parse("),
                pos("decode_ticket("),
                pos("authorize_first_pair("),
            );
            assert!(
                pin < gate && ticket < gate,
                "SY-1: PIN parse and ticket decode must precede the native gate"
            );
        }
        checked += 1;
    }
    assert_eq!(
        checked, 4,
        "expected the 4 pinned openers in commands/sync.rs"
    );
}

// ── Review 2026-11-12, SY-3 / SY-4 / SY-5 / SY-7 ─────────────────────────────
//
// Textual guards over the IPC layer (`commands/sync.rs`, `commands/sync_endpoint.rs`).
// Same limits as SY-1 above: they see source order and the enclosing function, not
// control flow; the behaviour is proven by the unit tests named in each message.

/// A column-0 line that opens a function (`fn`, `pub async fn`, …).
fn is_top_level_fn(line: &str) -> bool {
    !line.starts_with(char::is_whitespace) && line.contains("fn ") && !line.trim().is_empty()
}

/// Name of the top-level function enclosing (or declared on) line `i`.
fn enclosing_fn(lines: &[String], i: usize) -> String {
    (0..=i)
        .rev()
        .find(|&j| is_top_level_fn(&lines[j]))
        .and_then(|j| lines[j].split("fn ").nth(1))
        .and_then(|rest| rest.split(['(', '<']).next())
        .unwrap_or("")
        .trim()
        .to_string()
}

/// The production lines of function `name`, header included (to its closing
/// column-0 `}`).
fn fn_body(lines: &[String], name: &str) -> Vec<String> {
    let start = lines
        .iter()
        .position(|l| is_top_level_fn(l) && l.contains(&format!("fn {name}(")))
        .unwrap_or_else(|| panic!("fn {name} not found"));
    let end = (start + 1..lines.len())
        .find(|&j| lines[j].starts_with('}'))
        .unwrap_or(lines.len() - 1);
    lines[start..=end].to_vec()
}

fn production_lines_of(rel: &str) -> Vec<String> {
    let text =
        fs::read_to_string(manifest_dir().join(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"));
    production_code_lines(&text)
}

/// Functions (enclosing, header lines excluded) that contain `needle`, sorted.
fn callers_of(lines: &[String], needle: &str) -> Vec<String> {
    let mut out: Vec<String> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| l.contains(needle) && !is_top_level_fn(l))
        .map(|(i, _)| enclosing_fn(lines, i))
        .collect();
    out.sort();
    out.dedup();
    out
}

/// SY-3: a prefs load that fails closed on the secret store took down init,
/// autostart, the queue/strict-P2P settings and — worst — `sync_stop` (the
/// engine stopped, then the load failed, so `enabled` stayed true and sync
/// resumed on the next launch against the user's choice). The fix splits the
/// prefs: `StoredPrefs` (no secret store, ever) for every flow that needs only
/// non-secret settings, `load_prefs_with_pins` only where a PIN is used.
///
/// This pins the split: the secret-bearing loader/saver and every direct
/// `secret_store` call in `commands/sync.rs` have an exact, named list of
/// callers. A new caller fails here until someone argues it needs a PIN.
#[test]
fn sy3_secret_bearing_prefs_loads_are_limited_to_pin_consumers() {
    let lines = production_lines_of("src/commands/sync.rs");

    assert!(
        !lines
            .iter()
            .any(|l| l.contains("load_sync_prefs(") || l.contains("save_sync_prefs(")),
        "SY-3: `load_sync_prefs`/`save_sync_prefs` no longer exist — non-secret flows use \
         `load_stored_prefs`/`save_stored_prefs`, PIN flows `load_prefs_with_pins`/`save_prefs_with_pins`"
    );

    // Needle -> the exact set of functions (other than its own definition) that may use it.
    let pinned: [(&str, &[&str]); 4] = [
        // The PIN consumers: the listener PIN at autostart, "Show pairing code",
        // and the iroh dialer's own sticky PIN.
        (
            "load_prefs_with_pins(",
            &[
                "sync_connect_iroh",
                "sync_listen_iroh",
                "sync_try_autostart",
            ],
        ),
        // Writers of a PIN (or the migration that moves a plaintext one).
        (
            "save_prefs_with_pins(",
            &[
                "hydrate_prefs",
                "sync_connect_iroh",
                "sync_listen_iroh",
                "sync_start_listening",
                "sync_try_autostart",
            ],
        ),
        (
            "hydrate_prefs(",
            &["load_prefs_with_pins", "sync_try_autostart"],
        ),
        // Every direct secret-store call: hydrate (get), save (set), purge (delete).
        (
            "secret_store::",
            &[
                "hydrate_prefs",
                "purge_hosting_pins",
                "save_prefs_with_pins",
            ],
        ),
    ];
    for (needle, allowed) in pinned {
        let callers = callers_of(&lines, needle);
        let own = needle.trim_end_matches(['(', ':']);
        let callers: Vec<&str> = callers
            .iter()
            .map(String::as_str)
            .filter(|c| *c != own)
            .collect();
        assert_eq!(
            callers, allowed,
            "SY-3: callers of `{needle}` in commands/sync.rs changed. A flow that needs only \
             non-secret settings must use `load_stored_prefs`/`save_stored_prefs` (they cannot \
             reach the secret store); a flow that really needs a PIN must be added here."
        );
    }

    // `sync_stop` must record the user's choice without any secret access.
    let stop = fn_body(&lines, "sync_stop");
    let stop_text = stop.join("\n");
    assert!(
        stop_text.contains("record_stop_in_prefs("),
        "SY-3: sync_stop must clear `enabled` through record_stop_in_prefs"
    );
    for forbidden in ["with_pins", "secret_store", "hydrate", "load_stored_prefs"] {
        assert!(
            !stop_text.contains(forbidden),
            "SY-3: sync_stop must not touch `{forbidden}` — it cannot depend on the keychain"
        );
    }

    // The stored-prefs layer itself never names the secret store.
    for name in [
        "read_stored_prefs",
        "write_stored_prefs",
        "save_stored_prefs",
        "load_stored_prefs",
        "record_stop_in_prefs",
        "bring_up_prefs",
    ] {
        let body = fn_body(&lines, name).join("\n");
        assert!(
            !body.contains("secret_store") && !body.contains("strip_hosting_pins"),
            "SY-3: `{name}` must not reach the secret store"
        );
    }
}

/// SY-4: `sync_unpair` must drop the peer's first-pair confirmation (record AND
/// session memory) — and must do it BEFORE `engine.unpair`, so a record that
/// cannot be cleared refuses the unpair instead of leaving a silent re-pair path
/// open. Behaviour: `sync_endpoint::first_pair_gate_tests::unpairing_*`.
#[test]
fn sy4_unpair_forgets_the_confirmation_before_dropping_the_peer() {
    let lines = production_lines_of("src/commands/sync.rs");
    let body = fn_body(&lines, "sync_unpair");
    let pos = |needle: &str| {
        body.iter()
            .position(|l| l.contains(needle))
            .unwrap_or_else(|| panic!("SY-4: sync_unpair lost `{needle}`"))
    };
    let (keys, forget, unpair) = (
        pos("confirm_keys_for_peer"),
        pos("forget_confirmed_peer("),
        pos(".unpair("),
    );
    assert!(
        keys < forget && forget < unpair,
        "SY-4: sync_unpair must read the peer's keys, clear its confirmation, THEN unpair"
    );
}

/// SY-5b: a removed roost's hosting-PIN slots (keyed by the roost PATH) are
/// deleted at the one place that sees every removal — the authorized manifest
/// write — and SY-5a: the mobile secret store's `write_atomic` fsyncs the file
/// and its directory.
#[test]
fn sy5_removed_roosts_lose_their_pin_slots_and_mobile_writes_are_durable() {
    let roost = production_lines_of("src/commands/roost.rs");
    assert_eq!(
        callers_of(&roost, "purge_hosting_pins("),
        ["write_roost_manifest"],
        "SY-5b: the hosting-PIN purge must run from write_roost_manifest's removal block"
    );

    // The mobile module is `cfg(any(android, ios, test))`, which the production
    // splitter treats as test code — read the raw text.
    let text =
        fs::read_to_string(manifest_dir().join("src/secret_store.rs")).expect("secret_store.rs");
    let section = |from: &str, to: &str| {
        text.split(from)
            .nth(1)
            .and_then(|s| s.split(to).next())
            .unwrap_or_else(|| panic!("secret_store.rs lost `{from}`"))
            .to_string()
    };
    let write_atomic = section("fn write_atomic(", "fn write_synced(");
    assert!(
        write_atomic.contains("write_synced(")
            && write_atomic.contains("sync_parent_dir(")
            && write_atomic.contains("fs::rename("),
        "SY-5a: write_atomic must write+fsync the temp, rename, then fsync the directory"
    );
    assert!(
        write_atomic.find("write_synced(") < write_atomic.find("fs::rename(")
            && write_atomic.find("fs::rename(") < write_atomic.find("sync_parent_dir("),
        "SY-5a: order must be fsync file -> rename -> fsync directory"
    );
    assert!(
        section("fn write_synced(", "fn sync_parent_dir(").contains(".sync_all()"),
        "SY-5a: the temp file must be fsynced before the rename"
    );
}

/// SY-7: `commands/sync_endpoint.rs` (pairing trust records) is a sync file for
/// the raw-fs and spawn ratchets above (via `SYNC_COMMAND_FILES`). This adds the
/// lock rules those line-counting ratchets cannot express:
///
/// * its two static locks are exactly the known ones;
/// * the std mutex's guard is only ever a temporary inside one expression
///   (never a `let` binding), so it cannot be held across an `.await`;
/// * once the tokio record lane is taken a function awaits nothing else — in
///   particular never the native dialog, which can stay open for minutes;
/// * the record is written only under that lane.
#[test]
fn sy7_pairing_trust_record_lock_discipline() {
    let lines = production_lines_of("src/commands/sync_endpoint.rs");

    // 1. The static locks, by name.
    let statics: Vec<&String> = lines
        .iter()
        .filter(|l| l.contains("static ") && l.contains("Mutex"))
        .collect();
    assert_eq!(
        statics.len(),
        2,
        "SY-7: sync_endpoint.rs static locks changed: {statics:?}. A new lock needs its own \
         rule here (what may be awaited while it is held?)"
    );
    assert!(statics
        .iter()
        .any(|l| l.contains("CONFIRMED_PEERS_WRITE") && l.contains("tokio::sync::Mutex")));
    assert!(statics
        .iter()
        .any(|l| l.contains("SESSION_CONFIRMED_PEERS") && l.contains("std::sync::Mutex")));

    // 2. The std guard is never bound.
    let mut uses = 0;
    for line in &lines {
        if let Some(at) = line.find("session_confirmed_peers()") {
            if line.contains("fn session_confirmed_peers(") {
                continue;
            }
            uses += 1;
            assert!(
                line[at + "session_confirmed_peers()".len()..].starts_with('.')
                    && !line.trim_start().starts_with("let "),
                "SY-7: `session_confirmed_peers()` must be used as a temporary in one expression \
                 (never bound to a variable that could live across an .await): `{}`",
                line.trim()
            );
        }
    }
    assert!(
        uses >= 4,
        "SY-7: expected the session-set call sites, found {uses}"
    );

    // 3. After taking the record lane: no further await, no dialog.
    let lane_fns: Vec<String> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| l.contains("CONFIRMED_PEERS_WRITE.lock()"))
        .map(|(i, _)| enclosing_fn(&lines, i))
        .collect();
    assert_eq!(
        lane_fns,
        ["record_user_confirmation", "forget_confirmed_peer"],
        "SY-7: the set of functions that take the record lane changed"
    );
    for name in &lane_fns {
        let body = fn_body(&lines, name);
        let taken = body
            .iter()
            .position(|l| l.contains("CONFIRMED_PEERS_WRITE.lock()"))
            .expect("lane line");
        for line in &body[taken + 1..] {
            assert!(
                !line.contains(".await") && !line.contains(".confirm("),
                "SY-7: `{name}` awaits `{}` while holding the record lane — a native dialog or \
                 any other await under this lock stalls every pairing and unpair",
                line.trim()
            );
        }
    }

    // 4. The record is only written under the lane.
    assert_eq!(
        callers_of(&lines, "save_confirmed_peers("),
        ["forget_confirmed_peer", "record_user_confirmation"],
        "SY-7: `confirmed_sync_peers.json` must only be rewritten from a function that holds \
         CONFIRMED_PEERS_WRITE (an unlocked read-modify-write loses concurrent confirmations)"
    );
}

/// SY-Q: the store-and-forward queue client must never follow an HTTP redirect.
/// reqwest's default policy follows up to 10 hops and re-sends the body on a
/// 307/308, so a queue server (or any hop in front of it) could bounce the signed
/// auth proof, the bearer token and the pushed ciphertext to a host of its choosing.
///
/// 1. Ratchet: the only HTTP client built on the sync surface is the queue
///    client's, so a second one (which would have its own, unreviewed redirect
///    policy) fails until someone classifies it here.
/// 2. Policy: that one builder sets `Policy::none()` and nothing weakens it.
///
/// Limits: text, not behaviour. The behaviour is proven by
/// `queue::client::tests::queue_client_never_follows_a_redirect`.
#[test]
fn syq_queue_http_client_never_follows_redirects() {
    let root = manifest_dir();
    let mut files = Vec::new();
    collect_rs_files(&root.join("src/sync"), &mut files);
    for rel in SYNC_COMMAND_FILES {
        files.push(root.join(rel));
    }
    check_ratchet(
        "SY-Q HTTP client on the sync surface",
        &files,
        &["Client::builder(", "reqwest::Client::new(", "reqwest::get("],
        &BTreeMap::from([
            // The queue client's `build_http_client()`.
            ("src/sync/queue/client.rs", 1),
        ]),
        "Every HTTP client on the sync surface must refuse redirects \
         (`.redirect(reqwest::redirect::Policy::none())`). Reuse the queue client's builder, or \
         classify the new client here WITH a comment naming its redirect policy.",
    );

    let text = fs::read_to_string(root.join("src/sync/queue/client.rs")).expect("read client.rs");
    let lines = production_code_lines(&text);
    assert!(
        lines
            .iter()
            .any(|l| l.contains(".redirect(reqwest::redirect::Policy::none())")),
        "SY-Q: the queue HTTP client no longer sets `redirect(Policy::none())`"
    );
    for weaker in ["Policy::limited(", "Policy::default()", "Policy::custom("] {
        assert!(
            !lines.iter().any(|l| l.contains(weaker)),
            "SY-Q: the queue HTTP client uses `{weaker}`; it must never follow a redirect"
        );
    }
}
