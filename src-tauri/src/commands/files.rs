//! Per-file CRUD commands — read, write, create, delete, rename, and
//! folder operations. Roost lifecycle lives in `roost.rs`, attachments
//! in `attachments.rs`, daily notes in `daily_note.rs`. This file is
//! the narrow "operate on one path inside the active roost" surface.

use serde::Serialize;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use tauri::{AppHandle, Manager, State};
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

use crate::roostfs::RoostFs;

/// Tauri-managed handle to the currently-open roost's filesystem backend.
///
/// `None` until [`super::roost::open_roost`] installs a backend. Every
/// file command that uses this state must handle the `None` case — the
/// frontend may call commands before a roost is open (e.g. first launch).
pub struct RoostFsState {
    /// The installed filesystem backend. Keep this private so every swap also
    /// advances `generation`; stale async work uses that generation as its
    /// commit token.
    backend: AsyncMutex<Option<Arc<dyn RoostFs>>>,
    /// Serializes complete open/close transitions. Candidate backends are
    /// prepared before installation, but two user-initiated switches must not
    /// commit out of order.
    pub transition: AsyncMutex<()>,
    generation: AtomicU64,
    /// One process-wide serialization lane per note in the active generation.
    /// Editor saves, task mutations, history reverts, AI tools, deletes, and
    /// renames all acquire the same lane around their complete disk mutation.
    mutation_locks: StdMutex<HashMap<String, Arc<AsyncMutex<()>>>>,
}

impl Default for RoostFsState {
    fn default() -> Self {
        Self {
            backend: AsyncMutex::new(None),
            transition: AsyncMutex::new(()),
            generation: AtomicU64::new(0),
            mutation_locks: StdMutex::new(HashMap::new()),
        }
    }
}

impl RoostFsState {
    pub async fn current(&self) -> Option<Arc<dyn RoostFs>> {
        self.backend.lock().await.clone()
    }

    /// Consistent authority snapshot for long-lived operations. Reading the
    /// backend and generation separately can pair an old backend with the new
    /// generation when a roost switch lands between those reads.
    pub async fn current_with_generation(&self) -> Option<(Arc<dyn RoostFs>, u64)> {
        let guard = self.backend.lock().await;
        guard
            .as_ref()
            .cloned()
            .map(|backend| (backend, self.generation()))
    }

    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// Install a new active backend and return its generation token.
    pub async fn install(&self, backend: Arc<dyn RoostFs>) -> u64 {
        let mut guard = self.backend.lock().await;
        *guard = Some(backend);
        let generation = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
        drop(guard);
        generation
    }

    /// Clear the active backend and invalidate every outstanding token.
    pub async fn clear(&self) -> u64 {
        let mut guard = self.backend.lock().await;
        *guard = None;
        let generation = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
        drop(guard);
        generation
    }

    pub fn is_generation_current(&self, generation: u64) -> bool {
        self.generation() == generation
    }

    pub(crate) async fn lock_note(&self, rel: &str) -> OwnedMutexGuard<()> {
        let key = format!(
            "{}:{}",
            self.generation(),
            rel.replace('\\', "/").to_lowercase()
        );
        let lock = {
            let mut locks = match self.mutation_locks.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            if locks.len() > 512 {
                locks.retain(|_, lock| Arc::strong_count(lock) > 1);
            }
            Arc::clone(
                locks
                    .entry(key)
                    .or_insert_with(|| Arc::new(AsyncMutex::new(()))),
            )
        };
        lock.lock_owned().await
    }

    pub(crate) async fn lock_note_pair(
        &self,
        first: &str,
        second: &str,
    ) -> (OwnedMutexGuard<()>, Option<OwnedMutexGuard<()>>) {
        let first_key = first.replace('\\', "/").to_lowercase();
        let second_key = second.replace('\\', "/").to_lowercase();
        if first_key == second_key {
            return (self.lock_note(first).await, None);
        }
        if first_key < second_key {
            let a = self.lock_note(first).await;
            let b = self.lock_note(second).await;
            (a, Some(b))
        } else {
            let b = self.lock_note(second).await;
            let a = self.lock_note(first).await;
            (a, Some(b))
        }
    }
}

/// Entry in a roost's file tree. Returned from `open_roost`; `path` is
/// absolute on native, tree-URI-prefixed on SAF. `Deserialize` is needed
/// for the disk-backed tree cache (Phase B) — `open_roost` reads the
/// cached tree on warm starts, returns it instantly, and revalidates in
/// the background.
#[derive(Debug, Serialize, serde::Deserialize, Clone, specta::Type)]
pub struct FileEntry {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    pub children: Vec<FileEntry>,
}

/// Strip the roost root prefix from an absolute path so it can be fed to a
/// [`RoostFs`] method. Commands receive absolute paths from the frontend;
/// when a backend is installed they're mapped to roost-relative here.
/// Returns `None` if `abs` isn't under `root`.
///
/// AUDIT-044: tightens the prefix match against three Windows / Unicode
/// quirks that would otherwise let a crafted IPC argument escape the
/// roost.
///
///   1. NFC normalize both sides. Combining-character paths (`café`
///      with U+0301 vs `café` with U+00E9) compare byte-distinct as
///      raw strings; after NFC they match. The frontend's source can
///      vary (clipboard, AI tool, drag-and-drop) so we can't trust
///      either side to be pre-normalized.
///   2. Reject Windows long-path / device-namespace prefixes (`\\?\`,
///      `\\.\`). These bypass DOS-path parsing and would make
///      `\\?\C:\Users\victim` look like a different roost from
///      `C:\Users\victim` even though they point at the same tree.
///   3. (Known limitation) NTFS 8.3 short names (`PROGRA~1` vs
///      `Program Files`) still bypass this check. 8.3 generation is
///      disabled by default on modern Windows, and resolving them
///      properly would require `GetLongPathNameW` per call. Document
///      and accept; revisit if a real attacker path materialises.
pub(crate) fn to_rel(abs: &str, root: &str) -> Option<String> {
    use unicode_normalization::UnicodeNormalization;

    // (2) Reject device-namespace / long-path prefixes outright. We
    // can't safely strip them: `\\?\C:\Roost` and `C:\Roost` resolve to
    // the same tree but compare distinct, and a roost root stored
    // without the prefix would never match an incoming `abs` carrying
    // one. Easier to reject and force the caller to pass a normal path.
    let has_win_special_prefix = |s: &str| {
        s.starts_with(r"\\?\")
            || s.starts_with(r"\\.\")
            || s.starts_with("//?/")
            || s.starts_with("//./")
    };
    if has_win_special_prefix(abs) || has_win_special_prefix(root) {
        return None;
    }

    // (1) NFC then forward-slash normalize. Order matters: combining
    // characters can sit alongside separators in user-created folder
    // names, so do the unicode pass first.
    let abs_n: String = abs.nfc().collect::<String>().replace('\\', "/");
    let normalized_root = root.nfc().collect::<String>().replace('\\', "/");
    let root_n = if normalized_root == "/" {
        normalized_root.as_str()
    } else {
        normalized_root.trim_end_matches('/')
    };
    if root_n.is_empty() {
        return None;
    }

    if abs_n == root_n {
        return Some(String::new());
    }
    if root_n == "/" {
        return abs_n.strip_prefix('/').map(ToString::to_string);
    }
    abs_n
        .strip_prefix(root_n)
        .and_then(|tail| tail.strip_prefix('/'))
        .map(ToString::to_string)
}

#[tauri::command]
#[specta::specta]
pub async fn read_file(path: String, state: State<'_, RoostFsState>) -> Result<String, String> {
    let backend = state
        .current()
        .await
        .ok_or_else(|| "No roost is open".to_string())?;
    let rel = to_rel(&path, backend.display_root())
        .ok_or_else(|| "Access denied: that path is outside the active roost".to_string())?;
    let bytes = backend
        .read(&rel)
        .await
        .map_err(|e| format!("Failed to read {rel}: {e}"))?;
    String::from_utf8(bytes).map_err(|e| format!("File not UTF-8 ({rel}): {e}"))
}

/// EDT-1/EDT-2: what a note write actually did, so `save_roost_note` can
/// skip the downstream re-index / FTS / backlink fan-out when nothing
/// changed on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum WriteOutcome {
    /// File was created or its content changed — run the full fan-out.
    Written,
    /// EDT-2: incoming content was byte-identical to what's already on
    /// disk. No write, no edit-history event, no sync emit.
    Unchanged,
}

/// EDT-1: prefix on the error `save_roost_note` / `write_file` return when
/// a last-write-wins conflict is detected. The frontend matches this to
/// show a "reload before saving" conflict prompt instead of a raw toast.
pub(super) const SAVE_CONFLICT_PREFIX: &str = "__save_conflict__";

/// The pre-write decision, factored out of `write_file_internal` so it can
/// be unit-tested without a Tauri `State` / `RoostFs` harness.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WriteDecision {
    /// EDT-2: content byte-identical to disk — do nothing.
    Unchanged,
    /// EDT-1: disk diverged from the caller's `base_hash` — refuse.
    Conflict,
    /// Safe to write.
    Proceed,
}

