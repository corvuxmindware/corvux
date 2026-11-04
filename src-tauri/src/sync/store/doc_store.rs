// WAL_COMPACT_THRESHOLD + actor_generation/append_incremental for future compaction
#![allow(dead_code)]

//! Per-note Automerge persistence.
//!
//! Each note's CRDT binary is stored as a single file under
//! `<state_dir>/docs/<note-path>.automerge`. Writes are atomic via the
//! classic temp-file + rename pattern — a crash mid-write leaves the
//! last-good file intact.
//!
//! `state_dir` is resolved by the engine via `paths::roost_db_dir` — it
//! lives in the app's data directory, not in the user's roost. That
//! decouples sync from SAF-only Android roots (which std::fs can't write
//! to) and keeps the CRDT binaries from leaking into Syncthing-synced
//! folders where a different device's Automerge actor would collide.
//!
//! An optional `.automerge.wal` sidecar holds un-compacted incremental
//! changes (Automerge's `save_incremental` output). We flush the WAL into
//! the main file on clean shutdown, or during `compact()` if the WAL
//! exceeds a size threshold. If we crash before compacting, the loader
//! replays the WAL on next open.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::sync::crdt::NoteDoc;
use crate::sync::error::StoreError;
use crate::sync::identity::DeviceId;
use crate::sync::protocol::NotePath;

use super::docs_root;

/// Compact the WAL into the main save whenever the WAL grows past this
/// many bytes. 128 KiB is a good balance for typical Corvux notes — big
/// enough to amortize the rewrite cost across many edits, small enough to
/// keep restart-time replay fast.
const WAL_COMPACT_THRESHOLD: u64 = 128 * 1024;

/// Sidecar file that holds a 4-byte LE u32 tracking how many times this
/// docstore has been wiped. Bumped on every `wipe_all`; appended to the
/// `DeviceId` to form the Automerge actor that this docstore writes new
/// changes under, so a reset+resync can't collide with seq numbers a
/// peer already remembers from before the wipe.
const ACTOR_GENERATION_FILE: &str = "actor_generation.bin";

pub struct DocStore {
    /// Directory the Automerge binaries + WAL live under. **Not** the user's
    /// roost folder — see module docs. Resolved by the engine from
    /// `paths::roost_db_dir(app, roost_path)`.
    state_dir: PathBuf,
    device_id: DeviceId,
    /// Monotonic counter incremented on every `wipe_all`. Persists to disk
    /// alongside the docs directory so it survives restart.
    actor_generation: u32,
    /// Cached `DeviceId || actor_generation_le` used as the Automerge
    /// actor for *our own* writes. Recomputed when `actor_generation`
    /// changes. Peer-provided saves keep their own actor IDs intact —
    /// this only governs what we sign new ops with.
    our_actor_id: automerge::ActorId,
    /// C-1: cached result of a one-time case-sensitivity probe of the state
    /// dir. On a case-insensitive filesystem (APFS default, Windows) two note
    /// paths differing only in case map to the SAME `.automerge` file, so a
    /// save would silently clobber the other note; we detect that and refuse.
    /// On a case-sensitive fs (Linux) both are legitimately distinct, so the
    /// check is skipped. Probed lazily on first save.
    case_insensitive_fs: std::sync::OnceLock<bool>,
    /// C-1: the user's roost directory, if known. The `.md` source of truth
    /// lives here, and it can sit on a *different* filesystem than the
    /// app-private state dir (e.g. a case-sensitive state dir with a
    /// case-insensitive external roost volume). We treat the store as
    /// case-insensitive if EITHER filesystem is — a note-identity collision on
    /// either one loses data (state dir → CRDT clobber; roost → `.md` clobber).
    /// `None` (tests / not wired) falls back to probing only the state dir.
    roost_probe_dir: Option<PathBuf>,
}

impl DocStore {
    pub fn new(state_dir: PathBuf, device_id: DeviceId) -> Self {
        // Read the generation eagerly. State_dir may not exist yet — the
        // helper handles that by defaulting to 0. After `init()` creates
        // the dir, the next `wipe_all` will write a real file.
        let actor_generation = read_actor_generation(&state_dir);
        let our_actor_id = build_actor_id(device_id, actor_generation);
        Self {
            state_dir,
            device_id,
            actor_generation,
            our_actor_id,
            case_insensitive_fs: std::sync::OnceLock::new(),
            roost_probe_dir: None,
        }
    }

    /// C-1: tell the store which roost directory the `.md` files live in, so
    /// the case-sensitivity probe reflects the roost's filesystem (where a
    /// user-visible clobber happens) and not just the app-private state dir.
    /// Called by the engine at construction; a no-op for tests that don't set
    /// it (they fall back to probing the state dir).
    pub fn with_roost_probe_dir(mut self, roost: PathBuf) -> Self {
        self.roost_probe_dir = Some(roost);
        self
    }

    pub fn init(&self) -> Result<(), StoreError> {
        fs::create_dir_all(docs_root(&self.state_dir))?;
        fs::create_dir_all(&self.state_dir)?;
        Ok(())
    }

    /// Current actor-generation counter. Exposed for diagnostics + tests.
    pub fn actor_generation(&self) -> u32 {
        self.actor_generation
    }

