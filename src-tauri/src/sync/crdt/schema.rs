//! Automerge document schema constants.
//!
//! Kept in one place so a schema migration is one diff. When we bump
//! `SCHEMA_VERSION`, register the `vN -> vN+1` step in `NoteDoc::migrate_doc`
//! (the load-and-migrate seam, invoked by `load_with_actor` before the doc is
//! trusted). See C-5.

/// Current schema version we *write* (the version `new_with_actor` stamps
/// into freshly-created docs).
///
/// Migration history:
/// - v1: initial — `{ schema_version, text, md_hash }`
/// - v2: added `deleted` (bool) + `deleted_at` (i64 millis) for CRDT
///   tombstones. See `docs/decisions/` and the delete-vs-edit fix.
///
/// **Stage 2: writes v2.** Freshly-created docs now carry `deleted` +
/// `deleted_at`, and deletes are CRDT tombstones (the `deleted` flag) rather
/// than destructive removal. Safe to ship only once the Stage-1 read-tolerant
/// build (`MAX_SUPPORTED_SCHEMA_VERSION = 2`) is deployed everywhere, so a
/// peer that receives a v2 doc never hits `UnsupportedSchema`.
pub const SCHEMA_VERSION: u16 = 2;

/// Highest schema version this build can load. `load_with_actor` accepts any
/// version in `1..=MAX_SUPPORTED_SCHEMA_VERSION` and rejects anything newer.
pub const MAX_SUPPORTED_SCHEMA_VERSION: u16 = 2;

/// Root-map key for the text body.
pub const FIELD_TEXT: &str = "text";

/// Root-map key for the locally-tracked markdown hash.
pub const FIELD_MD_HASH: &str = "md_hash";

/// Root-map key for the schema version integer.
pub const FIELD_SCHEMA_VERSION: &str = "schema_version";

/// Root-map key for the tombstone flag (schema v2+). Absent on v1 docs;
/// readers default it to `false`.
pub const FIELD_DELETED: &str = "deleted";

/// Root-map key for the deletion timestamp in unix millis (schema v2+).
/// **Advisory metadata only** (used for tombstone GC age) — it does NOT
/// decide any CRDT conflict. A concurrent delete-vs-restore on the `deleted`
/// register is resolved by Automerge's deterministic LWW tiebreak: it
/// converges to a single, identical winner on every device (no divergence),
/// though *which* side wins is not something we rely on. Absent on v1 docs;
/// readers default it to `0`.
pub const FIELD_DELETED_AT: &str = "deleted_at";
