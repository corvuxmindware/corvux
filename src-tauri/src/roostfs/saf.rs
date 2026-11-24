//! Android Storage Access Framework backend for [`RoostFs`].
//!
//! All operations route through [`tauri_plugin_saf`] — the Rust side owns
//! a persisted tree URI, and every call marshals to the Kotlin plugin,
//! which resolves the relative path against the tree via `DocumentFile`
//! and performs the actual IO via `ContentResolver`.
//!
//! `trash` moves into `.corvux-trash/` with the provider's own move. When the
//! provider cannot move, the copy-then-delete fallback in `super::trash` is
//! DISABLED (`COPY_THEN_DELETE_ENABLED`): the delete fails with a clear
//! message and the original is left untouched, because a write landing after
//! the final check would otherwise be lost. It is never a plain permanent
//! delete.

use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use tauri::{AppHandle, Runtime};
use tauri_plugin_saf::SafExt;

use super::trash::{
    base_name, classify_move_error, ensure_trashable, now_secs, trash_by_copy, unique_trash_rel,
    MoveFailure, RawEntry, RawTree, TRASH_DIR, TRASH_LOCK,
};
use super::{hidden_below, is_incomplete_listing, validate_rel, Entry, RoostError, RoostFs};

pub struct SafFs<R: Runtime> {
    tree_uri: String,
    /// Human-friendly label from the SAF picker (e.g. "Corvux"). Kept
    /// separately so UI can show it; the trait's `display_root()` must
    /// return the URI because that's the prefix of every FileEntry.path
    /// produced by scan_roost_backend, and `to_rel` relies on stripping
    /// exactly that prefix.
    #[allow(dead_code)]
    friendly_name: String,
    app: AppHandle<R>,
}

impl<R: Runtime> SafFs<R> {
    pub fn new(
        tree_uri: impl Into<String>,
        display_name: impl Into<String>,
        app: AppHandle<R>,
    ) -> Self {
        Self {
            tree_uri: tree_uri.into(),
            friendly_name: display_name.into(),
            app,
        }
    }
}

impl<R: Runtime> SafFs<R> {
    async fn trash_inner(&self, rel: &str, expected: &[String]) -> Result<(), RoostError> {
        // Same promise as desktop: the note or folder lands in
        // `.corvux-trash/<ts>.<name>`. Until 2026-11-05 this was a hard
        // delete, although the delete prompt said "recoverable".
        let rel = validate_rel(rel)?;
        let rel = rel.trim_end_matches('/');
        if rel.is_empty() {
            return Err(RoostError::Io("cannot trash the roost root".into()));
        }
        ensure_trashable(rel)?;
        let saf = self.app.saf();
        let stat = saf
            .stat_document(&self.tree_uri, rel)
            .map_err(map_plugin_err)?;
        if !stat.exists {
            return Err(RoostError::NotFound(rel.to_string()));
        }
        match self.mkdir(TRASH_DIR).await {
            Ok(()) | Err(RoostError::AlreadyExists(_)) => {}
            Err(e) => return Err(e),
        }
        // One trash at a time, from picking the name until the move/copy is
        // done, so a concurrent delete can't claim the same name.
        let _one_at_a_time = TRASH_LOCK.lock().await;

        // Fast path: the storage provider moves it (a folder as one unit).
        // If the name got taken after we checked, pick another; never copy
        // onto someone else's name. (`exists` reports a failed lookup as an
        // error, so a lookup failure aborts here rather than reading "free".)
        let mut attempts = 0;
        let mut cleared_stray = false;
        let dest = loop {
            let dest = unique_trash_rel(self, now_secs(), base_name(rel)).await?;
            let err = match saf.move_document(&self.tree_uri, rel, &dest) {
                Ok(()) => return Ok(()),
                Err(e) => e.to_string(),
            };
            match classify_move_error(&err) {
                MoveFailure::StagingNameTaken if !cleared_stray => {
                    // The provider moves under the item's OLD name before it
                    // renames, and `.corvux-trash/<name>` is taken: an earlier
                    // trashed item whose follow-up rename failed. Give THAT item
                    // its own unique trash name (a same-folder rename: nothing
                    // is deleted or overwritten) and try once more. Without
                    // this the disabled copy fallback would turn one stray file
                    // into a permanent refusal for every note of that name.
                    cleared_stray = true;
                    let stray = format!("{TRASH_DIR}/{}", base_name(rel));
                    let keep = unique_trash_rel(self, now_secs(), base_name(rel)).await?;
                    // A failure here is an I/O failure, never `NotFound`:
                    // `trash_in_roost` reads `NotFound` as "already gone" and
                    // would ACK a delete whose file survives.
                    self.rename(&stray, &keep).await.map_err(|e| {
                        RoostError::Io(format!("could not move the stray trash entry aside: {e}"))
                    })?;
                }
                MoveFailure::Unsupported => {
                    tracing::info!(
                        rel = %rel,
                        error = %err,
                        "saf: provider can't move into trash; the copy fallback decides (disabled unless validated)"
                    );
                    break dest;
                }
                MoveFailure::NameTaken if attempts < 3 => attempts += 1,
                MoveFailure::StagingNameTaken | MoveFailure::NameTaken | MoveFailure::Other => {
                    return Err(map_plugin_err_str(err));
                }
            }
        };

        // Fallback: copy everything (the raw tree listing keeps hidden files
        // such as `.attachments/`), PROVE the copy complete, and only then
        // delete the original. The whole sequence — and every reason it
        // refuses to delete — lives in `trash_by_copy` (roostfs/trash.rs) so
        // it is unit-tested on a desktop; the plugin calls it needs are the
        // `RawTree` impl below. A failed or partial listing, a copy that does
        // not read back identical, or a folder that changed mid-copy all
        // leave the original untouched.
        trash_by_copy(self, rel, stat.is_dir, &dest, expected).await
    }
}

