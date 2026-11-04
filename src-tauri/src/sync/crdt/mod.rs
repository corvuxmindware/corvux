// NoteDoc constructors are exposed for tests + Phase 2 CRDT consumers
#![allow(dead_code)]

//! Per-note CRDT lifecycle.
//!
//! Each markdown file in the roost corresponds to one Automerge document.
//! The document carries three fields:
//!
//! - `schema_version: u16` — format version for forward compatibility.
//! - `text: Text`         — the body of the note as a collaborative string.
//! - `md_hash: bytes[32]` — SHA-256 of the last markdown render we flushed.
//!
//! `md_hash` is a *local* concern: it's how we distinguish "the watcher
//! fired because we just wrote this file ourselves" from "the watcher fired
//! because an external editor modified it". It's stored in the doc so it
//! survives restarts; it's never authoritative for sync — the CRDT text is.
//!
//! All Automerge operations go through `NoteDoc`. Nothing else in the sync
//! engine touches the `automerge` crate directly — if we swap CRDT engines
//! later (e.g. to Yjs or a custom one), the blast radius is this file.

pub mod roundtrip;
pub mod schema;

use automerge::transaction::Transactable;
use automerge::{Automerge, ObjType, ReadDoc, ScalarValue, Value};
use sha2::{Digest, Sha256};

use super::error::{CrdtError, SyncResult};
use super::identity::DeviceId;
use schema::{
    FIELD_DELETED, FIELD_DELETED_AT, FIELD_MD_HASH, FIELD_SCHEMA_VERSION, FIELD_TEXT,
    MAX_SUPPORTED_SCHEMA_VERSION, SCHEMA_VERSION,
};

/// A single note's Automerge document.
///
/// We wrap `Automerge` (not `AutoCommit`) because the sync protocol methods
/// live on `Automerge` directly in the 0.5 API, and because we batch all
/// mutations inside our own explicit `transact` calls — every `apply_markdown`
/// is one atomic transaction regardless of how many splice ops it emits.
pub struct NoteDoc {
    inner: Automerge,
}

impl NoteDoc {
    /// Create a brand-new doc owned by `device`. The initial state has
    /// `schema_version` set and an empty text. Caller is expected to
    /// immediately `apply_markdown` with the current on-disk content.
    pub fn new(device: DeviceId) -> SyncResult<Self> {
        Self::new_with_actor(device.as_actor_id())
    }

    /// Variant of `new` that accepts a fully-formed `ActorId`. The
    /// `DocStore` uses this to spin up notes under a *generation-bumped*
    /// actor (`device_id || generation`) so that resetting the local
    /// docstore doesn't collide on the same `(actor, seq)` slots peers
    /// already remember from before the reset.
    pub fn new_with_actor(actor: automerge::ActorId) -> SyncResult<Self> {
        let mut inner = Automerge::new().with_actor(actor);
        inner
            .transact::<_, _, automerge::AutomergeError>(|tx| {
                tx.put(automerge::ROOT, FIELD_SCHEMA_VERSION, SCHEMA_VERSION as i64)?;
                tx.put_object(automerge::ROOT, FIELD_TEXT, ObjType::Text)?;
                tx.put(automerge::ROOT, FIELD_MD_HASH, vec![0u8; 32])?;
                tx.put(automerge::ROOT, FIELD_DELETED, false)?;
                tx.put(automerge::ROOT, FIELD_DELETED_AT, 0_i64)?;
                Ok(())
            })
            .map_err(|e| CrdtError::Automerge(e.error.to_string()))?;
        Ok(Self { inner })
    }