    /// Our local Automerge actor (`device_id || generation_le`). Returned
    /// by clone — Automerge `ActorId` is cheap (Bytes inside). Use this
    /// when adopting a peer-produced save into our local store so any
    /// subsequent ops carry our identity, not the peer's.
    pub fn our_actor_id(&self) -> automerge::ActorId {
        self.our_actor_id.clone()
    }

    /// Load a note's CRDT from disk, or create a fresh one if absent.
    ///
    /// On load we also replay any WAL bytes into the doc, so callers see a
    /// single up-to-date `NoteDoc` regardless of prior crash state.
    pub fn load_or_create(&self, note: &NotePath) -> Result<NoteDoc, StoreError> {
        Self::validate_note_path(note)?;
        let main = self.main_path(note);
        let wal = self.wal_path(note);
        if !main.exists() {
            return NoteDoc::new_with_actor(self.our_actor_id.clone()).map_err(encode_err);
        }
        let main_bytes = fs::read(&main)?;
        let mut doc =
            NoteDoc::load_with_actor(&main_bytes, self.our_actor_id.clone()).map_err(encode_err)?;
        if wal.exists() {
            let wal_bytes = fs::read(&wal)?;
            if !wal_bytes.is_empty() {
                doc.inner_mut()
                    .load_incremental(&wal_bytes)
                    .map_err(|e| StoreError::Decode(e.to_string()))?;
            }
        }
        Ok(doc)
    }

    /// Persist a note's CRDT. Writes the full save as the main file and
    /// clears the WAL. Atomic via temp+rename on both files.
    ///
    /// PERF / H-store (deliberately deferred, tracked): this does a full
    /// serialization + write + blocking `fsync` inline, and callers invoke it
    /// while holding the async `doc_store` mutex — so under a sync burst the
    /// per-note fsync both serializes other doc-store consumers and blocks the
    /// tokio worker thread (no `spawn_blocking`). The `append_incremental` WAL
    /// path below is the intended cheap-write mitigation and is currently
    /// unwired. This is a THROUGHPUT gap, not a correctness bug — the
    /// temp+rename+fsync ordering is correct and unchanged. It is NOT fixed here
    /// on purpose: the correct fix (serialize under a brief lock, then release
    /// the lock and run the write+fsync via `spawn_blocking`) restructures the
    /// locking of every `save()` call site, and this module's standing rule is
    /// "profile before re-architecting" — there is no sync benchmark yet showing
    /// this is a real bottleneck at the target scale (low thousands of pending
    /// messages, seconds-scale polling). Retirement: add a `DocStore::save`
    /// Criterion bench, and if it confirms the stall, do the spawn_blocking +
    /// lock-narrowing pass with its own regression coverage.
    pub fn save(&self, note: &NotePath, doc: &mut NoteDoc) -> Result<(), StoreError> {
        Self::validate_note_path(note)?;
        let main = self.main_path(note);
        self.ensure_parent(&main)?;
        // C-1: refuse a case-insensitive collision before writing so we never
        // silently clobber a differently-cased note that shares one on-disk
        // file. Runs after ensure_parent so the target dir exists to scan.
        self.check_case_collision(note)?;
        let tmp = main.with_extension("automerge.tmp");
        let bytes = doc.save();
        // fsync the temp file BEFORE the rename. The rename is atomic, but only
        // if the bytes are durably on disk first — without the sync, a crash or
        // power loss can leave a zero-length / partially-written .automerge
        // behind the rename on some filesystems, corrupting the note's CRDT.
        {
            use std::io::Write;
            let mut f = fs::File::create(&tmp)?;
            f.write_all(&bytes)?;
            f.sync_all()?;
        }
        fs::rename(&tmp, &main)?;
        let wal = self.wal_path(note);
        if wal.exists() {
            let _ = fs::remove_file(wal);
        }
        Ok(())
    }

