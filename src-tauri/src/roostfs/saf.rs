//! Android Storage Access Framework backend for [`RoostFs`].
//!
//! All operations route through [`tauri_plugin_saf`] — the Rust side owns
//! a persisted tree URI, and every call marshals to the Kotlin plugin,
//! which resolves the relative path against the tree via `DocumentFile`
//! and performs the actual IO via `ContentResolver`.
//!
//! `trash` moves into `.corvux-trash/` with the provider's own move and
//! falls back to copy + delete (see `super::trash`); it is never a plain
//! permanent delete.

use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use tauri::{AppHandle, Runtime};
use tauri_plugin_saf::SafExt;

use super::trash::{
    base_name, classify_move_error, copy_to, ensure_trashable, now_secs, unique_trash_rel,
    MoveFailure, TRASH_DIR, TRASH_LOCK,
};
use super::{hidden_below, validate_rel, Entry, RoostError, RoostFs};

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

fn map_plugin_err(e: tauri_plugin_saf::Error) -> RoostError {
    map_plugin_err_str(e.to_string())
}

fn map_plugin_err_str(s: String) -> RoostError {
    // Best-effort: translate the most common Kotlin-side reject messages
    // back to structured variants so callers can match on them.
    if s.contains("Already exists") {
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
        // If the name got taken after we checked (`exists` reads a SAF error
        // as "free"), pick another; never copy onto someone else's name.
        let mut attempts = 0;
        let dest = loop {
            let dest = unique_trash_rel(self, now_secs(), base_name(rel)).await?;
            let err = match saf.move_document(&self.tree_uri, rel, &dest) {
                Ok(()) => return Ok(()),
                Err(e) => e.to_string(),
            };
            match classify_move_error(&err) {
                MoveFailure::Unsupported => {
                    tracing::info!(
                        rel = %rel,
                        error = %err,
                        "saf: provider can't move into trash, copying instead"
                    );
                    break dest;
                }
                MoveFailure::NameTaken if attempts < 3 => attempts += 1,
                MoveFailure::NameTaken | MoveFailure::Other => {
                    return Err(map_plugin_err_str(err));
                }
            }
        };

        // Fallback: copy everything (the raw tree listing keeps hidden
        // files such as `.attachments/`), and delete the original only
        // after the whole copy succeeded.
        let descendants: Vec<Entry> = if stat.is_dir {
            saf.list_tree(&self.tree_uri, rel)
                .map_err(map_plugin_err)?
                .entries
                .into_iter()
                .map(|e| Entry {
                    name: e.name,
                    rel_path: e.rel_path,
                    is_dir: e.is_dir,
                })
                .collect()
        } else {
            Vec::new()
        };
        if let Err(e) = copy_to(self, rel, stat.is_dir, &descendants, &dest).await {
            // Leave no half-copied folder behind; the original is untouched.
            // Only remove `dest` if this copy created it: otherwise it is
            // another trashed note that happens to own the name.
            if e.dest_claimed {
                if let Err(cleanup) = saf.delete_document(&self.tree_uri, &dest) {
                    tracing::debug!(dest = %dest, error = %cleanup, "saf: partial trash copy not removed");
                }
            }
            return Err(e.error);
        }
        saf.delete_document(&self.tree_uri, rel)
            .map_err(map_plugin_err)
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

    async fn rmdir(&self, rel_dir: &str) -> Result<(), RoostError> {
        // DocumentFile.delete() on a directory only works when empty in
        // practice across most providers, so this is equivalent to the
        // NativeFs rmdir contract.
        self.app
            .saf()
            .delete_document(&self.tree_uri, rel_dir)
            .map_err(map_plugin_err)
    }

    async fn exists(&self, rel: &str) -> Result<bool, RoostError> {
        // Goes through `statDocument`, which does only a `findFile` walk.
        // The old code path read the entire document and discarded the
        // bytes — fine for tiny notes, catastrophic for any attachment.
        // Treat plugin errors as "exists: false" so behavior matches the
        // previous best-effort contract on transient SAF failures.
        match self.app.saf().stat_document(&self.tree_uri, rel) {
            Ok(resp) => Ok(resp.exists),
            Err(_) => Ok(false),
        }
    }

    fn display_root(&self) -> &str {
        &self.tree_uri
    }

    fn root_id(&self) -> String {
        self.tree_uri.clone()
    }
}