    /// Build a note doc for a **first-time disk seed** — a note the seeding
    /// device holds on disk but that no paired peer has yet (the SAF-bootstrap
    /// / Hole-B path). Uses a deterministic, path-derived actor
    /// ([`seed_actor_for_path`]) instead of the device actor, so two devices
    /// that independently seed the SAME path from IDENTICAL on-disk content
    /// produce byte-identical Automerge histories. Those merge to a single
    /// text object — sidestepping the dual-text-object divergence a per-device
    /// actor would cause (two independent `text` objects, actor-id tiebreak
    /// silently hiding one side's content).
    ///
    /// Differing content under the same path-derived actor is a genuine
    /// conflict: it forks the actor's seq and surfaces as a `duplicate seq`
    /// error on merge — detectable (the sync layer's corruption-recovery path
    /// handles it and preserves the losing side to a conflict artifact) rather
    /// than a silent partial loss.
    ///
    /// After the seed is persisted, the DocStore reloads it under the device's
    /// own actor, so every *subsequent* local edit is authored normally; the
    /// path-derived actor only ever owns the seed's initial ops.
    pub fn seed_from_markdown(note_path: &str, content: &str) -> SyncResult<Self> {
        let mut doc = Self::new_with_actor(seed_actor_for_path(note_path))?;
        doc.apply_markdown(content)?;
        Ok(doc)
    }

    /// Load from a previously saved byte blob (full `save()` output). The
    /// byte format is Automerge's compressed save format, stable across
    /// Automerge 0.5 patch releases.
    pub fn load(bytes: &[u8], device: DeviceId) -> SyncResult<Self> {
        Self::load_with_actor(bytes, device.as_actor_id())
    }

    /// Variant of `load` that accepts a fully-formed `ActorId`. See
    /// `new_with_actor` for why the docstore uses this.
    pub fn load_with_actor(bytes: &[u8], actor: automerge::ActorId) -> SyncResult<Self> {
        let mut inner = Automerge::load(bytes)
            .map_err(CrdtError::from)?
            .with_actor(actor);
        // Validate schema version before we trust anything else in the doc.
        let version = inner
            .get(automerge::ROOT, FIELD_SCHEMA_VERSION)
            .map_err(CrdtError::from)?
            .and_then(|(v, _)| match v {
                Value::Scalar(s) => match s.as_ref() {
                    ScalarValue::Int(i) => Some(*i),
                    ScalarValue::Uint(u) => Some(*u as i64),
                    _ => None,
                },
                _ => None,
            })
            .ok_or(CrdtError::MissingField(FIELD_SCHEMA_VERSION))?;
        // Read-tolerant: accept anything from v1 up to the highest version
        // this build understands. A v2 doc from a Stage-2 peer loads cleanly
        // on a Stage-1 build (it simply ignores the `deleted`/`deleted_at`
        // fields it doesn't write yet). We do NOT mutate the doc on load —
        // injecting a `put` op on every load would bloat history and
        // propagate spurious changes to peers. Missing fields are handled by
        // the readers (`is_deleted`, `deleted_at`) defaulting safely.
        if !(1..=MAX_SUPPORTED_SCHEMA_VERSION as i64).contains(&version) {
            return Err(CrdtError::UnsupportedSchema {
                expected: SCHEMA_VERSION,
                found: version as u16,
            }
            .into());
        }
        // C-5: run the migration seam. This is the load-and-migrate path the
        // schema docs promised but that previously did not exist, so future
        // schema bumps have a home instead of the two-stage rollout pattern
        // being un-extendable.
        Self::migrate_doc(&mut inner, version)?;
        Ok(Self { inner })
    }

    /// C-5: schema-migration seam, invoked by `load_with_actor` after the
    /// stored version is read. Brings a doc written under an older schema up to
    /// [`SCHEMA_VERSION`] by applying each registered `vN -> vN+1` step in
    /// order. This is the extension point the schema docs described but that
    /// previously did not exist.
    ///
    /// v1 -> v2 is intentionally a no-op: the v2 fields (`deleted` /
    /// `deleted_at`) are simply absent on a v1 doc and every reader defaults
    /// them safely, and we deliberately avoid injecting `put` ops on load
    /// (that would bloat Automerge history and propagate spurious changes to
    /// peers). Future *structural* bumps register their step in the match.
    fn migrate_doc(_inner: &mut Automerge, stored_version: i64) -> SyncResult<()> {
        let mut v = stored_version;
        while v < SCHEMA_VERSION as i64 {
            match v {
                1 => { /* v1 -> v2: read-tolerant, no structural change */ }
                other => {
                    // No migration registered for this step. Refuse rather
                    // than silently loading a doc we can't correctly forward.
                    return Err(CrdtError::UnsupportedSchema {
                        expected: SCHEMA_VERSION,
                        found: other as u16,
                    }
                    .into());
                }
            }
            v += 1;
        }
        Ok(())
    }

