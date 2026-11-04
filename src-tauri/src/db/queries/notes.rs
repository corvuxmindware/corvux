//! Note-relation queries and indexing. `LinkResolver` lives here because
//! it's built from one full scan of `notes{id}` and used immediately by
//! `index_note_pass2` — splitting them across files would just create a
//! circular-feeling dependency.

use cozo::{DataValue, DbInstance, ScriptMutability};
use std::collections::{BTreeMap, HashMap};

use crate::parser::checkboxes::parse_tasks_in_note;
use crate::parser::links::parse_links;
use crate::parser::tags::parse_tags;

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
/// Returns `(tag, count)` rows. Caller decides cap/formatting. We cap
/// inside the query so a roost with 5K unique tags doesn't ship the
/// whole tail to the LLM.
pub fn top_tags(db: &DbInstance, limit: usize) -> Result<Vec<(String, usize)>, String> {
    let script = r#"
        ?[tag, count(note_id)] := *tags{ note_id, tag }
        :order -count(note_id)
        :limit $limit
    "#;
    let mut params = BTreeMap::new();
    params.insert(
        "limit".to_string(),
        DataValue::Num(cozo::Num::Int(limit as i64)),
    );
    let result = db
        .run_script(script, params, ScriptMutability::Immutable)
        .map_err(|e| format!("top_tags query failed: {e}"))?;

    let mut out = Vec::with_capacity(result.rows.len());
    for row in result.rows.iter() {
        if row.len() >= 2 {
            if let (Some(tag), Some(count)) = (
                row[0].get_str().map(String::from),
                row[1].get_int().map(|n| n as usize),
            ) {
                out.push((tag, count));
            }
        }
    }
    Ok(out)
}

/// All distinct tag names currently indexed in the roost. Used by
/// write-tool validation to reject tags the agent invented out of
/// thin air — see `roost_write::execute`. Cheaper than `top_tags`
/// when the caller only needs membership, not ranking.
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

    /// Remove a note id from the cache. Idempotent. Called by future
    /// delete/rename code paths so the resolver doesn't keep ghost ids
    /// around that resolve to nothing on disk.
    #[allow(dead_code)] // wired up in Phase 0.5 (delete/rename invalidation)
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
    std::fs::metadata(abs_path)
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

/// Pass 1 of indexing: upsert the note row and replace its tag set. Does
/// NOT touch the links relation — link edges need the full roost index
/// to resolve correctly.
pub fn index_note_pass1(
    db: &DbInstance,
    roost_root: &str,
    abs_path: &str,
    content: &str,
) -> Result<(), String> {
    let id = note_id_from_abs_path(abs_path, roost_root);
    let title = title_from_path(abs_path);
    // IP-A2: prefer the file's mtime so ordering by `updated_at` reflects real
    // edit time and survives a cold rebuild; fall back to now() on backends we
    // can't stat (SAF).
    let updated_at = file_mtime_secs(abs_path).unwrap_or_else(now_secs);

    let mut params = BTreeMap::new();
    params.insert("id".to_string(), DataValue::Str(id.clone().into()));
    params.insert("title".to_string(), DataValue::Str(title.into()));
    params.insert(
        "path".to_string(),
        DataValue::Str(abs_path.to_string().into()),
    );
    params.insert("updated_at".to_string(), DataValue::from(updated_at));
    db.run_script(
        r#"?[id, title, path, updated_at] <- [[$id, $title, $path, $updated_at]]
           :put notes { id => title, path, updated_at }"#,
        params,
        ScriptMutability::Mutable,
    )
    .map_err(|e| format!("Failed to upsert note: {e}"))?;

    // Replace tag set for this note.
    let mut params = BTreeMap::new();
    params.insert("note_id".to_string(), DataValue::Str(id.clone().into()));
    db.run_script(
        r#"?[note_id, tag] := *tags{ note_id, tag }, note_id == $note_id
           :rm tags { note_id, tag }"#,
        params,
        ScriptMutability::Mutable,
    )
    .map_err(|e| format!("Failed to clear tags: {e}"))?;

    // Batch insert all tags in a single script — mirrors the bulk path
    // in `bulk_index_pass1`. The previous per-tag loop was 1+N
    // run_script calls under the global Mutex; on a note with 15 tags
    // that's 15 SQLite auto-commits per save.
    let parsed_tags = parse_tags(content.to_string());
    if !parsed_tags.is_empty() {
        let tag_rows: Vec<DataValue> = parsed_tags
            .into_iter()
            .map(|tag| {
                DataValue::List(vec![
                    DataValue::Str(id.clone().into()),
                    DataValue::Str(tag.tag.into()),
                ])
            })
            .collect();
        let mut params = BTreeMap::new();
        params.insert("rows".to_string(), DataValue::List(tag_rows));
        db.run_script(
            r#"?[note_id, tag] <- $rows
               :put tags { note_id, tag }"#,
            params,
            ScriptMutability::Mutable,
        )
        .map_err(|e| format!("Failed to insert tags: {e}"))?;
    }

    // Tasks are derived from the same content. Doing this in pass1 (not pass2)
    // keeps the per-save cost low — no resolver build needed; tasks are
    // single-note. The pane reads `:task` rows directly with no JOIN to
    // `links` for the basic groupings.
    index_note_tasks(db, &id, content)?;

    Ok(())
}

