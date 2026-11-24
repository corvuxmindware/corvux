//! Note-relation queries and indexing. `LinkResolver` lives here because
//! it's built from one full scan of `notes{id}` and used immediately by the
//! per-note index (`index_note_full_with_resolver`) — splitting them across
//! files would just create a circular-feeling dependency.

use cozo::{DataValue, DbInstance, ScriptMutability};
use std::collections::{BTreeMap, HashMap};

use crate::parser::checkboxes::parse_tasks_in_note;
use crate::parser::links::parse_links;
use crate::parser::tags::{fold_tag, parse_tags, TagIdentities};

use super::path::{note_id_from_abs_path, note_rel_dir, shared_dir_prefix, title_from_path};
use super::types::NoteIndexEntry;

/// A dated note's own date, taken from its id's filename stem when that stem
/// is a valid ISO date (a daily note like `daily notes/2026-06-10`). The
/// note's frontmatter `date:` (handled inside the parser) takes precedence;
/// this is the fallback for a daily note created without a `date:` property.
fn note_date_from_id(note_id: &str) -> Option<String> {
    let stem = note_id.rsplit('/').next().unwrap_or(note_id);
    chrono::NaiveDate::parse_from_str(stem, "%Y-%m-%d")
        .is_ok()
        .then(|| stem.to_string())
}

/// List all notes in the roost, sorted by most recently updated first.
/// Used to inject a roost index into the AI's system prompt so it knows
/// what notes exist without relying on semantic search.
pub fn list_all_notes(db: &DbInstance, limit: usize) -> Result<Vec<NoteIndexEntry>, String> {
    let script = r#"
        ?[id, title, path, updated_at] := *notes{ id, title, path, updated_at }
        :order -updated_at
        :limit $limit
    "#;

    let mut params = BTreeMap::new();
    params.insert(
        "limit".to_string(),
        DataValue::Num(cozo::Num::Int(limit as i64)),
    );

    let result = db
        .run_script(script, params, ScriptMutability::Immutable)
        .map_err(|e| format!("List notes query failed: {e}"))?;

    let mut notes = Vec::new();
    for row in result.rows.iter() {
        if row.len() >= 3 {
            if let (Some(id), Some(title), Some(path)) = (
                row[0].get_str().map(String::from),
                row[1].get_str().map(String::from),
                row[2].get_str().map(String::from),
            ) {
                notes.push(NoteIndexEntry { id, title, path });
            }
        }
    }
    Ok(notes)
}

/// Hydrate a list of note ids into `{id, title, path}` rows. Order of
/// the returned vec mirrors the input — caller-supplied ordering is
/// preserved so a path of ids stays a path. Missing ids are silently
/// dropped (a note that was deleted between the algorithm running and
/// this lookup shouldn't fail the whole render).
pub fn lookup_notes_by_ids(db: &DbInstance, ids: &[String]) -> Result<Vec<NoteIndexEntry>, String> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let script = r#"
        ?[id, title, path] := *notes{ id, title, path }, is_in(id, $ids)
    "#;
    let mut params = BTreeMap::new();
    params.insert(
        "ids".to_string(),
        DataValue::List(
            ids.iter()
                .map(|id| DataValue::Str(id.clone().into()))
                .collect(),
        ),
    );
    let result = db
        .run_script(script, params, ScriptMutability::Immutable)
        .map_err(|e| format!("lookup_notes_by_ids failed: {e}"))?;

    let mut by_id: HashMap<String, NoteIndexEntry> = HashMap::with_capacity(result.rows.len());
    for row in result.rows.iter() {
        if row.len() >= 3 {
            if let (Some(id), Some(title), Some(path)) = (
                row[0].get_str().map(String::from),
                row[1].get_str().map(String::from),
                row[2].get_str().map(String::from),
            ) {
                by_id.insert(id.clone(), NoteIndexEntry { id, title, path });
            }
        }
    }
    let mut ordered = Vec::with_capacity(ids.len());
    for id in ids {
        if let Some(entry) = by_id.get(id) {
            ordered.push(entry.clone());
        }
    }
    Ok(ordered)
}

/// Top roost tags by usage count, descending. Drives the "Roost tag
/// inventory" section we inject into the agent's system prompt so it
/// reuses existing tags instead of inventing new ones (which would
/// fragment the knowledge graph with synonym/singleton noise — see
/// `bundle.rs` rule about tag reuse).
///
/// Returns `(tag, count)` rows — one per tag IDENTITY (LR-6): `#ML` and `#ml`
/// are one tag, counted once per NOTE, shown under their most-used stored
/// spelling. Stored rows stay verbatim; the fold happens here, so a graph.db
/// from before this rule and one from after it group identically. Caller
/// decides cap/formatting; we cap to `limit` so a roost with 5K unique tags
/// doesn't ship the whole tail to the LLM.
pub fn top_tags(db: &DbInstance, limit: usize) -> Result<Vec<(String, usize)>, String> {
    // Per-SPELLING note counts: a Cozo aggregate, so the output is
    // O(distinct spellings), never O(rows) — this runs on every chat send.
    let result = db
        .run_script(
            r#"?[tag, count(note_id)] := *tags{ note_id, tag }"#,
            BTreeMap::new(),
            ScriptMutability::Immutable,
        )
        .map_err(|e| format!("top_tags query failed: {e}"))?;

    let mut identities = TagIdentities::default();
    // key → (summed per-spelling counts, the spellings that fold to it)
    let mut groups: HashMap<String, (usize, Vec<String>)> = HashMap::new();
    for row in result.rows.iter() {
        if row.len() >= 2 {
            if let (Some(tag), Some(count)) = (
                row[0].get_str().map(String::from),
                row[1].get_int().map(|n| n as usize),
            ) {
                let key = identities.add(&tag, count);
                let group = groups.entry(key).or_default();
                group.0 += count;
                group.1.push(tag);
            }
        }
    }

    // A note carrying two spellings of one tag (`#ML` and `#ml`) is counted by
    // each spelling above. Recount ONLY the colliding identities by distinct
    // note — the common case (no collisions) costs nothing extra.
    let colliding: Vec<&String> = groups
        .values()
        .filter(|(_, spellings)| spellings.len() > 1)
        .flat_map(|(_, spellings)| spellings.iter())
        .collect();
    if !colliding.is_empty() {
        let mut params = BTreeMap::new();
        params.insert(
            "spellings".to_string(),
            DataValue::List(
                colliding
                    .iter()
                    .map(|s| DataValue::Str((*s).as_str().into()))
                    .collect(),
            ),
        );
        let rows = db
            .run_script(
                r#"?[tag, note_id] := *tags{ note_id, tag }, is_in(tag, $spellings)"#,
                params,
                ScriptMutability::Immutable,
            )
            .map_err(|e| format!("top_tags collision recount failed: {e}"))?;
        let mut notes_by_key: HashMap<String, std::collections::HashSet<String>> = HashMap::new();
        for row in rows.rows.iter() {
            if let (Some(tag), Some(note_id)) = (
                row.first().and_then(|v| v.get_str()),
                row.get(1).and_then(|v| v.get_str()),
            ) {
                notes_by_key
                    .entry(fold_tag(tag))
                    .or_default()
                    .insert(note_id.to_string());
            }
        }
        for (key, notes) in notes_by_key {
            if let Some(group) = groups.get_mut(&key) {
                group.0 = notes.len();
            }
        }
    }

    let mut out: Vec<(String, usize)> = groups
        .into_iter()
        .map(|(key, (count, _))| {
            let display = identities.display(&key).unwrap_or(&key).to_string();
            (display, count)
        })
        .collect();
    // Count desc; ties by spelling so the inventory is deterministic.
    out.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    out.truncate(limit);
    Ok(out)
}

/// All distinct tag spellings currently stored in the roost — VERBATIM, so a
/// roost holding `#ML` and `#ml` returns both. Used by write-tool validation to
/// reject tags the agent invented out of thin air (`roost_write`,
/// `roost_edit`). Callers MUST compare through [`fold_tag`] (build the lookup
/// set with `known.iter().map(|t| fold_tag(t))`), never by exact string —
/// exact comparison was the LR-6 gap. Cheaper than `top_tags` when the caller
/// only needs membership, not ranking.
pub fn all_tag_names(db: &DbInstance) -> Result<std::collections::HashSet<String>, String> {
    let script = r#"
        ?[tag] := *tags{ tag }
    "#;
    let result = db
        .run_script(script, BTreeMap::new(), ScriptMutability::Immutable)
        .map_err(|e| format!("all_tag_names query failed: {e}"))?;

    let mut out = std::collections::HashSet::with_capacity(result.rows.len());
    for row in result.rows.iter() {
        if let Some(tag) = row.first().and_then(|v| v.get_str()) {
            out.insert(tag.to_string());
        }
    }
    Ok(out)
}

/// Look up a note's canonical id by its absolute path. Used by command
/// handlers that receive a note path from the frontend and don't have
/// the roost root handy to reconstruct the id themselves.
pub fn lookup_note_id_by_path(db: &DbInstance, abs_path: &str) -> Result<Option<String>, String> {
    let mut params = BTreeMap::new();
    params.insert(
        "path".to_string(),
        DataValue::Str(abs_path.to_string().into()),
    );
    let result = db
        .run_script(
            r#"?[id] := *notes{ id, path }, path == $path"#,
            params,
            ScriptMutability::Immutable,
        )
        .map_err(|e| format!("Note id lookup failed: {e}"))?;
    Ok(result
        .rows
        .first()
        .and_then(|row| row.first())
        .and_then(|v| v.get_str())
        .map(String::from))
}

/// Wiki-link resolver — maps `[[target]]` strings to canonical note ids
/// using Obsidian's nearest-match rule:
///   1. Exact id match (target contains `/` and matches a note's full id).
///   2. Basename match scoped to the source note's folder subtree.
///   3. Basename match with longest shared path prefix against source folder.
///   4. If still ambiguous, pick the shortest path (depth tiebreak).
///
/// Built once per indexing pass from a single DB scan so repeated lookups
/// are O(1) instead of re-querying Cozo per link.
pub struct LinkResolver {
    /// Every existing note id (normalized relative path, no ext).
    id_set: std::collections::HashSet<String>,
    /// Basename (file stem, lowercased) → list of full note ids sharing it.
    by_basename: HashMap<String, Vec<String>>,
}

/// Outcome of classifying a `[[wiki-link]]` target against the roost — the
/// diagnostic counterpart to [`LinkResolver::resolve`], which silently picks
/// one note (or none) without saying whether it was sure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkResolution {
    /// Lands on exactly one note (unique basename or exact path match).
    Resolved(String),
    /// Basename shared by several notes — `resolve` guesses by folder
    /// proximity, so the link may point at the WRONG note. Candidates listed.
    Ambiguous(Vec<String>),
    /// No note matches — the link is dangling (creates no graph edge).
    Dangling,
}

/// Dangling + ambiguous links found in a note's content. Empty = clean.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct LinkReport {
    /// `[[targets]]` matching no note (deduped, first-seen order).
    pub dangling: Vec<String>,
    /// `(target, candidate ids)` for targets that could mean several notes.
    pub ambiguous: Vec<(String, Vec<String>)>,
}

impl LinkReport {
    pub fn is_empty(&self) -> bool {
        self.dangling.is_empty() && self.ambiguous.is_empty()
    }
}

/// Classify every `[[wiki-link]]` in `content` against `resolver`, collecting
/// the dangling and ambiguous ones (deduped by target). `source_rel_dir` is
/// the linking note's roost-relative folder, which drives nearest-folder
/// ranking. Powers the write-time link diagnostics so a bad link surfaces to
/// the agent instead of silently producing no edge / the wrong edge.
pub fn check_links(content: &str, resolver: &LinkResolver, source_rel_dir: &str) -> LinkReport {
    let mut report = LinkReport::default();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for link in parse_links(content.to_string()) {
        if !seen.insert(link.target.to_lowercase()) {
            continue;
        }
        match resolver.classify(&link.target, source_rel_dir) {
            LinkResolution::Resolved(_) => {}
            LinkResolution::Dangling => report.dangling.push(link.target),
            LinkResolution::Ambiguous(candidates) => {
                report.ambiguous.push((link.target, candidates));
            }
        }
    }
    report
}

impl LinkResolver {
    pub fn build(db: &DbInstance) -> Result<Self, String> {
        let result = db
            .run_script(
                r#"?[id] := *notes{ id }"#,
                Default::default(),
                ScriptMutability::Immutable,
            )
            .map_err(|e| format!("Resolver build failed: {e}"))?;

        let mut id_set = std::collections::HashSet::new();
        let mut by_basename: HashMap<String, Vec<String>> = HashMap::new();
        for row in &result.rows {
            let Some(id) = row.first().and_then(|v| v.get_str()).map(String::from) else {
                continue;
            };
            let basename = id
                .rsplit_once('/')
                .map(|(_, b)| b.to_string())
                .unwrap_or_else(|| id.clone());
            by_basename.entry(basename).or_default().push(id.clone());
            id_set.insert(id);
        }
        Ok(Self {
            id_set,
            by_basename,
        })
    }

    /// Resolve a wiki-link target to a canonical note id.
    /// `source_rel_dir` is the roost-relative folder of the linking note.
    ///
    /// Returns `None` if no matching note exists — the caller can choose
    /// to drop the edge or keep it pointing at a phantom id.
    pub fn resolve(&self, target: &str, source_rel_dir: &str) -> Option<String> {
        let normalized = Self::normalize_target(target);
        if normalized.is_empty() {
            return None;
        }

        // 1. Explicit path in the link — accept as-is if it resolves.
        if normalized.contains('/') && self.id_set.contains(&normalized) {
            return Some(normalized);
        }

        // Basename lookup covers both `[[Note]]` and `[[Folder/Note]]` forms.
        let basename = normalized
            .rsplit_once('/')
            .map(|(_, b)| b.to_string())
            .unwrap_or_else(|| normalized.clone());

        let candidates = self.by_basename.get(&basename)?;

        if candidates.len() == 1 {
            return Some(candidates[0].clone());
        }

        // 2/3. Rank by shared folder prefix with the linking note; tiebreak
        // on shortest id (prefers shallower files, matching Obsidian's bias
        // toward roost-root notes over deeply-nested duplicates).
        candidates
            .iter()
            .max_by(|a, b| {
                let sa = shared_dir_prefix(a, source_rel_dir);
                let sb = shared_dir_prefix(b, source_rel_dir);
                sa.cmp(&sb)
                    .then_with(|| b.split('/').count().cmp(&a.split('/').count()))
                    // LR-4: final total-order tiebreak so a link with two
                    // equally-close, equally-shallow candidates always resolves
                    // to the SAME note. Without it, `max_by` returned whichever
                    // candidate happened to be last in `by_basename` insertion
                    // order — which differs between a cold bulk index and
                    // incremental `add_id`s, so the edge could flip across
                    // restarts. `b.cmp(a)` picks the lexicographically smaller
                    // id, matching the ascending order `classify` sorts its
                    // Ambiguous list into.
                    .then_with(|| b.cmp(a))
            })
            .cloned()
    }

    /// Like [`resolve`](Self::resolve), but reports *why*: whether the target
    /// lands cleanly, is ambiguous (the basename is shared and folder
    /// proximity gives no clear winner, so `resolve` is just guessing), or is
    /// dangling (matches nothing). Drives the write-time link diagnostics.
    ///
    /// A shared basename is only flagged `Ambiguous` when the pick is a
    /// genuine guess: if exactly one candidate shares a deeper folder prefix
    /// with the linking note than all others, that's a confident match (e.g. a
    /// `[[puli-fitness]]` link from inside the right daily folder), so it's
    /// `Resolved` — not noise.
    pub fn classify(&self, target: &str, source_rel_dir: &str) -> LinkResolution {
        let normalized = Self::normalize_target(target);
        if normalized.is_empty() {
            return LinkResolution::Dangling;
        }
        // Explicit, fully-qualified path that exists → unambiguous.
        if normalized.contains('/') && self.id_set.contains(&normalized) {
            return LinkResolution::Resolved(normalized);
        }
        let basename = normalized
            .rsplit_once('/')
            .map(|(_, b)| b.to_string())
            .unwrap_or_else(|| normalized.clone());
        let Some(candidates) = self.by_basename.get(&basename) else {
            return LinkResolution::Dangling;
        };
        if candidates.len() == 1 {
            return LinkResolution::Resolved(candidates[0].clone());
        }
        // Multiple notes share the basename. It's a confident match only when
        // ONE candidate shares a strictly-deeper folder prefix with the source
        // than every other; otherwise `resolve` picks arbitrarily → ambiguous.
        let best = candidates
            .iter()
            .map(|c| shared_dir_prefix(c, source_rel_dir))
            .max()
            .unwrap_or(0);
        let winners = candidates
            .iter()
            .filter(|c| shared_dir_prefix(c, source_rel_dir) == best)
            .count();
        if best > 0 && winners == 1 {
            // Same confident pick `resolve` would make.
            return match self.resolve(target, source_rel_dir) {
                Some(id) => LinkResolution::Resolved(id),
                None => LinkResolution::Dangling,
            };
        }
        let mut sorted = candidates.clone();
        sorted.sort();
        LinkResolution::Ambiguous(sorted)
    }