fn map_plugin_err(e: tauri_plugin_saf::Error) -> RoostError {
    map_plugin_err_str(e.to_string())
}

fn map_plugin_err_str(s: String) -> RoostError {
    // Best-effort: translate the most common Kotlin-side reject messages
    // back to structured variants so callers can match on them.
    //
    // An incomplete directory listing (null cursor / provider error / still
    // loading) is checked FIRST and is always a plain I/O failure: the
    // provider's own wording may contain "not found", and `NotFound` reads as
    // "it isn't there" — uncertain access must never look like absence.
    if is_incomplete_listing(&s) {
        RoostError::Io(s)
    } else if s.contains("Already exists") {
        RoostError::AlreadyExists(s)
    } else if s.contains("Not found") || s.contains("not found") {
        RoostError::NotFound(s)
    } else if s.contains("Not a directory") {
        RoostError::NotDir(s)
    } else if s.contains("traversal") {
        RoostError::OutsideRoost(s)
    } else {
        RoostError::Io(s)
    }
}

fn decode_base64(input: &str) -> Result<Vec<u8>, RoostError> {
    B64.decode(input.as_bytes())
        .map_err(|e| RoostError::Io(format!("invalid base64 from SAF plugin: {e}")))
}

#[async_trait]
impl<R: Runtime> RoostFs for SafFs<R> {
    async fn list(&self, rel_dir: &str) -> Result<Vec<Entry>, RoostError> {
        let started = std::time::Instant::now();
        let resp = self
            .app
            .saf()
            .list_folder(&self.tree_uri, rel_dir)
            .map_err(map_plugin_err)?;
        tracing::trace!(
            rel_dir = %rel_dir,
            entries = resp.entries.len(),
            elapsed_us = started.elapsed().as_micros() as u64,
            "saf: list_folder"
        );
        Ok(resp
            .entries
            .into_iter()
            // Mirror NativeFs: hide dotfiles and our own trash. Callers
            // that need them must query via a raw SAF method (none today).
            .filter(|e| !e.name.starts_with('.') && e.name != ".corvux-trash")
            .map(|e| Entry {
                name: e.name,
                rel_path: e.rel_path,
                is_dir: e.is_dir,
            })
            .collect())
    }

    async fn list_recursive(&self, rel_dir: &str) -> Result<Vec<Entry>, RoostError> {
        // SAF override: one JNI round-trip walks every descendant. The
        // Kotlin side also warms its DocumentFile cache for each entry,
        // so the parallel-read phase that follows in `walk.rs` skips the
        // per-file `findFile` resolution chain entirely.
        let started = std::time::Instant::now();
        let resp = self
            .app
            .saf()
            .list_tree(&self.tree_uri, rel_dir)
            .map_err(map_plugin_err)?;
        tracing::info!(
            rel_dir = %rel_dir,
            entries = resp.entries.len(),
            elapsed_ms = started.elapsed().as_millis() as u64,
            "saf: list_tree (bulk walk)"
        );
        Ok(resp
            .entries
            .into_iter()
            // Same contract as `list` + the default recursion: hidden
            // entries AND everything inside hidden folders are dropped
            // (`.corvux-trash/` holds deleted notes). The raw walk returns
            // descendants of hidden folders too, so a name-only check
            // leaked them.
            .filter(|e| !hidden_below(rel_dir, &e.rel_path))
            .map(|e| Entry {
                name: e.name,
                rel_path: e.rel_path,
                is_dir: e.is_dir,
            })
            .collect())
    }

