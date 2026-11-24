//! Guard for the Android (SAF) delete contract. Pure file reads — runs on any
//! host, no Android toolchain needed.
//!
//! The delete prompt tells the user a deleted note or folder goes to
//! `.corvux-trash/` and can be restored. On Android user-picked folders
//! (`SafFs`) `trash()` used to call `delete_document` straight away — a
//! permanent delete (found 2026-11-05). It must move into the trash (the
//! provider's `moveDocument`), fall back to a full copy, and only then delete
//! the original. The copy/naming logic itself is unit-tested in
//! `roostfs::trash` against a real folder.

use std::fs;
use std::path::PathBuf;

fn read(rel: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// Body of `SafFs::trash_inner` (what both `trash` and `trash_expecting` run),
/// up to the end of that method.
fn saf_trash_body() -> String {
    let src = read("src/roostfs/saf.rs");
    let start = src
        .find("async fn trash_inner(")
        .expect("SafFs must implement trash_inner()");
    let rest = &src[start..];
    let end = rest.find("\n    }\n").expect("end of trash_inner");
    rest[..end].to_string()
}

#[test]
fn saf_trash_moves_before_it_deletes() {
    let body = saf_trash_body();
    let mv = body
        .find("move_document(")
        .expect("SafFs::trash must try the provider move into .corvux-trash first");
    let copy = body.find("trash_by_copy(").expect(
        "SafFs::trash must fall back to the verified copy-then-delete (roostfs::trash::trash_by_copy)",
    );
    assert!(
        mv < copy,
        "SafFs::trash must try the provider move before copying"
    );
    // The only removal of an original lives at the end of `trash_by_copy`,
    // behind its verification. A direct delete here would bypass the proof.
    assert!(
        !body.contains("delete_document("),
        "SafFs::trash must not delete anything itself: trash_by_copy removes the original only after the copy is verified"
    );
    assert!(
        body.contains("unique_trash_rel("),
        "SafFs::trash must pick a free trash name (shared helper)"
    );
    // Code review 2026-11-05: without these, two concurrent same-named
    // deletes could pick one name, and the failed copy's cleanup deleted the
    // OTHER note's trashed copy.
    assert!(
        body.contains("TRASH_LOCK.lock()"),
        "SafFs::trash must hold TRASH_LOCK while picking the name and moving"
    );
    assert!(
        body.contains("ensure_trashable("),
        "SafFs::trash must refuse the trash folder itself"
    );
}

/// The destructive sequence of the copy fallback, in order. Each step is what
/// stops one way of deleting a folder whose copy is not complete:
/// a checked listing, the independently discovered notes all being in the
/// copy (a provider that is consistently wrong agrees with itself), a copy that
/// reads back identical, a source that still matches what was copied, and a
/// folder that lists the same AFTER the copy.
#[test]
fn copy_verify_then_remove_proves_the_copy_before_removing_the_original() {
    let src = read("src/roostfs/trash.rs");
    let start = src
        .find("pub(crate) async fn copy_verify_then_remove")
        .expect("roostfs::trash::copy_verify_then_remove");
    let body = &src[start..start + src[start..].find("\n}\n").expect("end of the fn")];

    let first_list = body.find("raw_descendants(").expect("checked listing");
    let copy = body.find("copy_to(").expect("copy");
    let cover = body
        .find("verify_manifest_covers(")
        .expect("expected-notes cross-check");
    let content = body
        .find("verify_copied_content(")
        .expect("content verification");
    let last_list = body.rfind("raw_descendants(").expect("re-listing");
    let listing = body
        .find("verify_same_listing(")
        .expect("listing comparison");
    let remove = body.find("remove_original(").expect("original removal");
    assert!(
        first_list < copy && copy < cover && cover < content && content < last_list && last_list < listing,
        "order must be: list, copy, cross-check expected notes, verify content, re-list, compare listings"
    );
    assert!(
        listing < remove,
        "the original may only be removed after every verification"
    );
    assert_eq!(
        body.matches("remove_original(").count(),
        1,
        "exactly one removal of the original, and it is the last step"
    );
    assert!(
        body.contains("dest_claimed"),
        "may only delete a partial copy it created (CopyError::dest_claimed)"
    );
}

/// An error message does not protect a write that lands after the final check,
/// and Corvux's own lock cannot stop ANOTHER app writing to a shared folder. So
/// the copy-then-delete fallback is disabled: on a provider that cannot move,
/// deleting fails and the original stays. Flipping the switch needs recorded
/// device evidence (see the validation doc), not just a code change.
#[test]
fn copy_then_delete_fallback_stays_disabled_until_validated() {
    let src = read("src/roostfs/trash.rs");
    let line = src
        .lines()
        .find(|l| l.contains("const COPY_THEN_DELETE_ENABLED"))
        .expect("COPY_THEN_DELETE_ENABLED");
    // Only the literal `= false;` counts as disabled. `= !false;`, a function
    // call or any other expression is treated as ENABLED and must then come
    // with the recorded device evidence.
    let enabled = !line.contains("= false;");
    if enabled {
        let doc = std::fs::read_to_string(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../docs/validation/android-saf-validation-2026-11-07.md"),
        )
        .expect("the validation doc must exist before the fallback can be enabled");
        // The WHOLE LINE must be the status: the runbook's prose quotes this
        // exact phrase when it explains the rule, so a substring match is
        // vacuous (found by mutation: `= !false;` slipped through).
        assert!(
            doc.lines()
                .any(|l| l.trim() == "Android validation status: COMPLETE"),
            "COPY_THEN_DELETE_ENABLED = true needs `Android validation status: COMPLETE` in docs/validation/android-saf-validation-2026-11-07.md, with the device evidence"
        );
    }
    // The gate must precede the copy, whatever the flag says.
    let start = src
        .find("pub(crate) async fn trash_by_copy")
        .expect("trash_by_copy");
    let body = &src[start..start + src[start..].find("\n}\n").expect("end of trash_by_copy")];
    let gate = body
        .find("COPY_THEN_DELETE_ENABLED")
        .expect("gate in trash_by_copy");
    let work = body.find("copy_verify_then_remove(").expect("delegation");
    assert!(
        gate < work,
        "trash_by_copy must check the gate before doing anything"
    );
    // SafFs must go through the gated entry point, never the raw sequence.
    let saf = saf_trash_body();
    assert!(
        saf.contains("trash_by_copy(") && !saf.contains("copy_verify_then_remove("),
        "SafFs::trash_inner must call the gated trash_by_copy"
    );
}

#[test]
fn kotlin_plugin_exposes_move_document() {
    let kt = read("plugins/tauri-plugin-saf/android/src/main/java/SafPlugin.kt");
    assert!(
        kt.contains("fun moveDocument(") && kt.contains("DocumentsContract.moveDocument("),
        "SafPlugin.kt must keep the moveDocument command used by the trash"
    );
    let rs = read("plugins/tauri-plugin-saf/src/lib.rs");
    assert!(
        rs.contains("\"moveDocument\""),
        "the plugin's Rust wrapper must call the Kotlin moveDocument command"
    );
}

#[test]
fn native_trash_uses_the_shared_unique_name() {
    let src = read("src/roostfs/native.rs");
    let start = src.find("async fn trash(").expect("NativeFs::trash");
    let body = &src[start..start + src[start..].find("async fn rename(").unwrap()];
    assert!(
        body.contains("unique_trash_rel("),
        "NativeFs::trash must use the shared unique trash name (same-second clash overwrote files)"
    );
    assert!(
        body.contains("TRASH_LOCK.lock()") && body.contains("ensure_trashable("),
        "NativeFs::trash must hold TRASH_LOCK and refuse the trash folder itself"
    );
}

/// Deleted notes now live in `.corvux-trash/`, so the Android bulk walk must
/// drop everything INSIDE hidden folders, not just entries whose own name
/// starts with a dot — or trashed notes reappear in the tree, graph, search
/// and sync seeding.
#[test]
fn saf_bulk_walk_hides_whole_hidden_folders() {
    let src = read("src/roostfs/saf.rs");
    let start = src
        .find("async fn list_recursive(")
        .expect("SafFs::list_recursive");
    let body = &src[start..start + src[start..].find("async fn read(").unwrap()];
    assert!(
        body.contains("hidden_below("),
        "SafFs::list_recursive must filter with roostfs::hidden_below"
    );
}

/// Cached SAF entries are `SingleDocumentFile`s (warmed by `walkTreeFast`),
/// and `SingleDocumentFile.renameTo/listFiles/findFile` throw
/// UnsupportedOperationException. `doc.renameTo` made every note rename on
/// an Android folder roost fail, and `dstParent.findFile` silently pushed
/// folder deletes onto the slow copy path (both found on-device 2026-11-05).
/// Rename through `DocumentsContract`, and check names by walking from the
/// tree root (`resolveRelative`).
#[test]
fn kotlin_never_calls_unsupported_ops_on_cached_documents() {
    let kt = read("plugins/tauri-plugin-saf/android/src/main/java/SafPlugin.kt");
    assert!(
        !kt.contains(".renameTo("),
        "use DocumentsContract.renameDocument(resolver, doc.uri, name) — cached SingleDocumentFile.renameTo throws"
    );
    for (i, line) in kt.lines().enumerate() {
        let code = line.split("//").next().unwrap_or("");
        assert!(
            !(code.contains("dstParent.findFile(") || code.contains("srcParent.findFile(")),
            "SafPlugin.kt:{}: findFile on a cached document throws; walk from the root instead",
            i + 1
        );
    }
}

/// Strip a Kotlin line's trailing `//` comment, and skip KDoc/block-comment
/// lines entirely, so only executable text is inspected.
fn kotlin_code_lines(kt: &str) -> Vec<(usize, String)> {
    kt.lines()
        .enumerate()
        .filter_map(|(i, line)| {
            let t = line.trim_start();
            if t.starts_with("//") || t.starts_with("/*") || t.starts_with('*') {
                return None;
            }
            Some((i + 1, line.split("//").next().unwrap_or("").to_string()))
        })
        .collect()
}

/// Class guard for "an incomplete directory read must be an ERROR, not a
/// smaller listing" (data-safety plan, Batch 1). Every children query goes
/// through `queryChildrenChecked`; a bare `query(...)?.use { }` reads a null
/// cursor as an empty directory, and `listFolder` / the recursive walk fed
/// that to the tree cache and to the SAF trash copy-then-delete fallback.
#[test]
fn kotlin_directory_queries_go_through_the_checked_helper() {
    let kt = read("plugins/tauri-plugin-saf/android/src/main/java/SafPlugin.kt");
    let code = kotlin_code_lines(&kt);

    let queries: Vec<usize> = code
        .iter()
        .filter(|(_, l)| l.contains("contentResolver.query("))
        .map(|(n, _)| *n)
        .collect();
    assert_eq!(
        queries.len(),
        1,
        "exactly one contentResolver.query( (inside queryChildrenChecked); found at lines {queries:?}"
    );
    for (n, l) in &code {
        assert!(
            !(l.contains("query(") && l.contains("?.use")),
            "SafPlugin.kt:{n}: `query(...)?.use` reads a null cursor as an empty directory"
        );
    }

    let helper = kt
        .find("private fun queryChildrenChecked(")
        .expect("queryChildrenChecked helper");
    let helper_body = &kt[helper..helper + kt[helper..].find("\n    }\n").expect("helper end")];
    assert!(
        helper_body.contains("contentResolver.query(")
            && helper_body.contains("EXTRA_LOADING")
            && helper_body.contains("EXTRA_ERROR")
            && helper_body.contains("classifyDirCursor("),
        "the helper must run the one query and check loading/error extras via classifyDirCursor"
    );

    for func in ["fun listFolder(", "private fun walkTreeFast("] {
        let at = kt.find(func).unwrap_or_else(|| panic!("{func} missing"));
        let rest = &kt[at..];
        let end = rest[1..]
            .find("\n    @Command")
            .or_else(|| rest[1..].find("\n    private fun "))
            .map(|i| i + 1)
            .unwrap_or(rest.len());
        assert!(
            rest[..end].contains("queryChildrenChecked("),
            "{func} must list children through queryChildrenChecked"
        );
    }
}

/// The Kotlin rejection prefix and the Rust matcher are one string in two
/// languages; if they drift, an incomplete listing is classified by the
/// generic substring guess ("not found" -> NotFound) and reads as absence.
#[test]
fn kotlin_and_rust_agree_on_the_incomplete_listing_prefix() {
    fn quoted(src: &str, marker: &str) -> String {
        let line = src
            .lines()
            .find(|l| l.contains(marker) && l.contains('"'))
            .unwrap_or_else(|| panic!("no line with {marker}"));
        let first = line.find('"').unwrap() + 1;
        let len = line[first..].find('"').unwrap();
        line[first..first + len].to_string()
    }
    let kt = read("plugins/tauri-plugin-saf/android/src/main/java/DirectoryListing.kt");
    let rs = read("src/roostfs/mod.rs");
    let k = quoted(&kt, "const val INCOMPLETE_LISTING_PREFIX");
    let r = quoted(&rs, "const INCOMPLETE_LISTING_PREFIX");
    assert_eq!(k, r, "Kotlin and Rust INCOMPLETE_LISTING_PREFIX must match");
    assert!(!k.is_empty());
}

/// `map_plugin_err_str` must classify an incomplete listing BEFORE its
/// substring guesses: a provider's own message can contain "not found".
#[test]
fn saf_error_mapping_checks_incomplete_listing_first() {
    let src = read("src/roostfs/saf.rs");
    let start = src
        .find("fn map_plugin_err_str(")
        .expect("map_plugin_err_str");
    let body = &src[start..start + src[start..].find("\n}\n").expect("end of fn")];
    let incomplete = body
        .find("is_incomplete_listing(")
        .expect("map_plugin_err_str must check is_incomplete_listing");
    let first_guess = body.find("s.contains(").expect("substring guesses");
    assert!(
        incomplete < first_guess,
        "is_incomplete_listing must run before any substring classification"
    );
}

/// A listing row with no name or id must fail the listing, not be skipped:
/// the SAF trash fallback copies what the walk reports and then deletes the
/// original folder, so a silently dropped row is deleted with its parent
/// (audit of the Batch 1 fix, finding 2).
#[test]
fn kotlin_listings_never_skip_unnamed_rows() {
    let kt = read("plugins/tauri-plugin-saf/android/src/main/java/SafPlugin.kt");
    for (n, l) in kotlin_code_lines(&kt) {
        let reads_row_key = l.contains("getString(nameIdx)") || l.contains("getString(idIdx)");
        assert!(
            !(reads_row_key && l.contains("?: continue")),
            "SafPlugin.kt:{n}: a row without a name/id must throw INCOMPLETE_LISTING_PREFIX, not `?: continue`"
        );
    }
    // The throw must be the replacement, in both listings.
    assert!(
        kt.matches("unnamed row in").count() >= 2
            && kt.matches("row without an id in").count() >= 2,
        "listFolder and walkTreeFast must both reject unnamed / id-less rows"
    );
}

/// "Could not tell" is not "absent". Every child-by-name lookup in the plugin
/// goes through `findChildVerified`: raw `findFile` swallows provider failures
/// into null, which `statDocument` reported as `exists: false` and Rust's
/// startup recovery then overwrote a real note over.
#[test]
fn kotlin_child_lookup_goes_through_the_verified_helper() {
    let kt = read("plugins/tauri-plugin-saf/android/src/main/java/SafPlugin.kt");
    let code = kotlin_code_lines(&kt);
    let raw: Vec<usize> = code
        .iter()
        .filter(|(_, l)| l.contains(".findFile("))
        .map(|(n, _)| *n)
        .collect();
    assert_eq!(
        raw.len(),
        2,
        "`.findFile(` may appear only inside findChildVerified (twice); found at lines {raw:?}"
    );
    let helper = kt
        .find("private fun findChildVerified(")
        .expect("findChildVerified");
    let helper_end = helper + kt[helper..].find("\n    }\n").expect("helper end");
    let helper_lines = (kt[..helper].lines().count() + 1)..=kt[..helper_end].lines().count();
    for n in &raw {
        assert!(
            helper_lines.contains(n),
            "SafPlugin.kt:{n}: `.findFile(` outside findChildVerified"
        );
    }
    assert!(
        kt[helper..helper_end].contains("childListedChecked("),
        "the helper must confirm a null with the checked listing"
    );
    let callers = code
        .iter()
        .filter(|(_, l)| l.contains("findChildVerified("))
        .count();
    assert!(
        callers >= 5,
        "definition + 4 call sites (resolveRelative, writeDocument x2, mkdir), found {callers}"
    );
}

/// `SafFs::exists` must report a failed lookup as an error. It used to map
/// every plugin error to `Ok(false)`.
#[test]
fn saf_exists_propagates_provider_errors() {
    let src = read("src/roostfs/saf.rs");
    let start = src
        .find("async fn exists(&self, rel: &str)")
        .expect("SafFs::exists");
    let body = &src[start..start + src[start..].find("\n    }\n").expect("end of exists")];
    assert!(
        !body.contains("Ok(false)") && !body.contains("unwrap_or(false)"),
        "SafFs::exists must not turn a plugin error into `false` (absent)"
    );
    assert!(body.contains("map_plugin_err"), "errors must be propagated");
}

/// Startup recovery is CREATE-ONLY. `recover_missing_files` decides "missing"
/// from a probe that can be wrong; an overwrite replaced real notes (and edits
/// made in other apps) with the stale stored copy.
#[test]
fn recovery_of_missing_files_is_create_only() {
    let src = read("src/sync/engine/scan.rs");
    let start = src
        .find("pub async fn recover_missing_files")
        .expect("recover_missing_files");
    let body = &src[start..start + src[start..].find("\n    }\n").expect("end of fn")];
    assert!(
        body.contains("create_recovered_note("),
        "recover_missing_files must write with create_recovered_note (create-only)"
    );
    assert!(
        !body.contains("write_recovered_note(") && !body.contains("write_atomic("),
        "recover_missing_files must never overwrite"
    );
    // And the shared writer must use `create` for the create-only mode.
    let inner = src
        .find("async fn write_recovered_inner(")
        .expect("write_recovered_inner");
    let inner_body = &src[inner..inner + src[inner..].find("\n    }\n").expect("end")];
    assert!(
        inner_body.contains("fs.create(") && inner_body.contains("create_only"),
        "create-only mode must call RoostFs::create"
    );
}

/// Desktop twin: `Path::exists()` is false for EACCES/EIO/a lost iOS grant. The
/// decisions that gate an overwrite (`create`, `rename`'s target, `exists`) use
/// `exists_checked`.
#[test]
fn native_overwrite_gating_uses_the_checked_existence_helper() {
    let src = read("src/roostfs/native.rs");
    let prod = &src[..src.find("#[cfg(test)]\nmod tests {").expect("tests mod")];
    assert!(
        !prod.contains("if target.exists()") && !prod.contains("if to.exists()"),
        "create/rename must not gate on Path::exists()"
    );
    assert!(
        prod.matches("exists_checked(").count() >= 5,
        "exists, exists_blocking, create and rename must use exists_checked (+ its definition)"
    );
}

/// The ONLY way an original is removed on SAF is the last step of the gated
/// `copy_verify_then_remove`. Pin the callers: a new `remove_original(` call
/// anywhere else is an ungated destructive step (the audit found `rmdir` was
/// one).
#[test]
fn remove_original_has_exactly_the_gated_callers() {
    fn prod(src: &str) -> &str {
        &src[..src.find("#[cfg(test)]").unwrap_or(src.len())]
    }
    let trash = read("src/roostfs/trash.rs");
    let saf = read("src/roostfs/saf.rs");
    let t = prod(&trash);
    // trait declaration + the single call in copy_verify_then_remove
    assert_eq!(
        t.matches("remove_original(").count(),
        2,
        "trash.rs: remove_original may be declared once and called once (in copy_verify_then_remove)"
    );
    // the SafFs impl only
    assert_eq!(
        prod(&saf).matches("remove_original(").count(),
        1,
        "saf.rs: only `impl RawTree for SafFs` may define remove_original; nothing may call it"
    );
    for dir in ["src/commands", "src/sync", "src/ai"] {
        // Pure-Rust equivalent of `grep -rl "remove_original(" <dir>`:
        // the external binary does not exist on Windows hosts, and this
        // guard must run on every host that can run the gate.
        fn files_mentioning(needle: &str, dir: &std::path::Path) -> Vec<String> {
            let mut hits = Vec::new();
            let mut stack = vec![dir.to_path_buf()];
            while let Some(current) = stack.pop() {
                for entry in std::fs::read_dir(&current).expect("read_dir") {
                    let entry = entry.expect("read_dir entry");
                    let path = entry.path();
                    if path.is_dir() {
                        stack.push(path);
                    } else if path.extension().is_some_and(|e| e == "rs") {
                        let src = std::fs::read_to_string(&path).expect("read source file");
                        if src.contains(needle) {
                            hits.push(path.display().to_string());
                        }
                    }
                }
            }
            hits
        }
        let hits = files_mentioning(
            "remove_original(",
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(dir),
        );
        assert!(
            hits.is_empty(),
            "{dir} must not call remove_original: {hits:?}"
        );
    }
}

/// `SafFs::rmdir` used to list-then-delete recursively: an ungated destructive
/// step with the race the trash fallback is disabled for. It refuses now.
#[test]
fn saf_rmdir_is_not_a_delete() {
    let src = read("src/roostfs/saf.rs");
    let start = src.find("async fn rmdir(").expect("SafFs::rmdir");
    let body = &src[start..start + src[start..].find("\n    }\n").expect("end of rmdir")];
    assert!(
        !body.contains("delete_document(") && !body.contains("remove_original("),
        "SafFs::rmdir must not delete anything"
    );
    assert!(body.contains("Err("), "SafFs::rmdir must refuse");
}

/// Providers do not fail a create on a name clash; they store "name (1).ext".
/// Every create in the plugin goes through `snapshotBeforeCreate` (refuse a
/// case-variant sibling, remember the existing ids) and `settleCreated` (after
/// the create, reject a renamed result and remove it ONLY if it is provably the
/// document this call just made).
#[test]
fn kotlin_creates_go_through_the_name_check() {
    let kt = read("plugins/tauri-plugin-saf/android/src/main/java/SafPlugin.kt");
    let code = kotlin_code_lines(&kt);
    let raw: Vec<usize> = code
        .iter()
        .filter(|(_, l)| l.contains(".createFile(") || l.contains(".createDirectory("))
        .map(|(n, _)| *n)
        .collect();
    assert_eq!(
        raw.len(),
        2,
        "raw createFile/createDirectory only inside createFileChecked/createDirectoryChecked; found at lines {raw:?}"
    );
    for name in [
        "private fun createFileChecked(",
        "private fun createDirectoryChecked(",
    ] {
        let at = kt.find(name).unwrap_or_else(|| panic!("{name} missing"));
        let body = &kt[at..at + kt[at..].find("\n    }\n").expect("end")];
        let snap = body
            .find("snapshotBeforeCreate(")
            .unwrap_or_else(|| panic!("{name} must snapshot the parent first"));
        let create = body.find(".create").expect("the create itself");
        let settle = body
            .find("settleCreated(")
            .unwrap_or_else(|| panic!("{name} must verify the stored name"));
        assert!(
            snap < create && create < settle,
            "{name}: order must be snapshot, create, settle"
        );
    }
    let snap = kt
        .find("private fun snapshotBeforeCreate(")
        .expect("snapshotBeforeCreate");
    let sb = &kt[snap..snap + kt[snap..].find("\n    }\n").expect("end")];
    assert!(
        sb.contains("ignoreCase = true") && sb.contains("Already exists"),
        "a case-variant sibling must be refused as `Already exists`, before creating anything"
    );
}

/// `settleCreated` may delete only a document that is PROVABLY the one this
/// call just made. A provider that breaks the contract can return an EXISTING
/// document on a name clash (the existing folder, say), and deleting that
/// destroys user data (round-3 audit).
#[test]
fn kotlin_name_check_never_deletes_a_preexisting_document() {
    let kt = read("plugins/tauri-plugin-saf/android/src/main/java/SafPlugin.kt");
    let at = kt
        .find("private fun settleCreated(")
        .expect("settleCreated");
    let body = &kt[at..at + kt[at..].find("\n    }\n").expect("end")];
    let delete = body.find(".delete()").expect("the removal");
    let ours = body.find("val ours").expect("a provenance check");
    assert!(
        ours < delete,
        "the provenance check must precede the delete"
    );
    let ours_expr = &body[ours..delete];
    assert!(
        ours_expr.contains("!in before")
            || ours_expr.contains("!in(before)")
            || ours_expr.contains("!in before)"),
        "removal requires the id to be ABSENT from the pre-create listing"
    );
    assert!(
        ours_expr.contains("hasNoChildrenChecked(") && ours_expr.contains("length() == 0L"),
        "removal requires an empty folder / zero-length file"
    );
    // The delete must sit inside the `if (ours)` branch and nowhere else.
    assert_eq!(
        body.matches(".delete()").count(),
        1,
        "exactly one delete in settleCreated"
    );
    let guard = body.find("if (ours)").expect("if (ours)");
    assert!(guard < delete, "the delete must be inside `if (ours)`");
}

/// The staging-collision marker is one string in two languages. If they drift,
/// a stray `.corvux-trash/<name>` is classified as "provider cannot move" and,
/// with the copy fallback disabled, blocks every delete of that name.
#[test]
fn kotlin_and_rust_agree_on_the_staging_marker() {
    fn quoted(src: &str, marker: &str) -> String {
        let line = src
            .lines()
            .find(|l| l.contains(marker) && l.contains('"'))
            .unwrap_or_else(|| panic!("no line with {marker}"));
        let first = line.find('"').unwrap() + 1;
        let len = line[first..].find('"').unwrap();
        line[first..first + len].to_string()
    }
    let kt = read("plugins/tauri-plugin-saf/android/src/main/java/DirectoryListing.kt");
    let rs = read("src/roostfs/mod.rs");
    let k = quoted(&kt, "const val STAGING_NAME_TAKEN_PREFIX");
    let r = quoted(&rs, "const STAGING_NAME_TAKEN_PREFIX");
    assert_eq!(k, r, "Kotlin and Rust STAGING_NAME_TAKEN_PREFIX must match");
    assert!(!k.is_empty());
    // And Kotlin's moveDocument must actually use the marker, not the old phrase.
    let sp = read("plugins/tauri-plugin-saf/android/src/main/java/SafPlugin.kt");
    let at = sp.find("fun moveDocument(").expect("moveDocument");
    let body = &sp[at..at + sp[at..].find("\n    }\n").expect("end")];
    assert!(
        body.contains("STAGING_NAME_TAKEN_PREFIX")
            && !body.contains("Move not supported: $oldName already in"),
        "moveDocument must report the staging collision under STAGING_NAME_TAKEN_PREFIX"
    );
}

/// The children query itself can throw (provider died, lost grant). Its
/// wording must not reach Rust unprefixed, where a "not found" substring is
/// read as NotFound — i.e. absence.
#[test]
fn kotlin_children_query_is_wrapped_so_a_throw_stays_an_incomplete_listing() {
    let kt = read("plugins/tauri-plugin-saf/android/src/main/java/SafPlugin.kt");
    let at = kt
        .find("private fun queryChildrenChecked(")
        .expect("helper");
    let body = &kt[at..at + kt[at..].find("\n    }\n").expect("end")];
    let try_at = body.find("try {").expect("try around the query");
    let q = body.find("contentResolver.query(").expect("the query");
    assert!(try_at < q, "the query must run inside a try block");
    assert!(
        body[q..].contains("catch (e: Exception)")
            && body[q..].contains("INCOMPLETE_LISTING_PREFIX"),
        "a throwing query must become an INCOMPLETE_LISTING_PREFIX error"
    );
}

/// With the copy fallback disabled, "Move not supported" is a permanent
/// refusal. Our own staging-name pre-check must therefore be told apart from a
/// provider that cannot move, and the stray cleared with a same-folder rename.
#[test]
fn stray_staging_name_is_cleared_not_refused() {
    let saf = read("src/roostfs/saf.rs");
    let start = saf.find("async fn trash_inner(").expect("trash_inner");
    let body = &saf[start..start + saf[start..].find("\n    }\n").expect("end")];
    let arm = body
        .find("MoveFailure::StagingNameTaken if")
        .expect("trash_inner must handle StagingNameTaken before Unsupported");
    let unsupported = body
        .find("MoveFailure::Unsupported =>")
        .expect("Unsupported arm");
    assert!(
        arm < unsupported,
        "StagingNameTaken must be matched before Unsupported"
    );
    assert!(
        body[arm..unsupported].contains("self.rename("),
        "the stray must be renamed aside (a same-folder rename), never deleted"
    );
    assert!(
        !body[arm..unsupported].contains("delete_document("),
        "clearing the stray must not delete anything"
    );
}