    pub fn normalize_target(target: &str) -> String {
        let t = target.trim();
        // LR-1: strip any in-note anchor — `#heading`, `#heading#sub`,
        // `^blockid`, `#^blockid`. The anchor addresses a location WITHIN the
        // target note, so it's not part of the note's identity. Without this,
        // `[[Note#Section]]` never matched `note` → the link was always
        // classified Dangling (a false "broken link" warning) and no backlink
        // edge was created, even though MCP's resolver stripped the anchor and
        // resolved it fine (an app-vs-MCP parity break). A bare `[[#Heading]]`
        // (same-note anchor) normalizes to empty → no cross-note edge, correct.
        let t = match t.find(['#', '^']) {
            Some(idx) => t[..idx].trim(),
            None => t,
        };
        let t = t
            .strip_suffix(".md")
            .or_else(|| t.strip_suffix(".MD"))
            .unwrap_or(t);
        t.replace('\\', "/").trim_start_matches('/').to_lowercase()
    }

    fn basename_of(id: &str) -> String {
        id.rsplit_once('/')
            .map(|(_, b)| b.to_string())
            .unwrap_or_else(|| id.to_string())
    }

    /// Insert a note id into the cache. Idempotent — re-inserting an
    /// existing id is a no-op. Used by `index_note` to keep the cached
    /// resolver fresh without rescanning `notes{ id }` from CozoDB.
    pub fn add_id(&mut self, id: &str) {
        if self.id_set.insert(id.to_string()) {
            let basename = Self::basename_of(id);
            self.by_basename
                .entry(basename)
                .or_default()
                .push(id.to_string());
        }
    }

    /// Is this note id in the cache?
    pub fn contains_id(&self, id: &str) -> bool {
        self.id_set.contains(id)
    }

    /// Remove a note id from the cache. Idempotent. Used to roll back an
    /// `add_id` when the index transaction that was meant to create the note
    /// failed, so the resolver never advertises a ghost id that resolves to
    /// nothing in the graph.
    pub fn remove_id(&mut self, id: &str) {
        if self.id_set.remove(id) {
            let basename = Self::basename_of(id);
            if let Some(v) = self.by_basename.get_mut(&basename) {
                v.retain(|x| x != id);
                if v.is_empty() {
                    self.by_basename.remove(&basename);
                }
            }
        }
    }
}

/// File modification time as fractional seconds since the Unix epoch, or
/// `None` when the path can't be `stat`ed as a local file.
///
/// On native backends `abs_path` is a real filesystem path, so this is the
/// file's mtime. On Android SAF the "absolute" path is a `content://` tree URI
/// that `std::fs` can't stat, so this returns `None` and callers fall back to
/// wall-clock. That mtime is used two ways:
///   - IP-A2: it's the `updated_at` we store, so the AI system-prompt's
///     "recent notes" ordering reflects real edit time, not index time (which
///     a cold rebuild would otherwise collapse to a single `now()` for all).
///   - IP-A1: the warm heal compares it against the stored value to re-index
///     notes edited while Corvux was closed.
///
/// (Android's SAF `lastModified` — reachable over JNI — is a scoped follow-up;
/// until it lands, mobile falls back to wall-clock and the modified-while-
/// closed heal is a no-op there. On mobile the roost is normally the sync
/// client, and the sync engine already re-indexes files it receives, so the
/// sync-OFF external-edit scenario this guards is desktop-shaped.)
pub fn file_mtime_secs(abs_path: &str) -> Option<f64> {
    // F7 hardening: `symlink_metadata` does NOT follow a final symlink, so a
    // vault-folder attacker cannot swap an indexed note for a link and use
    // the TARGET's mtime as an existence/oracle signal here. For real notes
    // (never symlinks — RoostFs rejects link escapes) it is identical to
    // `metadata`. Full fix is still tracked: store roost-relative paths and
    // take mtimes through the backend.
    std::fs::symlink_metadata(abs_path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs_f64())
}

/// Wall-clock seconds since the Unix epoch — the fallback timestamp when a
/// file's mtime can't be read (see [`file_mtime_secs`]).
fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

// ── Atomic single-note indexing (DB-4) ──────────────────────────────────────
//
// A note's derived rows live in five relations: `notes` (carries the mtime
// stamp the warm heal keys on), `tags`, `task`, `links`, `dangling`. They used
// to be written by 5–9 SEPARATE `run_script` calls, and on the SQLite backend
// every call is its own `begin; … commit;`. The `notes` row with the NEW mtime
// was written FIRST, so a crash / power loss mid-save left the note with
// stale-or-missing links and tags AND an up-to-date stamp — and the warm heal
// (`reconcile_added_and_modified_notes`: file mtime > stored `updated_at`)
// then saw "nothing to do" and skipped it until the note was next edited.
//
// Fix: every relation write for one note — the note row + stamp, tags, tasks,
// links, dangling rows, and the dangling→link promotions a brand-new note
// triggers — is staged into ONE multi-block CozoScript (`{…} {…}`) and run as
// a single write transaction. Cozo runs an imperative script inside one
// `SessionTx`; on SQLite that is one `begin;` … `commit;`, a failed block
// returns before the commit and the drop rolls back, and a crash leaves the
// pre-image (SQLite journal/WAL recovery). So the note is either fully old
// (stamp included → the heal re-detects it) or fully new — never torn.

/// An ordered list of CozoScript blocks executed as ONE transaction. Blocks are
/// static text; row sets are passed as `$params` so nothing user-controlled is
/// ever spliced into the script.
struct TxScript {
    blocks: Vec<&'static str>,
    params: BTreeMap<String, DataValue>,
}

impl TxScript {
    fn new() -> Self {
        Self {
            blocks: Vec::new(),
            params: BTreeMap::new(),
        }
    }

    /// Script scoped to one note: binds `$id` (the canonical note id) for the
    /// per-note `:rm` blocks.
    fn for_note(note_id: &str) -> Self {
        let mut tx = Self::new();
        tx.param("id", DataValue::Str(note_id.into()));
        tx
    }

    fn block(&mut self, body: &'static str) {
        self.blocks.push(body);
    }

    fn param(&mut self, name: &str, value: DataValue) {
        self.params.insert(name.to_string(), value);
    }

    /// Run every staged block in one write transaction. An empty script is a
    /// no-op.
    fn run(self, db: &DbInstance) -> Result<(), String> {
        if self.blocks.is_empty() {
            return Ok(());
        }
        let mut script = String::new();
        for body in &self.blocks {
            script.push_str("{\n");
            script.push_str(body);
            script.push_str("\n}\n");
        }
        // Test-only fail-point: a final block that errors AT RUNTIME, after
        // every staged write has already executed inside the transaction —
        // the shape of a crash between statements.
        #[cfg(test)]
        if tx_fail_point::armed() {
            script.push_str(tx_fail_point::POISON_BLOCK);
        }
        db.run_script(&script, self.params, ScriptMutability::Mutable)
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}

#[cfg(test)]
pub(crate) mod tx_fail_point {
    //! Thread-local fail-point for [`TxScript::run`]. While a [`Guard`] is
    //! alive on this thread, every transactional index write gets a trailing
    //! block that fails at runtime (a `:put` of a non-Float into a Float
    //! column), so the whole transaction must roll back. Thread-local so
    //! parallel tests never see each other's fail-point.
    use std::cell::Cell;

    thread_local! {
        static ARMED: Cell<bool> = const { Cell::new(false) };
        static POST_COMMIT_ARMED: Cell<bool> = const { Cell::new(false) };
    }

    pub(super) const POISON_BLOCK: &str = r#"{
?[id, title, path, updated_at] <- [["__fail_point__", "t", "p", "not-a-float"]]
:put notes { id => title, path, updated_at }
}
"#;

    pub(super) fn armed() -> bool {
        ARMED.with(Cell::get)
    }

    pub(super) fn post_commit_armed() -> bool {
        POST_COMMIT_ARMED.with(Cell::get)
    }

    pub(crate) struct Guard;

    impl Guard {
        pub(crate) fn arm() -> Self {
            ARMED.with(|a| a.set(true));
            Guard
        }
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            ARMED.with(|a| a.set(false));
        }
    }

    /// Fail-point AFTER the commit: the index transaction succeeds, then the
    /// post-commit measurement reads fail. Proves a committed index never
    /// reports `Err` and that callers drop (not trust) their caches.
    pub(crate) struct PostCommitGuard;

    impl PostCommitGuard {
        pub(crate) fn arm() -> Self {
            POST_COMMIT_ARMED.with(|a| a.set(true));
            PostCommitGuard
        }
    }

    impl Drop for PostCommitGuard {
        fn drop(&mut self) {
            POST_COMMIT_ARMED.with(|a| a.set(false));
        }
    }
}

// Per-note blocks. Row lookups use `x = $id, *rel{ x, .. }` (a constant
// unification joined into the relation's KEY PREFIX) — NOT `*rel{ x, .. },
// x == $id`, which Cozo plans as a filter over a FULL relation scan. The
// difference is O(this note) vs O(whole roost) on every save.
const B_PUT_NOTE: &str = r#"?[id, title, path, updated_at] <- [[$id, $title, $path, $updated_at]]
:put notes { id => title, path, updated_at }"#;
const B_RM_TAGS: &str = r#"?[note_id, tag] := note_id = $id, *tags{ note_id, tag }
:rm tags { note_id, tag }"#;
const B_PUT_TAGS: &str = r#"?[note_id, tag] <- $tag_rows
:put tags { note_id, tag }"#;
const B_RM_TASKS: &str = r#"?[note_id, source_line] := note_id = $id, *task{ note_id, source_line }
:rm task { note_id, source_line }"#;
const B_PUT_TASKS: &str = r#"?[note_id, source_line, text, content_anchor, due_at, completed, completed_at, linked_person, linked_project, updated_at] <- $task_rows
:put task { note_id, source_line => text, content_anchor, due_at, completed, completed_at, linked_person, linked_project, updated_at }"#;
const B_RM_LINKS: &str = r#"?[from_id, to_id] := from_id = $id, *links{ from_id, to_id }
:rm links { from_id, to_id }"#;
const B_RM_DANGLING: &str = r#"?[from_id, target] := from_id = $id, *dangling{ from_id, target }
:rm dangling { from_id, target }"#;
const B_PUT_LINKS: &str = r#"?[from_id, to_id, link_text] <- $link_rows
:put links { from_id, to_id => link_text }"#;
const B_PUT_DANGLING: &str = r#"?[from_id, target, link_text, target_base] <- $dangling_rows
:put dangling { from_id, target => link_text, target_base }"#;
const B_PROMOTE_PUT_LINKS: &str = r#"?[from_id, to_id, link_text] <- $promo_links
:put links { from_id, to_id => link_text }"#;
const B_PROMOTE_RM_DANGLING: &str = r#"?[from_id, target] <- $promo_dangling
:rm dangling { from_id, target }"#;

/// Stage the note row. `updated_at` is the stamp the warm heal compares against
/// the file mtime — it commits atomically with every other row of this note.
fn stage_note_row(tx: &mut TxScript, title: &str, abs_path: &str, updated_at: f64) {
    tx.param("title", DataValue::Str(title.into()));
    tx.param("path", DataValue::Str(abs_path.into()));
    tx.param("updated_at", DataValue::from(updated_at));
    tx.block(B_PUT_NOTE);
}

/// Stage "replace this note's tag set" (clear + insert).
fn stage_tags(tx: &mut TxScript, note_id: &str, content: &str) {
    tx.block(B_RM_TAGS);
    let tag_rows: Vec<DataValue> = parse_tags(content.to_string())
        .into_iter()
        .map(|tag| {
            DataValue::List(vec![
                DataValue::Str(note_id.into()),
                DataValue::Str(tag.tag.into()),
            ])
        })
        .collect();
    if !tag_rows.is_empty() {
        tx.param("tag_rows", DataValue::List(tag_rows));
        tx.block(B_PUT_TAGS);
    }
}

/// Stage "replace this note's `:task` rows" (clear + insert). Tasks are derived
/// from the same content as tags — single-note, no resolver needed.
fn stage_tasks(tx: &mut TxScript, note_id: &str, content: &str) {
    tx.block(B_RM_TASKS);

    // Convention 0.3: a checkbox is a task only with an effective date. Feed
    // the note's own date (daily-note stem; frontmatter `date:` wins inside
    // the parser) and today so dated/daily-note checkboxes and lone-⏰ lines
    // resolve correctly.
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let note_date = note_date_from_id(note_id);
    let parsed = parse_tasks_in_note(content, note_date.as_deref(), Some(&today));
    if parsed.is_empty() {
        return;
    }
    let now = now_secs();
    let rows: Vec<DataValue> = parsed
        .into_iter()
        .map(|task| {
            DataValue::List(vec![
                DataValue::Str(note_id.into()),
                DataValue::Num(cozo::Num::Int(task.source_line as i64)),
                DataValue::Str(task.text.into()),
                DataValue::Str(task.content_anchor.into()),
                DataValue::Str(task.due_at.into()),
                DataValue::Bool(task.completed),
                DataValue::Str(task.completed_at.into()),
                DataValue::Str(task.linked_person.into()),
                DataValue::Str(task.linked_project.into()),
                DataValue::from(now),
            ])
        })
        .collect();
    tx.param("task_rows", DataValue::List(rows));
    tx.block(B_PUT_TASKS);
}

/// Stage "replace this note's outgoing edges": clear its old links AND its old
/// dangling (unresolved) links — both sets are recomputed — then insert the
/// freshly resolved ones. Clearing dangling matters because a link that
/// previously had no target may now resolve (its target was created), so its
/// stale dangling row must go.
///
/// Resolved → a real `:links` edge. Unresolved → a `:dangling` row (instead of
/// being silently dropped), so when the target note is later created,
/// promotion turns it into a real edge immediately rather than waiting for THIS
/// note to be re-saved.
fn stage_links(
    tx: &mut TxScript,
    note_id: &str,
    rel_dir: &str,
    content: &str,
    resolver: &LinkResolver,
) {
    tx.block(B_RM_LINKS);
    tx.block(B_RM_DANGLING);

    let mut link_rows: Vec<DataValue> = Vec::new();
    let mut dangling_rows: Vec<DataValue> = Vec::new();
    for link in parse_links(content.to_string()) {
        let link_text = link.display.as_deref().unwrap_or(&link.target).to_string();
        match resolver.resolve(&link.target, rel_dir) {
            Some(to_id) => link_rows.push(DataValue::List(vec![
                DataValue::Str(note_id.into()),
                DataValue::Str(to_id.into()),
                DataValue::Str(link_text.into()),
            ])),
            None => {
                if let Some((target, base)) = dangling_key(&link.target) {
                    dangling_rows.push(DataValue::List(vec![
                        DataValue::Str(note_id.into()),
                        DataValue::Str(target.into()),
                        DataValue::Str(link_text.into()),
                        DataValue::Str(base.into()),
                    ]));
                }
            }
        }
    }
    if !link_rows.is_empty() {
        tx.param("link_rows", DataValue::List(link_rows));
        tx.block(B_PUT_LINKS);
    }
    if !dangling_rows.is_empty() {
        tx.param("dangling_rows", DataValue::List(dangling_rows));
        tx.block(B_PUT_DANGLING);
    }
}

/// Does this note id have a row in `notes`? Key lookup — O(1), unlike
/// [`lookup_note_id_by_path`] (path is not a key → full scan).
fn note_exists(db: &DbInstance, note_id: &str) -> Result<bool, String> {
    let mut params = BTreeMap::new();
    params.insert("id".to_string(), DataValue::Str(note_id.into()));
    let res = db
        .run_script(
            r#"?[title] := *notes{ id: $id, title }"#,
            params,
            ScriptMutability::Immutable,
        )
        .map_err(|e| format!("note existence lookup failed: {e}"))?;
    Ok(!res.rows.is_empty())
}

/// Resolved outgoing link targets of one note (one entry per `:links` row).
/// Key-prefix lookup — O(this note's links).
fn note_link_targets(db: &DbInstance, note_id: &str) -> Result<Vec<String>, String> {
    let mut params = BTreeMap::new();
    params.insert("id".to_string(), DataValue::Str(note_id.into()));
    let res = db
        .run_script(
            r#"?[to_id] := *links{ from_id: $id, to_id }"#,
            params,
            ScriptMutability::Immutable,
        )
        .map_err(|e| format!("note link lookup failed: {e}"))?;
    Ok(res
        .rows
        .iter()
        .filter_map(|r| r.first().and_then(|v| v.get_str()).map(String::from))
        .collect())
}