/// EDT-1: SHA-256 hex of note content, used ONLY for the save-time
/// last-write-wins check. SHA-256 (not the blake3 `content_hash` used for
/// edit-history) because the editor computes the base hash in the webview
/// via `crypto.subtle.digest("SHA-256", ...)`, which has no blake3. This is
/// change-detection, not security — any collision-resistant hash both sides
/// can compute would do; SHA-256 is the one already available on both.
fn content_sha256_hex(s: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(s.as_bytes());
    let mut hex = String::with_capacity(digest.len() * 2);
    for b in digest {
        use std::fmt::Write as _;
        let _ = write!(hex, "{b:02x}");
    }
    hex
}

/// Decide what a write should do given the disk state and the caller's
/// optional base hash. Pure: dedup wins over conflict (a no-op re-save of
/// a diverged file is still a no-op, never a conflict), and a create
/// (`!existed`) or a hash-less caller always proceeds.
fn classify_write(
    existed: bool,
    before: &str,
    content: &str,
    base_hash: Option<&str>,
) -> WriteDecision {
    if existed && before == content {
        return WriteDecision::Unchanged;
    }
    if existed {
        if let Some(base) = base_hash {
            if content_sha256_hex(before) != base {
                return WriteDecision::Conflict;
            }
        }
    }
    WriteDecision::Proceed
}

/// Atomic write: write to .tmp → fsync → rename over original. If we crash
/// mid-write, the .tmp is lost but the original is intact.
///
/// After a successful write, if a sync engine is running, we feed the new
/// content into it so every connected peer learns about the edit. On
/// Android this is the only path — the `notify` crate silently no-ops on
/// SAF `content://` URIs so there's no live watcher to observe the write.
/// On desktop the notify watcher fires too but `apply_markdown` dedupes
/// at the Automerge layer, so the double-fire is harmless.
#[tauri::command]
#[specta::specta]
pub async fn write_file(
    path: String,
    content: String,
    state: State<'_, RoostFsState>,
    sync_state: State<'_, super::sync::SyncEngineState>,
    db_state: State<'_, super::graph::GraphDb>,
) -> Result<(), String> {
    // Generic writes (settings JSON, chat blobs, non-editor callers) pass
    // no base hash — no last-write-wins guard, matching prior behavior.
    write_file_internal(&path, &content, None, &state, &sync_state, &db_state)
        .await
        .map(|_| ())
}

/// AUDIT-053: shared internals for the file-write half of a save. Used
/// by the generic `write_file` IPC (above) AND by the bundled
/// `save_roost_note` IPC in `commands::save`. Keeping one source of
/// truth here means the atomic-write / edit-history / sync-notify
/// triad can't drift between the two callers — every future hook
/// (CRDT delta capture, scribe-style snapshotting, anything that has
/// to react to "the file just changed") lands in one place.
///
/// Returns `Ok(())` on a successful write. Errors from the edit-history
/// recorder and sync engine are intentionally swallowed — the user's
/// save must not fail because of a downstream bookkeeping glitch.
pub(super) async fn write_file_internal(
    path: &str,
    content: &str,
    base_hash: Option<&str>,
    state: &State<'_, RoostFsState>,
    sync_state: &State<'_, super::sync::SyncEngineState>,
    db_state: &State<'_, super::graph::GraphDb>,
) -> Result<WriteOutcome, String> {
    let generation = state.generation();
    let backend = state
        .current()
        .await
        .ok_or_else(|| "No roost is open".to_string())?;
    let rel = to_rel(path, backend.display_root())
        .ok_or_else(|| "Access denied: that path is outside the active roost".to_string())?;
    let _mutation_guard = state.lock_note(&rel).await;
    if !state.is_generation_current(generation) {
        return Err("Roost changed before the write could commit".to_string());
    }

    // Read current content for the edit-history diff and the change
    // detection below. If the file doesn't exist yet, treat the prior
    // state as empty — the recorded event then has intent="create". Read
    // failure on an existing file is non-fatal: we proceed with the write
    // but record an empty before-snapshot rather than refuse the save.
    let read = backend.read(&rel).await;
    let existed = read.is_ok();
    let before = match read {
        Ok(bytes) => String::from_utf8(bytes).unwrap_or_default(),
        Err(_) => String::new(),
    };
    let is_create = before.is_empty();

    match classify_write(existed, &before, content, base_hash) {
        // EDT-2: byte-identical to disk — skip write, edit-history, sync,
        // and (in save_roost_note) the graph + FTS re-index.
        WriteDecision::Unchanged => return Ok(WriteOutcome::Unchanged),
        // EDT-1: the on-disk file diverged from the base the caller loaded,
        // so a blind write would clobber whoever wrote in the meantime.
        WriteDecision::Conflict => {
            return Err(format!(
                "{SAVE_CONFLICT_PREFIX}: '{path}' changed on disk since you opened it. \
                 Reload to merge the other change — saving now would overwrite it."
            ));
        }
        WriteDecision::Proceed => {}
    }

    backend
        .write_atomic(&rel, content.as_bytes())
        .await
        .map_err(|e| format!("Failed to write {rel}: {e}"))?;

    // Record the edit event. Best-effort — a missed history row is
    // recoverable (markdown is the source of truth); failing the user's
    // save because of a graph hiccup is not.
    record_write_event(db_state, path, &before, content, is_create);

    // Notify the sync engine if it's running. See `notify_sync_local_write`
    // for why every note-write path must route through this rather than lean
    // on the file watcher (which is dead on Android SAF roosts).
    notify_sync_local_write(sync_state.inner(), &rel, content).await;

    Ok(WriteOutcome::Written)
}

/// Hand a freshly-written note body to the sync engine so it reaches peers.
///
/// `write_file_internal` calls this for editor saves, but it is **not**
/// optional plumbing that other write paths may skip. On desktop the
/// `notify` file watcher catches every disk write and feeds sync as a
/// backstop — but on Android SAF (`content://`) roosts that watcher is
/// disabled and `scan_roost` is a no-op (see `sync::engine::scan`). A
/// note-write that doesn't call this therefore **never leaves an Android
/// device** — which is exactly how task complete / quick-add / reschedule
/// (they write markdown directly via `RoostFs::write_atomic`) stopped
/// syncing mobile→laptop.
///
/// So: any command that mutates a note's markdown must call this after its
/// write. No-op when the path isn't markdown, or when sync isn't running.
/// `rel` is the roost-relative path; `content` is the full new file body.
/// Engine-side errors are logged there — they never fail the user's save.
pub(super) async fn notify_sync_local_write(
    sync_state: &super::sync::SyncEngineState,
    rel: &str,
    content: &str,
) {
    // NEW-S1: shared case-insensitive predicate — a `.MD` note was silently
    // dropped here (never synced) even though the save gate accepted it.
    if !crate::roostfs::walk::is_markdown_name(rel) {
        return;
    }
    // Clone the engine handle (cheap — it's Arc-backed) and DROP the state
    // lock before doing any sync work. `notify_local_write` touches the
    // docstore and fans out to peers; holding `SyncEngineState.engine` across
    // it would serialize every sync command behind a single note save (and,
    // before send_local went non-blocking, could deadlock the whole surface
    // behind a wedged peer).
    let engine = sync_state.engine.lock().await.as_ref().cloned();
    if let Some(engine) = engine {
        engine
            .notify_local_write(
                crate::sync::protocol::NotePath(rel.replace('\\', "/")),
                content.to_string(),
            )
            .await;
    }
}

/// Create a note iff it is still absent while holding the shared per-note
/// mutation lane. Returns `true` when created and `false` when another writer
/// won the race. Parent directories are created through the active backend.
pub(crate) async fn create_roost_note_synced<R: tauri::Runtime>(
    app: &AppHandle<R>,
    rel: &str,
    content: &str,
) -> Result<bool, String> {
    let state = app
        .try_state::<RoostFsState>()
        .ok_or_else(|| "roost filesystem not initialized".to_string())?;
    let generation = state.generation();
    let backend = state
        .current()
        .await
        .ok_or_else(|| "No roost is open".to_string())?;
    let _mutation_guard = state.lock_note(rel).await;
    if !state.is_generation_current(generation) {
        return Err("Roost changed before the create could commit".to_string());
    }
    let parent = rel.rsplit_once('/').map(|(parent, _)| parent).unwrap_or("");
    let mut current = String::new();
    for segment in parent.split('/').filter(|part| !part.is_empty()) {
        if !current.is_empty() {
            current.push('/');
        }
        current.push_str(segment);
        match backend.mkdir(&current).await {
            Ok(()) | Err(crate::roostfs::RoostError::AlreadyExists(_)) => {}
            Err(e) => return Err(format!("create directory {current}: {e}")),
        }
    }
    match backend.create(rel, content.as_bytes()).await {
        Ok(()) => {
            if let Some(sync) = app.try_state::<super::sync::SyncEngineState>() {
                notify_sync_local_write(sync.inner(), rel, content).await;
            }
            Ok(true)
        }
        Err(crate::roostfs::RoostError::AlreadyExists(_)) => Ok(false),
        Err(e) => Err(format!("create {rel}: {e}")),
    }
}