    async fn read(&self, rel: &str) -> Result<Vec<u8>, RoostError> {
        let started = std::time::Instant::now();
        let resp = self
            .app
            .saf()
            .read_document(&self.tree_uri, rel)
            .map_err(map_plugin_err)?;
        let bytes = decode_base64(&resp.base64)?;
        tracing::trace!(
            rel = %rel,
            bytes = bytes.len(),
            elapsed_us = started.elapsed().as_micros() as u64,
            "saf: read_document"
        );
        Ok(bytes)
    }

    async fn write_atomic(&self, rel: &str, bytes: &[u8]) -> Result<(), RoostError> {
        let b64 = B64.encode(bytes);
        self.app
            .saf()
            .write_document(&self.tree_uri, rel, b64, false)
            .map_err(map_plugin_err)
    }

    async fn create(&self, rel: &str, initial: &[u8]) -> Result<(), RoostError> {
        let b64 = B64.encode(initial);
        self.app
            .saf()
            .write_document(&self.tree_uri, rel, b64, true)
            .map_err(map_plugin_err)
    }

    async fn trash(&self, rel: &str) -> Result<(), RoostError> {
        self.trash_inner(rel, &[]).await
    }

    async fn trash_expecting(&self, rel: &str, expected: &[String]) -> Result<(), RoostError> {
        self.trash_inner(rel, expected).await
    }

    async fn rename(&self, from_rel: &str, to_rel: &str) -> Result<(), RoostError> {
        self.app
            .saf()
            .rename_document(&self.tree_uri, from_rel, to_rel)
            .map_err(map_plugin_err)
    }

    async fn mkdir(&self, rel_dir: &str) -> Result<(), RoostError> {
        self.app
            .saf()
            .mkdir(&self.tree_uri, rel_dir)
            .map_err(map_plugin_err)
    }

    async fn rmdir(&self, _rel_dir: &str) -> Result<(), RoostError> {
        // Deliberately unsupported on SAF. The plugin's delete is RECURSIVE, so
        // "list, see it empty, delete" would be an ungated destructive step with
        // the same race the trash fallback is disabled for: a file another app
        // writes in between is lost. Nothing in production calls `rmdir`.
        Err(RoostError::Io(
            "removing a folder is not supported on this storage location".into(),
        ))
    }

    async fn exists(&self, rel: &str) -> Result<bool, RoostError> {
        // A lookup that FAILED is not "absent". This used to read every plugin
        // error as `exists: false`, and `recover_missing_files` (which runs at
        // startup) then wrote the stored sync copy over a note that was there.
        // Kotlin now answers `exists: false` only after a checked listing of
        // the parent confirms the name is not in it (`findChildVerified`) and
        // REJECTS otherwise, so an `Err` here means "could not tell"; callers
        // treat it as such (skip, never create/overwrite/delete).
        self.app
            .saf()
            .stat_document(&self.tree_uri, rel)
            .map(|resp| resp.exists)
            .map_err(map_plugin_err)
    }

    fn display_root(&self) -> &str {
        &self.tree_uri
    }

    fn root_id(&self) -> String {
        self.tree_uri.clone()
    }
}

/// The raw tree access and removal `trash_by_copy` needs (see `super::trash`).
#[async_trait]
impl<R: Runtime> RawTree for SafFs<R> {
    async fn raw_descendants(&self, rel: &str) -> Result<Vec<RawEntry>, RoostError> {
        // Unfiltered on purpose: the copy must include hidden files such as
        // `.attachments/`. The plugin REJECTS (never returns a shorter list)
        // when any directory could not be read completely — see
        // `DirectoryListing.kt` / `queryChildrenChecked`.
        Ok(self
            .app
            .saf()
            .list_tree(&self.tree_uri, rel)
            .map_err(map_plugin_err)?
            .entries
            .into_iter()
            .map(|e| RawEntry {
                entry: Entry {
                    name: e.name,
                    rel_path: e.rel_path,
                    is_dir: e.is_dir,
                },
                size: e.size,
                modified: e.modified,
            })
            .collect())
    }

    async fn remove_original(&self, rel: &str) -> Result<(), RoostError> {
        self.app
            .saf()
            .delete_document(&self.tree_uri, rel)
            .map_err(map_plugin_err)
    }

    async fn remove_partial_copy(&self, rel: &str) {
        if let Err(cleanup) = self.app.saf().delete_document(&self.tree_uri, rel) {
            tracing::debug!(dest = %rel, error = %cleanup, "saf: rejected trash copy not removed");
        }
    }
}