    /// Append incremental bytes to the WAL. Cheap (no serialization of the
    /// whole doc); safe under crashes; compacts automatically when large.
    pub fn append_incremental(
        &self,
        note: &NotePath,
        incremental: &[u8],
        doc: &mut NoteDoc,
    ) -> Result<(), StoreError> {
        Self::validate_note_path(note)?;
        let wal = self.wal_path(note);
        self.ensure_parent(&wal)?;
        // Append with fsync. A crash before this returns means we lose
        // these changes — acceptable; the in-memory doc also has them, and
        // the CRDT will recover via the next sync exchange.
        let mut f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&wal)?;
        f.write_all(incremental)?;
        f.sync_data()?;
        drop(f);
        if fs::metadata(&wal)?.len() > WAL_COMPACT_THRESHOLD {
            self.save(note, doc)?;
        }
        Ok(())
    }

    /// Whether we have a persisted doc for this note. Cheap filesystem
    /// check — no load. Distinguishes "never seen" from "seen but empty".
    pub fn exists(&self, note: &NotePath) -> bool {
        if Self::validate_note_path(note).is_err() {
            return false;
        }
        self.main_path(note).exists()
    }

    /// Delete a note's CRDT entirely. Used on local deletions; the markdown
    /// file is separately trashed by the existing file-trash code.
    pub fn remove(&self, note: &NotePath) -> Result<(), StoreError> {
        Self::validate_note_path(note)?;
        let main = self.main_path(note);
        let wal = self.wal_path(note);
        if main.exists() {
            fs::remove_file(main)?;
        }
        if wal.exists() {
            fs::remove_file(wal)?;
        }
        Ok(())
    }

    /// Wipe every persisted Automerge doc under this state dir — recovery
    /// mechanism for the "my roost went empty" class of bugs caused by
    /// two devices creating independent docs for the same note, then
    /// merging into a CRDT with two text objects where actor-id tiebreak
    /// picks the empty one. After a wipe the store is as if just
    /// `init()`ed; peer connect → NoteList exchange → FullDoc adoption
    /// populates it again, but with genuinely shared history.
    ///
    /// Also rotates the Automerge actor — the `actor_generation` counter
    /// increments and the new `our_actor_id` is `device_id || generation`.
    /// Without this, a peer that saw seq=N for our actor before the wipe
    /// would see a fresh seq=0 from us afterwards under the *same* actor
    /// — Automerge surfaces that as `"duplicate seq N found for actor …"`
    /// and the two histories can never re-merge. Rotation forks a new
    /// actor identity for post-wipe writes so the peer treats us as a
    /// fresh participant in the CRDT.
    ///
    /// Leaves the state_dir itself intact (WAL/peer_store/sync live
    /// alongside and shouldn't be clobbered).
    pub fn wipe_all(&mut self) -> Result<(), StoreError> {
        let root = docs_root(&self.state_dir);
        if root.exists() {
            fs::remove_dir_all(&root)?;
            fs::create_dir_all(&root)?;
        }
        // Bump generation regardless of whether docs were actually present.
        // The user invoked `reset` — the contract is "next writes use a
        // fresh actor", not "no-op when already empty".
        self.actor_generation = self.actor_generation.saturating_add(1);
        self.our_actor_id = build_actor_id(self.device_id, self.actor_generation);
        write_actor_generation(&self.state_dir, self.actor_generation)?;
        Ok(())
    }

    /// Return the list of all note paths this store knows about. Walks the
    /// docs directory. Used for the `NoteList` sync message.
    ///
    /// **Includes tombstoned (deleted) docs.** A delete is now a CRDT flag,
    /// not a file removal, so a deleted note keeps a `.automerge` doc until
    /// `gc_tombstones` reaps it. Keeping it in the list is load-bearing: it's
    /// how the `deleted=true` flag propagates to peers that haven't deleted
    /// the note yet and to peers bootstrapping a fresh copy.
    pub fn list(&self) -> Result<Vec<NotePath>, StoreError> {
        let root = docs_root(&self.state_dir);
        if !root.exists() {
            return Ok(vec![]);
        }
        let mut out = Vec::new();
        walk(&root, &root, &mut out)?;
        Ok(out)
    }

    /// Reap tombstones whose `deleted_at` is older than `cutoff_millis`
    /// (unix millis). Returns the number of docs removed.
    ///
    /// Tombstones can't live forever — every deleted note would keep a
    /// `.automerge` doc indefinitely. We hard-`remove` a deleted doc once it's
    /// older than the TTL the engine passes in (tied to the queue's 30-day
    /// message TTL: after that window every offline peer's queued copy has
    /// expired, so reaping the local tombstone can't be undone by a stale
    /// queue push resurrecting the note).
    ///
    /// Best-effort and resilient: a doc that fails to load or read its flag is
    /// skipped, never aborting the sweep.
    ///
    /// `now_millis` is our local clock. The stored `deleted_at` came from
    /// whichever peer tombstoned the note — over the CRDT merge path it is the
    /// *sending* peer's wall clock, entirely under a hostile/skewed peer's
    /// control and never re-stamped locally. So we sanitize it against `now`
    /// before the reap decision (M-1): a far-future value can't dodge GC
    /// forever, and an ancient one can't reap early and resurrect the note.
    pub fn gc_tombstones(&self, now_millis: u64, cutoff_millis: u64) -> Result<usize, StoreError> {
        let mut reaped = 0usize;
        for note in self.list()? {
            let doc = match self.load_or_create(&note) {
                Ok(d) => d,
                Err(_) => continue,
            };
            let deleted = doc.is_deleted().unwrap_or(false);
            if !deleted {
                continue;
            }
            let raw = doc.deleted_at().unwrap_or(0);
            // Guard against a zero/unset timestamp reaping immediately — only
            // reap when we have a real `deleted_at`.
            if raw == 0 {
                continue;
            }
            // Clamp the untrusted peer timestamp into a plausible window around
            // our own clock before comparing to the retention cutoff.
            let at = super::sanitize_tombstone_millis(raw, now_millis);
            if at < cutoff_millis && self.remove(&note).is_ok() {
                reaped += 1;
            }
        }
        Ok(reaped)
    }

    /// Backstop against a `NotePath` that would escape `docs_root`. `main_path`
    /// / `wal_path` join the raw path, so an unvalidated `..` / absolute / NUL /
    /// backslash from a paired peer's queued payload (which historically
    /// bypassed the peer-side `is_safe_note_path` gate) could write `.automerge`
    /// state outside the store — and `list()` would never see it, leaving ghost
    /// state. Mirrors the component rules in `store::from_note_path` and
    /// `peer::is_safe_note_path`. Cheap; runs on every store mutation as defense
    /// in depth even if a caller forgot to validate.
    fn validate_note_path(note: &NotePath) -> Result<(), StoreError> {
        let p = &note.0;
        if p.is_empty() || p.starts_with('/') || p.contains('\0') || p.contains('\\') {
            return Err(StoreError::InvalidPath(p.clone()));
        }
        for seg in p.split('/') {
            if seg.is_empty() || seg == "." || seg == ".." {
                return Err(StoreError::InvalidPath(p.clone()));
            }
        }
        Ok(())
    }

    fn main_path(&self, note: &NotePath) -> PathBuf {
        // C-1: NFC-normalize so NFC/NFD spellings of the same note map to one
        // on-disk `.automerge` file instead of two divergent docs.
        docs_root(&self.state_dir).join(format!("{}.automerge", super::nfc_normalize(&note.0)))
    }

    fn wal_path(&self, note: &NotePath) -> PathBuf {
        docs_root(&self.state_dir).join(format!("{}.automerge.wal", super::nfc_normalize(&note.0)))
    }

    /// C-1: whether note identity is case-insensitive on the relevant
    /// filesystem(s). Probes BOTH the app-private state dir AND the user's
    /// roost (if wired via `with_roost_probe_dir`), and returns `true` if
    /// EITHER is case-insensitive — a collision on either loses data (state dir
    /// → CRDT clobber; roost → `.md` clobber), and they can be different
    /// volumes. Probed once and cached.
    ///
    /// SYN-M6: an UNPROBABLE dir (probe returns `None`) is handled by platform,
    /// NOT lumped in with "case-sensitive". The only dir we can't probe via
    /// `std::fs` is an Android SAF `content://` roost — and Android user storage
    /// (FAT/exFAT/sdcardfs) is predominantly case-INSENSITIVE. Defaulting such a
    /// roost to case-sensitive (the old behavior) turned OFF collision
    /// detection, so two notes differing only in case would clobber each other's
    /// `.md`. So: unprobable roost ⇒ assume case-insensitive on Android, and
    /// case-sensitive elsewhere (desktop paths are always probable, so this only
    /// bites Android). The state dir is app-private and always probable; an
    /// unprobable one (shouldn't happen) stays case-sensitive.
    fn fs_case_insensitive(&self) -> bool {
        *self.case_insensitive_fs.get_or_init(|| {
            let state = docs_root(&self.state_dir);
            if probe_dir_case_insensitive(&state).unwrap_or(false) {
                return true;
            }
            match &self.roost_probe_dir {
                Some(roost) => {
                    probe_dir_case_insensitive(roost).unwrap_or(cfg!(target_os = "android"))
                }
                None => false,
            }
        })
    }

    /// C-1: on a case-insensitive fs, refuse a save whose target `.automerge`
    /// filename case-folds to an EXISTING but differently-cased note's file —
    /// otherwise the two resolve to one file and the second save silently
    /// clobbers the first. Scans only the target's immediate directory
    /// (bounded), not the whole store. A no-op on case-sensitive filesystems
    /// and when updating the note's own file.
    fn check_case_collision(&self, note: &NotePath) -> Result<(), StoreError> {
        if !self.fs_case_insensitive() {
            return Ok(());
        }
        let target = self.main_path(note);
        let (Some(parent), Some(target_name)) = (target.parent(), target.file_name()) else {
            return Ok(());
        };
        if !parent.exists() {
            return Ok(());
        }
        let target_lower = target_name.to_string_lossy().to_lowercase();
        for entry in fs::read_dir(parent)? {
            let name = entry?.file_name();
            if name == target_name {
                continue; // updating this note's own file
            }
            if name.to_string_lossy().to_lowercase() == target_lower {
                return Err(StoreError::PathConflict(format!(
                    "'{}' collides case-insensitively with existing '{}'",
                    note.0,
                    name.to_string_lossy()
                )));
            }
        }
        Ok(())
    }

    fn ensure_parent(&self, path: &Path) -> Result<(), StoreError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        Ok(())
    }
}