/// Number of `:tags` rows of one note. Key-prefix lookup — O(this note's tags).
fn note_tag_count(db: &DbInstance, note_id: &str) -> Result<usize, String> {
    let mut params = BTreeMap::new();
    params.insert("id".to_string(), DataValue::Str(note_id.into()));
    let res = db
        .run_script(
            r#"?[tag] := *tags{ note_id: $id, tag }"#,
            params,
            ScriptMutability::Immutable,
        )
        .map_err(|e| format!("note tag lookup failed: {e}"))?;
    Ok(res.rows.len())
}

/// What one atomic single-note index did. `links_delta` / `tags_delta` /
/// `notes_delta` are the EXACT change to the three graph-wide totals that
/// [`get_graph_stats`](super::get_graph_stats) counts (it counts relation
/// rows), measured over the rows this index touched — so `GraphDb` can keep its
/// cached totals correct without re-counting whole relations on every save.
#[derive(Debug, Clone)]
pub struct NoteIndexOutcome {
    pub note_id: String,
    /// The note had no `notes` row before this index (first save / newly
    /// discovered file).
    pub was_new: bool,
    /// This note's resolved outgoing link targets before / after the index.
    /// Callers compare them to decide whether the link topology changed.
    pub link_targets_before: Vec<String>,
    pub link_targets_after: Vec<String>,
    pub notes_delta: i64,
    pub links_delta: i64,
    pub tags_delta: i64,
    /// `false` when the index COMMITTED but the post-commit measurements could
    /// not be read: the deltas above are zeros and `link_targets_after` is
    /// empty — NOT the truth. Callers must treat every derived cache (stats
    /// snapshot, algo cache) as stale instead of patching it.
    pub deltas_trusted: bool,
}

/// IP-8 + DB-4: the full single-note (re)index — note row + tags + tasks +
/// links + dangling rows, plus dangling→link promotion when the note is new —
/// as ONE atomic transaction. Packaged for every path that (re)indexes a single
/// note: the editor save, AI roost/task edits, in-app quick-add, history
/// revert, folder-move re-index, and the warm heal. (The old `index_note_pass1`
/// alone never produced link edges; the old pass1/pass2 pair was torn by a
/// crash between the two — see the section comment above.)
///
/// Fresh-build entry point: rebuilds a `LinkResolver` from a full `*notes` scan
/// (O(roost)). Prefer [`index_note_full_with_resolver`] via
/// [`GraphDb::index_note_core`](crate::commands::graph::GraphDb::index_note_core)
/// when a cached resolver is available (IP-A3 / TRN-7).
pub fn index_note_full(
    db: &DbInstance,
    roost_root: &str,
    abs_path: &str,
    content: &str,
) -> Result<(), String> {
    // Build BEFORE the index: the shared impl `add_id`s this note's id, so a
    // brand-new note is still resolvable without the scan having seen it.
    let mut resolver = LinkResolver::build(db)?;
    index_note_full_with_resolver(db, roost_root, abs_path, content, &mut resolver).map(|_| ())
}

/// `index_note_full` against a caller-supplied resolver instead of a fresh
/// `LinkResolver::build` (O(roost)). The caller owns the resolver — typically
/// `GraphDb`'s cached one.
///
/// This note's own id is `add_id`'d BEFORE the links are resolved so a freshly
/// created note can resolve a self-link and so later saves see it. If the
/// transaction fails and this call was the one that added the id, the id is
/// removed again so the cache never advertises a note that isn't in the graph.
/// Because the resolver is maintained incrementally here, callers must NOT
/// invalidate it afterward: these paths only ever ADD a note id (a content
/// edit / task append / revert never deletes the note), so the cache stays
/// correct. Id-removing paths — delete / rename / purge — still invalidate the
/// cached resolver elsewhere, which forces the next save to rebuild.
///
/// Only O(this note) database work: key-prefix lookups, one transaction, and a
/// scan of `:dangling` for a NEW note's basename.
pub fn index_note_full_with_resolver(
    db: &DbInstance,
    roost_root: &str,
    abs_path: &str,
    content: &str,
    resolver: &mut LinkResolver,
) -> Result<NoteIndexOutcome, String> {
    let note_id = note_id_from_abs_path(abs_path, roost_root);
    let rel_dir = note_rel_dir(abs_path, roost_root);
    let title = title_from_path(abs_path);
    // IP-A2: prefer the file's mtime so ordering by `updated_at` reflects real
    // edit time and survives a cold rebuild; fall back to now() on backends we
    // can't stat (SAF).
    let updated_at = file_mtime_secs(abs_path).unwrap_or_else(now_secs);

    // Measured BEFORE the write, over this note's partition of each relation.
    // Always measured (even for a "new" note) so a leftover orphan row can't
    // skew the delta — the transaction clears them either way.
    let was_new = !note_exists(db, &note_id)?;
    let link_targets_before = note_link_targets(db, &note_id)?;
    let tags_before = note_tag_count(db, &note_id)?;

    let resolver_had_id = resolver.contains_id(&note_id);
    resolver.add_id(&note_id);

    let mut tx = TxScript::for_note(&note_id);
    stage_note_row(&mut tx, &title, abs_path, updated_at);
    stage_tags(&mut tx, &note_id, content);
    stage_tasks(&mut tx, &note_id, content);
    stage_links(&mut tx, &note_id, &rel_dir, content, resolver);

    // A NEW note may be the target other notes were waiting on: promote their
    // dangling rows inside the SAME transaction (backlinks appear immediately,
    // and a crash can't leave the note indexed but its backlinks unpromoted).
    // A planning failure only loses the promotion (as before — it was a
    // warn-only step), never the index itself.
    let mut promotions = Vec::new();
    if was_new {
        match plan_promotions(db, resolver, &note_id) {
            // This note's own edges are fully determined by `stage_links`.
            Ok(mut planned) => {
                planned.retain(|p| p.from_id != note_id);
                promotions = planned;
            }
            Err(e) => {
                tracing::warn!(%note_id, error = %e, "index_note_full: promote planning failed");
            }
        }
        stage_promotions(&mut tx, &promotions);
    }

    if let Err(e) = tx.run(db) {
        if !resolver_had_id {
            resolver.remove_id(&note_id);
        }
        return Err(format!("Failed to index note: {e}"));
    }

    // ── COMMIT POINT ────────────────────────────────────────────────────────
    // Everything above this line is rolled back on error. From here the note IS
    // indexed, so nothing below may turn that into an `Err` (a caller would
    // treat "Err" as "nothing changed", skip its cache maintenance, and keep
    // serving a snapshot that no longer matches the graph). The post-commit
    // measurements are therefore best-effort: if they can't be read, the
    // outcome says so (`deltas_trusted: false`) and callers drop their caches
    // so the next reader recomputes from the DB.
    let promoted_new_edges = promotions.iter().filter(|p| p.new_edge).count() as i64;
    match read_post_commit_state(db, &note_id) {
        Ok((link_targets_after, tags_after)) => Ok(NoteIndexOutcome {
            notes_delta: i64::from(was_new),
            links_delta: link_targets_after.len() as i64 - link_targets_before.len() as i64
                + promoted_new_edges,
            tags_delta: tags_after as i64 - tags_before as i64,
            was_new,
            note_id,
            link_targets_before,
            link_targets_after,
            deltas_trusted: true,
        }),
        Err(e) => {
            tracing::warn!(%note_id, error = %e, "index_note_full: committed, but post-commit read failed; callers must drop derived caches");
            Ok(NoteIndexOutcome {
                notes_delta: 0,
                links_delta: 0,
                tags_delta: 0,
                was_new,
                note_id,
                link_targets_before,
                link_targets_after: Vec::new(),
                deltas_trusted: false,
            })
        }
    }
}

/// This note's `(link targets, tag count)` AFTER the index transaction
/// committed. Separate fn so tests can fail exactly this step.
fn read_post_commit_state(db: &DbInstance, note_id: &str) -> Result<(Vec<String>, usize), String> {
    #[cfg(test)]
    if tx_fail_point::post_commit_armed() {
        return Err("injected post-commit read failure".to_string());
    }
    Ok((
        note_link_targets(db, note_id)?,
        note_tag_count(db, note_id)?,
    ))
}

/// Test fixture: pass 1 alone (note row + tags + tasks, no link edges), as ONE
/// transaction. The shipped indexer never splits a note across passes any more
/// (see [`index_note_full_with_resolver`]); tests use this to build "note exists
/// but its links haven't been resolved yet" states.
#[cfg(test)]
fn index_note_pass1(
    db: &DbInstance,
    roost_root: &str,
    abs_path: &str,
    content: &str,
) -> Result<(), String> {
    let note_id = note_id_from_abs_path(abs_path, roost_root);
    let updated_at = file_mtime_secs(abs_path).unwrap_or_else(now_secs);
    let mut tx = TxScript::for_note(&note_id);
    stage_note_row(&mut tx, &title_from_path(abs_path), abs_path, updated_at);
    stage_tags(&mut tx, &note_id, content);
    stage_tasks(&mut tx, &note_id, content);
    tx.run(db)
}

/// Test fixture: pass 2 alone (resolve + replace this note's outgoing edges).
#[cfg(test)]
fn index_note_pass2(
    db: &DbInstance,
    roost_root: &str,
    abs_path: &str,
    content: &str,
    resolver: &LinkResolver,
) -> Result<(), String> {
    let note_id = note_id_from_abs_path(abs_path, roost_root);
    let rel_dir = note_rel_dir(abs_path, roost_root);
    let mut tx = TxScript::for_note(&note_id);
    stage_links(&mut tx, &note_id, &rel_dir, content, resolver);
    tx.run(db)
}

/// `(normalized_target, basename)` for a raw `[[target]]`, or `None` when the
/// target is empty. Keys a `:dangling` row the same way `LinkResolver` matches,
/// so `promote_dangling_links` finds it by basename when the target is created.
fn dangling_key(raw_target: &str) -> Option<(String, String)> {
    let target = LinkResolver::normalize_target(raw_target);
    if target.is_empty() {
        return None;
    }
    let base = target
        .rsplit_once('/')
        .map(|(_, b)| b.to_string())
        .unwrap_or_else(|| target.clone());
    Some((target, base))
}

/// One dangling row that now resolves to a real note.
struct Promotion {
    from_id: String,
    to_id: String,
    link_text: String,
    /// The `:dangling` row's key (`from_id`, `target`) to delete once promoted.
    target: String,
    /// Promoting this row adds a NEW `:links` row (the edge didn't exist and no
    /// earlier promotion in this batch adds it) — i.e. it moves the graph-wide
    /// link total. A `:put` onto an existing `(from_id, to_id)` key doesn't.
    new_edge: bool,
}

/// Find the dangling links that now resolve to the just-created `new_id`.
///
/// Scans `:dangling` filtered to the new note's basename (the common case is
/// zero or a handful of rows; `target_base` is not a key column, so this is a
/// scan of the dangling relation — paid only when a NEW note appears, never on
/// an ordinary save) and re-resolves each through the resolver, which must
/// already contain `new_id`. Read-only: stage the result with
/// [`stage_promotions`].
fn plan_promotions(
    db: &DbInstance,
    resolver: &LinkResolver,
    new_id: &str,
) -> Result<Vec<Promotion>, String> {
    let base = new_id
        .rsplit_once('/')
        .map(|(_, b)| b.to_string())
        .unwrap_or_else(|| new_id.to_string());

    let mut params = BTreeMap::new();
    params.insert("base".to_string(), DataValue::Str(base.into()));
    let rows = db
        .run_script(
            r#"?[from_id, target, link_text] :=
                 *dangling{ from_id, target, link_text, target_base },
                 target_base == $base"#,
            params,
            ScriptMutability::Immutable,
        )
        .map_err(|e| format!("promote_dangling: query failed: {e}"))?
        .rows;

    let mut planned: Vec<Promotion> = Vec::new();
    let mut seen_edges: std::collections::HashSet<(String, String)> =
        std::collections::HashSet::new();
    for row in rows {
        let (Some(from_id), Some(target), Some(link_text)) = (
            row.first().and_then(|v| v.get_str()),
            row.get(1).and_then(|v| v.get_str()),
            row.get(2).and_then(|v| v.get_str()),
        ) else {
            continue;
        };
        // The dangling row's `from_id` encodes the source folder for the
        // resolver's nearest-folder ranking.
        let source_rel_dir = from_id.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
        let Some(to_id) = resolver.resolve(target, source_rel_dir) else {
            continue; // still unresolved (e.g. a different basename collision)
        };
        let edge = (from_id.to_string(), to_id.clone());
        let new_edge = if seen_edges.contains(&edge) {
            false
        } else {
            let exists = link_exists(db, from_id, &to_id)?;
            seen_edges.insert(edge);
            !exists
        };
        planned.push(Promotion {
            from_id: from_id.to_string(),
            to_id,
            link_text: link_text.to_string(),
            target: target.to_string(),
            new_edge,
        });
    }
    Ok(planned)
}

/// Does the `:links` row `from_id → to_id` already exist? Full-key lookup.
fn link_exists(db: &DbInstance, from_id: &str, to_id: &str) -> Result<bool, String> {
    let mut params = BTreeMap::new();
    params.insert("f".to_string(), DataValue::Str(from_id.into()));
    params.insert("t".to_string(), DataValue::Str(to_id.into()));
    let res = db
        .run_script(
            r#"?[link_text] := *links{ from_id: $f, to_id: $t, link_text }"#,
            params,
            ScriptMutability::Immutable,
        )
        .map_err(|e| format!("promote_dangling: link lookup failed: {e}"))?;
    Ok(!res.rows.is_empty())
}

/// Stage "turn these dangling rows into real edges": every promoted `:links`
/// row is written and every promoted `:dangling` row deleted by the same
/// transaction, so a crash can't leave an edge half-promoted. No-op when empty.
fn stage_promotions(tx: &mut TxScript, promotions: &[Promotion]) {
    if promotions.is_empty() {
        return;
    }
    let link_rows: Vec<DataValue> = promotions
        .iter()
        .map(|p| {
            DataValue::List(vec![
                DataValue::Str(p.from_id.as_str().into()),
                DataValue::Str(p.to_id.as_str().into()),
                DataValue::Str(p.link_text.as_str().into()),
            ])
        })
        .collect();
    let dangling_rows: Vec<DataValue> = promotions
        .iter()
        .map(|p| {
            DataValue::List(vec![
                DataValue::Str(p.from_id.as_str().into()),
                DataValue::Str(p.target.as_str().into()),
            ])
        })
        .collect();
    tx.param("promo_links", DataValue::List(link_rows));
    tx.param("promo_dangling", DataValue::List(dangling_rows));
    tx.block(B_PROMOTE_PUT_LINKS);
    tx.block(B_PROMOTE_RM_DANGLING);
}

/// Promote dangling links that now resolve to the just-created `new_id` into
/// real `:links` edges, and delete the promoted `:dangling` rows — in one
/// transaction. The save path no longer calls this separately (it is fused
/// into [`index_note_full_with_resolver`]'s transaction); it remains for
/// callers/tests that promote against an already-indexed note.
///
/// A `[[NewNote]]` written before NewNote existed becomes a backlink the moment
/// NewNote is created — without waiting for the referencing note to be
/// re-saved. Returns the number of dangling rows promoted.
pub fn promote_dangling_links(
    db: &DbInstance,
    resolver: &LinkResolver,
    new_id: &str,
) -> Result<usize, String> {
    let promotions = plan_promotions(db, resolver, new_id)?;
    if promotions.is_empty() {
        return Ok(0);
    }
    let mut tx = TxScript::new();
    stage_promotions(&mut tx, &promotions);
    tx.run(db)
        .map_err(|e| format!("promote_dangling: write failed: {e}"))?;
    Ok(promotions.len())
}

/// Re-derive `:task` rows for a single note. Clears every existing task
/// row for `note_id`, parses the markdown for `- [ ]` / `- [x]` checkboxes
/// and frontmatter `tasks:`, and inserts the new set — clear and insert in ONE
/// transaction, so a crash can't leave the note with its tasks wiped.
///
/// Used by the task-edit paths (`complete_task` etc.) that rewrite one checkbox
/// on disk and need the pane to reflect it. The full per-note index
/// ([`index_note_full_with_resolver`]) stages the same blocks. The parse is
/// pure regex (`parser::checkboxes`) — no LLM call, no embedding, cheap to run.
pub fn index_note_tasks(db: &DbInstance, note_id: &str, content: &str) -> Result<(), String> {
    let mut tx = TxScript::for_note(note_id);
    stage_tasks(&mut tx, note_id, content);
    tx.run(db)
        .map_err(|e| format!("Failed to reindex tasks for note: {e}"))
}