/// Pass 2 of indexing: resolve wiki-links against the supplied
/// `LinkResolver` and replace this note's outgoing edges.
pub fn index_note_pass2(
    db: &DbInstance,
    roost_root: &str,
    abs_path: &str,
    content: &str,
    resolver: &LinkResolver,
) -> Result<(), String> {
    let id = note_id_from_abs_path(abs_path, roost_root);
    let rel_dir = note_rel_dir(abs_path, roost_root);

    // Clear this note's old links AND its old dangling (unresolved) links —
    // both sets are recomputed below. Clearing dangling matters because a
    // link that previously had no target may now resolve (its target was
    // created), so its stale dangling row must go.
    let mut params = BTreeMap::new();
    params.insert("from_id".to_string(), DataValue::Str(id.clone().into()));
    db.run_script(
        r#"?[from_id, to_id] := *links{ from_id, to_id }, from_id == $from_id
           :rm links { from_id, to_id }"#,
        params,
        ScriptMutability::Mutable,
    )
    .map_err(|e| format!("Failed to clear links: {e}"))?;
    let mut params = BTreeMap::new();
    params.insert("from_id".to_string(), DataValue::Str(id.clone().into()));
    db.run_script(
        r#"?[from_id, target] := *dangling{ from_id, target }, from_id == $from_id
           :rm dangling { from_id, target }"#,
        params,
        ScriptMutability::Mutable,
    )
    .map_err(|e| format!("Failed to clear dangling links: {e}"))?;

    // Resolve each wiki-link. Resolved → a real `:links` edge. Unresolved →
    // a `:dangling` row (instead of being silently dropped), so when the
    // target note is later created, `promote_dangling_links` turns it into a
    // real edge immediately rather than waiting for THIS note to be re-saved.
    let mut link_rows: Vec<DataValue> = Vec::new();
    let mut dangling_rows: Vec<DataValue> = Vec::new();
    for link in parse_links(content.to_string()) {
        let link_text = link.display.as_deref().unwrap_or(&link.target).to_string();
        match resolver.resolve(&link.target, &rel_dir) {
            Some(to_id) => link_rows.push(DataValue::List(vec![
                DataValue::Str(id.clone().into()),
                DataValue::Str(to_id.into()),
                DataValue::Str(link_text.into()),
            ])),
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

    if !link_rows.is_empty() {
        let mut params = BTreeMap::new();
        params.insert("rows".to_string(), DataValue::List(link_rows));
        db.run_script(
            r#"?[from_id, to_id, link_text] <- $rows
               :put links { from_id, to_id => link_text }"#,
            params,
            ScriptMutability::Mutable,
        )
        .map_err(|e| format!("Failed to insert links: {e}"))?;
    }

    if !dangling_rows.is_empty() {
        let mut params = BTreeMap::new();
        params.insert("rows".to_string(), DataValue::List(dangling_rows));
        db.run_script(
            r#"?[from_id, target, link_text, target_base] <- $rows
               :put dangling { from_id, target => link_text, target_base }"#,
            params,
            ScriptMutability::Mutable,
        )
        .map_err(|e| format!("Failed to insert dangling links: {e}"))?;
    }

    Ok(())
}

/// IP-8: full single-note (re)index — the two-pass sequence plus dangling
/// promotion that the editor save runs, packaged for the auxiliary write
/// paths (AI roost/task edits, in-app quick-add, history revert). Those paths
/// previously called `index_note_pass1` ALONE, so a `[[link]]` they wrote
/// never became an edge (pass2 never ran) and any note waiting on this one as
/// a dangling target never got promoted — the links stayed invisible until
/// the note was next saved through the editor.
///
/// Fresh-build entry point: rebuilds a `LinkResolver` from a full `*notes`
/// scan (O(roost)). Prefer [`index_note_full_with_resolver`] via
/// [`GraphDb::index_note_full_cached`](crate::commands::graph::GraphDb::index_note_full_cached)
/// when a cached resolver is available — the auxiliary write paths now do,
/// sparing every task toggle / quick-add / revert / agent edit a full-roost
/// resolver rebuild under the DB lock (IP-A3 / TRN-7).
pub fn index_note_full(
    db: &DbInstance,
    roost_root: &str,
    abs_path: &str,
    content: &str,
) -> Result<(), String> {
    // Build BEFORE pass1: the shared impl `add_id`s this note's id, so a
    // brand-new note is still resolvable in pass2 without the scan having
    // seen it — behaviourally identical to the old build-after-pass1 order.
    let mut resolver = LinkResolver::build(db)?;
    index_note_full_with_resolver(db, roost_root, abs_path, content, &mut resolver)
}

/// IP-A3 / TRN-7: `index_note_full` against a caller-supplied resolver instead
/// of a fresh `LinkResolver::build` (O(roost)). The caller owns the resolver —
/// typically `GraphDb`'s cached one, reached via
/// [`GraphDb::index_note_full_cached`](crate::commands::graph::GraphDb::index_note_full_cached).
///
/// This note's own id is `add_id`'d (idempotent) BEFORE pass2 so a freshly
/// created note can resolve a self-link and so later saves see it — exactly
/// the discipline the editor save path (`index_note_internal`) already uses.
/// Because the resolver is maintained incrementally here, callers must NOT
/// invalidate it afterward: these paths only ever ADD a note id (a content
/// edit / task append / revert never deletes the note), so the cache stays
/// correct. Id-removing paths — delete / rename / purge — still invalidate the
/// cached resolver elsewhere, which forces the next save to rebuild.
pub fn index_note_full_with_resolver(
    db: &DbInstance,
    roost_root: &str,
    abs_path: &str,
    content: &str,
    resolver: &mut LinkResolver,
) -> Result<(), String> {
    let note_is_new = lookup_note_id_by_path(db, abs_path)?.is_none();
    index_note_pass1(db, roost_root, abs_path, content)?;
    let new_id = note_id_from_abs_path(abs_path, roost_root);
    // Make this note's id visible before pass2 (self-links) and to future
    // saves; idempotent for an id already in the resolver.
    resolver.add_id(&new_id);
    index_note_pass2(db, roost_root, abs_path, content, resolver)?;
    if note_is_new {
        if let Err(e) = promote_dangling_links(db, resolver, &new_id) {
            tracing::warn!(%new_id, error = %e, "index_note_full_with_resolver: promote_dangling_links failed");
        }
    }
    Ok(())
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

/// Promote dangling links that now resolve to the just-created `new_id` into
/// real `:links` edges, and delete the promoted `:dangling` rows. Called from
/// the save path when a NEW note appears, so a `[[NewNote]]` written before
/// NewNote existed becomes a backlink the moment NewNote is created — without
/// waiting for the referencing note to be re-saved.
///
/// Scans `:dangling` filtered to the new note's basename (the common case is
/// zero or a handful of rows), re-resolves each through the resolver (which
/// must already contain `new_id`), and promotes the ones that now point at a
/// real note. Returns the number of edges promoted.
pub fn promote_dangling_links(
    db: &DbInstance,
    resolver: &LinkResolver,
    new_id: &str,
) -> Result<usize, String> {
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

    let mut promoted = 0usize;
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

        let mut p = BTreeMap::new();
        p.insert(
            "from_id".to_string(),
            DataValue::Str(from_id.to_string().into()),
        );
        p.insert("to_id".to_string(), DataValue::Str(to_id.into()));
        p.insert(
            "link_text".to_string(),
            DataValue::Str(link_text.to_string().into()),
        );
        db.run_script(
            r#"?[from_id, to_id, link_text] <- [[$from_id, $to_id, $link_text]]
               :put links { from_id, to_id => link_text }"#,
            p,
            ScriptMutability::Mutable,
        )
        .map_err(|e| format!("promote_dangling: insert link failed: {e}"))?;

        let mut p = BTreeMap::new();
        p.insert(
            "from_id".to_string(),
            DataValue::Str(from_id.to_string().into()),
        );
        p.insert(
            "target".to_string(),
            DataValue::Str(target.to_string().into()),
        );
        db.run_script(
            r#"?[from_id, target] <- [[$from_id, $target]]
               :rm dangling { from_id, target }"#,
            p,
            ScriptMutability::Mutable,
        )
        .map_err(|e| format!("promote_dangling: remove dangling failed: {e}"))?;
        promoted += 1;
    }
    Ok(promoted)
}

/// Re-derive `:task` rows for a single note. Clears every existing task
/// row for `note_id`, parses the markdown for `- [ ]` / `- [x]` checkboxes
/// + frontmatter `tasks:`, and inserts the new set.
///
/// Called from the per-note save path inside `index_note_pass1` so a
/// freshly-edited note's tasks are visible to the Tasks pane within the
/// same save tick. The parse is pure regex (`parser::checkboxes`) — no
/// LLM call, no embedding, cheap to run on every save.
pub fn index_note_tasks(db: &DbInstance, note_id: &str, content: &str) -> Result<(), String> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64();

    // Clear existing rows for this note. Source-of-truth is the markdown,
    // so a clear-then-rewrite per save is cheaper than a row-level diff
    // (typical note has 0–10 tasks).
    let mut params = BTreeMap::new();
    params.insert(
        "note_id".to_string(),
        DataValue::Str(note_id.to_string().into()),
    );
    db.run_script(
        r#"?[note_id, source_line] := *task{ note_id, source_line }, note_id == $note_id
           :rm task { note_id, source_line }"#,
        params,
        ScriptMutability::Mutable,
    )
    .map_err(|e| format!("Failed to clear tasks for note: {e}"))?;

    // Convention 0.3: a checkbox is a task only with an effective date. Feed
    // the note's own date (daily-note stem; frontmatter `date:` wins inside
    // the parser) and today so dated/daily-note checkboxes and lone-⏰ lines
    // resolve correctly.
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let note_date = note_date_from_id(note_id);
    let parsed = parse_tasks_in_note(content, note_date.as_deref(), Some(&today));
    if parsed.is_empty() {
        return Ok(());
    }

    let mut rows: Vec<DataValue> = Vec::with_capacity(parsed.len());
    for task in parsed {
        rows.push(DataValue::List(vec![
            DataValue::Str(note_id.to_string().into()),
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

    let mut params = BTreeMap::new();
    params.insert("rows".to_string(), DataValue::List(rows));
    db.run_script(
        r#"?[note_id, source_line, text, content_anchor, due_at, completed, completed_at, linked_person, linked_project, updated_at] <- $rows
           :put task { note_id, source_line => text, content_anchor, due_at, completed, completed_at, linked_person, linked_project, updated_at }"#,
        params,
        ScriptMutability::Mutable,
    )
    .map_err(|e| format!("Failed to insert tasks: {e}"))?;

    Ok(())
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

    let steps: [(&str, &str, BTreeMap<String, DataValue>); 6] = [
        (
            "notes",
            r#"?[id] := *notes{ id }, id == $id
               :rm notes { id }"#,
            id_param(note_id, "id"),
        ),
        (
            "links_outgoing",
            r#"?[from_id, to_id] := *links{ from_id, to_id }, from_id == $id
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
            r#"?[note_id, tag] := *tags{ note_id, tag }, note_id == $note_id
               :rm tags { note_id, tag }"#,
            id_param(note_id, "note_id"),
        ),
        (
            // drop derived `- [ ]` rows for the note — without this,
            // tasks linger in the pane until the next full re-index.
            "task",
            r#"?[note_id, source_line] := *task{ note_id, source_line }, note_id == $note_id
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
            r#"?[from_id, target] := *dangling{ from_id, target }, from_id == $id
               :rm dangling { from_id, target }"#,
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
/// resolver in memory, then writes the whole link set in two scripts:
/// one `:rm links` clearing the old relation, one `:put links` for the
/// new edges.
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
    db.run_script(
        r#"?[from_id, to_id] := *links{ from_id, to_id }
           :rm links { from_id, to_id }"#,
        BTreeMap::new(),
        ScriptMutability::Mutable,
    )
    .map_err(|e| format!("Bulk-clear links failed: {e}"))?;
    db.run_script(
        r#"?[from_id, target] := *dangling{ from_id, target }
           :rm dangling { from_id, target }"#,
        BTreeMap::new(),
        ScriptMutability::Mutable,
    )
    .map_err(|e| format!("Bulk-clear dangling failed: {e}"))?;

    if !link_rows.is_empty() {
        let mut params = BTreeMap::new();
        params.insert("rows".to_string(), DataValue::List(link_rows));
        db.run_script(
            r#"?[from_id, to_id, link_text] <- $rows
               :put links { from_id, to_id => link_text }"#,
            params,
            ScriptMutability::Mutable,
        )
        .map_err(|e| format!("Bulk-insert links failed: {e}"))?;
    }

    if !dangling_rows.is_empty() {
        let mut params = BTreeMap::new();
        params.insert("rows".to_string(), DataValue::List(dangling_rows));
        db.run_script(
            r#"?[from_id, target, link_text, target_base] <- $rows
               :put dangling { from_id, target => link_text, target_base }"#,
            params,
            ScriptMutability::Mutable,
        )
        .map_err(|e| format!("Bulk-insert dangling failed: {e}"))?;
    }

    Ok(())
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