    /// Serialize the full doc for storage. Includes every historical change
    /// (Automerge saves are compressed but retain history for future
    /// merges). Callers should write this atomically.
    pub fn save(&mut self) -> Vec<u8> {
        self.inner.save()
    }

    /// Current rendered text content. This is what we flush to the `.md`
    /// file on disk.
    pub fn render(&self) -> SyncResult<String> {
        let text_id = self.text_obj_id()?;
        let text = self.inner.text(&text_id).map_err(CrdtError::from)?;
        Ok(text)
    }

    /// Current `md_hash` stored in the doc.
    pub fn md_hash(&self) -> SyncResult<[u8; 32]> {
        let raw = self
            .inner
            .get(automerge::ROOT, FIELD_MD_HASH)
            .map_err(CrdtError::from)?
            .map(|(v, _)| v)
            .ok_or(CrdtError::MissingField(FIELD_MD_HASH))?;
        let bytes = match raw {
            Value::Scalar(s) => match s.as_ref() {
                ScalarValue::Bytes(b) => b.clone(),
                _ => return Err(CrdtError::MissingField(FIELD_MD_HASH).into()),
            },
            _ => return Err(CrdtError::MissingField(FIELD_MD_HASH).into()),
        };
        if bytes.len() != 32 {
            return Err(CrdtError::MissingField(FIELD_MD_HASH).into());
        }
        let mut out = [0u8; 32];
        out.copy_from_slice(&bytes);
        Ok(out)
    }

    /// Whether this note is tombstoned (schema v2+ `deleted` flag). Defaults
    /// to `false` for v1 docs that predate the field. This is the single
    /// source of truth for "is this note deleted" — the render layer trashes
    /// the on-disk markdown whenever this returns `true`.
    pub fn is_deleted(&self) -> SyncResult<bool> {
        let raw = self
            .inner
            .get(automerge::ROOT, FIELD_DELETED)
            .map_err(CrdtError::from)?;
        match raw {
            Some((Value::Scalar(s), _)) => match s.as_ref() {
                ScalarValue::Boolean(b) => Ok(*b),
                // Defensive: a non-bool here means a corrupt/foreign doc.
                // Treat as not-deleted rather than erroring — never lose a
                // note over a malformed flag.
                _ => Ok(false),
            },
            // Field absent (v1 doc) → not deleted.
            _ => Ok(false),
        }
    }

    /// Advisory deletion timestamp (unix millis), or `0` if not set / v1 doc.
    /// Used only for tombstone GC age, never for conflict resolution.
    pub fn deleted_at(&self) -> SyncResult<u64> {
        let raw = self
            .inner
            .get(automerge::ROOT, FIELD_DELETED_AT)
            .map_err(CrdtError::from)?;
        match raw {
            Some((Value::Scalar(s), _)) => match s.as_ref() {
                ScalarValue::Int(i) => Ok((*i).max(0) as u64),
                ScalarValue::Uint(u) => Ok(*u),
                _ => Ok(0),
            },
            _ => Ok(0),
        }
    }

    /// Tombstone this note: set `deleted = true` and stamp `deleted_at`.
    /// Deliberately leaves `text`/`md_hash` untouched so the content survives
    /// in the CRDT (recoverable via `restore`) and so a concurrent text edit
    /// on another device merges cleanly — `deleted` and `text` live under
    /// disjoint root keys, so the merged doc holds both and the render layer
    /// trashes the file on disk. Idempotent: marking an already-deleted note
    /// just refreshes `deleted_at`.
    pub fn mark_deleted(&mut self, at_millis: u64) -> SyncResult<()> {
        self.inner
            .transact::<_, _, automerge::AutomergeError>(|tx| {
                tx.put(automerge::ROOT, FIELD_DELETED, true)?;
                tx.put(automerge::ROOT, FIELD_DELETED_AT, at_millis as i64)?;
                Ok(())
            })
            .map_err(|e| CrdtError::Automerge(e.error.to_string()))?;
        Ok(())
    }