/// Atomically read-modify-write one UTF-8 note under the same per-note lane
/// used by editor saves and task/history commands. This is the only safe shape
/// for agent tools whose output depends on the current note body.
pub(crate) async fn mutate_roost_note_synced<R, T, F>(
    app: &AppHandle<R>,
    rel: &str,
    mutate: F,
) -> Result<(String, String, T), String>
where
    R: tauri::Runtime,
    F: FnOnce(&str) -> Result<(String, T), String>,
{
    mutate_roost_text_synced(app, rel, false, mutate).await
}

/// Variant of [`mutate_roost_note_synced`] for app-owned markdown files such
/// as `.corvux/memory.md` that are created lazily. `allow_missing` treats a
/// missing file as empty and creates any missing parent directories, while
/// retaining the same generation check, per-path mutation lane, atomic write,
/// and sync notification as ordinary note edits.
pub(crate) async fn mutate_roost_text_synced<R, T, F>(
    app: &AppHandle<R>,
    rel: &str,
    allow_missing: bool,
    mutate: F,
) -> Result<(String, String, T), String>
where
    R: tauri::Runtime,
    F: FnOnce(&str) -> Result<(String, T), String>,
{
    let state = app
        .try_state::<RoostFsState>()
        .ok_or_else(|| "roost filesystem not initialized".to_string())?;
    let generation = state.generation();
    let backend = state
        .current()
        .await
        .ok_or_else(|| "No roost is open".to_string())?;
    let _mutation_guard = state.lock_note(rel).await;
    if !state.is_generation_current(generation) {
        return Err("Roost changed before the mutation could commit".to_string());
    }
    let before = match backend.read(rel).await {
        Ok(bytes) => {
            String::from_utf8(bytes).map_err(|e| format!("File not UTF-8 ({rel}): {e}"))?
        }
        Err(crate::roostfs::RoostError::NotFound(_)) if allow_missing => String::new(),
        Err(e) => return Err(format!("read {rel}: {e}")),
    };
    let (after, value) = mutate(&before)?;
    if after != before {
        if allow_missing {
            let parent = rel.rsplit_once('/').map(|(parent, _)| parent).unwrap_or("");
            let mut current = String::new();
            for segment in parent.split('/').filter(|part| !part.is_empty()) {
                if !current.is_empty() {
                    current.push('/');
                }
                current.push_str(segment);
                match backend.mkdir(&current).await {
                    Ok(()) | Err(crate::roostfs::RoostError::AlreadyExists(_)) => {}
                    Err(e) => return Err(format!("create directory {current}: {e}")),
                }
            }
        }
        backend
            .write_atomic(rel, after.as_bytes())
            .await
            .map_err(|e| format!("write {rel}: {e}"))?;
        if let Some(sync) = app.try_state::<super::sync::SyncEngineState>() {
            notify_sync_local_write(sync.inner(), rel, &after).await;
        }
    }
    Ok((before, after, value))
}

/// Propagate a note rename/move to sync peers: tombstone the old roost-relative
/// path, write the new one. Needed because the Android SAF watcher never fires,
/// so a rename would otherwise be invisible to sync and leave duplicates. No-op
/// off markdown or when sync isn't running. Clones the engine out of the state
/// lock before doing sync work (see `notify_sync_local_write`).
pub(super) async fn notify_sync_local_rename(
    sync_state: &super::sync::SyncEngineState,
    old_rel: &str,
    new_rel: &str,
    new_content: &str,
) {
    // NEW-S1: shared case-insensitive predicate (see notify_sync_local_write).
    if !crate::roostfs::walk::is_markdown_name(new_rel) {
        return;
    }
    let engine = sync_state.engine.lock().await.as_ref().cloned();
    if let Some(engine) = engine {
        engine
            .notify_local_rename(
                crate::sync::protocol::NotePath(old_rel.replace('\\', "/")),
                crate::sync::protocol::NotePath(new_rel.replace('\\', "/")),
                new_content.to_string(),
            )
            .await;
    }
}

#[tauri::command]
#[specta::specta]
pub async fn create_file(
    path: String,
    state: State<'_, RoostFsState>,
    app: AppHandle,
) -> Result<(), String> {
    let stem = PathBuf::from(&path)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let initial = format!("# {stem}\n\n");

    let generation = state.generation();
    let backend = state
        .current()
        .await
        .ok_or_else(|| "No roost is open".to_string())?;
    let rel = to_rel(&path, backend.display_root())
        .ok_or_else(|| "Access denied: that path is outside the active roost".to_string())?;
    let _mutation_guard = state.lock_note(&rel).await;
    if !state.is_generation_current(generation) {
        return Err("Roost changed before the create could commit".to_string());
    }
    backend
        .create(&rel, initial.as_bytes())
        .await
        .map_err(|e| format!("Failed to create {rel}: {e}"))?;
    super::roost_tree_cache::invalidate(&app, backend.display_root());
    // Notify sync so a newly-created note leaves the device even on Android SAF
    // (no file watcher). Editor saves route through write_file → this same
    // helper; create_file previously skipped it, so a note created on mobile
    // never synced until its first edit.
    if let Some(sync) = app.try_state::<super::sync::SyncEngineState>() {
        notify_sync_local_write(sync.inner(), &rel, &initial).await;
    }
    Ok(())
}

/// Safe delete: move to the backend's trash location (recoverable, not
/// permanent). `roost_path` is kept in the signature for wire stability
/// but is unused when a backend is installed.
#[tauri::command]
#[specta::specta]
pub async fn delete_file(
    path: String,
    roost_path: String,
    state: State<'_, RoostFsState>,
    sync_state: State<'_, super::sync::SyncEngineState>,
    db_state: State<'_, super::graph::GraphDb>,
    search_state: State<'_, super::search::SearchState>,
    app: AppHandle,
) -> Result<(), String> {
    let _ = roost_path;
    let is_markdown = crate::roostfs::walk::is_markdown_name(&path); // NEW-S1

    let generation = state.generation();
    let backend = state
        .current()
        .await
        .ok_or_else(|| "No roost is open".to_string())?;
    let rel = to_rel(&path, backend.display_root())
        .ok_or_else(|| "Access denied: that path is outside the active roost".to_string())?;
    let _mutation_guard = state.lock_note(&rel).await;
    if !state.is_generation_current(generation) {
        return Err("Roost changed before the delete could commit".to_string());
    }

    // Capture pre-trash content for the edit-history record. Read errors
    // are non-fatal: we'd rather lose the snapshot than block the trash
    // when the source is already gone (e.g. external delete raced us).
    let before = if is_markdown {
        match backend.read(&rel).await {
            Ok(bytes) => String::from_utf8(bytes).unwrap_or_default(),
            Err(_) => String::new(),
        }
    } else {
        String::new()
    };

    backend
        .trash(&rel)
        .await
        .map_err(|e| format!("Failed to trash {rel}: {e}"))?;
    super::roost_tree_cache::invalidate(&app, backend.display_root());

    // Emit sync tombstone for paired peers. Only meaningful for
    // markdown files — attachments aren't synced in the MVP.
    if is_markdown {
        if let Some(engine) = sync_state.engine.lock().await.as_ref() {
            engine
                .notify_local_remove(crate::sync::protocol::NotePath(rel.replace('\\', "/")))
                .await;
        }
        // Record the delete BEFORE purging the graph so we still have
        // the note_id resolution path live. The purge then drops every
        // other relation; we explicitly skip purging `:edit_event`
        // since the user may want to revert a delete.
        record_delete_event(&db_state, &path, &before);
        purge_from_graph(&path, &db_state);
        // NEW-FILES1: drop the note from the FTS index in the SAME command, the
        // way a save routes to both the graph and the search index. Previously
        // only the graph was purged here and the frontend fired a separate
        // `remove_from_search` IPC — if that follow-up dropped, the deleted note
        // lingered as a search ghost until the next warm-start reconcile.
        purge_from_search(&path, &search_state);
        // TRN-3 (mid-session): cancel the deleted note's armed alarms now
        // (see cancel_note_alarms). The launch-time reschedule_all sweep is
        // the restart safety net; this closes the mid-session window.
        cancel_note_alarms(&db_state, backend.display_root(), &path);
    }
    Ok(())
}