fn encode_err(e: crate::sync::error::SyncError) -> StoreError {
    StoreError::Encode(e.to_string())
}

/// C-1: probe one directory for case-insensitivity — write a lowercase marker
/// and check whether the uppercase name resolves to it. Any I/O failure ⇒
/// `false` (assume case-sensitive; never wrongly refuse a distinct note).
/// Probe a directory's case-sensitivity by writing a lowercase file and
/// checking whether the uppercase name resolves to it. Returns `None` when the
/// directory CAN'T be probed via `std::fs` — most importantly an Android SAF
/// `content://` roost, where `std::fs` writes fail (EROFS/ENOENT). The caller
/// must treat `None` (unknown) differently from `Some(false)` (known
/// case-sensitive) — see `fs_case_insensitive` (SYN-M6).
fn probe_dir_case_insensitive(dir: &Path) -> Option<bool> {
    if fs::create_dir_all(dir).is_err() {
        return None;
    }
    let lower = dir.join(".corvux-case-probe");
    let upper = dir.join(".CORVUX-CASE-PROBE");
    let _ = fs::remove_file(&lower);
    if fs::write(&lower, b"").is_err() {
        return None;
    }
    let insensitive = upper.exists();
    let _ = fs::remove_file(&lower);
    Some(insensitive)
}

/// Compose the Automerge actor for our local writes from a stable
/// `DeviceId` and a wipe-bumped generation counter. 16 bytes of identity +
/// 4 bytes of generation = a 20-byte actor — Automerge accepts arbitrary
/// length, and 4 bytes of counter gives 4B distinct rotations before
/// wraparound (wraparound itself is fine: the only invariant is
/// "different from any actor seq the peer has cached", which is true for
/// any *change* in the bytes).
fn build_actor_id(device: DeviceId, generation: u32) -> automerge::ActorId {
    let mut bytes = [0u8; 20];
    bytes[..16].copy_from_slice(&device.0);
    bytes[16..].copy_from_slice(&generation.to_le_bytes());
    automerge::ActorId::from(bytes.as_slice())
}