    /// Clear the tombstone (explicit "restore from trash"). This is the ONLY
    /// way to un-delete a note — a plain `apply_markdown` never resurrects a
    /// deleted note. That asymmetry is what enforces the policy for the common
    /// case: a concurrent text **edit** vs a delete → delete wins (the edit
    /// updates `text` but can't clear `deleted`), so the note stays trashed,
    /// recoverable. A concurrent **restore** vs a delete is a different, rarer
    /// race on the same `deleted` register; Automerge's LWW tiebreak converges
    /// it deterministically across devices (no divergence), and restore — an
    /// explicit user action — is the intended way to win it. The text the note
    /// carried at deletion time is still present, so restore brings back the
    /// last-known content.
    pub fn restore(&mut self) -> SyncResult<()> {
        self.inner
            .transact::<_, _, automerge::AutomergeError>(|tx| {
                tx.put(automerge::ROOT, FIELD_DELETED, false)?;
                Ok(())
            })
            .map_err(|e| CrdtError::Automerge(e.error.to_string()))?;
        Ok(())
    }

    /// Apply a new markdown string to the CRDT, generating minimal splice
    /// operations via Myers diff. Returns `true` if the doc actually
    /// changed. Updates `md_hash` on any real change.
    ///
    /// Call this from two places:
    /// - In-app editor saves: turns every keystroke-batch into ops.
    /// - Watcher-detected external edits: the current text is whatever the
    ///   external editor wrote, so the diff might be large.
    pub fn apply_markdown(&mut self, new_text: &str) -> SyncResult<bool> {
        let current = self.render()?;
        let new_hash = sha256(new_text);
        if current == new_text {
            // Still update md_hash if it's stale (e.g. first load after an
            // identical external write).
            if new_hash != self.md_hash()? {
                self.inner
                    .transact::<_, _, automerge::AutomergeError>(|tx| {
                        tx.put(automerge::ROOT, FIELD_MD_HASH, new_hash.to_vec())?;
                        Ok(())
                    })
                    .map_err(|e| CrdtError::Automerge(e.error.to_string()))?;
                return Ok(true);
            }
            return Ok(false);
        }
        let text_id = self.text_obj_id()?;
        let current_owned = current;
        self.inner
            .transact::<_, _, automerge::AutomergeError>(|tx| {
                roundtrip::apply_diff(tx, &text_id, &current_owned, new_text)?;
                tx.put(automerge::ROOT, FIELD_MD_HASH, new_hash.to_vec())?;
                Ok(())
            })
            .map_err(|e| CrdtError::Automerge(e.error.to_string()))?;
        Ok(true)
    }

    /// Merge another doc into this one. Returns `true` if our state changed
    /// (i.e. the remote had ops we didn't). Used by the sync protocol when
    /// we receive a full-doc snapshot from a peer.
    pub fn merge(&mut self, other: &mut NoteDoc) -> SyncResult<bool> {
        let before_heads = self.inner.get_heads();
        self.inner
            .merge(&mut other.inner)
            .map_err(CrdtError::from)?;
        let after_heads = self.inner.get_heads();
        Ok(before_heads != after_heads)
    }

    /// The document's current heads — the Merkle hashes of its latest changes.
    /// Automerge returns these in a canonical sorted order, so two docs with
    /// identical history produce equal `Vec`s. Used as a cheap version token for
    /// optimistic-concurrency (CAS) checks: capture the heads of the stored doc
    /// before a lock-releasing operation, then re-read after re-locking — if
    /// they differ, a concurrent write landed and the operation must reconcile
    /// rather than blindly overwrite.
    pub fn heads(&self) -> Vec<automerge::ChangeHash> {
        self.inner.get_heads()
    }