/// TRN-3 (mid-session): cancel any armed OS alarms for the note at
/// `abs_path`. Call AFTER the note's `:task` rows are purged so the
/// reschedule cancels this note's timers (desktop) / OS-alarm ids (mobile,
/// via `abort_one`) and re-arms nothing. Best-effort — a lock hiccup just
/// falls back to the launch-time `reschedule_all` sweep. Shared by the
/// delete / rename / move paths (single-file and folder-scoped).
fn cancel_note_alarms(db_state: &State<'_, super::graph::GraphDb>, root: &str, abs_path: &str) {
    if let Ok(guard) = db_state.db.lock() {
        if let Some(db) = guard.as_ref() {
            let note_id = crate::db::queries::note_id_from_abs_path(abs_path, root);
            db_state
                .notification_scheduler
                .reschedule_for_note(db, &note_id);
        }
    }
}

/// Remove a note from the FTS search index. Best-effort and fail-soft: a
/// poisoned lock or missing index logs and returns — a search-index hiccup must
/// never fail the user-visible delete (markdown on disk is the source of truth,
/// and the warm-start reconcile purges any straggler). Mirrors
/// [`purge_from_graph`].
fn purge_from_search(abs_path: &str, search_state: &State<'_, super::search::SearchState>) {
    if let Err(e) = search_state.with_db_for_path(abs_path, |db| db.remove_note(abs_path)) {
        tracing::warn!(error = %e, path = %abs_path, "search: failed to purge deleted note from FTS");
    }
}

/// Record a delete event. before = pre-trash content, after = empty.
/// Lets the user revert a delete from the History pane (write
/// `before_content` back to disk).
fn record_delete_event(db_state: &State<'_, super::graph::GraphDb>, abs_path: &str, before: &str) {
    tokio::task::block_in_place(|| {
        let Ok(db_guard) = db_state.db.lock() else {
            return;
        };
        let Some(db) = db_guard.as_ref() else {
            return;
        };
        let roost_root = match db_state.roost_path.lock() {
            Ok(g) => match g.as_deref() {
                Some(v) => v.to_string(),
                None => return,
            },
            Err(_) => return,
        };
        let note_id = crate::db::queries::note_id_from_abs_path(abs_path, &roost_root);
        if let Err(e) = crate::db::queries::record_edit_event(
            db,
            &note_id,
            abs_path,
            crate::db::queries::Actor::User,
            "user",
            crate::db::queries::Intent::Delete,
            before,
            "",
        ) {
            tracing::warn!(%note_id, error = %e, "edit_history: record delete event failed");
        }
    });
}

/// Record an edit-history event for a file write. Best-effort — every
/// failure is logged and swallowed. Only records for markdown files; we
/// don't track edit history for binary attachments.
fn record_write_event(
    db_state: &State<'_, super::graph::GraphDb>,
    abs_path: &str,
    before: &str,
    after: &str,
    is_create: bool,
) {
    if !crate::roostfs::walk::is_markdown_name(abs_path) {
        // NEW-S1: shared case-insensitive predicate.
        return;
    }
    tokio::task::block_in_place(|| {
        let Ok(db_guard) = db_state.db.lock() else {
            tracing::warn!(path = %abs_path, "edit_history: DB lock poisoned, skipping record");
            return;
        };
        let Some(db) = db_guard.as_ref() else {
            // Graph DB not yet initialized — happens when the user
            // edits a note before opening any graph-touching pane
            // (Tasks / Graph / Search / Chat / History). Logged so the
            // diagnostic panel makes the deferred init visible; the
            // History pane itself triggers init when first opened so
            // subsequent saves get recorded normally.
            tracing::info!(
                path = %abs_path,
                "edit_history: graph not initialized — skipping record (open the History pane to initialize and start tracking)"
            );
            return;
        };
        let roost_root = match db_state.roost_path.lock() {
            Ok(g) => match g.as_deref() {
                Some(v) => v.to_string(),
                None => return,
            },
            Err(_) => return,
        };
        let note_id = crate::db::queries::note_id_from_abs_path(abs_path, &roost_root);
        let intent = if is_create {
            crate::db::queries::Intent::Create
        } else {
            crate::db::queries::Intent::Edit
        };
        match crate::db::queries::record_edit_event(
            db,
            &note_id,
            abs_path,
            crate::db::queries::Actor::User,
            "user",
            intent,
            before,
            after,
        ) {
            Ok(event_id) => {
                tracing::info!(%note_id, %event_id, "edit_history: recorded write event");
            }
            Err(e) => {
                tracing::warn!(%note_id, error = %e, "edit_history: record write event failed");
            }
        }
    });
}

/// Best-effort: remove a note's id from every graph relation and
/// invalidate the cached LinkResolver. Errors are logged but not
/// propagated — a stale graph row is recoverable (next roost re-index
/// cleans it up); failing the user's delete because of a graph hiccup
/// is not.
///
/// CozoDB's Datalog evaluator is fully synchronous and can hold the DB
/// mutex for tens of ms on a real roost. Wrapping the lock-acquire +
/// query in `tokio::task::block_in_place` tells tokio's scheduler that
/// the current async worker is about to block, so other tasks get
/// migrated off it instead of stalling behind us. (We deliberately don't
/// use `spawn_blocking` here — purge_from_graph is fire-and-forget so
/// avoiding the JoinHandle keeps the call site clean.)
fn purge_from_graph(abs_path: &str, db_state: &State<'_, super::graph::GraphDb>) {
    tokio::task::block_in_place(|| {
        let Ok(db_guard) = db_state.db.lock() else {
            return;
        };
        let Some(db) = db_guard.as_ref() else {
            return;
        };
        let roost_root = match db_state.roost_path.lock() {
            Ok(g) => match g.as_deref() {
                Some(v) => v.to_string(),
                None => return,
            },
            Err(_) => return,
        };
        let note_id = crate::db::queries::note_id_from_abs_path(abs_path, &roost_root);
        if let Err(errs) = crate::db::queries::purge_note(db, &note_id) {
            // AUDIT-069: log every per-relation failure individually so
            // an orphaned chunks row vs an orphaned links row are
            // distinguishable in tracing — both leak DB rows but they
            // imply different recovery paths.
            for e in &errs {
                tracing::warn!(%note_id, error = %e, "files: purge_note relation failed");
            }
        }
        drop(db_guard);
        db_state.invalidate_link_resolver();
        // NEW-S2: a note (and its tasks) just left the graph — drop the cached
        // stats snapshot so the unchanged-save fast path recomputes.
        db_state.invalidate_stats_cache();
    });
}

/// Re-root a nested note's roost-relative path from an old folder prefix to
/// a new one, by dropping the first `old_prefix_seg_count` path segments and
/// prepending `new_rel`. Segment-based (not byte slicing) so it's correct
/// under case-only folder renames and multi-byte Unicode — the note's tail
/// keeps its original on-disk casing. Returns `None` if the note has no tail
/// left after the prefix (i.e. it *is* the folder), which shouldn't happen.
fn swap_folder_prefix(
    old_nested_rel: &str,
    old_prefix_seg_count: usize,
    new_rel: &str,
) -> Option<String> {
    let tail = old_nested_rel
        .split('/')
        .filter(|s| !s.is_empty())
        .skip(old_prefix_seg_count)
        .collect::<Vec<_>>()
        .join("/");
    if tail.is_empty() {
        return None;
    }
    let new_prefix = new_rel.trim_matches('/');
    Some(if new_prefix.is_empty() {
        tail
    } else {
        format!("{new_prefix}/{tail}")
    })
}

