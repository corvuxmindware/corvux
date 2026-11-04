//! Android Storage Access Framework backend for [`RoostFs`].
//!
//! All operations route through [`tauri_plugin_saf`] — the Rust side owns
//! a persisted tree URI, and every call marshals to the Kotlin plugin,
//! which resolves the relative path against the tree via `DocumentFile`
//! and performs the actual IO via `ContentResolver`.
//!
//! Slice 4 scope: `list`, `read`, `exists` only. Write/mutate methods
//! return [`RoostError::Io("not yet implemented")`] until Slice 5 adds
//! `writeDocument`/`createDocument`/`deleteDocument`/`renameDocument`
//! on the Kotlin side.

use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use tauri::{AppHandle, Runtime};
use tauri_plugin_saf::SafExt;

use super::{Entry, RoostError, RoostFs};

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
    let s = e.to_string();
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
            // Same filtering contract as `list` — hidden dotfiles and our
            // trash dir are dropped. Note the trash check matches on the
            // basename, which `name` already is, so deeply-nested names
            // that *contain* `.corvux-trash` aren't accidentally hidden.
            .filter(|e| !e.name.starts_with('.') && e.name != ".corvux-trash")
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
        // SAF has no per-app trash; `renameTo` can't move across dirs, so
        // we fall back to a hard delete. Users who want undo on Android
        // rely on Syncthing version history or a file-manager undo. This
        // asymmetry is documented in the trait contract comment.
        self.app
            .saf()
            .delete_document(&self.tree_uri, rel)
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