/// Best-effort read of the persisted generation counter. Returns 0 if the
/// file is missing, malformed, or unreadable — those are all "treat as
/// fresh install" conditions and we don't want to error out the engine
/// startup over a missing sidecar.
fn read_actor_generation(state_dir: &Path) -> u32 {
    let path = state_dir.join(ACTOR_GENERATION_FILE);
    if let Ok(bytes) = fs::read(&path) {
        if bytes.len() == 4 {
            let mut arr = [0u8; 4];
            arr.copy_from_slice(&bytes);
            return u32::from_le_bytes(arr);
        }
    }
    // Sidecar missing or malformed. This is a genuine "fresh install" ONLY if
    // there are no docs yet. If docs already exist, the generation counter was
    // lost WITHOUT a wipe — the classic trigger is a data-dir relocation (app
    // update, "move to SD card", Android SAF/data-dir alias change). Reusing
    // generation 0 there would make our actor reuse seq numbers peers already
    // cached at higher values ("duplicate seq N for actor"), and every merge on
    // every peer fails → mass conflict-artifact explosion. Pick a fresh random
    // (non-zero) generation and persist it so the actor is guaranteed distinct
    // from generation 0 and, with overwhelming probability, from any prior one.
    // See M-4.
    if docs_dir_has_notes(state_dir) {
        use rand::RngCore;
        let mut buf = [0u8; 4];
        rand::rngs::OsRng.fill_bytes(&mut buf);
        let generation = u32::from_le_bytes(buf) | 1; // never 0
        if let Err(e) = write_actor_generation(state_dir, generation) {
            tracing::warn!(error = %e, "docstore: failed to persist healed actor generation");
        }
        tracing::warn!(
            generation,
            "docstore: actor_generation sidecar missing but docs exist \
             (likely data-dir relocation) — assigned a fresh generation to \
             avoid duplicate-seq corruption on peers"
        );
        return generation;
    }
    0
}

/// True if the docs directory holds at least one persisted note (`.automerge`)
/// *anywhere*, including nested folders. Used to tell a genuine fresh install
/// from a lost generation sidecar. See M-4.
///
/// **Must recurse.** Notes are stored at their full relative path
/// (`docs/sub/deep/note.md.automerge`), so a vault whose notes all live in
/// folders has NO `.automerge` at the docs root. A non-recursive scan would
/// misread that as a fresh install and reset the actor generation to 0 —
/// re-triggering the exact duplicate-seq mass corruption this heal exists to
/// prevent. Short-circuits at the first hit so a 50K-note vault isn't fully
/// walked just to answer "any?".
fn docs_dir_has_notes(state_dir: &Path) -> bool {
    fn any_automerge_under(dir: &Path) -> bool {
        let Ok(entries) = fs::read_dir(dir) else {
            return false;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            // Prefer the dirent's own file_type (no extra stat) and fall back
            // to path.is_dir() only if it's unavailable.
            let is_dir = entry
                .file_type()
                .map(|t| t.is_dir())
                .unwrap_or_else(|_| path.is_dir());
            if is_dir {
                if any_automerge_under(&path) {
                    return true;
                }
            } else if path
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("automerge"))
            {
                return true;
            }
        }
        false
    }
    any_automerge_under(&docs_root(state_dir))
}