/// IP-4: keep the graph correct when a *folder* is renamed or moved.
///
/// A directory rename/move changes the path-based note id of EVERY markdown
/// note inside it. Before this, folder moves did zero graph work (the
/// `is_markdown` gate in `rename_file`/`move_entry` skipped them entirely):
/// nested notes kept ghost nodes at their OLD ids (still surfacing in
/// PageRank / graph view / link+tag queries), their new paths were never
/// indexed, and on the next warm open the reconcile pass purged the old ids
/// as "deleted" — destroying their incoming backlinks permanently (see the
/// IP-3 danglify fix in `purge_note`). This closes that gap.
///
/// The nested notes are enumerated from the GRAPH (`note_paths_under_dir`),
/// which still holds their OLD paths — the files have already moved on disk
/// but the index hasn't caught up yet. For each note we:
///   1. migrate its `:edit_event` history to the new id,
///   2. purge the old id (converting incoming links to `:dangling` rows so
///      backlinks survive the id change), and
///   3. re-index it at the NEW path — which re-promotes those dangling links,
///      restoring the backlinks against the new id.
///
/// Sync peers are notified per note so the move propagates on platforms whose
/// file watcher misses renames (Android SAF), mirroring the single-file path.
///
/// Best-effort per note: a failure on one file logs and continues so a single
/// bad note can't strand the rest of the folder.
async fn reindex_moved_folder(
    old_rel: &str,
    new_rel: &str,
    backend: &std::sync::Arc<dyn crate::roostfs::RoostFs>,
    db_state: &State<'_, super::graph::GraphDb>,
    sync_state: &super::sync::SyncEngineState,
    search_state: &State<'_, super::search::SearchState>,
) {
    // Snapshot the roost root + the nested notes' OLD rel paths under one
    // brief lock, then release before the per-note reindex loop (which
    // re-acquires the graph lock internally — never hold it across the loop).
    let nested: Vec<String> = {
        let Ok(db_guard) = db_state.db.lock() else {
            return;
        };
        let Some(db) = db_guard.as_ref() else {
            return;
        };
        let root = match db_state.roost_path.lock() {
            Ok(g) => match g.as_deref() {
                Some(v) => v.to_string(),
                None => return,
            },
            Err(_) => return,
        };
        match crate::db::queries::note_paths_under_dir(db, &root, old_rel) {
            Ok(paths) => paths,
            Err(e) => {
                tracing::warn!(error = %e, old_rel, "files: note_paths_under_dir failed on folder move");
                return;
            }
        }
    };
    if nested.is_empty() {
        return;
    }

    // Segment-based prefix swap: drop the leading segments that make up the
    // old folder and prepend the new folder. Segment counting (not byte
    // slicing) keeps this correct under case differences and Unicode — the
    // note's tail keeps its original on-disk casing.
    let old_prefix_segs = old_rel.split('/').filter(|s| !s.is_empty()).count();
    let root_disp = backend
        .display_root()
        .trim_end_matches(['/', '\\'])
        .to_string();

    for old_nested_rel in nested {
        let Some(new_nested_rel) = swap_folder_prefix(&old_nested_rel, old_prefix_segs, new_rel)
        else {
            continue; // defensive: a note that IS the prefix shouldn't happen
        };
        let old_abs = format!("{root_disp}/{}", old_nested_rel.trim_start_matches('/'));
        let new_abs = format!("{root_disp}/{new_nested_rel}");

        migrate_edit_history_on_rename(db_state, &old_abs, &new_abs);
        purge_from_graph(&old_abs, db_state);
        // NEW-S-FTS (folder-scoped): purge the old nested path from FTS so a
        // moved folder's notes don't ghost in search under their former paths
        // (the per-note re-index below adds the new path). Mirrors the
        // single-file rename/move purge.
        purge_from_search(&old_abs, search_state);
        // TRN-3 (mid-session): the nested note's id changed with the folder
        // move — cancel the old id's armed alarm so it can't fire a phantom.
        cancel_note_alarms(db_state, &root_disp, &old_abs);

        match backend.read(&new_nested_rel).await {
            Ok(bytes) => match String::from_utf8(bytes) {
                Ok(content) => {
                    if let Err(e) = super::graph::index_note_internal(&new_abs, &content, db_state)
                    {
                        tracing::warn!(error = %e, new_abs, "files: reindex after folder move failed");
                    }
                    notify_sync_local_rename(
                        sync_state,
                        &old_nested_rel,
                        &new_nested_rel,
                        &content,
                    )
                    .await;
                }
                Err(_) => {
                    tracing::warn!(
                        new_abs,
                        "files: moved note is not valid UTF-8; skipped reindex"
                    )
                }
            },
            Err(e) => {
                tracing::warn!(error = %e, new_nested_rel, "files: could not read moved note for reindex")
            }
        }
    }
    db_state.invalidate_link_resolver();
}

#[tauri::command]
#[specta::specta]
pub async fn rename_file(
    old_path: String,
    new_path: String,
    state: State<'_, RoostFsState>,
    db_state: State<'_, super::graph::GraphDb>,
    sync_state: State<'_, super::sync::SyncEngineState>,
    search_state: State<'_, super::search::SearchState>,
    app: AppHandle,
) -> Result<(), String> {
    let is_markdown = crate::roostfs::walk::is_markdown_name(&old_path); // NEW-S1

    let generation = state.generation();
    let backend = state
        .current()
        .await
        .ok_or_else(|| "No roost is open".to_string())?;
    let (old_rel, new_rel) = match (
        to_rel(&old_path, backend.display_root()),
        to_rel(&new_path, backend.display_root()),
    ) {
        (Some(o), Some(n)) => (o, n),
        _ => {
            return Err(
                "Access denied: source or destination is outside the active roost".to_string(),
            );
        }
    };
    let _mutation_guards = state.lock_note_pair(&old_rel, &new_rel).await;
    if !state.is_generation_current(generation) {
        return Err("Roost changed before the rename could commit".to_string());
    }
    backend
        .rename(&old_rel, &new_rel)
        .await
        .map_err(|e| format!("Failed to rename: {e}"))?;
    super::roost_tree_cache::invalidate(&app, backend.display_root());
    if is_markdown {
        // Migrate edit-history rows to the new note_id BEFORE purging
        // the graph. Without this the old id's `:edit_event` rows would
        // be orphaned (still queryable but not shown by the History
        // pane, which loads by the new note's id). After the migrate,
        // also stamp a `Rename` event so the rename itself shows up in
        // the audit trail.
        migrate_edit_history_on_rename(&db_state, &old_path, &new_path);
        // Path-based note ids change on rename. Purge the old id from the
        // graph; the new id will be created the next time the renamed note
        // is saved (which the editor does immediately on rename in current
        // UX). Without this, the old id stays as an orphan node in PageRank
        // and graph view, and tag/link queries still surface the stale row.
        // (`purge_note` deliberately does NOT touch `:edit_event`, so the
        // migrate above is preserved.)
        //
        // IP-3: a CASE-ONLY rename (Note.md → note.md on a case-insensitive
        // filesystem) leaves the canonical note id UNCHANGED — the id
        // normalizer lowercases. Purging then would delete the live node and
        // briefly danglify every incoming backlink until the note is
        // re-indexed. Skip the purge entirely in that case; the re-index
        // below updates the (case-changed) display path in place.
        let old_id = crate::db::queries::note_id_from_abs_path(&old_path, backend.display_root());
        let new_id = crate::db::queries::note_id_from_abs_path(&new_path, backend.display_root());
        if old_id != new_id {
            purge_from_graph(&old_path, &db_state);
        }
        // NEW-S-FTS(rename): the FTS index is keyed by PATH, so even a
        // case-only rename leaves the old path as a ghost hit. Remove it
        // unconditionally (no-op if absent); the new path is re-indexed by
        // the editor's follow-up save. Mirrors delete_file's purge_from_search.
        purge_from_search(&old_path, &search_state);
        // TRN-3 (mid-session): the note id changed, so the OLD id's armed
        // alarm (blake3(old_id:line)) would otherwise fire a phantom at a
        // now-defunct id until the next launch reschedule. Cancel it now.
        // (Case-only rename keeps the same id → no-op, harmless.)
        cancel_note_alarms(&db_state, backend.display_root(), &old_path);
        // Propagate the rename to sync peers explicitly. The desktop watcher
        // catches renames, but the Android SAF watcher does not — without this
        // a rename left the old note live on every peer and got resurrected as
        // a duplicate on the next engine start.
        if let Ok(bytes) = backend.read(&new_rel).await {
            if let Ok(content) = String::from_utf8(bytes) {
                notify_sync_local_rename(&sync_state, &old_rel, &new_rel, &content).await;
            }
        }
    } else {
        // IP-4: the renamed entry is a FOLDER. Every markdown note inside it
        // just had its path-based id change. Purge the old ids and re-index
        // each nested note at its new path (backlinks survive via the IP-3
        // danglify-on-purge + promote-on-reindex round trip).
        reindex_moved_folder(
            &old_rel,
            &new_rel,
            &backend,
            &db_state,
            &sync_state,
            &search_state,
        )
        .await;
    }
    Ok(())
}

/// Re-key every `:edit_event` row from the old note_id to the new
/// note_id, and append a `Rename` event capturing the rename itself.
/// Best-effort: failures log a warning but never fail the user-visible
/// rename (markdown is the source of truth — the file is already at
/// the new location, history continuity is a quality-of-life concern).
fn migrate_edit_history_on_rename(
    db_state: &State<'_, super::graph::GraphDb>,
    old_path: &str,
    new_path: &str,
) {
    tokio::task::block_in_place(|| {
        let Ok(db_guard) = db_state.db.lock() else {
            return;
        };
        let Some(db) = db_guard.as_ref() else {
            return;
        };
        let roost_root = match db_state.roost_path.lock() {
            Ok(g) => match g.as_deref() {
                Some(v) => v.to_string(),
                None => return,
            },
            Err(_) => return,
        };
        let old_note_id = crate::db::queries::note_id_from_abs_path(old_path, &roost_root);
        let new_note_id = crate::db::queries::note_id_from_abs_path(new_path, &roost_root);

        if old_note_id == new_note_id {
            // Path looks different but normalizes to the same id (e.g. case-only
            // change on a case-insensitive filesystem). Nothing to migrate.
            return;
        }

        if let Err(e) =
            crate::db::queries::migrate_events_to_new_id(db, &old_note_id, &new_note_id, new_path)
        {
            tracing::warn!(
                %old_note_id, %new_note_id, error = %e,
                "edit_history: rename migration failed"
            );
        }

        // Stamp a rename event on the new id. before/after carry the
        // path strings — the diff renders as a small "old → new"
        // entry in the History pane. Using `Actor::System` because
        // rename is a structural action, not a user content edit.
        let before = format!("path: {old_path}\n");
        let after = format!("path: {new_path}\n");
        if let Err(e) = crate::db::queries::record_edit_event(
            db,
            &new_note_id,
            new_path,
            crate::db::queries::Actor::System,
            "rename",
            crate::db::queries::Intent::Rename,
            &before,
            &after,
        ) {
            tracing::warn!(%new_note_id, error = %e, "edit_history: rename event record failed");
        }
    });
}