    /// Expose the inner `Automerge` for the sync protocol layer. Intentionally
    /// `pub(crate)` — external code should go through the narrow API above.
    pub(crate) fn inner_mut(&mut self) -> &mut Automerge {
        &mut self.inner
    }

    fn text_obj_id(&self) -> SyncResult<automerge::ObjId> {
        let (_, id) = self
            .inner
            .get(automerge::ROOT, FIELD_TEXT)
            .map_err(CrdtError::from)?
            .ok_or(CrdtError::MissingField(FIELD_TEXT))?;
        Ok(id)
    }
}

/// SHA-256 of a UTF-8 string, matching the `md_hash` field stored in
/// every NoteDoc. Exposed so callers (e.g. the `render_to_roost`
/// safety guard) can detect the "rendered text doesn't match its own
/// hash" class of CRDT-divergence bugs.
pub fn sha256_text(s: &str) -> [u8; 32] {
    sha256(s)
}

fn sha256(s: &str) -> [u8; 32] {
    let digest = Sha256::digest(s.as_bytes());
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    out
}

/// Deterministic Automerge actor for a first-time disk seed of `note_path`
/// (see [`NoteDoc::seed_from_markdown`]). Derived purely from the note path so
/// it is identical on every device — the property that lets independent seeds
/// of the same path+content converge to one text object.
///
/// 24 bytes long on purpose: a device actor is exactly 20 bytes
/// (`device_id[16] || generation[4]`, see `doc_store::build_actor_id`), so a
/// 24-byte seed actor can never be mistaken for — or collide with — a real
/// device actor, regardless of the path hash.
fn seed_actor_for_path(note_path: &str) -> automerge::ActorId {
    let digest = Sha256::digest(format!("corvux-seed-v1:{note_path}").as_bytes());
    automerge::ActorId::from(&digest[..24])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(seed: u8) -> DeviceId {
        DeviceId([seed; 16])
    }

    #[test]
    fn new_doc_has_empty_text() {
        let doc = NoteDoc::new(dev(1)).unwrap();
        assert_eq!(doc.render().unwrap(), "");
    }

    #[test]
    fn migration_seam_is_noop_for_current_versions() {
        // C-5: the seam exists and cleanly handles the versions this build
        // writes/accepts. v1 -> v2 and v2 are both no-op success paths; a
        // real save/load round-trip exercises the seam end to end.
        let mut a = Automerge::new();
        assert!(NoteDoc::migrate_doc(&mut a, 1).is_ok());
        assert!(NoteDoc::migrate_doc(&mut a, SCHEMA_VERSION as i64).is_ok());

        let mut doc = NoteDoc::new(dev(1)).unwrap();
        doc.apply_markdown("through the seam").unwrap();
        let bytes = doc.save();
        let reloaded = NoteDoc::load(&bytes, dev(1)).unwrap();
        assert_eq!(reloaded.render().unwrap(), "through the seam");
    }

    #[test]
    fn apply_markdown_roundtrip() {
        let mut doc = NoteDoc::new(dev(1)).unwrap();
        doc.apply_markdown("# Hello\n\nworld").unwrap();
        assert_eq!(doc.render().unwrap(), "# Hello\n\nworld");
    }

    #[test]
    fn save_load_roundtrip() {
        let mut doc = NoteDoc::new(dev(1)).unwrap();
        doc.apply_markdown("alpha").unwrap();
        let bytes = doc.save();
        let loaded = NoteDoc::load(&bytes, dev(1)).unwrap();
        assert_eq!(loaded.render().unwrap(), "alpha");
    }

    #[test]
    fn md_hash_updates_on_change() {
        let mut doc = NoteDoc::new(dev(1)).unwrap();
        let h0 = doc.md_hash().unwrap();
        doc.apply_markdown("x").unwrap();
        let h1 = doc.md_hash().unwrap();
        assert_ne!(h0, h1);
        assert_eq!(h1, sha256("x"));
    }

    #[test]
    fn apply_markdown_noop_reports_false() {
        let mut doc = NoteDoc::new(dev(1)).unwrap();
        doc.apply_markdown("abc").unwrap();
        let changed = doc.apply_markdown("abc").unwrap();
        assert!(!changed);
    }

    /// Forward-compat fixture: 287 bytes captured from `NoteDoc::new(dev(1))`
    /// followed by `apply_markdown("hello world")` and `save()` on Automerge
    /// 0.5 (commit 5299489, 2026-05-17). When we eventually bump Automerge
    /// to 0.9+, this test will pre-warn us if user docs in the field stop
    /// loading.
    const FIXTURE_AUTOMERGE_0_5: &str = "\
        856f4a83106ebe7700940201100101010101010101010101010101010101d435\
        c63e000bacd4386b87501ef2f391767e600cfa8317ec8c5dfecf943a64b50701\
        020302130323024003430256020e0104020411041307152021022308340242065\
        608574c800104810102830102020002017e030c02007e00017f00020700040b00\
        00040b0200050a0000047e0004090102076d645f686173687e0e736368656d61\
        5f76657273696f6e0474657874000b0f007b030c7201020a01040b03017f040b\
        010287047e14000b160000000000000000000000000000000000000000000000\
        000000000000000000b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380\
        ee9088f7ace2efcde90168656c6c6f20776f726c647f010e007f007f0f01";

    fn decode_hex(s: &str) -> Vec<u8> {
        let cleaned: String = s.chars().filter(|c| !c.is_whitespace()).collect();
        (0..cleaned.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&cleaned[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn loads_canonical_fixture_with_expected_fields() {
        let bytes = decode_hex(FIXTURE_AUTOMERGE_0_5);
        assert_eq!(
            bytes.len(),
            287,
            "fixture byte-count drifted — re-capture if intentional"
        );
        let doc = NoteDoc::load(&bytes, dev(1))
            .expect("Automerge must still load the 0.5-format canonical fixture");
        assert_eq!(doc.render().unwrap(), "hello world");
        assert_eq!(doc.md_hash().unwrap(), sha256("hello world"));
    }

    #[test]
    fn v1_doc_is_not_deleted_by_default() {
        // Stage-1 read-tolerance: a v1 doc (no `deleted` field) must load and
        // report not-deleted / deleted_at == 0, never error on the missing
        // field.
        let bytes = decode_hex(FIXTURE_AUTOMERGE_0_5);
        let doc = NoteDoc::load(&bytes, dev(1)).expect("v1 fixture must load");
        assert!(!doc.is_deleted().unwrap(), "v1 doc must default to live");
        assert_eq!(doc.deleted_at().unwrap(), 0, "v1 doc has no deleted_at");
        assert_eq!(doc.render().unwrap(), "hello world", "text intact");
    }

    #[test]
    fn fresh_doc_is_not_deleted() {
        let doc = NoteDoc::new(dev(1)).unwrap();
        assert!(!doc.is_deleted().unwrap());
        assert_eq!(doc.deleted_at().unwrap(), 0);
    }

    #[test]
    fn mark_deleted_sets_flag() {
        let mut doc = NoteDoc::new(dev(1)).unwrap();
        doc.apply_markdown("doomed").unwrap();
        assert!(!doc.is_deleted().unwrap());
        doc.mark_deleted(1234).unwrap();
        assert!(doc.is_deleted().unwrap());
        assert_eq!(doc.deleted_at().unwrap(), 1234);
        // Text survives the tombstone (recoverable via restore).
        assert_eq!(doc.render().unwrap(), "doomed");
    }

    #[test]
    fn restore_clears_delete() {
        let mut doc = NoteDoc::new(dev(1)).unwrap();
        doc.apply_markdown("body").unwrap();
        doc.mark_deleted(1).unwrap();
        assert!(doc.is_deleted().unwrap());
        doc.restore().unwrap();
        assert!(!doc.is_deleted().unwrap());
        assert_eq!(doc.render().unwrap(), "body");
    }

    #[test]
    fn plain_edit_does_not_resurrect() {
        // delete-wins policy: editing a tombstoned note updates text but must
        // NOT clear the deleted flag. Only an explicit restore un-deletes.
        let mut doc = NoteDoc::new(dev(1)).unwrap();
        doc.apply_markdown("v1").unwrap();
        doc.mark_deleted(1).unwrap();
        doc.apply_markdown("v1 edited while deleted").unwrap();
        assert!(doc.is_deleted().unwrap(), "edit must not un-delete");
        assert_eq!(doc.render().unwrap(), "v1 edited while deleted");
    }

    #[test]
    fn concurrent_restore_and_delete_converge() {
        // A restores a tombstoned note while B (concurrently, offline) re-deletes
        // it. The `deleted` register is the one true conflict point. Whatever
        // Automerge's LWW picks, BOTH devices must agree — no divergence. (We
        // don't assert which side wins; the contract is convergence.)
        let mut base = NoteDoc::new(dev(5)).unwrap();
        base.apply_markdown("body").unwrap();
        base.mark_deleted(100).unwrap();
        let bytes = base.save();

        let mut a = NoteDoc::load(&bytes, dev(1)).unwrap();
        let mut b = NoteDoc::load(&bytes, dev(2)).unwrap();
        a.restore().unwrap();
        b.mark_deleted(200).unwrap();

        a.merge(&mut b).unwrap();
        b.merge(&mut a).unwrap();

        assert_eq!(
            a.is_deleted().unwrap(),
            b.is_deleted().unwrap(),
            "both devices must converge to the same deleted state"
        );
        assert_eq!(a.render().unwrap(), b.render().unwrap());
    }

    #[test]
    fn deleted_flag_survives_save_load() {
        // The store-and-forward queue ships `doc.save()` (a FullDoc). This is
        // the contract that lets a delete reach an offline peer via the queue
        // with no separate delete protocol: the tombstone must round-trip
        // through save/load intact.
        let mut doc = NoteDoc::new(dev(1)).unwrap();
        doc.apply_markdown("queued body").unwrap();
        doc.mark_deleted(42).unwrap();
        let bytes = doc.save();
        let loaded = NoteDoc::load(&bytes, dev(2)).unwrap();
        assert!(loaded.is_deleted().unwrap(), "tombstone survives FullDoc");
        assert_eq!(loaded.deleted_at().unwrap(), 42);
        assert_eq!(loaded.render().unwrap(), "queued body");
    }

    #[test]
    fn concurrent_delete_and_edit_converge() {
        // The headline regression: A deletes, B edits the same note offline,
        // they merge both ways. delete-wins-but-recoverable means both sides
        // converge to deleted==true AND retain the edited text.
        let mut base = NoteDoc::new(dev(1)).unwrap();
        base.apply_markdown("original content").unwrap();
        let base_bytes = base.save();

        let mut a = NoteDoc::load(&base_bytes, dev(1)).unwrap();
        let mut b = NoteDoc::load(&base_bytes, dev(2)).unwrap();

        a.mark_deleted(999).unwrap();
        b.apply_markdown("original content + B edit").unwrap();

        a.merge(&mut b).unwrap();
        b.merge(&mut a).unwrap();

        assert!(a.is_deleted().unwrap(), "A converges to deleted");
        assert!(
            b.is_deleted().unwrap(),
            "B converges to deleted (delete wins)"
        );
        assert_eq!(
            a.render().unwrap(),
            b.render().unwrap(),
            "both sides converge to identical text"
        );
        assert_eq!(
            a.render().unwrap(),
            "original content + B edit",
            "B's edit is preserved in the CRDT"
        );
    }

    #[test]
    fn seed_actor_is_deterministic_and_not_device_length() {
        // Hole B: the seed actor must be identical across devices for a given
        // path (that's what makes two independent seeds converge) and must never
        // be 20 bytes — the device-actor length — so it can't collide with one.
        let a = seed_actor_for_path("Daily/2026-07-10.md");
        let b = seed_actor_for_path("Daily/2026-07-10.md");
        assert_eq!(a, b, "same path must yield the same seed actor");
        assert_ne!(
            seed_actor_for_path("Daily/2026-07-10.md"),
            seed_actor_for_path("Daily/2026-07-11.md"),
            "different paths must yield different seed actors"
        );
        assert_eq!(a.to_bytes().len(), 24, "seed actor must be 24 bytes");
    }

    #[test]
    fn identical_seeds_converge_to_single_text_object() {
        // Hole B headline invariant: two SAF devices that each seed the SAME
        // path from IDENTICAL on-disk content must converge cleanly — NOT fork
        // two text objects where actor-id tiebreak hides one side's content.
        // With the deterministic path-derived actor the two seeds are
        // byte-identical, so the merge is a genuine no-op.
        let path = "Notes/untracked.md";
        let body = "# Untracked\n\nWritten on the phone before pairing.";
        let mut a = NoteDoc::seed_from_markdown(path, body).unwrap();
        let mut b = NoteDoc::seed_from_markdown(path, body).unwrap();

        assert_eq!(
            a.save(),
            b.save(),
            "identical path+content seeds must be byte-identical across devices"
        );

        // Merge both ways; content must be intact and identical (one text object).
        a.merge(&mut b).unwrap();
        b.merge(&mut a).unwrap();
        assert_eq!(a.render().unwrap(), body);
        assert_eq!(b.render().unwrap(), body);
        assert_eq!(a.render().unwrap(), b.render().unwrap());
    }

    #[test]
    fn seed_then_device_edit_merges_cleanly() {
        // After a seed is adopted, normal device-actor edits layer on top of the
        // shared seed history and merge without divergence — the seed actor only
        // owns the initial ops.
        let path = "Notes/n.md";
        let mut seeded = NoteDoc::seed_from_markdown(path, "base").unwrap();
        let bytes = seeded.save();

        // Two devices load the same seed under their own actors and edit.
        let mut a = NoteDoc::load(&bytes, dev(1)).unwrap();
        let mut b = NoteDoc::load(&bytes, dev(2)).unwrap();
        a.apply_markdown("base + A").unwrap();
        b.apply_markdown("base + B").unwrap();
        a.merge(&mut b).unwrap();
        b.merge(&mut a).unwrap();
        assert_eq!(
            a.render().unwrap(),
            b.render().unwrap(),
            "device edits on a shared seed must converge"
        );
    }

    #[test]
    fn merge_converges_two_offline_edits() {
        // Two devices start from the same base, edit offline, merge.
        // Both sides must converge to the same text.
        let mut a = NoteDoc::new(dev(1)).unwrap();
        a.apply_markdown("shared").unwrap();
        let base = a.save();

        let mut a = NoteDoc::load(&base, dev(1)).unwrap();
        let mut b = NoteDoc::load(&base, dev(2)).unwrap();

        a.apply_markdown("shared + A").unwrap();
        b.apply_markdown("shared + B").unwrap();

        a.merge(&mut b).unwrap();
        b.merge(&mut a).unwrap();

        assert_eq!(a.render().unwrap(), b.render().unwrap());
    }

    /// `heads()` is the version token the queue-adopt CAS relies on: it must
    /// CHANGE when the document's content changes (so a concurrent local save
    /// is detected at recommit) and be STABLE across a save/reload round-trip
    /// (so the check doesn't false-positive on an unchanged doc). Also: two docs
    /// with identical history produce equal heads (canonical order).
    #[test]
    fn heads_discriminate_content_and_survive_roundtrip() {
        let mut a = NoteDoc::new(dev(1)).unwrap();
        a.apply_markdown("v1").unwrap();
        let h1 = a.heads();

        // An edit moves the heads — this is what lets the CAS detect a
        // concurrent write.
        a.apply_markdown("v2").unwrap();
        let h2 = a.heads();
        assert_ne!(h1, h2, "an edit must change the heads");

        // Save + reload preserves heads — so an unchanged stored doc reads back
        // with the SAME token and the CAS won't spuriously retry.
        let bytes = a.save();
        let reloaded = NoteDoc::load_with_actor(&bytes, dev(9).as_actor_id()).unwrap();
        assert_eq!(
            reloaded.heads(),
            h2,
            "heads must be stable across a save/reload (loading under a different \
             actor doesn't change existing history)"
        );
    }
}