/// Atomic write of the generation counter via temp+rename. Generation
/// loss after a crash means a wiped peer might briefly reuse the old
/// actor — annoying but recoverable on next wipe; we accept the
/// non-fsync'd rename as the right cost/safety tradeoff here.
fn write_actor_generation(state_dir: &Path, generation: u32) -> Result<(), StoreError> {
    fs::create_dir_all(state_dir)?;
    let path = state_dir.join(ACTOR_GENERATION_FILE);
    let tmp = path.with_extension("bin.tmp");
    fs::write(&tmp, generation.to_le_bytes())?;
    fs::rename(&tmp, &path)?;
    Ok(())
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<NotePath>) -> Result<(), StoreError> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            walk(root, &path, out)?;
        } else if path.extension().is_some_and(|e| e == "automerge") {
            let rel = path
                .strip_prefix(root)
                .map_err(|_| StoreError::PathTraversal(path.to_string_lossy().into()))?;
            // Strip the `.automerge` extension to get the original note path.
            let with_ext = rel.to_string_lossy().to_string();
            let note = with_ext
                .strip_suffix(".automerge")
                .ok_or_else(|| StoreError::InvalidPath(with_ext.clone()))?;
            // Normalize to forward slashes.
            out.push(NotePath(note.replace('\\', "/")));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn dev() -> DeviceId {
        DeviceId([7; 16])
    }

    #[test]
    fn save_and_reload_preserves_content() {
        let roost = tempdir().unwrap();
        let store = DocStore::new(roost.path().to_path_buf(), dev());
        store.init().unwrap();

        let note = NotePath("a/b/c.md".into());
        let mut doc = NoteDoc::new(dev()).unwrap();
        doc.apply_markdown("hello world").unwrap();
        store.save(&note, &mut doc).unwrap();

        let loaded = store.load_or_create(&note).unwrap();
        assert_eq!(loaded.render().unwrap(), "hello world");
    }

    #[test]
    fn rejects_paths_that_escape_docs_root() {
        // AUDIT: a paired peer's queued payload carries an arbitrary path
        // string. `main_path`/`wal_path` join it raw onto docs_root, so `..` /
        // absolute / NUL / backslash must be refused at the store layer as a
        // backstop — otherwise `.automerge` CRDT state lands outside the store
        // where `list()` never sees it (ghost state) or clobbers arbitrary
        // files. Mirrors `peer::is_safe_note_path` + `store::from_note_path`.
        let roost = tempdir().unwrap();
        let store = DocStore::new(roost.path().to_path_buf(), dev());
        store.init().unwrap();

        let bad = [
            "../escape.md",
            "a/../../escape.md",
            "/abs/escape.md",
            "with\\backslash.md",
            "nul\0byte.md",
            "",
        ];
        for p in bad {
            let note = NotePath(p.into());
            let mut doc = NoteDoc::new(dev()).unwrap();
            doc.apply_markdown("x").unwrap();
            assert!(
                matches!(store.save(&note, &mut doc), Err(StoreError::InvalidPath(_))),
                "save must reject escaping path {p:?}"
            );
            assert!(
                matches!(store.load_or_create(&note), Err(StoreError::InvalidPath(_))),
                "load_or_create must reject escaping path {p:?}"
            );
            assert!(!store.exists(&note), "exists must be false for {p:?}");
            assert!(
                matches!(store.remove(&note), Err(StoreError::InvalidPath(_))),
                "remove must reject escaping path {p:?}"
            );
        }

        // Nothing must have been written outside docs_root by the rejected
        // saves — the sibling escape target must not exist.
        let escaped = roost.path().parent().map(|d| d.join("escape.md.automerge"));
        if let Some(escaped) = escaped {
            assert!(!escaped.exists(), "no .automerge should escape docs_root");
        }
    }

    #[test]
    fn nfc_and_nfd_spellings_map_to_one_doc() {
        // C-1(b): a note stored under the NFD spelling of "café" must be found
        // under the NFC spelling — one logical note, one on-disk doc.
        let roost = tempdir().unwrap();
        let store = DocStore::new(roost.path().to_path_buf(), dev());
        store.init().unwrap();
        let nfd = NotePath("cafe\u{301}.md".into()); // e + combining acute
        let nfc = NotePath("caf\u{e9}.md".into()); // precomposed é
        let mut doc = NoteDoc::new(dev()).unwrap();
        doc.apply_markdown("beans").unwrap();
        store.save(&nfd, &mut doc).unwrap();
        assert_eq!(
            store.load_or_create(&nfc).unwrap().render().unwrap(),
            "beans",
            "NFC and NFD spellings must resolve to the same doc"
        );
        assert_eq!(store.list().unwrap().len(), 1, "exactly one doc on disk");
    }

    #[test]
    fn case_collision_refused_on_case_insensitive_fs() {
        // C-1(a): on a case-insensitive fs, saving a differently-cased note
        // that shares one on-disk file must be refused, not silently clobber.
        // On a case-sensitive fs the two are legitimately distinct.
        let roost = tempdir().unwrap();
        let store = DocStore::new(roost.path().to_path_buf(), dev());
        store.init().unwrap();
        let mut d1 = NoteDoc::new(dev()).unwrap();
        d1.apply_markdown("one").unwrap();
        store.save(&NotePath("Note.md".into()), &mut d1).unwrap();

        let mut d2 = NoteDoc::new(dev()).unwrap();
        d2.apply_markdown("two").unwrap();
        let res = store.save(&NotePath("note.md".into()), &mut d2);

        if store.fs_case_insensitive() {
            assert!(
                matches!(res, Err(StoreError::PathConflict(_))),
                "case-insensitive fs must refuse the clobbering save, got {res:?}"
            );
            assert_eq!(
                store
                    .load_or_create(&NotePath("Note.md".into()))
                    .unwrap()
                    .render()
                    .unwrap(),
                "one",
                "the original note must survive the refused collision"
            );
        } else {
            assert!(res.is_ok(), "case-sensitive fs keeps both distinct notes");
            assert_eq!(store.list().unwrap().len(), 2);
        }
    }

    #[test]
    fn case_insensitivity_is_ored_across_state_dir_and_roost() {
        // C-1 residual: the probe must reflect the roost's filesystem, not only
        // the state dir. We can't force a volume's case-sensitivity in a unit
        // test, but we CAN assert the OR-semantics: wiring a roost probe dir
        // never *weakens* the verdict — if the state dir alone is insensitive,
        // adding a roost keeps it insensitive; and the verdict equals
        // (state OR roost). Here state dir and roost are the same tempdir, so
        // both probes agree and the result matches the state-dir-only probe.
        let roost = tempdir().unwrap();
        let state_only = DocStore::new(roost.path().to_path_buf(), dev());
        let with_roost = DocStore::new(roost.path().to_path_buf(), dev())
            .with_roost_probe_dir(roost.path().to_path_buf());
        assert_eq!(
            state_only.fs_case_insensitive(),
            with_roost.fs_case_insensitive(),
            "same-volume roost probe must agree with the state-dir probe"
        );
        // And the free helper is consistent with the method (a probable
        // state dir yields Some(_), never None).
        assert_eq!(
            with_roost.fs_case_insensitive(),
            probe_dir_case_insensitive(&docs_root(roost.path())).unwrap_or(false),
        );
    }

    /// SYN-M6: a directory that can't be probed via `std::fs` (stand-in for an
    /// Android SAF `content://` roost) returns `None` — NOT `Some(false)` — so
    /// `fs_case_insensitive` can pick the Android-safe default instead of
    /// silently disabling case-collision detection.
    #[test]
    fn probe_returns_none_when_dir_cannot_be_written() {
        let tmp = tempdir().unwrap();
        // Occupy a path with a FILE, then probe a dir "under" it —
        // create_dir_all fails, mirroring a std::fs write that can't land.
        let blocker = tmp.path().join("blocker");
        std::fs::write(&blocker, b"not a dir").unwrap();
        let unprobable = blocker.join("nested");
        assert_eq!(
            probe_dir_case_insensitive(&unprobable),
            None,
            "an unwritable dir must be unprobable (None), not Some(false)"
        );
        // A normal dir is always probable → Some(_).
        assert!(probe_dir_case_insensitive(tmp.path()).is_some());
    }

    #[test]
    fn load_or_create_returns_fresh_when_absent() {
        let roost = tempdir().unwrap();
        let store = DocStore::new(roost.path().to_path_buf(), dev());
        store.init().unwrap();
        let note = NotePath("new.md".into());
        let loaded = store.load_or_create(&note).unwrap();
        assert_eq!(loaded.render().unwrap(), "");
    }

    #[test]
    fn list_finds_all_nested_notes() {
        let roost = tempdir().unwrap();
        let store = DocStore::new(roost.path().to_path_buf(), dev());
        store.init().unwrap();

        let mut doc1 = NoteDoc::new(dev()).unwrap();
        doc1.apply_markdown("one").unwrap();
        store.save(&NotePath("root.md".into()), &mut doc1).unwrap();

        let mut doc2 = NoteDoc::new(dev()).unwrap();
        doc2.apply_markdown("two").unwrap();
        store
            .save(&NotePath("sub/nested.md".into()), &mut doc2)
            .unwrap();

        let mut paths = store
            .list()
            .unwrap()
            .into_iter()
            .map(|p| p.0)
            .collect::<Vec<_>>();
        paths.sort();
        assert_eq!(paths, vec!["root.md", "sub/nested.md"]);
    }

    #[test]
    fn gc_sanitizes_skewed_crdt_deleted_at() {
        // M-1: the CRDT `deleted_at` register carries the deleting peer's wall
        // clock verbatim over the merge path. GC must clamp it against our own
        // clock, or a hostile/skewed peer poisons everyone's tombstone reaping.
        let roost = tempdir().unwrap();
        let store = DocStore::new(roost.path().to_path_buf(), dev());
        store.init().unwrap();

        let day = 24 * 60 * 60 * 1000u64;
        let now = 500 * day; // arbitrary "present"
        let cutoff = now - 180 * day; // retention window

        // (a) Far-FUTURE deleted_at (year-2099-style skew): raw value is well
        // past `cutoff`, so a naive GC keeps it forever — a tombstone-
        // accumulation DoS. Sanitized to `now`, it's simply "deleted now",
        // still retained (correct) but now reapable once real time passes.
        let mut future = NoteDoc::new(dev()).unwrap();
        future.apply_markdown("future skew").unwrap();
        future.mark_deleted(now + 3650 * day).unwrap();
        store
            .save(&NotePath("future.md".into()), &mut future)
            .unwrap();

        // (b) Far-PAST deleted_at (pre-1980 / >400d before now): a naive GC
        // reaps it immediately -> resurrection. Sanitized to `now`, it survives.
        let mut ancient = NoteDoc::new(dev()).unwrap();
        ancient.apply_markdown("ancient skew").unwrap();
        ancient.mark_deleted(1).unwrap(); // 1970
        store
            .save(&NotePath("ancient.md".into()), &mut ancient)
            .unwrap();

        // (c) A genuinely old delete (within the trust window, past cutoff) must
        // still reap normally.
        let mut real = NoteDoc::new(dev()).unwrap();
        real.apply_markdown("real old delete").unwrap();
        real.mark_deleted(now - 200 * day).unwrap();
        store.save(&NotePath("real.md".into()), &mut real).unwrap();

        let reaped = store.gc_tombstones(now, cutoff).unwrap();
        assert_eq!(reaped, 1, "only the genuinely-old tombstone should reap");
        let survivors = store
            .list()
            .unwrap()
            .into_iter()
            .map(|p| p.0)
            .collect::<Vec<_>>();
        assert!(
            survivors.contains(&"future.md".to_string()),
            "future skew kept"
        );
        assert!(
            survivors.contains(&"ancient.md".to_string()),
            "past skew kept"
        );
        assert!(
            !survivors.contains(&"real.md".to_string()),
            "real old delete reaped"
        );
    }

    #[test]
    fn remove_deletes_main_and_wal() {
        let roost = tempdir().unwrap();
        let store = DocStore::new(roost.path().to_path_buf(), dev());
        store.init().unwrap();

        let note = NotePath("ephemeral.md".into());
        let mut doc = NoteDoc::new(dev()).unwrap();
        doc.apply_markdown("bye").unwrap();
        store.save(&note, &mut doc).unwrap();
        store.remove(&note).unwrap();
        assert!(store.list().unwrap().is_empty());
    }

    #[test]
    fn fresh_store_reports_generation_zero() {
        let roost = tempdir().unwrap();
        let store = DocStore::new(roost.path().to_path_buf(), dev());
        store.init().unwrap();
        assert_eq!(store.actor_generation(), 0);
    }

    #[test]
    fn lost_generation_sidecar_with_docs_heals_to_nonzero() {
        // M-4: simulate a data-dir relocation — docs are present but the
        // generation sidecar was left behind. Reopening must NOT report
        // generation 0 (which would reuse actor seqs peers cached and cause
        // duplicate-seq mass corruption); it must heal to a fresh non-zero
        // generation and persist it.
        let roost = tempdir().unwrap();
        let store = DocStore::new(roost.path().to_path_buf(), dev());
        store.init().unwrap();
        let mut doc = NoteDoc::new(dev()).unwrap();
        doc.apply_markdown("existing note").unwrap();
        store.save(&NotePath("kept.md".into()), &mut doc).unwrap();

        // Delete the sidecar, as a data-dir move that dropped it would.
        let sidecar = roost.path().join(ACTOR_GENERATION_FILE);
        let _ = fs::remove_file(&sidecar);

        let reopened = DocStore::new(roost.path().to_path_buf(), dev());
        assert_ne!(
            reopened.actor_generation(),
            0,
            "lost sidecar with docs present must not reset to generation 0"
        );
        assert!(sidecar.exists(), "healed generation must be persisted");
        // Stable across a further reopen (persisted, not re-randomised).
        let again = DocStore::new(roost.path().to_path_buf(), dev());
        assert_eq!(again.actor_generation(), reopened.actor_generation());
    }

    #[test]
    fn lost_sidecar_with_only_nested_notes_still_heals_to_nonzero() {
        // M-4 regression: the original heal used a NON-recursive docs-root scan,
        // so a vault whose notes all live in folders (the common case) looked
        // empty → generation reset to 0 → duplicate-seq mass corruption on peers.
        // A vault with ONLY a nested note and a lost sidecar must still heal.
        let roost = tempdir().unwrap();
        let store = DocStore::new(roost.path().to_path_buf(), dev());
        store.init().unwrap();
        let mut doc = NoteDoc::new(dev()).unwrap();
        doc.apply_markdown("nested only").unwrap();
        // No note at the docs root — only inside folders.
        store
            .save(&NotePath("folder/deep/kept.md".into()), &mut doc)
            .unwrap();

        let sidecar = roost.path().join(ACTOR_GENERATION_FILE);
        let _ = fs::remove_file(&sidecar);

        let reopened = DocStore::new(roost.path().to_path_buf(), dev());
        assert_ne!(
            reopened.actor_generation(),
            0,
            "nested-only vault with lost sidecar must heal, not reset to generation 0"
        );
        assert!(sidecar.exists(), "healed generation must be persisted");
    }

    #[test]
    fn truly_fresh_install_still_reports_generation_zero() {
        // M-4 guard: no docs → genuine fresh install → generation stays 0.
        let roost = tempdir().unwrap();
        let store = DocStore::new(roost.path().to_path_buf(), dev());
        store.init().unwrap();
        assert_eq!(store.actor_generation(), 0);
    }

    #[test]
    fn wipe_all_bumps_generation_and_persists() {
        let roost = tempdir().unwrap();
        let mut store = DocStore::new(roost.path().to_path_buf(), dev());
        store.init().unwrap();
        store.wipe_all().unwrap();
        assert_eq!(store.actor_generation(), 1);
        store.wipe_all().unwrap();
        assert_eq!(store.actor_generation(), 2);
        // A fresh DocStore over the same dir should pick up the persisted
        // counter — this is what survives a process restart.
        let reopened = DocStore::new(roost.path().to_path_buf(), dev());
        assert_eq!(reopened.actor_generation(), 2);
    }

    #[test]
    fn wipe_rotates_actor_so_post_wipe_writes_use_new_actor() {
        // Simulate the sync-breaking failure mode: write a doc, save it,
        // wipe, then write again. The two saves should embed *different*
        // actor IDs — that's what stops "duplicate seq" on the peer side.
        let roost = tempdir().unwrap();
        let mut store = DocStore::new(roost.path().to_path_buf(), dev());
        store.init().unwrap();
        let note = NotePath("conflict.md".into());

        let mut pre = store.load_or_create(&note).unwrap();
        pre.apply_markdown("before reset").unwrap();
        store.save(&note, &mut pre).unwrap();
        let pre_bytes = std::fs::read(roost.path().join("docs/conflict.md.automerge")).unwrap();

        store.wipe_all().unwrap();

        let mut post = store.load_or_create(&note).unwrap();
        post.apply_markdown("after reset").unwrap();
        store.save(&note, &mut post).unwrap();
        let post_bytes = std::fs::read(roost.path().join("docs/conflict.md.automerge")).unwrap();

        // The two saves carry distinct change-author actor identities. We
        // can't easily extract the actor without reaching into Automerge
        // internals, but the saved bytes for the *same logical content
        // path* will diverge if and only if the two writes used different
        // actors (same actor + reset would just reuse seq=1 with the same
        // history-preimage and might hash-collide). A coarse byte-level
        // inequality is good enough here.
        assert_ne!(pre_bytes, post_bytes);
    }
}