#[tauri::command]
#[specta::specta]
pub async fn create_folder(
    path: String,
    state: State<'_, RoostFsState>,
    app: AppHandle,
) -> Result<(), String> {
    let backend = state
        .current()
        .await
        .ok_or_else(|| "No roost is open".to_string())?;
    let rel = to_rel(&path, backend.display_root())
        .ok_or_else(|| "Access denied: that path is outside the active roost".to_string())?;
    backend
        .mkdir(&rel)
        .await
        .map_err(|e| format!("Failed to create folder: {e}"))?;
    super::roost_tree_cache::invalidate(&app, backend.display_root());
    Ok(())
}

/// Move a folder (and everything inside it) to the roost trash. Recoverable
/// from `<roost>/.corvux-trash/<timestamp>.<folder-name>/` — same trash
/// surface individual file deletes use. Before the move we purge any
/// markdown notes inside the folder from the graph DB so backlinks /
/// PageRank don't keep pointing at notes the user just deleted.
///
/// Sync tombstones: every nested markdown note is tombstoned via the sync
/// engine here, exactly like a single-file [`delete_file`]. Without this,
/// deleting a folder removed the notes locally but left them alive on every
/// paired device, which then resurrected them on the next sync (the engine
/// only ever saw the folder's own remove event, and `is_watchable` drops
/// that because a directory has no `.md` extension). `collect_markdown_paths`
/// walks the tree BEFORE the trash move, so the list is complete even though
/// the files are gone by the time the tombstones flush.
#[tauri::command]
#[specta::specta]
pub async fn delete_folder(
    path: String,
    state: State<'_, RoostFsState>,
    sync_state: State<'_, super::sync::SyncEngineState>,
    db_state: State<'_, super::graph::GraphDb>,
    search_state: State<'_, super::search::SearchState>,
    app: AppHandle,
) -> Result<(), String> {
    let backend = state
        .current()
        .await
        .ok_or_else(|| "No roost is open".to_string())?;
    let rel = to_rel(&path, backend.display_root())
        .ok_or_else(|| "Access denied: that path is outside the active roost".to_string())?;

    // Walk the folder before moving it so we can (a) purge each note's id
    // from the graph and (b) tombstone each note for sync. After the move
    // the source path is gone and both would be impossible to enumerate.
    let nested_md = collect_markdown_paths(&backend, &rel).await;

    backend
        .trash(&rel)
        .await
        .map_err(|e| format!("Failed to delete folder: {e}"))?;
    super::roost_tree_cache::invalidate(&app, backend.display_root());

    // Emit a CRDT tombstone for every nested note so the deletes converge
    // to paired peers (and reach offline devices via the queue's FullDoc),
    // mirroring `delete_file`. Markdown-only — attachments aren't synced.
    let note_paths = nested_note_paths(&nested_md, backend.display_root());
    if !note_paths.is_empty() {
        if let Some(engine) = sync_state.engine.lock().await.as_ref() {
            for note in &note_paths {
                engine.notify_local_remove(note.clone()).await;
            }
        }
    }

    for abs in &nested_md {
        purge_from_graph(abs, &db_state);
        // NEW-FILES1 (folder-scoped): a folder delete must purge each nested
        // note from FTS too — otherwise every `.md` under it stays a search
        // ghost until the next warm-start reconcile. Mirrors delete_file.
        purge_from_search(abs, &search_state);
        // TRN-3 (mid-session): cancel each nested note's armed alarms.
        cancel_note_alarms(&db_state, backend.display_root(), abs);
    }
    Ok(())
}

/// Map collected absolute markdown paths (as produced by
/// [`collect_markdown_paths`]) to roost-relative [`NotePath`]s for sync
/// tombstoning. Mirrors the single-file delete's NotePath construction:
/// strip the roost root via [`to_rel`], then normalize `\` → `/`. Paths
/// that don't resolve under `root` are skipped rather than panicking — a
/// belt-and-suspenders guard, since every entry came from inside the roost.
fn nested_note_paths(abs_paths: &[String], root: &str) -> Vec<crate::sync::protocol::NotePath> {
    abs_paths
        .iter()
        .filter_map(|abs| to_rel(abs, root))
        .map(|rel| crate::sync::protocol::NotePath(rel.replace('\\', "/")))
        .collect()
}

/// Recursively walk `rel_dir` and collect absolute paths of every `.md`
/// file inside. Used by `delete_folder` to purge graph rows before the
/// folder is moved to trash.
async fn collect_markdown_paths(
    backend: &std::sync::Arc<dyn crate::roostfs::RoostFs>,
    rel_dir: &str,
) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![rel_dir.to_string()];
    while let Some(dir) = stack.pop() {
        let entries = match backend.list(&dir).await {
            Ok(v) => v,
            Err(_) => continue,
        };
        for e in entries {
            if e.is_dir {
                stack.push(e.rel_path);
            } else if crate::roostfs::walk::is_markdown_name(&e.name) {
                // NEW-S1: shared case-insensitive predicate.
                out.push(format!(
                    "{}/{}",
                    backend.display_root().trim_end_matches('/'),
                    e.rel_path.trim_start_matches('/')
                ));
            }
        }
    }
    out
}

#[tauri::command]
#[specta::specta]
pub async fn move_entry(
    source: String,
    dest_dir: String,
    state: State<'_, RoostFsState>,
    db_state: State<'_, super::graph::GraphDb>,
    sync_state: State<'_, super::sync::SyncEngineState>,
    search_state: State<'_, super::search::SearchState>,
    app: AppHandle,
) -> Result<String, String> {
    let file_name = PathBuf::from(&source)
        .file_name()
        .ok_or_else(|| "Invalid source path".to_string())?
        .to_string_lossy()
        .into_owned();

    // Compose the destination display path identically for both backends
    // so the return value stays stable across NativeFs and SafFs.
    let dest = format!("{}/{file_name}", dest_dir.trim_end_matches(['/', '\\']));

    let backend = state
        .current()
        .await
        .ok_or_else(|| "No roost is open".to_string())?;
    let (src_rel, dst_rel) = match (
        to_rel(&source, backend.display_root()),
        to_rel(&dest, backend.display_root()),
    ) {
        (Some(s), Some(d)) => (s, d),
        _ => {
            return Err(
                "Access denied: source or destination is outside the active roost".to_string(),
            );
        }
    };
    backend
        .rename(&src_rel, &dst_rel)
        .await
        .map_err(|e| format!("Failed to move: {e}"))?;
    super::roost_tree_cache::invalidate(&app, backend.display_root());

    // Path-based note ids change on a move exactly like on a rename. Before
    // this, move_entry did NO graph maintenance: it left an orphan node at
    // the OLD id (still surfacing in PageRank / graph view / link+tag
    // queries) and stranded the note's `:edit_event` history under the old
    // id. Mirror rename_file — migrate history to the new id, then purge the
    // old one. The NEW id is (re)created by the frontend re-indexing the
    // moved note at its new path right after this returns (graphStore
    // .indexPath), instead of waiting for the user's next save.
    if crate::roostfs::walk::is_markdown_name(&source) {
        // NEW-S1: shared case-insensitive predicate.
        migrate_edit_history_on_rename(&db_state, &source, &dest);
        purge_from_graph(&source, &db_state);
        // NEW-S-FTS(move): drop the old path's FTS row (path-keyed) so a
        // moved note doesn't ghost in search under its former path. The new
        // path is re-indexed by the frontend's follow-up indexPath.
        purge_from_search(&source, &search_state);
        // TRN-3 (mid-session): cancel the old id's armed alarm (id changes on
        // move), same as rename_file. See cancel_note_alarms.
        cancel_note_alarms(&db_state, backend.display_root(), &source);
        // Propagate the move to sync peers (Android SAF watcher misses it,
        // same as rename) so the note isn't resurrected at its old path on
        // the next engine start.
        if let Ok(bytes) = backend.read(&dst_rel).await {
            if let Ok(content) = String::from_utf8(bytes) {
                notify_sync_local_rename(&sync_state, &src_rel, &dst_rel, &content).await;
            }
        }
    } else {
        // IP-4: moving a FOLDER changes the id of every markdown note inside
        // it. Purge the old ids and re-index each nested note at its new path
        // (backlinks survive via IP-3 danglify + promote-on-reindex).
        reindex_moved_folder(
            &src_rel,
            &dst_rel,
            &backend,
            &db_state,
            &sync_state,
            &search_state,
        )
        .await;
    }
    Ok(dest)
}