/// Remove every trace of `note_id` from the graph relations:
/// `notes`, outgoing + incoming `links`, `tags`, and `task`. Called from
/// the file-delete and file-rename paths so deleted markdown files don't
/// leave dangling rows behind that pollute graph algorithms (PageRank
/// treats orphaned ids as 0-degree nodes that shouldn't exist).
///
/// (The embeddings/concepts/episodic relations were removed in the
/// Stage-2 teardown, so there are no chunk/embed_queue rows to purge.)
///
/// Best-effort per relation — a missing row in any single table doesn't
/// fail the whole purge. The caller is also expected to invalidate the
/// LinkResolver cache; we don't reach into `GraphDb` from this layer
/// because `db::queries` sits below `commands`.
///
/// AUDIT-069: returns `Result<(), Vec<String>>` so a failure after
/// `notes` succeeded surfaces to the caller instead of being hidden
/// behind a `let _ = ...`. Each relation is attempted regardless of
/// whether earlier ones failed. The Err variant carries one string per
/// failed relation so callers can log structured context.
pub fn purge_note(db: &DbInstance, note_id: &str) -> Result<(), Vec<String>> {
    let mut errors: Vec<String> = Vec::new();

    fn id_param(note_id: &str, key: &str) -> BTreeMap<String, DataValue> {
        let mut p = BTreeMap::new();
        p.insert(key.to_string(), DataValue::Str(note_id.to_string().into()));
        p
    }

    // IP-3/LR-3: convert INCOMING links (edges OTHER notes point at this one)
    // into `:dangling` rows BEFORE the steps below delete them. Without this,
    // deleting or renaming a note silently destroyed every backlink to it —
    // the edges were dropped but never recorded as dangling, so recreating the
    // note (or renaming it back to the same name) never re-promoted them.
    // Each incoming link carries its original `[[link_text]]`, which we re-key
    // via `dangling_key` so `promote_dangling_links` re-links it the moment a
    // matching note reappears. Best-effort: a failure here only means those
    // specific backlinks won't auto-restore (the pre-fix behaviour).
    match db.run_script(
        r#"?[from_id, link_text] := *links{ from_id, to_id, link_text }, to_id == $id"#,
        id_param(note_id, "id"),
        ScriptMutability::Immutable,
    ) {
        Ok(res) => {
            let mut dangling_rows: Vec<DataValue> = Vec::new();
            for row in res.rows {
                let (Some(from_id), Some(link_text)) = (
                    row.first().and_then(|v| v.get_str()),
                    row.get(1).and_then(|v| v.get_str()),
                ) else {
                    continue;
                };
                if let Some((target, target_base)) = dangling_key(link_text) {
                    dangling_rows.push(DataValue::List(vec![
                        DataValue::Str(from_id.to_string().into()),
                        DataValue::Str(target.into()),
                        DataValue::Str(link_text.to_string().into()),
                        DataValue::Str(target_base.into()),
                    ]));
                }
            }
            if !dangling_rows.is_empty() {
                let mut params = BTreeMap::new();
                params.insert("rows".to_string(), DataValue::List(dangling_rows));
                if let Err(e) = db.run_script(
                    r#"?[from_id, target, link_text, target_base] <- $rows
                       :put dangling { from_id, target => link_text, target_base }"#,
                    params,
                    ScriptMutability::Mutable,
                ) {
                    errors.push(format!("purge_note: danglify incoming links: {e}"));
                }
            }
        }
        Err(e) => errors.push(format!("purge_note: query incoming links: {e}")),
    }

    // DB-4: ORDER IS LOAD-BEARING — the `notes` row goes LAST. A crash / power
    // loss part-way through leaves the `notes` row (and so the note in the
    // graph) for a file that is already gone from disk, which the next warm
    // open's `reconcile_deleted_notes` finds and purges completely. With the
    // `notes` row first, an interrupted purge left orphan links/tags/tasks
    // under an id no listing could ever reach — nothing would clean them.
    // Per-note rows use `x = $id, *rel{ x, .. }` (key-prefix join), not a
    // `*rel{ .. }, x == $id` full-relation scan.
    let steps: [(&str, &str, BTreeMap<String, DataValue>); 6] = [
        (
            "links_outgoing",
            r#"?[from_id, to_id] := from_id = $id, *links{ from_id, to_id }
               :rm links { from_id, to_id }"#,
            id_param(note_id, "id"),
        ),
        (
            // incoming links: drop edges OTHER notes had pointing at
            // this deleted note. Without this, backlinks queries for
            // OTHER notes would still surface the deleted note.
            "links_incoming",
            r#"?[from_id, to_id] := *links{ from_id, to_id }, to_id == $id
               :rm links { from_id, to_id }"#,
            id_param(note_id, "id"),
        ),
        (
            "tags",
            r#"?[note_id, tag] := note_id = $note_id, *tags{ note_id, tag }
               :rm tags { note_id, tag }"#,
            id_param(note_id, "note_id"),
        ),
        (
            // drop derived `- [ ]` rows for the note — without this,
            // tasks linger in the pane until the next full re-index.
            "task",
            r#"?[note_id, source_line] := note_id = $note_id, *task{ note_id, source_line }
               :rm task { note_id, source_line }"#,
            id_param(note_id, "note_id"),
        ),
        (
            // drop this note's OWN dangling (unresolved) links so they don't
            // leak after deletion. (Incoming links from OTHER notes are
            // converted back into `:dangling` rows in the pre-step above
            // before `links_incoming` removes them, so a delete-then-recreate
            // or rename-back auto-restores those backlinks.)
            "dangling",
            r#"?[from_id, target] := from_id = $id, *dangling{ from_id, target }
               :rm dangling { from_id, target }"#,
            id_param(note_id, "id"),
        ),
        (
            // LAST (see the ordering note above): the stamp row that keeps an
            // interrupted purge discoverable by the warm-open ghost reconcile.
            "notes",
            r#"?[id] := id = $id, *notes{ id }
               :rm notes { id }"#,
            id_param(note_id, "id"),
        ),
    ];

    for (relation, script, params) in steps {
        if let Err(e) = db.run_script(script, params, ScriptMutability::Mutable) {
            errors.push(format!("purge_note: {relation}: {e}"));
        }
    }

    // A purged note must also drop its cached graph-algorithm results
    // (PageRank rank, community label). Without this, `cached_page_rank` /
    // `cached_community_detection` keep returning the deleted id to the UI
    // and the agent. Centralized here so EVERY purge path is covered —
    // in-app delete, rename (purge old id), synced delete, and
    // reconcile-on-open. The per-command invalidation this replaces was
    // present only on the save path, so delete and rename leaked stale
    // ranks. The wipe is blunt (whole cache); a purge is rare and the next
    // read recomputes once, matching how an edit already invalidates.
    if let Err(e) = super::algorithms::invalidate_algo_cache(db) {
        errors.push(format!("purge_note: algo_cache: {e}"));
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// Set of every note id currently in the graph. Used by the warm-open
/// reconcile to detect files that exist on disk but have no node yet
/// (added-while-closed — IP-2).
pub fn note_ids_set(db: &DbInstance) -> Result<std::collections::HashSet<String>, String> {
    let result = db
        .run_script(
            r#"?[id] := *notes{ id }"#,
            Default::default(),
            ScriptMutability::Immutable,
        )
        .map_err(|e| format!("note_ids_set query failed: {e}"))?;
    Ok(result
        .rows
        .iter()
        .filter_map(|row| row.first().and_then(|v| v.get_str()).map(String::from))
        .collect())
}

/// Map of note id → stored `updated_at` (the file mtime we last indexed, on
/// backends that expose it). The warm heal compares this against each file's
/// current disk mtime to find notes edited while Corvux was closed (IP-A1).
/// Fetched in one scan alongside `note_ids_set`'s presence check.
pub fn note_updated_ats(db: &DbInstance) -> Result<std::collections::HashMap<String, f64>, String> {
    let result = db
        .run_script(
            r#"?[id, updated_at] := *notes{ id, updated_at }"#,
            Default::default(),
            ScriptMutability::Immutable,
        )
        .map_err(|e| format!("note_updated_ats query failed: {e}"))?;
    Ok(result
        .rows
        .iter()
        .filter_map(|row| {
            let id = row.first().and_then(|v| v.get_str()).map(String::from)?;
            let updated_at = match row.get(1) {
                Some(DataValue::Num(cozo::Num::Float(f))) => *f,
                Some(DataValue::Num(cozo::Num::Int(i))) => *i as f64,
                _ => return None,
            };
            Some((id, updated_at))
        })
        .collect())
}

/// Count of dangling (unresolved) link rows across the roost — a `[[link]]`
/// whose target note doesn't exist yet. Surfaced in the diagnostic report
/// (IP-A8) as an index-health signal; a large or growing count points at
/// broken wiki-links worth cleaning up.
pub fn dangling_count(db: &DbInstance) -> Result<usize, String> {
    let result = db
        .run_script(
            r#"?[count(from_id)] := *dangling{ from_id, target }"#,
            Default::default(),
            ScriptMutability::Immutable,
        )
        .map_err(|e| format!("dangling_count query failed: {e}"))?;
    Ok(result
        .rows
        .first()
        .and_then(|row| row.first())
        .and_then(|v| match v {
            DataValue::Num(cozo::Num::Int(i)) => Some(*i as usize),
            _ => None,
        })
        .unwrap_or(0))
}

/// Heal the graph index against disk: purge every `*notes` row (and all
/// its links/tags/tasks) whose markdown file no longer exists in the
/// roost. Returns the number of notes purged.
///
/// `disk_paths` are the absolute/display paths of every `.md` file
/// currently in the roost (from `walk::list_markdown_path_pairs`); each is
/// canonicalized with `note_id_from_abs_path` the same way indexing does,
/// then any DB note id NOT in that set is purged.
///
/// Why this exists: Corvux's own delete path (`delete_file` → `purge_note`)
/// keeps the index clean for in-app deletes, but a file removed or moved by
/// ANYTHING else — the OS file manager, `git`, a cloud-sync client — while
/// the app was closed never hits it. The warm-open fast path then loads the
/// stale persisted DB verbatim, leaving the deleted note as a ghost node in
/// the graph forever. Running this on roost open closes that gap.
///
/// Best-effort: a `purge_note` failure on one ghost is logged and skipped
/// so one bad row can't block healing the rest.
pub fn reconcile_deleted_notes(
    db: &DbInstance,
    roost_root: &str,
    disk_paths: &[String],
) -> Result<usize, String> {
    let on_disk: std::collections::HashSet<String> = disk_paths
        .iter()
        .map(|p| note_id_from_abs_path(p, roost_root))
        .collect();

    let result = db
        .run_script(
            r#"?[id] := *notes{ id }"#,
            Default::default(),
            ScriptMutability::Immutable,
        )
        .map_err(|e| format!("reconcile: list notes failed: {e}"))?;

    let ghosts: Vec<String> = result
        .rows
        .iter()
        .filter_map(|row| row.first().and_then(|v| v.get_str()).map(String::from))
        .filter(|id| !on_disk.contains(id))
        .collect();

    for id in &ghosts {
        if let Err(errs) = purge_note(db, id) {
            for e in &errs {
                tracing::warn!(note_id = %id, error = %e, "reconcile: purge_note relation failed");
            }
        }
    }
    Ok(ghosts.len())
}

/// Roost-relative display paths (original case) of every indexed note under
/// directory `rel_prefix`, segment-aware (`projects` matches `projects/a.md`
/// but NOT the sibling `projects-archive/c.md`).
///
/// Used to expand an external folder delete — observed by the index watcher
/// while sync is OFF — into a delete per nested note: the files are already
/// gone from disk, so the graph is the only record of what was inside.
/// Matching reuses [`note_id_from_abs_path`]'s normalization (lowercase,
/// `/`-separated, extension stripped) so it's robust to path-form and case
/// differences; the returned paths keep their ORIGINAL case so the frontend
/// can resolve them against the file tree for FTS cleanup.
pub fn note_paths_under_dir(
    db: &DbInstance,
    roost_root: &str,
    rel_prefix: &str,
) -> Result<Vec<String>, String> {
    let base = rel_prefix
        .replace('\\', "/")
        .trim_matches('/')
        .to_lowercase();
    if base.is_empty() {
        return Ok(Vec::new()); // a bare/root prefix would match the whole roost
    }
    let with_slash = format!("{base}/");
    let root_norm = roost_root.replace('\\', "/");
    let root_norm = root_norm.trim_end_matches('/');

    let result = db
        .run_script(
            r#"?[path] := *notes{ path }"#,
            Default::default(),
            ScriptMutability::Immutable,
        )
        .map_err(|e| format!("note_paths_under_dir query failed: {e}"))?;

    let mut out = Vec::new();
    for row in result.rows.iter() {
        let Some(abs) = row.first().and_then(|v| v.get_str()) else {
            continue;
        };
        // Normalized id (lowercase, no extension) for segment-aware matching.
        let id = note_id_from_abs_path(abs, roost_root);
        if id == base || id.starts_with(&with_slash) {
            let abs_norm = abs.replace('\\', "/");
            let rel = abs_norm
                .strip_prefix(root_norm)
                .unwrap_or(&abs_norm)
                .trim_start_matches('/')
                .to_string();
            out.push(rel);
        }
    }
    Ok(out)
}

/// Pass 1 bulk variant — used by `init_graph` to replace the whole note +
/// tag set in 3 transactional scripts instead of ~(3 + tag_count) per note.
/// Orders of magnitude fewer SQLite commits, which is the real driver of
/// the "database is locked" errors when the init loop races with the
/// auto-save command on every keystroke.
///
/// Semantics: wipes `notes` (actually: upserts every incoming note) and
/// completely replaces the `tags` relation. Safe because init_graph is
/// a full re-scan of the roost — any tags/notes we skip are stale.
pub fn bulk_index_pass1(
    db: &DbInstance,
    roost_root: &str,
    files: &[(String, String)],
) -> Result<(), String> {
    if files.is_empty() {
        return Ok(());
    }
    // Fallback timestamp for files we can't stat (SAF); note rows prefer the
    // real file mtime (IP-A2) so a cold rebuild doesn't collapse every note's
    // `updated_at` to one shared instant and scramble the recent-notes order.
    let now = now_secs();

    // Batch note rows.
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let mut note_rows: Vec<DataValue> = Vec::with_capacity(files.len());
    let mut tag_rows: Vec<DataValue> = Vec::new();
    let mut task_rows: Vec<DataValue> = Vec::new();
    for (abs_path, content) in files {
        let id = note_id_from_abs_path(abs_path, roost_root);
        let title = title_from_path(abs_path);
        let updated_at = file_mtime_secs(abs_path).unwrap_or(now);
        note_rows.push(DataValue::List(vec![
            DataValue::Str(id.clone().into()),
            DataValue::Str(title.into()),
            DataValue::Str(abs_path.clone().into()),
            DataValue::from(updated_at),
        ]));
        for tag in parse_tags(content.clone()) {
            tag_rows.push(DataValue::List(vec![
                DataValue::Str(id.clone().into()),
                DataValue::Str(tag.tag.into()),
            ]));
        }
        let note_date = note_date_from_id(&id);
        for task in parse_tasks_in_note(content, note_date.as_deref(), Some(&today)) {
            task_rows.push(DataValue::List(vec![
                DataValue::Str(id.clone().into()),
                DataValue::Num(cozo::Num::Int(task.source_line as i64)),
                DataValue::Str(task.text.into()),
                DataValue::Str(task.content_anchor.into()),
                DataValue::Str(task.due_at.into()),
                DataValue::Bool(task.completed),
                DataValue::Str(task.completed_at.into()),
                DataValue::Str(task.linked_person.into()),
                DataValue::Str(task.linked_project.into()),
                DataValue::from(now),
            ]));
        }
    }

    // 1. Bulk-put all notes in a single script.
    let mut params = BTreeMap::new();
    params.insert("rows".to_string(), DataValue::List(note_rows));
    db.run_script(
        r#"?[id, title, path, updated_at] <- $rows
           :put notes { id => title, path, updated_at }"#,
        params,
        ScriptMutability::Mutable,
    )
    .map_err(|e| format!("Bulk-insert notes failed: {e}"))?;

    // 2. Clear the entire tags relation — init_graph always rebuilds it
    //    from scratch, so per-note :rm would just be slow redundant work.
    db.run_script(
        r#"?[note_id, tag] := *tags{ note_id, tag }
           :rm tags { note_id, tag }"#,
        BTreeMap::new(),
        ScriptMutability::Mutable,
    )
    .map_err(|e| format!("Bulk-clear tags failed: {e}"))?;

    // 3. Bulk-put all tags.
    if !tag_rows.is_empty() {
        let mut params = BTreeMap::new();
        params.insert("rows".to_string(), DataValue::List(tag_rows));
        db.run_script(
            r#"?[note_id, tag] <- $rows
               :put tags { note_id, tag }"#,
            params,
            ScriptMutability::Mutable,
        )
        .map_err(|e| format!("Bulk-insert tags failed: {e}"))?;
    }

    // 4. Wholesale rebuild `:task` for the same reason as tags — init_graph
    //    is a full roost scan and per-note :rm would just be slower.
    db.run_script(
        r#"?[note_id, source_line] := *task{ note_id, source_line }
           :rm task { note_id, source_line }"#,
        BTreeMap::new(),
        ScriptMutability::Mutable,
    )
    .map_err(|e| format!("Bulk-clear tasks failed: {e}"))?;

    if !task_rows.is_empty() {
        let mut params = BTreeMap::new();
        params.insert("rows".to_string(), DataValue::List(task_rows));
        db.run_script(
            r#"?[note_id, source_line, text, content_anchor, due_at, completed, completed_at, linked_person, linked_project, updated_at] <- $rows
               :put task { note_id, source_line => text, content_anchor, due_at, completed, completed_at, linked_person, linked_project, updated_at }"#,
            params,
            ScriptMutability::Mutable,
        )
        .map_err(|e| format!("Bulk-insert tasks failed: {e}"))?;
    }

    Ok(())
}

/// Pass 2 bulk variant — resolves every wiki-link against the supplied
/// resolver in memory, then replaces the whole link + dangling relations in ONE
/// transaction (clear both, put both).
///
/// One transaction matters for the IP-5 completion gate: `init_graph` treats
/// `links > 0` as "pass 2 landed". With the four statements separate, a crash
/// after the links were put but before the dangling rows were put left
/// `links > 0` and NO `:dangling` rows — served warm forever, so a note created
/// later never promoted its incoming backlinks.
pub fn bulk_index_pass2(
    db: &DbInstance,
    roost_root: &str,
    files: &[(String, String)],
    resolver: &LinkResolver,
) -> Result<(), String> {
    if files.is_empty() {
        return Ok(());
    }

    let mut link_rows: Vec<DataValue> = Vec::new();
    let mut dangling_rows: Vec<DataValue> = Vec::new();
    for (abs_path, content) in files {
        let id = note_id_from_abs_path(abs_path, roost_root);
        let rel_dir = note_rel_dir(abs_path, roost_root);
        for link in parse_links(content.clone()) {
            let link_text = link.display.as_deref().unwrap_or(&link.target).to_string();
            match resolver.resolve(&link.target, &rel_dir) {
                Some(to_id) => link_rows.push(DataValue::List(vec![
                    DataValue::Str(id.clone().into()),
                    DataValue::Str(to_id.into()),
                    DataValue::Str(link_text.into()),
                ])),
                // Record unresolved links so a note created after a full
                // rebuild still promotes its incoming backlinks (otherwise the
                // lag would silently return until each referencing note was
                // re-saved).
                None => {
                    if let Some((target, base)) = dangling_key(&link.target) {
                        dangling_rows.push(DataValue::List(vec![
                            DataValue::Str(id.clone().into()),
                            DataValue::Str(target.into()),
                            DataValue::Str(link_text.into()),
                            DataValue::Str(base.into()),
                        ]));
                    }
                }
            }
        }
    }

    // Clear links + dangling relations wholesale — init_graph rebuilds both.
    let mut tx = TxScript::new();
    tx.block(
        r#"?[from_id, to_id] := *links{ from_id, to_id }
:rm links { from_id, to_id }"#,
    );
    tx.block(
        r#"?[from_id, target] := *dangling{ from_id, target }
:rm dangling { from_id, target }"#,
    );
    if !link_rows.is_empty() {
        tx.param("link_rows", DataValue::List(link_rows));
        tx.block(B_PUT_LINKS);
    }
    if !dangling_rows.is_empty() {
        tx.param("dangling_rows", DataValue::List(dangling_rows));
        tx.block(B_PUT_DANGLING);
    }
    tx.run(db)
        .map_err(|e| format!("Bulk-replace links failed: {e}"))
}

#[cfg(test)]
mod lookup_tests {
    use super::*;
    use cozo::DbInstance;

    fn db_with_notes() -> DbInstance {
        let db = DbInstance::new("mem", "", Default::default()).expect("mem db");
        db.run_script(
            r#":create notes { id: String => title: String, path: String, updated_at: Float }"#,
            Default::default(),
            ScriptMutability::Mutable,
        )
        .expect("create notes");
        for (id, title, path) in [
            ("a", "Alpha", "/v/a.md"),
            ("b", "Beta", "/v/b.md"),
            ("c", "Gamma", "/v/c.md"),
        ] {
            let mut p = BTreeMap::new();
            p.insert("id".into(), DataValue::Str(id.into()));
            p.insert("t".into(), DataValue::Str(title.into()));
            p.insert("pa".into(), DataValue::Str(path.into()));
            p.insert("u".into(), DataValue::from(0.0_f64));
            db.run_script(
                r#"?[id, title, path, updated_at] <- [[$id, $t, $pa, $u]]
                   :put notes { id => title, path, updated_at }"#,
                p,
                ScriptMutability::Mutable,
            )
            .expect("put note");
        }
        db
    }

    #[test]
    fn lookup_notes_by_ids_preserves_input_order() {
        let db = db_with_notes();
        let ids = vec!["c".to_string(), "a".to_string(), "b".to_string()];
        let out = lookup_notes_by_ids(&db, &ids).expect("lookup");
        let titles: Vec<_> = out.iter().map(|n| n.title.clone()).collect();
        assert_eq!(titles, vec!["Gamma", "Alpha", "Beta"]);
    }

    #[test]
    fn lookup_notes_by_ids_drops_missing_silently() {
        let db = db_with_notes();
        let ids = vec!["a".to_string(), "ghost".to_string(), "b".to_string()];
        let out = lookup_notes_by_ids(&db, &ids).expect("lookup");
        assert_eq!(out.len(), 2, "missing id must be skipped, not error");
        assert_eq!(out[0].id, "a");
        assert_eq!(out[1].id, "b");
    }

    #[test]
    fn lookup_notes_by_ids_empty_input_returns_empty() {
        let db = db_with_notes();
        let out = lookup_notes_by_ids(&db, &[]).expect("lookup");
        assert!(out.is_empty());
    }

    fn db_with_folder_notes() -> DbInstance {
        let db = DbInstance::new("mem", "", Default::default()).expect("mem db");
        db.run_script(
            r#":create notes { id: String => title: String, path: String, updated_at: Float }"#,
            Default::default(),
            ScriptMutability::Mutable,
        )
        .expect("create notes");
        // Stored `path` is the absolute display path (original case); the
        // stored `id` is irrelevant to `note_paths_under_dir` (it recomputes
        // from `path`), so we just give each a unique placeholder.
        for (i, path) in [
            "/v/Projects/a.md",
            "/v/Projects/sub/b.md",
            "/v/Projects-archive/c.md", // sibling folder — must NOT match
            "/v/Other/d.md",
        ]
        .iter()
        .enumerate()
        {
            let mut p = BTreeMap::new();
            p.insert("id".into(), DataValue::Str(format!("n{i}").into()));
            p.insert("t".into(), DataValue::Str("T".into()));
            p.insert("pa".into(), DataValue::Str((*path).into()));
            p.insert("u".into(), DataValue::from(0.0_f64));
            db.run_script(
                r#"?[id, title, path, updated_at] <- [[$id, $t, $pa, $u]]
                   :put notes { id => title, path, updated_at }"#,
                p,
                ScriptMutability::Mutable,
            )
            .expect("put note");
        }
        db
    }

    #[test]
    fn note_paths_under_dir_is_segment_aware_and_case_insensitive() {
        let db = db_with_folder_notes();
        // Prefix case differs from stored ("Projects" vs the lowercased id) —
        // matching must still find the nested notes, and must NOT catch the
        // `Projects-archive` sibling.
        let mut got = note_paths_under_dir(&db, "/v", "projects").expect("query");
        got.sort();
        assert_eq!(
            got,
            vec!["Projects/a.md".to_string(), "Projects/sub/b.md".to_string()],
            "must return original-case relative paths of nested notes only"
        );
    }

    #[test]
    fn note_paths_under_dir_empty_prefix_matches_nothing() {
        let db = db_with_folder_notes();
        // A bare/root prefix must not nuke the whole roost.
        assert!(note_paths_under_dir(&db, "/v", "")
            .expect("query")
            .is_empty());
        assert!(note_paths_under_dir(&db, "/v", "/")
            .expect("query")
            .is_empty());
    }
}

#[cfg(test)]
mod purge_tests {
    use super::*;
    use cozo::DbInstance;
    use tempfile::TempDir;

    fn db_with_full_schema() -> DbInstance {
        let db = DbInstance::new("mem", "", Default::default()).expect("mem db");
        for script in [
            r#":create notes { id: String => title: String, path: String, updated_at: Float }"#,
            r#":create links { from_id: String, to_id: String => link_text: String }"#,
            r#":create dangling { from_id: String, target: String => link_text: String, target_base: String }"#,
            r#":create tags { note_id: String, tag: String }"#,
            // AUDIT-069: the previous schema fixture didn't include
            // `task`, so the silent-swallow behaviour of the old
            // `purge_note` hid those failures. The error-collecting
            // version surfaces them, so it MUST be here for the per-note
            // round-trip tests to pass. (The embeddings/concepts/episodic
            // relations were removed in the Stage-2 teardown — purge_note
            // no longer touches them, so they're gone from this fixture.)
            r#":create task {
                note_id: String, source_line: Int
                => text: String, content_anchor: String, due_at: String,
                   completed: Bool, completed_at: String, linked_person: String,
                   linked_project: String, updated_at: Float
            }"#,
            // `purge_note` invalidates graph-algorithm results after
            // removing a note. Keep this test schema aligned with the
            // production schema so purge tests exercise that cleanup too.
            r#":create graph_algo_cache {
                algo: String, note_id: String
                => value: Float, computed_at: Float
            }"#,
        ] {
            db.run_script(script, Default::default(), ScriptMutability::Mutable)
                .unwrap_or_else(|e| panic!("create failed for {script}: {e}"));
        }
        db
    }

    fn put(db: &DbInstance, script: &str, params: BTreeMap<String, DataValue>) {
        db.run_script(script, params, ScriptMutability::Mutable)
            .expect("put");
    }

    fn count(db: &DbInstance, query: &str) -> usize {
        let r = db
            .run_script(query, Default::default(), ScriptMutability::Immutable)
            .expect("count");
        r.rows
            .first()
            .and_then(|row| row.first())
            .and_then(|v| match v {
                cozo::DataValue::Num(cozo::Num::Int(i)) => Some(*i as usize),
                _ => None,
            })
            .unwrap_or(0)
    }

    /// Read a note's stored `updated_at`, or `None` if the note row is absent.
    fn stored_updated_at(db: &DbInstance, id: &str) -> Option<f64> {
        let mut p = BTreeMap::new();
        p.insert("id".to_string(), DataValue::Str(id.into()));
        let r = db
            .run_script(
                r#"?[updated_at] := *notes{ id, updated_at }, id == $id"#,
                p,
                ScriptMutability::Immutable,
            )
            .expect("query updated_at");
        r.rows
            .first()
            .and_then(|row| row.first())
            .and_then(|v| match v {
                DataValue::Num(cozo::Num::Float(f)) => Some(*f),
                DataValue::Num(cozo::Num::Int(i)) => Some(*i as f64),
                _ => None,
            })
    }

    /// Pin a file's mtime to a KNOWN instant (seconds past the Unix epoch) so
    /// mtime-stamping tests can assert against a fixed value that's plainly
    /// distinct from wall-clock — no real-time sleeps, no dev-dep. Uses only
    /// `std::fs::File::set_modified` (stable since 1.75; CI runs newer).
    fn set_mtime(path: &std::path::Path, secs: u64) {
        let f = std::fs::File::options()
            .write(true)
            .open(path)
            .expect("open for set_modified");
        f.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs))
            .expect("set_modified");
    }

    #[test]
    fn dangling_link_is_promoted_when_target_is_created() {
        let db = db_with_full_schema();
        let root = "/roost";
        let body_b = "see [[NewNote]]\n";

        // B references [[NewNote]] before it exists → link is recorded as
        // dangling, NOT silently dropped, and there's no edge yet.
        index_note_pass1(&db, root, "/roost/B.md", body_b).unwrap();
        let resolver = LinkResolver::build(&db).unwrap(); // only B exists
        index_note_pass2(&db, root, "/roost/B.md", body_b, &resolver).unwrap();
        assert_eq!(
            count(
                &db,
                r#"?[count(from_id)] := *links{from_id, to_id}, to_id == "newnote""#
            ),
            0,
            "no backlink edge before the target exists"
        );
        assert_eq!(
            count(
                &db,
                r#"?[count(from_id)] := *dangling{from_id, target_base}, target_base == "newnote""#
            ),
            1,
            "the unresolved link is recorded as dangling"
        );

        // NewNote is created → its dangling backlinks get promoted.
        index_note_pass1(&db, root, "/roost/NewNote.md", "# NewNote\n").unwrap();
        let resolver = LinkResolver::build(&db).unwrap(); // now B + newnote
        let promoted = promote_dangling_links(&db, &resolver, "newnote").unwrap();
        assert_eq!(promoted, 1, "one dangling link promoted");

        // The real edge now exists and the dangling row is gone.
        assert_eq!(
            count(
                &db,
                r#"?[count(from_id)] := *links{from_id, to_id}, to_id == "newnote""#
            ),
            1,
            "backlink edge created on target creation"
        );
        assert_eq!(
            count(
                &db,
                r#"?[count(from_id)] := *dangling{from_id, target_base}, target_base == "newnote""#
            ),
            0,
            "dangling row cleared after promotion"
        );
    }

    #[test]
    fn index_note_full_with_resolver_reuses_passed_resolver_not_a_fresh_scan() {
        // IP-A3 / TRN-7: the cached variant must resolve links against the
        // caller-supplied resolver, NOT secretly rebuild from `*notes`. We
        // prove that by feeding it a resolver that is deliberately STALE (it
        // predates a note that already exists in the DB): a hidden rebuild
        // would see the note and make an edge; a true reuse cannot.
        let db = db_with_full_schema();
        let root = "/roost";

        // Resolver built over an empty roost → knows nothing.
        let mut resolver = LinkResolver::build(&db).unwrap();

        // Ghost is added to the DB but NOT to the resolver.
        index_note_pass1(&db, root, "/roost/Ghost.md", "# Ghost\n").unwrap();

        // Index Src (links to [[Ghost]]) through the stale resolver.
        index_note_full_with_resolver(&db, root, "/roost/Src.md", "see [[Ghost]]\n", &mut resolver)
            .unwrap();

        assert_eq!(
            count(
                &db,
                r#"?[count(from_id)] := *links{from_id, to_id}, from_id == "src", to_id == "ghost""#
            ),
            0,
            "reused resolver never learned Ghost → no edge (a hidden rebuild would be 1)"
        );
        assert_eq!(
            count(
                &db,
                r#"?[count(from_id)] := *dangling{from_id, target_base}, from_id == "src", target_base == "ghost""#
            ),
            1,
            "the unresolved link is recorded as dangling"
        );

        // add_id (what the save paths do to keep the cache in sync) + re-index
        // now creates the edge — still with no DB rebuild.
        resolver.add_id("ghost");
        index_note_full_with_resolver(&db, root, "/roost/Src.md", "see [[Ghost]]\n", &mut resolver)
            .unwrap();
        assert_eq!(
            count(
                &db,
                r#"?[count(from_id)] := *links{from_id, to_id}, from_id == "src", to_id == "ghost""#
            ),
            1,
            "after add_id the reused resolver resolves the link — incremental, no rebuild"
        );
        assert_eq!(
            count(
                &db,
                r#"?[count(from_id)] := *dangling{from_id, target_base}, from_id == "src", target_base == "ghost""#
            ),
            0,
            "the promoted link's dangling row is cleared on re-index"
        );
    }

    #[test]
    fn index_note_full_fresh_build_sees_all_current_notes() {
        // Contract of the fresh-build entry point (the path an invalidation
        // falls back to): it rebuilds the resolver from the DB, so it resolves
        // links to notes it was never explicitly told about.
        let db = db_with_full_schema();
        let root = "/roost";
        index_note_pass1(&db, root, "/roost/Ghost.md", "# Ghost\n").unwrap();
        index_note_full(&db, root, "/roost/Src.md", "see [[Ghost]]\n").unwrap();
        assert_eq!(
            count(
                &db,
                r#"?[count(from_id)] := *links{from_id, to_id}, from_id == "src", to_id == "ghost""#
            ),
            1,
            "fresh build sees Ghost and creates the edge"
        );
    }

    #[test]
    fn purge_danglifies_incoming_links_so_backlinks_survive_recreate() {
        // IP-3/LR-3 regression: deleting (or renaming) a note used to DROP
        // every incoming backlink edge without recording it as dangling, so
        // recreating the note at the same name never restored those
        // backlinks. purge_note now converts each incoming link into a
        // `:dangling` row keyed by the note's basename; promote_dangling_links
        // re-links them the moment a matching note reappears.
        let db = db_with_full_schema();
        let root = "/roost";

        // Target exists; A links to it → a real backlink edge A→target.
        index_note_pass1(&db, root, "/roost/Target.md", "# Target\n").unwrap();
        let body_a = "see [[Target]]\n";
        index_note_pass1(&db, root, "/roost/A.md", body_a).unwrap();
        let resolver = LinkResolver::build(&db).unwrap();
        index_note_pass2(&db, root, "/roost/A.md", body_a, &resolver).unwrap();
        assert_eq!(
            count(
                &db,
                r#"?[count(from_id)] := *links{from_id, to_id}, to_id == "target""#
            ),
            1,
            "precondition: A→Target backlink edge exists"
        );

        // Delete Target.
        purge_note(&db, "target").expect("purge");

        // The edge is gone (Target no longer exists)…
        assert_eq!(
            count(
                &db,
                r#"?[count(from_id)] := *links{from_id, to_id}, to_id == "target""#
            ),
            0,
            "incoming edge removed with the deleted note"
        );
        // …but A's link is now recorded as DANGLING (this is the fix — pre-IP-3
        // it was silently lost), keyed so a recreate can find it.
        assert_eq!(
            count(
                &db,
                r#"?[count(from_id)] := *dangling{from_id, target_base}, from_id == "a", target_base == "target""#
            ),
            1,
            "incoming link converted to a dangling row on purge"
        );

        // Recreate Target → its dangling backlinks promote back to real edges.
        index_note_pass1(&db, root, "/roost/Target.md", "# Target\n").unwrap();
        let resolver = LinkResolver::build(&db).unwrap();
        let promoted = promote_dangling_links(&db, &resolver, "target").unwrap();
        assert_eq!(promoted, 1, "the danglified backlink is re-promoted");
        assert_eq!(
            count(
                &db,
                r#"?[count(from_id)] := *links{from_id, to_id}, to_id == "target""#
            ),
            1,
            "backlink restored after recreate — no permanent loss"
        );
    }

    #[test]
    fn purge_removes_every_relation_for_a_note() {
        let db = db_with_full_schema();
        // Seed: two notes (target = "doomed", neighbor = "ally") with
        // links + tags rows for "doomed".
        let mut p = BTreeMap::new();
        p.insert("id".into(), DataValue::Str("doomed".into()));
        p.insert("title".into(), DataValue::Str("doomed".into()));
        p.insert("path".into(), DataValue::Str("/v/doomed.md".into()));
        p.insert("u".into(), DataValue::from(0.0_f64));
        put(
            &db,
            r#"?[id, title, path, updated_at] <- [[$id, $title, $path, $u]]
               :put notes { id => title, path, updated_at }"#,
            p,
        );
        let mut p = BTreeMap::new();
        p.insert("id".into(), DataValue::Str("ally".into()));
        p.insert("title".into(), DataValue::Str("ally".into()));
        p.insert("path".into(), DataValue::Str("/v/ally.md".into()));
        p.insert("u".into(), DataValue::from(0.0_f64));
        put(
            &db,
            r#"?[id, title, path, updated_at] <- [[$id, $title, $path, $u]]
               :put notes { id => title, path, updated_at }"#,
            p,
        );

        // Outgoing + incoming links touching doomed.
        for (from, to) in [("doomed", "ally"), ("ally", "doomed")] {
            let mut p = BTreeMap::new();
            p.insert("f".into(), DataValue::Str(from.into()));
            p.insert("t".into(), DataValue::Str(to.into()));
            p.insert("x".into(), DataValue::Str("link".into()));
            put(
                &db,
                r#"?[from_id, to_id, link_text] <- [[$f, $t, $x]]
                   :put links { from_id, to_id => link_text }"#,
                p,
            );
        }

        // Tag row.
        let mut p = BTreeMap::new();
        p.insert("n".into(), DataValue::Str("doomed".into()));
        p.insert("t".into(), DataValue::Str("ml".into()));
        put(
            &db,
            r#"?[note_id, tag] <- [[$n, $t]]
               :put tags { note_id, tag }"#,
            p,
        );

        // Sanity: every relation has the doomed row (or refs to it).
        assert_eq!(
            count(&db, r#"?[count(id)] := *notes{ id }, id == "doomed""#),
            1
        );
        assert_eq!(
            count(
                &db,
                r#"?[count(from_id)] := *links{ from_id, to_id }, from_id == "doomed" || to_id == "doomed""#
            ),
            2
        );
        assert_eq!(
            count(
                &db,
                r#"?[count(note_id)] := *tags{ note_id, tag }, note_id == "doomed""#
            ),
            1
        );

        // Purge it.
        purge_note(&db, "doomed").expect("purge");

        // Every doomed-related row gone. The unrelated `ally` note survives.
        assert_eq!(
            count(&db, r#"?[count(id)] := *notes{ id }, id == "doomed""#),
            0
        );
        assert_eq!(
            count(
                &db,
                r#"?[count(from_id)] := *links{ from_id, to_id }, from_id == "doomed" || to_id == "doomed""#
            ),
            0
        );
        assert_eq!(
            count(
                &db,
                r#"?[count(note_id)] := *tags{ note_id, tag }, note_id == "doomed""#
            ),
            0
        );

        // Ally is untouched.
        assert_eq!(
            count(&db, r#"?[count(id)] := *notes{ id }, id == "ally""#),
            1
        );
    }

    #[test]
    fn purge_unknown_id_is_noop() {
        let db = db_with_full_schema();
        // Should not error even though "nobody" was never inserted.
        purge_note(&db, "nobody").expect("purge nonexistent");
    }

    #[test]
    fn purge_preserves_edit_history() {
        // Move/rename both purge the OLD note id from the graph, after
        // migrating its edit-history rows to the NEW id. That migrate is
        // only safe because purge_note deliberately does NOT touch
        // `:edit_event`. Lock that invariant — if a future change makes
        // purge_note also drop history rows, every move/rename would
        // silently destroy the note's audit trail.
        let db = db_with_full_schema();
        db.run_script(
            r#":create edit_event {
                note_id: String, event_id: String =>
                actor_type: String, actor_id: String, intent: String,
                timestamp: Float, diff: String, before_content: String,
                lines_added: Int, lines_removed: Int, reverted_by: String,
                note_path: String
            }"#,
            Default::default(),
            ScriptMutability::Mutable,
        )
        .expect("create edit_event");

        // Seed a note node + one history row for it.
        let mut p = BTreeMap::new();
        p.insert("id".into(), DataValue::Str("doomed".into()));
        p.insert("title".into(), DataValue::Str("doomed".into()));
        p.insert("path".into(), DataValue::Str("/v/doomed.md".into()));
        p.insert("u".into(), DataValue::from(0.0_f64));
        put(
            &db,
            r#"?[id, title, path, updated_at] <- [[$id, $title, $path, $u]]
               :put notes { id => title, path, updated_at }"#,
            p,
        );
        let mut p = BTreeMap::new();
        p.insert("n".into(), DataValue::Str("doomed".into()));
        p.insert("e".into(), DataValue::Str("evt-1".into()));
        put(
            &db,
            r#"?[note_id, event_id, actor_type, actor_id, intent, timestamp,
                 diff, before_content, lines_added, lines_removed, reverted_by, note_path]
               <- [[$n, $e, "user", "user", "create", 0.0, "", "", 0, 0, "", "/v/doomed.md"]]
               :put edit_event { note_id, event_id => actor_type, actor_id, intent,
                 timestamp, diff, before_content, lines_added, lines_removed, reverted_by, note_path }"#,
            p,
        );

        purge_note(&db, "doomed").expect("purge");

        // Graph node gone…
        assert_eq!(
            count(&db, r#"?[count(id)] := *notes{ id }, id == "doomed""#),
            0
        );
        // …but the edit-history row survives the purge.
        assert_eq!(
            count(
                &db,
                r#"?[count(event_id)] := *edit_event{ note_id, event_id }, note_id == "doomed""#
            ),
            1
        );
    }

    /// AUDIT-069: a failure in one relation must NOT short-circuit the
    /// rest of the purge, AND every relation's error must be present in
    /// the returned `Vec<String>`. The old behaviour used
    /// `let _ = run_script(...)` and a `?` on `clear_note_chunks`,
    /// which made a chunks failure silently skip the embed_queue purge
    /// AND lose the chunks error entirely.
    #[test]
    fn purge_collects_per_relation_errors_without_short_circuiting() {
        // Bare DB — no relations exist at all. Every script in
        // `purge_note` should fail against an "unknown relation" error,
        // and all of them should appear in the returned Vec.
        let db = DbInstance::new("mem", "", Default::default()).expect("mem db");

        let errs = purge_note(&db, "doomed").expect_err("expected Err on bare DB");

        // Five relations are touched (notes, links×2, tags, task); every
        // script should fail against the bare DB and ALL errors should be
        // collected rather than short-circuiting at the first failure.
        assert!(
            errs.len() >= 4,
            "expected most relations to error, got {} errors: {errs:?}",
            errs.len()
        );

        // Spot-check that BOTH the first relation (notes) and the
        // LAST relation (task) appear — proves we ran the full sequence
        // instead of short-circuiting at the first failure.
        assert!(
            errs.iter().any(|e| e.contains("notes")),
            "missing notes error in {errs:?}"
        );
        assert!(
            errs.iter().any(|e| e.contains("task")),
            "missing task error in {errs:?}"
        );
    }

    /// Regression for ghost nodes: a note whose file was deleted off-app
    /// (so `delete_file`→`purge_note` never ran) must be removed by the
    /// warm-open reconcile, along with its links and tags. Notes still on
    /// disk are untouched.
    #[test]
    fn reconcile_purges_only_notes_missing_from_disk() {
        let db = db_with_full_schema();
        for (id, path) in [("welcome", "/v/welcome.md"), ("keep", "/v/keep.md")] {
            let mut p = BTreeMap::new();
            p.insert("id".into(), DataValue::Str(id.into()));
            p.insert("title".into(), DataValue::Str(id.into()));
            p.insert("path".into(), DataValue::Str(path.into()));
            p.insert("u".into(), DataValue::from(0.0_f64));
            put(
                &db,
                r#"?[id, title, path, updated_at] <- [[$id, $title, $path, $u]]
                   :put notes { id => title, path, updated_at }"#,
                p,
            );
        }
        // "welcome" has a self-link + a tag — both must be swept too.
        let mut lp = BTreeMap::new();
        lp.insert("f".into(), DataValue::Str("welcome".into()));
        lp.insert("t".into(), DataValue::Str("welcome".into()));
        lp.insert("x".into(), DataValue::Str("Welcome".into()));
        put(
            &db,
            r#"?[from_id, to_id, link_text] <- [[$f, $t, $x]]
               :put links { from_id, to_id => link_text }"#,
            lp,
        );
        let mut tp = BTreeMap::new();
        tp.insert("n".into(), DataValue::Str("welcome".into()));
        tp.insert("g".into(), DataValue::Str("intro".into()));
        put(
            &db,
            r#"?[note_id, tag] <- [[$n, $g]]
               :put tags { note_id, tag }"#,
            tp,
        );

        // Disk now has only keep.md — welcome.md was deleted off-app.
        let purged =
            reconcile_deleted_notes(&db, "/v", &["/v/keep.md".to_string()]).expect("reconcile");

        assert_eq!(purged, 1, "exactly the deleted note is purged");
        assert_eq!(count(&db, r#"?[count(id)] := *notes{ id }"#), 1);
        assert_eq!(
            count(&db, r#"?[count(from_id)] := *links{ from_id }"#),
            0,
            "the deleted note's self-link must be swept"
        );
        assert_eq!(
            count(&db, r#"?[count(note_id)] := *tags{ note_id }"#),
            0,
            "the deleted note's tag must be swept"
        );
        // "keep" survives.
        assert_eq!(
            count(&db, r#"?[count(id)] := *notes{ id }, id == "keep""#),
            1
        );
    }

    #[test]
    fn reconcile_noop_when_disk_matches_db() {
        let db = db_with_full_schema();
        for (id, path) in [("a", "/v/a.md"), ("b", "/v/b.md")] {
            let mut p = BTreeMap::new();
            p.insert("id".into(), DataValue::Str(id.into()));
            p.insert("title".into(), DataValue::Str(id.into()));
            p.insert("path".into(), DataValue::Str(path.into()));
            p.insert("u".into(), DataValue::from(0.0_f64));
            put(
                &db,
                r#"?[id, title, path, updated_at] <- [[$id, $title, $path, $u]]
                   :put notes { id => title, path, updated_at }"#,
                p,
            );
        }
        let disk = vec!["/v/a.md".to_string(), "/v/b.md".to_string()];
        let purged = reconcile_deleted_notes(&db, "/v", &disk).expect("reconcile");
        assert_eq!(purged, 0);
        assert_eq!(count(&db, r#"?[count(id)] := *notes{ id }"#), 2);
    }

    // ── mtime stamping (IP-A2) + index-health counters ─────────────────
    //
    // These lock the fixes that make `updated_at` the note's real FILE mtime
    // rather than wall-clock (so cold rebuilds keep recent-notes ordering and
    // the warm heal can detect off-app edits), plus the two diagnostic
    // read-outs (`note_updated_ats`, `dangling_count`).

    #[test]
    fn file_mtime_secs_reads_real_file_and_none_for_missing() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("real.md");
        std::fs::write(&path, "x").unwrap();
        let got = file_mtime_secs(&path.to_string_lossy());
        assert!(
            matches!(got, Some(m) if m > 0.0),
            "a real file must report a positive mtime, got {got:?}"
        );
        // A path that was never created must be None — not a panic, not 0.0.
        let missing = dir.path().join("ghost.md");
        assert_eq!(file_mtime_secs(&missing.to_string_lossy()), None);
    }

    #[test]
    fn index_note_pass1_stamps_updated_at_with_file_mtime() {
        // IP-A2: pass1 stamps `updated_at` from the FILE mtime, not now(). We
        // pin the file's mtime to a fixed 1970-era instant so a regression back
        // to wall-clock would miss by ~decades and fail the tolerance below.
        let db = db_with_full_schema();
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("note.md");
        std::fs::write(&path, "# Note\n#tag\n").unwrap();
        set_mtime(&path, 1_000_000); // ~1970-01-12, unmistakably not "now"
        let abs = path.to_string_lossy().to_string();
        let root = dir.path().to_string_lossy().to_string();

        index_note_pass1(&db, &root, &abs, "# Note\n#tag\n").unwrap();

        let id = note_id_from_abs_path(&abs, &root);
        let stored = stored_updated_at(&db, &id).expect("note row must have updated_at");
        let mtime = file_mtime_secs(&abs).expect("real file has an mtime");
        assert!(
            (stored - mtime).abs() < 1.0,
            "stored updated_at ({stored}) must equal the file mtime ({mtime})"
        );
        assert!(
            (stored - 1_000_000.0).abs() < 1.0,
            "stored updated_at ({stored}) must be the file mtime, not wall-clock"
        );
    }

    #[test]
    fn bulk_index_pass1_stamps_per_file_mtime_and_orders_by_it() {
        // IP-A2 cold path: each note must be stamped with its OWN file mtime,
        // not one shared now(). Two files pinned to far-apart mtimes let us
        // assert the per-file equality invariant (the real teeth) and the
        // resulting -updated_at order deterministically, without sleeps.
        let db = db_with_full_schema();
        let dir = TempDir::new().unwrap();
        let root = dir.path().to_string_lossy().to_string();

        let older = dir.path().join("older.md");
        let newer = dir.path().join("newer.md");
        std::fs::write(&older, "# Older\n").unwrap();
        std::fs::write(&newer, "# Newer\n").unwrap();
        set_mtime(&older, 1_000_000);
        set_mtime(&newer, 2_000_000);
        let older_abs = older.to_string_lossy().to_string();
        let newer_abs = newer.to_string_lossy().to_string();

        let files = vec![
            (older_abs.clone(), "# Older\n".to_string()),
            (newer_abs.clone(), "# Newer\n".to_string()),
        ];
        bulk_index_pass1(&db, &root, &files).unwrap();

        // Primary invariant: each stored value equals THAT file's own mtime
        // (a single shared now() would break at least one of these).
        for abs in [&older_abs, &newer_abs] {
            let id = note_id_from_abs_path(abs, &root);
            let stored = stored_updated_at(&db, &id).expect("note row");
            let mtime = file_mtime_secs(abs).expect("file mtime");
            assert!(
                (stored - mtime).abs() < 1.0,
                "note {id}: stored {stored} must match its own file mtime {mtime}"
            );
        }

        // Consequence: list_all_notes (-updated_at) puts the newer file first.
        let listed = list_all_notes(&db, 10).unwrap();
        let ids: Vec<_> = listed.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(ids, vec!["newer", "older"], "most-recent mtime first");
    }

    #[test]
    fn note_updated_ats_maps_ids_to_their_mtimes() {
        // Feeds the warm-heal modified-detection (IP-A1): it must return
        // id → stored updated_at for every note. Pin two files to known mtimes
        // and assert the map reflects each.
        let db = db_with_full_schema();
        let dir = TempDir::new().unwrap();
        let root = dir.path().to_string_lossy().to_string();

        let a = dir.path().join("a.md");
        let b = dir.path().join("b.md");
        std::fs::write(&a, "# A\n").unwrap();
        std::fs::write(&b, "# B\n").unwrap();
        set_mtime(&a, 1_500_000);
        set_mtime(&b, 2_500_000);
        let a_abs = a.to_string_lossy().to_string();
        let b_abs = b.to_string_lossy().to_string();

        index_note_pass1(&db, &root, &a_abs, "# A\n").unwrap();
        index_note_pass1(&db, &root, &b_abs, "# B\n").unwrap();

        let map = note_updated_ats(&db).unwrap();
        assert_eq!(map.len(), 2, "one entry per indexed note");
        let a_stored = *map
            .get(&note_id_from_abs_path(&a_abs, &root))
            .expect("a in map");
        let b_stored = *map
            .get(&note_id_from_abs_path(&b_abs, &root))
            .expect("b in map");
        assert!(
            (a_stored - 1_500_000.0).abs() < 1.0,
            "a's stored mtime, got {a_stored}"
        );
        assert!(
            (b_stored - 2_500_000.0).abs() < 1.0,
            "b's stored mtime, got {b_stored}"
        );
    }

    #[test]
    fn dangling_count_totals_unresolved_link_rows() {
        // IP-A8 index-health signal: dangling_count is the row count of the
        // `:dangling` relation. Mirror the promotion test's setup — a note
        // whose targets don't exist records one dangling row per distinct
        // target. (No real file needed; pass1 falls back to now() for mtime.)
        let db = db_with_full_schema();
        assert_eq!(dangling_count(&db).unwrap(), 0, "empty relation → 0");

        let root = "/roost";
        let body = "see [[Ghost1]] and [[Ghost2]]\n";
        index_note_pass1(&db, root, "/roost/a.md", body).unwrap();
        let resolver = LinkResolver::build(&db).unwrap(); // only "a" exists
        index_note_pass2(&db, root, "/roost/a.md", body, &resolver).unwrap();

        assert_eq!(
            dangling_count(&db).unwrap(),
            2,
            "two distinct unresolved targets → two dangling rows"
        );
    }
}

#[cfg(test)]
mod task_index_tests {
    use super::*;
    use cozo::DbInstance;

    fn db_with_task_schema() -> DbInstance {
        let db = DbInstance::new("mem", "", Default::default()).expect("mem db");
        db.run_script(
            r#":create task {
                note_id: String,
                source_line: Int,
                =>
                text: String,
                content_anchor: String,
                due_at: String,
                completed: Bool,
                completed_at: String,
                linked_person: String,
                linked_project: String,
                updated_at: Float
            }"#,
            Default::default(),
            ScriptMutability::Mutable,
        )
        .expect("create task relation");
        db
    }

    fn task_count(db: &DbInstance, note_id: &str) -> usize {
        let mut p = BTreeMap::new();
        p.insert("id".into(), DataValue::Str(note_id.to_string().into()));
        let r = db
            .run_script(
                r#"?[count(source_line)] := *task{ note_id, source_line }, note_id == $id"#,
                p,
                ScriptMutability::Immutable,
            )
            .expect("count task rows");
        r.rows
            .first()
            .and_then(|row| row.first())
            .and_then(|v| match v {
                cozo::DataValue::Num(cozo::Num::Int(i)) => Some(*i as usize),
                _ => None,
            })
            .unwrap_or(0)
    }

    // A daily-note id so its bare checkboxes inherit the note's date and
    // become tasks under convention 0.3 (exercises `note_date_from_id`).
    const DAILY_ID: &str = "daily notes/2026-06-11";

    #[test]
    fn index_inserts_one_row_per_checkbox() {
        let db = db_with_task_schema();
        let content = "# Title\n\n- [ ] first\n- [x] second\n";
        index_note_tasks(&db, DAILY_ID, content).expect("index");
        assert_eq!(task_count(&db, DAILY_ID), 2);
    }

    #[test]
    fn reindex_replaces_old_rows() {
        let db = db_with_task_schema();
        // Initial: 3 tasks (dated note → bare checkboxes inherit the date).
        let content_v1 = "- [ ] a\n- [ ] b\n- [ ] c\n";
        index_note_tasks(&db, DAILY_ID, content_v1).expect("v1");
        assert_eq!(task_count(&db, DAILY_ID), 3);

        // After edit: only 1 task remains. Stale rows must be cleared so
        // the pane doesn't show ghost tasks the user already deleted.
        let content_v2 = "- [ ] only one\n";
        index_note_tasks(&db, DAILY_ID, content_v2).expect("v2");
        assert_eq!(task_count(&db, DAILY_ID), 1);
    }

    #[test]
    fn empty_content_clears_all_rows() {
        let db = db_with_task_schema();
        index_note_tasks(&db, DAILY_ID, "- [ ] a\n- [ ] b\n").expect("seed");
        assert_eq!(task_count(&db, DAILY_ID), 2);
        index_note_tasks(&db, DAILY_ID, "no checkboxes here").expect("clear");
        assert_eq!(task_count(&db, DAILY_ID), 0);
    }

    #[test]
    fn tasks_off_property_indexes_zero_rows() {
        let db = db_with_task_schema();
        let content = "---\ntasks: off\n---\n\n- [ ] not a real todo\n- [x] nor this\n";
        index_note_tasks(&db, "n1", content).expect("index");
        assert_eq!(
            task_count(&db, "n1"),
            0,
            "a `tasks: off` note must write no :task rows"
        );
    }

    #[test]
    fn toggling_tasks_off_on_save_clears_existing_rows() {
        // The clear-and-rewrite contract is what makes the opt-out take
        // effect on the very next save with no migration: rows present,
        // user adds `tasks: off`, re-index drops them. Then removing the
        // opt-out brings them back.
        let db = db_with_task_schema();
        let live = "- [ ] alpha\n- [ ] beta\n";
        index_note_tasks(&db, DAILY_ID, live).expect("seed live tasks");
        assert_eq!(task_count(&db, DAILY_ID), 2);

        let opted_out = "---\ntasks: off\n---\n\n- [ ] alpha\n- [ ] beta\n";
        index_note_tasks(&db, DAILY_ID, opted_out).expect("reindex opted-out");
        assert_eq!(
            task_count(&db, DAILY_ID),
            0,
            "opt-out must clear stale rows"
        );

        // Re-enable: the same checkboxes are tasks again on next save.
        index_note_tasks(&db, DAILY_ID, live).expect("reindex re-enabled");
        assert_eq!(task_count(&db, DAILY_ID), 2);
    }

    #[test]
    fn one_note_reindex_does_not_affect_other_notes() {
        let db = db_with_task_schema();
        index_note_tasks(&db, "daily notes/2026-06-11", "- [ ] one\n").expect("n1");
        index_note_tasks(&db, "daily notes/2026-06-12", "- [ ] two\n- [ ] three\n").expect("n2");
        assert_eq!(task_count(&db, "daily notes/2026-06-11"), 1);
        assert_eq!(task_count(&db, "daily notes/2026-06-12"), 2);

        // Reindex the first with no tasks. The second must survive untouched.
        index_note_tasks(&db, "daily notes/2026-06-11", "").expect("clear n1");
        assert_eq!(task_count(&db, "daily notes/2026-06-11"), 0);
        assert_eq!(task_count(&db, "daily notes/2026-06-12"), 2);
    }

    #[test]
    fn frontmatter_tasks_get_distinct_negative_lines() {
        let db = db_with_task_schema();
        let content = "---\ntasks:\n  - text: \"a\"\n  - text: \"b\"\n  - text: \"c\"\n---\nbody\n";
        index_note_tasks(&db, "n1", content).expect("index");
        assert_eq!(task_count(&db, "n1"), 3);
    }

    #[test]
    fn due_date_and_completion_columns_round_trip() {
        let db = db_with_task_schema();
        let content = "- [x] Shipped 📅 2026-05-08 ⏰ 18:00 ✅ 2026-05-08\n";
        index_note_tasks(&db, "n1", content).expect("index");

        let mut p = BTreeMap::new();
        p.insert("id".into(), DataValue::Str("n1".into()));
        let r = db
            .run_script(
                r#"?[due_at, completed, completed_at] := *task{ note_id, due_at, completed, completed_at }, note_id == $id"#,
                p,
                ScriptMutability::Immutable,
            )
            .expect("query");
        assert_eq!(r.rows.len(), 1);
        let row = &r.rows[0];
        assert_eq!(row[0].get_str().unwrap(), "2026-05-08 18:00");
        assert!(matches!(&row[1], cozo::DataValue::Bool(true)));
        assert_eq!(row[2].get_str().unwrap(), "2026-05-08");
    }

    #[test]
    fn linked_person_and_project_columns_populated() {
        let db = db_with_task_schema();
        let content = "- [ ] Reply to [[Ravi]] #project/atlas 📅 2026-06-12\n";
        index_note_tasks(&db, "n1", content).expect("index");

        let mut p = BTreeMap::new();
        p.insert("id".into(), DataValue::Str("n1".into()));
        let r = db
            .run_script(
                r#"?[linked_person, linked_project] := *task{ note_id, linked_person, linked_project }, note_id == $id"#,
                p,
                ScriptMutability::Immutable,
            )
            .expect("query");
        assert_eq!(r.rows.len(), 1);
        assert_eq!(r.rows[0][0].get_str().unwrap(), "Ravi");
        assert_eq!(r.rows[0][1].get_str().unwrap(), "project/atlas");
    }
}

#[cfg(test)]
mod resolver_tests {
    use super::*;

    #[test]
    fn normalize_strips_heading_and_block_anchors() {
        // LR-1: anchors address a spot inside the note, not a different note.
        assert_eq!(LinkResolver::normalize_target("Note#Section"), "note");
        assert_eq!(LinkResolver::normalize_target("Note#Section#Sub"), "note");
        assert_eq!(LinkResolver::normalize_target("Note^block123"), "note");
        assert_eq!(LinkResolver::normalize_target("Note#^block123"), "note");
        // Anchor with a space before it.
        assert_eq!(LinkResolver::normalize_target("Note #Section"), "note");
        // Folder-qualified target keeps its path, drops the anchor.
        assert_eq!(
            LinkResolver::normalize_target("Folder/Note#Section"),
            "folder/note"
        );
        // Same-note anchor → empty (no cross-note edge).
        assert_eq!(LinkResolver::normalize_target("#Section"), "");
    }

    #[test]
    fn resolve_and_classify_follow_anchor_to_the_note() {
        // LR-1: `[[Note#Section]]` must resolve to `note`, not dangle.
        let r = LinkResolver::from_ids(&["note"]);
        assert_eq!(r.resolve("Note#Section", ""), Some("note".to_string()));
        assert!(matches!(
            r.classify("Note#Section", ""),
            LinkResolution::Resolved(id) if id == "note"
        ));
        // A bare same-note anchor resolves to nothing (no edge), not a ghost.
        assert_eq!(r.resolve("#Section", ""), None);
    }

    #[test]
    fn ambiguous_tiebreak_is_deterministic_across_insertion_order() {
        // LR-4: two equally-close, equally-shallow candidates must resolve to
        // the SAME note regardless of the order ids were inserted (cold bulk
        // vs incremental add_id). Lexicographically-smallest id wins.
        let forward = LinkResolver::from_ids(&["alpha/note", "beta/note"]);
        let reverse = LinkResolver::from_ids(&["beta/note", "alpha/note"]);
        // Source dir shares no prefix with either → pure tiebreak territory.
        assert_eq!(
            forward.resolve("note", "unrelated"),
            Some("alpha/note".to_string())
        );
        assert_eq!(
            reverse.resolve("note", "unrelated"),
            forward.resolve("note", "unrelated"),
            "resolution must not depend on insertion order"
        );
    }
}

#[cfg(test)]
impl LinkResolver {
    /// Test-only constructor: build a resolver from a list of ids without
    /// a DB, so unit tests don't have to stand up a Cozo instance.
    pub(super) fn from_ids(ids: &[&str]) -> Self {
        let mut id_set = std::collections::HashSet::new();
        let mut by_basename: HashMap<String, Vec<String>> = HashMap::new();
        for id in ids {
            let id = id.to_string();
            let basename = id
                .rsplit_once('/')
                .map(|(_, b)| b.to_string())
                .unwrap_or_else(|| id.clone());
            by_basename.entry(basename).or_default().push(id.clone());
            id_set.insert(id);
        }
        Self {
            id_set,
            by_basename,
        }
    }
}

#[cfg(test)]
mod atomic_index_tests {
    //! DB-4 (a note's derived rows are written atomically) and the exactness of
    //! the per-note stats deltas DB-3's cache relies on. All run against the
    //! real SQLite-backed schema (`init_db`), because the guarantee being
    //! tested — a failed script rolls back every earlier block — is a property
    //! of the storage transaction, not of CozoScript syntax.
    use super::tx_fail_point::Guard;
    use super::*;
    use crate::db::queries::get_graph_stats;
    use tempfile::TempDir;

    const ROOT: &str = "/roost";

    fn fresh_db() -> (TempDir, DbInstance) {
        let dir = TempDir::new().expect("tempdir");
        let db = crate::db::schema::init_db(dir.path()).expect("init_db");
        (dir, db)
    }

    fn abs(rel: &str) -> String {
        format!("{ROOT}/{rel}")
    }

    fn rows(db: &DbInstance, script: &str, id: &str) -> Vec<Vec<DataValue>> {
        let mut p = BTreeMap::new();
        p.insert("id".to_string(), DataValue::Str(id.into()));
        db.run_script(script, p, ScriptMutability::Immutable)
            .expect("query")
            .rows
    }

    /// Everything the index writes for one note, in a comparable shape.
    #[derive(Debug, PartialEq)]
    struct NoteRows {
        stamp: Option<f64>,
        links: Vec<String>,
        tags: Vec<String>,
        dangling: Vec<String>,
        tasks: usize,
    }

    fn note_rows(db: &DbInstance, id: &str) -> NoteRows {
        let stamp = rows(db, r#"?[u] := *notes{ id: $id, updated_at: u }"#, id)
            .first()
            .and_then(|r| r[0].get_float());
        let strs = |script: &str| {
            let mut v: Vec<String> = rows(db, script, id)
                .iter()
                .filter_map(|r| r[0].get_str().map(String::from))
                .collect();
            v.sort();
            v
        };
        NoteRows {
            stamp,
            links: strs(r#"?[t] := *links{ from_id: $id, to_id: t }"#),
            tags: strs(r#"?[t] := *tags{ note_id: $id, tag: t }"#),
            dangling: strs(r#"?[t] := *dangling{ from_id: $id, target: t }"#),
            tasks: rows(db, r#"?[l] := *task{ note_id: $id, source_line: l }"#, id).len(),
        }
    }

    fn index(
        db: &DbInstance,
        resolver: &mut LinkResolver,
        rel: &str,
        content: &str,
    ) -> Result<NoteIndexOutcome, String> {
        index_note_full_with_resolver(db, ROOT, &abs(rel), content, resolver)
    }

    #[test]
    fn failed_index_rolls_back_every_relation_and_the_stamp() {
        let (_d, db) = fresh_db();
        let mut resolver = LinkResolver::build(&db).unwrap();
        index(&db, &mut resolver, "b.md", "# b\n").unwrap();
        index(&db, &mut resolver, "c.md", "# c\n").unwrap();

        let v1 = "see [[b]] and [[nowhere]] #old\n- [ ] first 📅 2026-12-01\n";
        index(&db, &mut resolver, "a.md", v1).unwrap();
        let before = note_rows(&db, "a");
        assert_eq!(before.links, vec!["b".to_string()]);
        assert_eq!(before.tags, vec!["old".to_string()]);
        assert_eq!(before.dangling, vec!["nowhere".to_string()]);
        assert_eq!(before.tasks, 1);

        // v2 changes every relation — but the transaction dies AFTER all of its
        // blocks ran (the shape of a crash between statements).
        let v2 = "see [[c]] #new\n";
        {
            let _fp = Guard::arm();
            let err = index(&db, &mut resolver, "a.md", v2).unwrap_err();
            assert!(err.contains("Failed to index note"), "got: {err}");
        }
        assert_eq!(
            note_rows(&db, "a"),
            before,
            "an interrupted index must leave the note exactly as it was — stamp included, \
             so the warm heal still sees it as stale"
        );

        // With the fail-point gone the identical call lands completely.
        index(&db, &mut resolver, "a.md", v2).unwrap();
        let after = note_rows(&db, "a");
        assert_eq!(after.links, vec!["c".to_string()]);
        assert_eq!(after.tags, vec!["new".to_string()]);
        assert!(after.dangling.is_empty());
        assert_eq!(after.tasks, 0);
        assert_ne!(after.stamp, before.stamp, "stamp advances with the rows");
    }

    #[test]
    fn failed_first_index_creates_nothing_and_does_not_leak_the_resolver_id() {
        let (_d, db) = fresh_db();
        let mut resolver = LinkResolver::build(&db).unwrap();
        {
            let _fp = Guard::arm();
            index(&db, &mut resolver, "n.md", "# n #t\n[[x]]\n").unwrap_err();
        }
        assert!(!note_exists(&db, "n").unwrap(), "no notes row");
        assert_eq!(
            note_rows(&db, "n"),
            NoteRows {
                stamp: None,
                links: vec![],
                tags: vec![],
                dangling: vec![],
                tasks: 0
            },
            "no orphan rows of any relation"
        );
        assert!(
            !resolver.contains_id("n"),
            "the resolver must not advertise a note that isn't in the graph"
        );
    }

    #[test]
    fn promotion_of_waiting_backlinks_is_part_of_the_new_notes_transaction() {
        let (_d, db) = fresh_db();
        let mut resolver = LinkResolver::build(&db).unwrap();
        index(&db, &mut resolver, "a.md", "waiting on [[late]]\n").unwrap();
        assert_eq!(note_rows(&db, "a").dangling, vec!["late".to_string()]);

        {
            let _fp = Guard::arm();
            index(&db, &mut resolver, "late.md", "# late\n").unwrap_err();
        }
        let a = note_rows(&db, "a");
        assert_eq!(
            a.dangling,
            vec!["late".to_string()],
            "a failed create must not half-promote"
        );
        assert!(a.links.is_empty());

        let out = index(&db, &mut resolver, "late.md", "# late\n").unwrap();
        assert!(out.was_new);
        let a = note_rows(&db, "a");
        assert_eq!(a.links, vec!["late".to_string()], "promoted immediately");
        assert!(a.dangling.is_empty());
        assert_eq!(
            out.links_delta, 1,
            "the promoted edge moves the graph-wide link total"
        );
    }

    /// The exactness contract DB-3 depends on: for every index, the reported
    /// deltas equal the change in what `get_graph_stats` counts. Covers new
    /// notes, edits that add/drop links and tags, a byte-identical re-save, a
    /// promotion that adds an edge, and two dangling rows that promote to the
    /// SAME edge (one new row, not two).
    #[test]
    fn outcome_deltas_equal_the_change_in_graph_stats() {
        let (_d, db) = fresh_db();
        let mut resolver = LinkResolver::build(&db).unwrap();
        let steps: &[(&str, &str)] = &[
            ("a.md", "[[b]] [[later]] #x #y\n"),
            // `b` didn't exist yet, so a's `[[b]]` is dangling until here → promoted.
            ("b.md", "# b #x\n"),
            ("a.md", "[[b]] #x\n"), // drop a dangling row + a tag
            ("a.md", "[[b]] #x\n"), // byte-identical re-save
            ("z.md", "[[late2]] [[sub/late2]] #t\n"), // two dangling rows, one future target
            ("sub/late2.md", "# late2\n"), // 2 promotions → ONE new edge
            ("c.md", "[[a]] [[b]] [[b]] [[c]] #x\n"), // duplicate + self-link
            ("c.md", "no links now\n"),
            (
                "a.md",
                "[[b]] [[c]] [[sub/late2]] #x #q\n- [ ] t 📅 2026-12-02\n",
            ),
        ];
        let mut totals = get_graph_stats(&db).unwrap();
        for (i, (rel, body)) in steps.iter().enumerate() {
            let out = index(&db, &mut resolver, rel, body).unwrap();
            let want = get_graph_stats(&db).unwrap();
            let got = (
                totals.notes as i64 + out.notes_delta,
                totals.links as i64 + out.links_delta,
                totals.tags as i64 + out.tags_delta,
            );
            assert_eq!(
                got,
                (want.notes as i64, want.links as i64, want.tags as i64),
                "step {i} ({rel}): deltas {out:?} drifted from get_graph_stats {want:?}"
            );
            totals = want;
        }
    }

    #[test]
    fn index_note_tasks_replace_is_atomic() {
        let (_d, db) = fresh_db();
        let mut resolver = LinkResolver::build(&db).unwrap();
        index(
            &db,
            &mut resolver,
            "t.md",
            "- [ ] one 📅 2026-12-01\n- [ ] two 📅 2026-12-02\n",
        )
        .unwrap();
        assert_eq!(note_rows(&db, "t").tasks, 2);
        {
            let _fp = Guard::arm();
            index_note_tasks(&db, "t", "no tasks here\n").unwrap_err();
        }
        assert_eq!(
            note_rows(&db, "t").tasks,
            2,
            "a failed task re-index must not leave the note with its tasks wiped"
        );
        index_note_tasks(&db, "t", "no tasks here\n").unwrap();
        assert_eq!(note_rows(&db, "t").tasks, 0);
    }

    #[test]
    fn bulk_pass2_replaces_links_and_dangling_atomically() {
        let (_d, db) = fresh_db();
        let files = vec![
            (abs("a.md"), "[[b]] [[gone]]\n".to_string()),
            (abs("b.md"), "# b\n".to_string()),
        ];
        bulk_index_pass1(&db, ROOT, &files).unwrap();
        let resolver = LinkResolver::build(&db).unwrap();
        bulk_index_pass2(&db, ROOT, &files, &resolver).unwrap();
        let before = note_rows(&db, "a");
        assert_eq!(before.links, vec!["b".to_string()]);
        assert_eq!(before.dangling, vec!["gone".to_string()]);

        let edited = vec![
            (abs("a.md"), "[[a]]\n".to_string()),
            (abs("b.md"), "# b\n".to_string()),
        ];
        {
            let _fp = Guard::arm();
            bulk_index_pass2(&db, ROOT, &edited, &resolver).unwrap_err();
        }
        assert_eq!(
            note_rows(&db, "a"),
            before,
            "clear-then-put of links + dangling must be one transaction (IP-5: \
             links>0 is the 'pass 2 landed' gate, so a torn pass 2 was served warm)"
        );
        bulk_index_pass2(&db, ROOT, &edited, &resolver).unwrap();
        assert_eq!(note_rows(&db, "a").links, vec!["a".to_string()]);
    }

    /// DB-4 ordering invariant with no other observable form: an interrupted
    /// purge must leave the `notes` row behind (so the warm-open ghost
    /// reconcile finds and finishes it), which only holds if it is removed LAST.
    #[test]
    fn purge_note_removes_the_notes_row_last() {
        let src = include_str!("notes.rs");
        let steps = src
            .split("let steps: [(&str, &str, BTreeMap<String, DataValue>); 6]")
            .nth(1)
            .expect("purge_note steps array")
            .split("];")
            .next()
            .expect("end of steps array");
        let labels: Vec<&str> = steps
            .lines()
            .map(str::trim)
            .filter(|l| l.starts_with('"') && l.ends_with("\","))
            .collect();
        assert_eq!(labels.len(), 6, "labels: {labels:?}");
        assert_eq!(
            labels.last().copied(),
            Some("\"notes\","),
            "the `notes` row must be purged LAST; got order {labels:?}"
        );
    }

    #[test]
    fn interrupted_purge_remnant_is_finished_by_the_ghost_reconcile() {
        // The state an interrupted purge leaves (notes row present, file gone)
        // is exactly what `reconcile_deleted_notes` exists to clean.
        let (_d, db) = fresh_db();
        let mut resolver = LinkResolver::build(&db).unwrap();
        index(&db, &mut resolver, "keep.md", "# keep\n").unwrap();
        index(
            &db,
            &mut resolver,
            "gone.md",
            "[[keep]] #t\n- [ ] x 📅 2026-12-01\n",
        )
        .unwrap();
        let disk = vec![abs("keep.md")];
        assert_eq!(reconcile_deleted_notes(&db, ROOT, &disk).unwrap(), 1);
        assert!(!note_exists(&db, "gone").unwrap());
        assert_eq!(
            note_rows(&db, "gone"),
            NoteRows {
                stamp: None,
                links: vec![],
                tags: vec![],
                dangling: vec![],
                tasks: 0
            }
        );
    }

    /// N1: once the transaction commits, the call can no longer fail. If the
    /// post-commit measurements are unreadable it says so (`deltas_trusted:
    /// false`) instead of returning `Err` for a note that IS indexed.
    #[test]
    fn committed_index_never_reports_err_when_post_commit_reads_fail() {
        let (_d, db) = fresh_db();
        let mut resolver = LinkResolver::build(&db).unwrap();
        index(&db, &mut resolver, "b.md", "# b\n").unwrap();
        let out = {
            let _fp = super::tx_fail_point::PostCommitGuard::arm();
            index(&db, &mut resolver, "a.md", "[[b]] #t\n").expect("committed → Ok")
        };
        assert!(!out.deltas_trusted);
        assert!(out.was_new);
        assert_eq!(
            (out.notes_delta, out.links_delta, out.tags_delta),
            (0, 0, 0)
        );
        // The write itself landed.
        let a = note_rows(&db, "a");
        assert_eq!(a.links, vec!["b".to_string()]);
        assert_eq!(a.tags, vec!["t".to_string()]);
        // Healthy path reports trusted deltas.
        let ok = index(&db, &mut resolver, "a.md", "[[b]] #t #u\n").unwrap();
        assert!(ok.deltas_trusted);
        assert_eq!(ok.tags_delta, 1);
    }
}

/// LR-6 / LR-7: tag IDENTITY in the inventory + the agent tag-guard's inputs.
/// Real SQLite-backed schema and the real indexer: the contract is "stored rows
/// stay verbatim, every read folds", so the tests index real notes and inspect
/// both the stored rows and the folded views.
#[cfg(test)]
mod tag_identity_tests {
    use super::*;
    use crate::db::queries::get_graph_stats;
    use tempfile::TempDir;

    const ROOT: &str = "/roost";

    fn fresh_db() -> (TempDir, DbInstance) {
        let dir = TempDir::new().expect("tempdir");
        let db = crate::db::schema::init_db(dir.path()).expect("init_db");
        (dir, db)
    }

    fn index(
        db: &DbInstance,
        resolver: &mut LinkResolver,
        rel: &str,
        content: &str,
    ) -> NoteIndexOutcome {
        index_note_full_with_resolver(db, ROOT, &format!("{ROOT}/{rel}"), content, resolver)
            .expect("index")
    }

    fn stored_tags(db: &DbInstance) -> Vec<(String, String)> {
        let mut v: Vec<(String, String)> = db
            .run_script(
                r#"?[note_id, tag] := *tags{ note_id, tag }"#,
                BTreeMap::new(),
                ScriptMutability::Immutable,
            )
            .expect("query")
            .rows
            .iter()
            .map(|r| {
                (
                    r[0].get_str().unwrap().to_string(),
                    r[1].get_str().unwrap().to_string(),
                )
            })
            .collect();
        v.sort();
        v
    }

    fn put_raw_tag(db: &DbInstance, note_id: &str, tag: &str) {
        let mut p = BTreeMap::new();
        p.insert("n".to_string(), DataValue::Str(note_id.into()));
        p.insert("t".to_string(), DataValue::Str(tag.into()));
        db.run_script(
            r#"?[note_id, tag] <- [[$n, $t]]
               :put tags { note_id, tag }"#,
            p,
            ScriptMutability::Mutable,
        )
        .expect("put tag");
    }

    #[test]
    fn indexer_stores_tags_verbatim_and_keeps_stats_exact() {
        let (_d, db) = fresh_db();
        let mut r = LinkResolver::build(&db).unwrap();
        let out = index(&db, &mut r, "a.md", "#café #ML #ml #日本語\n");
        // Verbatim: NOT folded at write time (Locked Decision #12), and the
        // Unicode tags are whole.
        assert_eq!(
            stored_tags(&db),
            vec![
                ("a".to_string(), "ML".to_string()),
                ("a".to_string(), "café".to_string()),
                ("a".to_string(), "ml".to_string()),
                ("a".to_string(), "日本語".to_string()),
            ]
        );
        // The patched stats cache counts stored rows — untouched by folding.
        assert_eq!(out.tags_delta, 4);
        assert_eq!(get_graph_stats(&db).unwrap().tags, 4);
    }

    #[test]
    fn top_tags_groups_case_variants_and_counts_each_note_once() {
        let (_d, db) = fresh_db();
        let mut r = LinkResolver::build(&db).unwrap();
        index(&db, &mut r, "a.md", "#ML and #ml in one note\n");
        index(&db, &mut r, "b.md", "#ml\n");
        index(&db, &mut r, "c.md", "#Ml #other\n");
        // Spellings: ML=1, ml=2, Ml=1 → naive sum 4; distinct notes = 3.
        assert_eq!(
            top_tags(&db, 10).unwrap(),
            vec![("ml".to_string(), 3), ("other".to_string(), 1)]
        );
    }

    #[test]
    fn top_tags_without_collisions_keeps_the_stored_spelling() {
        let (_d, db) = fresh_db();
        let mut r = LinkResolver::build(&db).unwrap();
        index(&db, &mut r, "a.md", "#Q4 #Q4plan\n");
        index(&db, &mut r, "b.md", "#Q4\n");
        assert_eq!(
            top_tags(&db, 10).unwrap(),
            vec![("Q4".to_string(), 2), ("Q4plan".to_string(), 1)]
        );
    }

    #[test]
    fn top_tags_respects_limit_and_breaks_ties_deterministically() {
        let (_d, db) = fresh_db();
        let mut r = LinkResolver::build(&db).unwrap();
        index(&db, &mut r, "a.md", "#bbb #aaa #ccc\n");
        let top = top_tags(&db, 2).unwrap();
        assert_eq!(
            top,
            vec![("aaa".to_string(), 1), ("bbb".to_string(), 1)],
            "equal counts sort by spelling"
        );
        assert!(top_tags(&db, 0).unwrap().is_empty());
    }

    #[test]
    fn top_tags_folds_unicode_case_and_normalization() {
        let (_d, db) = fresh_db();
        let mut r = LinkResolver::build(&db).unwrap();
        index(&db, &mut r, "a.md", "#café\n");
        index(&db, &mut r, "b.md", "#CAFÉ\n");
        index(&db, &mut r, "c.md", "#cafe\u{301}\n"); // decomposed
        let top = top_tags(&db, 10).unwrap();
        assert_eq!(top.len(), 1, "{top:?}");
        assert_eq!(top[0].1, 3);
    }

    /// Migration safety: a graph.db written BEFORE this rule (verbatim mixed
    /// case, a `caf` truncated by the ASCII-only parser) groups with rows
    /// written after it. No migration, no reindex.
    #[test]
    fn rows_from_an_older_index_fold_with_new_rows() {
        let (_d, db) = fresh_db();
        put_raw_tag(&db, "old", "Ideas");
        put_raw_tag(&db, "old", "caf");
        put_raw_tag(&db, "new", "ideas");
        let top = top_tags(&db, 10).unwrap();
        assert_eq!(top.len(), 2);
        assert_eq!(top[0].1, 2, "Ideas + ideas are one tag: {top:?}");
        assert_eq!(top[1], ("caf".to_string(), 1));
    }

    #[test]
    fn all_tag_names_stays_verbatim_so_guards_must_fold() {
        let (_d, db) = fresh_db();
        let mut r = LinkResolver::build(&db).unwrap();
        index(&db, &mut r, "a.md", "#ML\n");
        index(&db, &mut r, "b.md", "#ml\n");
        let names = all_tag_names(&db).unwrap();
        assert!(names.contains("ML") && names.contains("ml"));
        let keys: std::collections::HashSet<String> = names.iter().map(|t| fold_tag(t)).collect();
        assert_eq!(keys.len(), 1);
        assert!(
            keys.contains(&fold_tag("Ml")),
            "the guard's fold-compare sees `Ml` as known"
        );
    }
}
