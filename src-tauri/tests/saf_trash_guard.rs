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

/// Body of `async fn trash` in the SAF backend, up to the next method.
fn saf_trash_body() -> String {
    let src = read("src/roostfs/saf.rs");
    let start = src
        .find("async fn trash(")
        .expect("SafFs must implement trash()");
    let rest = &src[start..];
    let end = rest[1..]
        .find("async fn ")
        .map(|i| i + 1)
        .unwrap_or(rest.len());
    rest[..end].to_string()
}

#[test]
fn saf_trash_moves_before_it_deletes() {
    let body = saf_trash_body();
    let mv = body
        .find("move_document(")
        .expect("SafFs::trash must try the provider move into .corvux-trash first");
    let copy = body
        .find("copy_to(")
        .expect("SafFs::trash must fall back to copying into .corvux-trash");
    let first_delete = body
        .find("delete_document(")
        .expect("SafFs::trash removes the original after the copy");
    assert!(
        mv < first_delete && copy < first_delete,
        "SafFs::trash must not delete anything before the move/copy into the trash"
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
        body.contains("dest_claimed"),
        "SafFs::trash may only delete a partial copy it created (CopyError::dest_claimed)"
    );
    assert!(
        body.contains("ensure_trashable("),
        "SafFs::trash must refuse the trash folder itself"
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