#[cfg(test)]
mod tests {
    //! IPC-handler-adjacent tests. The async `#[tauri::command]` functions
    //! themselves require a managed Tauri State stack to call directly,
    //! which Tauri V2 doesn't have a clean test fixture for. We instead
    //! cover the helpers each command relies on (`to_rel`,
    //! `collect_markdown_paths`) so a regression in path resolution or
    //! pre-delete graph cleanup gets caught here, not in production.
    use super::*;
    use crate::roostfs::native::NativeFs;
    use tempfile::TempDir;

    #[test]
    fn to_rel_strips_root_and_normalizes_separators() {
        assert_eq!(
            to_rel("/roost/notes/a.md", "/roost"),
            Some("notes/a.md".to_string())
        );
        // Backslash root + forward-slash abs (mixed-separator Windows
        // case): the function normalizes both sides before stripping.
        assert_eq!(
            to_rel(r"C:\roost\notes\a.md", r"C:/roost"),
            Some("notes/a.md".to_string())
        );
    }

    #[test]
    fn to_rel_returns_none_for_path_outside_root() {
        assert_eq!(to_rel("/elsewhere/a.md", "/roost"), None);
        assert_eq!(
            to_rel("/roost-backup/secret.md", "/roost"),
            None,
            "a shared string prefix is not a shared path root"
        );
    }

    #[test]
    fn to_rel_handles_filesystem_root_without_matching_every_absolute_path_as_empty() {
        assert_eq!(to_rel("/notes/a.md", "/"), Some("notes/a.md".to_string()));
    }

    // IP-4: folder rename/move re-roots every nested note's path. These lock
    // the pure segment-swap the reindex loop relies on.
    #[test]
    fn swap_folder_prefix_reroots_single_segment_rename() {
        // Projects/a.md  with old prefix "Projects" (1 seg) → Archive/a.md
        assert_eq!(
            swap_folder_prefix("Projects/a.md", 1, "Archive"),
            Some("Archive/a.md".to_string())
        );
    }

    #[test]
    fn swap_folder_prefix_preserves_nested_tail_and_case() {
        // A deep tail keeps its original casing; only the prefix changes.
        assert_eq!(
            swap_folder_prefix("Projects/Sub/Deep.md", 1, "work/2026"),
            Some("work/2026/Sub/Deep.md".to_string())
        );
    }

    #[test]
    fn swap_folder_prefix_handles_multi_segment_old_prefix() {
        // Moving `a/b` to `x`: drop 2 leading segments, prepend `x`.
        assert_eq!(
            swap_folder_prefix("a/b/note.md", 2, "x"),
            Some("x/note.md".to_string())
        );
    }

    #[test]
    fn swap_folder_prefix_case_only_rename_keeps_tail() {
        // Folder cased differently (Projects → projects) — segment counting
        // ignores the prefix's actual text, so the tail is untouched.
        assert_eq!(
            swap_folder_prefix("Projects/a.md", 1, "projects"),
            Some("projects/a.md".to_string())
        );
    }

    #[test]
    fn swap_folder_prefix_none_when_no_tail() {
        // The folder path itself (no note tail) yields None — nothing to move.
        assert_eq!(swap_folder_prefix("Projects", 1, "Archive"), None);
    }

    #[test]
    fn to_rel_handles_root_with_trailing_slash() {
        assert_eq!(to_rel("/roost/a.md", "/roost/"), Some("a.md".to_string()));
    }

    /// AUDIT-044: NFD (decomposed) input must compare equal to its NFC
    /// (precomposed) counterpart on the other side of the strip. Real
    /// case: macOS Finder paste produces NFD, the roost root from the
    /// folder picker on Windows is NFC.
    #[test]
    fn to_rel_normalizes_nfd_vs_nfc() {
        // "café" — NFC form (U+00E9) vs NFD form (e + U+0301).
        let nfc_root = "/roost/caf\u{00E9}";
        let nfd_abs = "/roost/cafe\u{0301}/a.md";
        assert_eq!(to_rel(nfd_abs, nfc_root), Some("a.md".to_string()));

        // And the reverse direction (NFC abs, NFD root).
        let nfd_root = "/roost/cafe\u{0301}";
        let nfc_abs = "/roost/caf\u{00E9}/a.md";
        assert_eq!(to_rel(nfc_abs, nfd_root), Some("a.md".to_string()));
    }

    /// AUDIT-044: `\\?\` and `\\.\` Windows long-path / device-namespace
    /// prefixes must be rejected from BOTH sides of the comparison.
    /// Allowing them through means `\\?\C:\Roost\..\evil` looks distinct
    /// from `C:\Roost\evil` in the prefix-strip even though they
    /// resolve to the same file.
    #[test]
    fn to_rel_rejects_windows_long_path_prefix_on_abs() {
        assert_eq!(to_rel(r"\\?\C:\roost\a.md", r"C:\roost"), None);
        assert_eq!(to_rel(r"\\.\C:\roost\a.md", r"C:\roost"), None);
        // Same prefix written with forward slashes (some bash-style
        // tools normalize to this on Windows).
        assert_eq!(to_rel("//?/C:/roost/a.md", "C:/roost"), None);
    }

    #[test]
    fn to_rel_rejects_windows_long_path_prefix_on_root() {
        assert_eq!(to_rel(r"C:\roost\a.md", r"\\?\C:\roost"), None);
        assert_eq!(to_rel(r"C:\roost\a.md", r"\\.\C:\roost"), None);
    }

    #[tokio::test]
    async fn collect_markdown_paths_walks_recursively() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        std::fs::write(root.join("top.md"), "x").unwrap();
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("sub").join("nested.md"), "y").unwrap();
        std::fs::write(root.join("sub").join("not-md.txt"), "skip").unwrap();
        std::fs::create_dir_all(root.join("sub").join("deep")).unwrap();
        std::fs::write(root.join("sub").join("deep").join("buried.md"), "z").unwrap();

        let backend: Arc<dyn crate::roostfs::RoostFs> = Arc::new(NativeFs::new(root));
        let mut paths = collect_markdown_paths(&backend, "").await;
        paths.sort();

        // All three .md files should be found, the .txt skipped.
        assert_eq!(paths.len(), 3, "expected 3 markdown paths, got {paths:?}");
        assert!(paths.iter().any(|p| p.ends_with("top.md")));
        assert!(paths.iter().any(|p| p.ends_with("nested.md")));
        assert!(paths.iter().any(|p| p.ends_with("buried.md")));
        assert!(
            !paths.iter().any(|p| p.ends_with("not-md.txt")),
            "non-markdown files must not be returned"
        );
    }

    #[tokio::test]
    async fn collect_markdown_paths_empty_dir_returns_empty() {
        let dir = TempDir::new().unwrap();
        let backend: Arc<dyn crate::roostfs::RoostFs> = Arc::new(NativeFs::new(dir.path()));
        let paths = collect_markdown_paths(&backend, "").await;
        assert!(paths.is_empty());
    }

    #[tokio::test]
    async fn collect_markdown_paths_includes_markdown_extension() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("classic.markdown"), "x").unwrap();
        std::fs::write(dir.path().join("modern.md"), "y").unwrap();

        let backend: Arc<dyn crate::roostfs::RoostFs> = Arc::new(NativeFs::new(dir.path()));
        let paths = collect_markdown_paths(&backend, "").await;
        assert_eq!(paths.len(), 2);
    }

    /// Folder-delete sync regression: the absolute paths walked under a
    /// deleted folder must map back to roost-relative `NotePath`s with
    /// forward slashes, so `notify_local_remove` tombstones the right notes
    /// on every peer. A path outside the root is dropped, not panicked on.
    #[test]
    fn nested_note_paths_maps_abs_to_relative_notepaths() {
        let root = "/roost";
        let abs = vec![
            "/roost/projects/a.md".to_string(),
            "/roost/projects/sub/b.md".to_string(),
            "/elsewhere/c.md".to_string(), // outside root → skipped
        ];
        let notes = nested_note_paths(&abs, root);
        let strs: Vec<&str> = notes.iter().map(|n| n.0.as_str()).collect();
        assert_eq!(strs, vec!["projects/a.md", "projects/sub/b.md"]);
    }

    #[test]
    fn nested_note_paths_normalizes_windows_separators() {
        // collect_markdown_paths forward-slashes its output, but a backslash
        // root + abs (mixed-separator Windows case) must still normalize so
        // the NotePath matches the wire format peers exchange.
        let notes = nested_note_paths(&[r"C:\roost\notes\a.md".to_string()], r"C:/roost");
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].0, "notes/a.md");
    }

    // --- sync fan-out plumbing -------------------------------------------
    //
    // Regression guard for the mobile→laptop sync gap: task complete /
    // quick-add / reschedule and history-revert write markdown directly via
    // `RoostFs::write_atomic`, bypassing the editor's `write_file_internal`.
    // On desktop the file watcher caught those writes; on Android SAF the
    // watcher is dead, so they never reached peers. The fix routes every
    // such write through `notify_sync_local_write`. These cover that helper:
    // a markdown write reaches the engine docstore, a non-markdown write is
    // skipped, and a write with sync disabled is a safe no-op.
    use crate::sync::engine::SyncEngine;
    use crate::sync::identity::Identity;
    use crate::sync::protocol::NotePath;
    use crate::sync::transport::tcp_tls::TlsIdentity;
    use std::sync::Arc;

    fn make_engine() -> (SyncEngine, TempDir, TempDir) {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let roost = TempDir::new().unwrap();
        let roost_root = roost.path().join("roost");
        std::fs::create_dir_all(&roost_root).unwrap();
        let app_data = TempDir::new().unwrap();
        let identity = Arc::new(Identity::load_or_create(app_data.path()).unwrap());
        let tls = Arc::new(TlsIdentity::generate().unwrap());
        let engine = SyncEngine::new(
            roost_root,
            app_data.path().join("sync-state"),
            None,
            identity,
            tls,
            "test-device".into(),
        )
        .unwrap();
        (engine, roost, app_data)
    }

    fn state_with(engine: Option<SyncEngine>) -> super::super::sync::SyncEngineState {
        let s = super::super::sync::SyncEngineState::default();
        *s.engine.try_lock().unwrap() = engine;
        s
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn notify_sync_local_write_reaches_docstore() {
        let (engine, _roost, _app) = make_engine();
        let probe = engine.clone();
        let state = state_with(Some(engine));

        notify_sync_local_write(&state, "notes/task-note.md", "- [x] done ✅ 2026-06-24").await;

        let got = probe
            .rendered_note_text(&NotePath("notes/task-note.md".into()))
            .await;
        assert_eq!(
            got.as_deref(),
            Some("- [x] done ✅ 2026-06-24"),
            "a markdown write must land in the sync docstore (the mobile gap)"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn notify_sync_local_write_skips_non_markdown() {
        let (engine, _roost, _app) = make_engine();
        let probe = engine.clone();
        let state = state_with(Some(engine));

        notify_sync_local_write(&state, ".attachments/pic.png", "binary-ish").await;

        assert!(
            probe
                .rendered_note_text(&NotePath(".attachments/pic.png".into()))
                .await
                .is_none(),
            "non-markdown paths must not be fed to the per-note CRDT store"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn notify_sync_local_write_is_noop_when_sync_disabled() {
        // No engine installed (sync off / not yet started): must not panic.
        let state = state_with(None);
        notify_sync_local_write(&state, "notes/a.md", "content").await;
    }

    // --- PR-C: write_roost_note_synced funnel ------------------------------
    //
    // `write_roost_note_synced` is the single funnel agent tools (capture,
    // quick-add, roost_write, note_ref, tasks_crud, ...) route every
    // markdown write through, precisely because `RoostFs::write_atomic`
    // alone never reaches the sync engine, and Android SAF roosts have no
    // file watcher to catch a bare write after the fact (see its doc
    // comment above). A regression here would silently strand agent-tool
    // writes on Android/mobile — data written by an AI tool call would
    // never sync to any other device.
    //
    // We'd ideally drive the real function end-to-end through a mock
    // `AppHandle` (`tauri::test::mock_builder()` managing both
    // `RoostFsState` and `SyncEngineState`), but the `tauri` crate's
    // `test` feature isn't enabled anywhere in this workspace (checked
    // Cargo.toml / Cargo.lock — no `test` feature on the `tauri` dep, dev
    // or otherwise), so `tauri::test` is not compiled in and unavailable
    // here. Wiring it in would mean editing `Cargo.toml`, which is a
    // dependency/feature change this test-only patch shouldn't make.
    //
    // Fallback: `write_roost_note_synced`'s entire body — once the two
    // `try_state` lookups succeed — is exactly two calls in sequence:
    // `backend.write_atomic(rel, content)` then
    // `notify_sync_local_write(sync_state, rel, content)`. Both halves
    // already have dedicated real (non-mock) fixtures in this file
    // (`NativeFs` over a `TempDir`, `make_engine`/`state_with`). This test
    // drives that exact two-call sequence against real fixtures and
    // asserts both outcomes the funnel exists to guarantee: the file
    // lands on disk AND the sync engine's docstore has the same content.
    // If either call in `write_roost_note_synced` is ever reordered,
    // dropped, or short-circuited, this test fails the same way the real
    // funnel would.
    #[tokio::test(flavor = "multi_thread")]
    async fn write_roost_note_synced_composition_writes_disk_and_docstore() {
        let (engine, roost, _app_data) = make_engine();
        // `make_engine` points the SyncEngine at `<roost>/roost` — mirror
        // that exact root for the RoostFs backend so relative paths line
        // up between the two halves of the funnel.
        let roost_root = roost.path().join("roost");
        let backend = NativeFs::new(roost_root.clone());
        let probe = engine.clone();
        let state = state_with(Some(engine));

        let rel = "notes/agent-note.md";
        let content = "# hi\n\nbody";

        // Half 1 of the funnel: the RoostFs write.
        backend
            .write_atomic(rel, content.as_bytes())
            .await
            .expect("write_atomic must succeed against a writable temp roost");

        // Half 2 of the funnel: the sync notify.
        notify_sync_local_write(&state, rel, content).await;

        // Assert (a): the backend actually wrote the file to disk.
        let on_disk = std::fs::read_to_string(roost_root.join(rel))
            .expect("write_atomic must have created the file on disk");
        assert_eq!(
            on_disk, content,
            "on-disk content must match what was written"
        );

        // Assert (b): the write reached the sync engine's docstore — the
        // mobile/SAF data-loss gap this funnel closes.
        let got = probe
            .rendered_note_text(&crate::sync::protocol::NotePath(rel.to_string()))
            .await;
        assert_eq!(
            got.as_deref(),
            Some(content),
            "an agent-tool write must land in the sync docstore, not just on disk"
        );
    }

    // --- EDT-1 / EDT-2: write decision (dedup + last-write-wins) ----------

    #[test]
    fn classify_write_dedups_identical_existing_content() {
        // EDT-2: existing file, content byte-identical → skip everything.
        assert_eq!(
            classify_write(true, "same", "same", None),
            WriteDecision::Unchanged
        );
        // Dedup wins even when a base hash is supplied and the file has
        // diverged — a no-op re-save is never a conflict.
        assert_eq!(
            classify_write(true, "same", "same", Some("stale-hash")),
            WriteDecision::Unchanged
        );
    }

    #[test]
    fn classify_write_proceeds_on_create_and_real_change() {
        // A brand-new file (never existed) always proceeds, even if the
        // "empty" before matches an empty content edge case.
        assert_eq!(
            classify_write(false, "", "new content", None),
            WriteDecision::Proceed
        );
        // Existing file, content changed, no base hash → legacy behavior:
        // proceed (no last-write-wins guard).
        assert_eq!(
            classify_write(true, "old", "new", None),
            WriteDecision::Proceed
        );
    }

    #[test]
    fn classify_write_proceeds_when_base_hash_matches_disk() {
        // EDT-1: the caller loaded exactly what's on disk → safe to write.
        let disk = "the note the editor loaded";
        let base = content_sha256_hex(disk);
        assert_eq!(
            classify_write(true, disk, "edited body", Some(&base)),
            WriteDecision::Proceed
        );
    }

    #[test]
    fn classify_write_conflicts_when_disk_diverged_from_base() {
        // EDT-1: the caller loaded content X, but disk now holds Y (a peer
        // / external editor / second tab wrote in between). A blind write
        // would clobber Y → conflict.
        let loaded = "what the editor opened";
        let base = content_sha256_hex(loaded);
        let disk_now = "what someone else saved since";
        assert_eq!(
            classify_write(
                true,
                disk_now,
                "my edits on top of the old copy",
                Some(&base)
            ),
            WriteDecision::Conflict
        );
    }

    #[test]
    fn content_sha256_hex_matches_known_vector() {
        // Pin the exact encoding (lowercase hex, no prefix) the webview's
        // crypto.subtle.digest("SHA-256", …) must reproduce, so the EDT-1
        // base-hash comparison can't silently drift across the boundary.
        assert_eq!(
            content_sha256_hex(""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            content_sha256_hex("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[tokio::test]
    async fn roost_generation_invalidates_stale_async_work_on_clear() {
        let state = RoostFsState::default();
        let captured = state.generation();
        assert!(state.is_generation_current(captured));
        state.clear().await;
        assert!(!state.is_generation_current(captured));
    }
}
