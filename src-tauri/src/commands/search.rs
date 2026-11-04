use serde::Serialize;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
use tauri::{AppHandle, Manager, State};

use crate::commands::files::{to_rel, RoostFsState};
use crate::db::search_db::SearchDb;
use crate::roostfs::RoostFs;

/// One FTS database together with the roost identity it was built for.
/// Keeping all three values behind one mutex makes path validation and the
/// subsequent DB operation atomic with respect to a roost switch.
pub(crate) struct BoundSearchDb {
    pub(crate) db: SearchDb,
    roost_path: String,
    generation: u64,
}

/// Tauri-managed state wrapping the active roost's FTS5 search database.
pub struct SearchState(pub(crate) Mutex<Option<BoundSearchDb>>);

impl Default for SearchState {
    fn default() -> Self {
        Self(Mutex::new(None))
    }
}

impl SearchState {
    pub fn clear(&self) {
        if let Ok(mut guard) = self.0.lock() {
            *guard = None;
        }
    }

    fn install(&self, db: SearchDb, roost_path: String, generation: u64) -> Result<(), String> {
        let mut guard = self.0.lock().map_err(|e| format!("Lock error: {e}"))?;
        *guard = Some(BoundSearchDb {
            db,
            roost_path,
            generation,
        });
        Ok(())
    }

    /// Run an FTS operation only when `path` belongs to the database's roost.
    /// The state mutex remains held for the whole operation, preventing a
    /// delayed write for roost A from racing an A -> B database replacement.
    pub(crate) fn with_db_for_path<T>(
        &self,
        path: &str,
        operation: impl FnOnce(&SearchDb) -> Result<T, String>,
    ) -> Result<T, String> {
        self.with_optional_db_for_path(path, operation)?
            .ok_or_else(|| "Search DB not initialized".to_string())
    }

    pub(crate) fn with_optional_db_for_path<T>(
        &self,
        path: &str,
        operation: impl FnOnce(&SearchDb) -> Result<T, String>,
    ) -> Result<Option<T>, String> {
        let guard = self.0.lock().map_err(|e| format!("Lock error: {e}"))?;
        let Some(bound) = guard.as_ref() else {
            return Ok(None);
        };
        to_rel(path, &bound.roost_path)
            .ok_or_else(|| "Search path is outside the active roost".to_string())?;
        operation(&bound.db).map(Some)
    }
}

/// DB-5: delay before the once-per-launch FTS maintenance runs, so the VACUUM
/// never competes with the user's first interactions right after a roost open.
const MAINTENANCE_DELAY_SECS: u64 = 120;

fn now_unix_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// DB-5: schedule opportunistic FTS maintenance (segment merge + VACUUM) on a
/// detached background task so `init_search` never blocks startup on it. The
/// task waits out `MAINTENANCE_DELAY_SECS`, then — only if a day has passed
/// since the last run — takes the `SearchState` lock (guaranteeing no
/// concurrent write) and compacts. Cheap no-op when not yet due or no DB open.
fn spawn_search_maintenance(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(MAINTENANCE_DELAY_SECS)).await;
        let now = now_unix_secs();
        let state = app.state::<SearchState>();
        // VACUUM is synchronous blocking I/O; `block_in_place` lets the
        // scheduler migrate other async tasks off this worker while it runs.
        tokio::task::block_in_place(|| {
            let guard = match state.0.lock() {
                Ok(g) => g,
                Err(_) => return,
            };
            let Some(bound) = guard.as_ref() else {
                return;
            };
            let db = &bound.db;
            if !db.maintenance_due(now) {
                return;
            }
            match db.run_maintenance() {
                Ok(()) => {
                    let _ = db.mark_maintenance_done(now);
                    tracing::info!("search: FTS maintenance (optimize + VACUUM) complete");
                }
                Err(e) => tracing::warn!(error = %e, "search: FTS maintenance failed"),
            }
        });
    });
}

#[derive(Debug, Serialize, Clone, specta::Type)]
pub struct SearchMatch {
    /// Line number (1-based)
    #[specta(type = u32)]
    pub line: usize,
    /// The full line text (trimmed)
    pub text: String,
    /// Character offset where the match starts within the line
    #[specta(type = u32)]
    pub col: usize,
}

#[derive(Debug, Serialize, Clone, specta::Type)]
pub struct SearchResult {
    pub path: String,
    pub name: String,
    /// Context snippet from FTS5 (with >>> <<< markers around matches)
    pub snippet: String,
    pub matches: Vec<SearchMatch>,
    /// Reciprocal-Rank-Fusion score across all retrieval lanes that
    /// surfaced this note. Higher = more relevant. Used for ordering;
    /// frontend can render it as a sort indicator if useful.
    #[serde(default)]
    pub score: f64,
    /// Which lane contributed to this result. Always `"fts"` now —
    /// retrieval is FTS5-only since the embeddings/HNSW lane was removed.
    /// Kept so the UI badge code keeps working without a frontend change.
    #[serde(default)]
    pub match_type: String,
}

/// Initialize the FTS5 search index for a roost. DB lives under
/// `app_data_dir/vaults/{hash}/search.db` — device-local, never inside
/// the roost folder. Walks the backend installed by `open_roost`, so
/// SAF roosts index the same way as desktop roosts.
///
/// **Warm-start optimisation:** if the DB already contains indexed notes
/// from a previous session, skip the clear + walk + re-insert entirely.
/// The persistent SQLite file is the cache; only first-ever launch pays
/// the full scan cost.
#[tauri::command]
#[specta::specta]
pub async fn init_search(
    roost_path: String,
    app: AppHandle,
    roost_fs: State<'_, crate::commands::files::RoostFsState>,
    state: State<'_, SearchState>,
) -> Result<u32, String> {
    tracing::info!(%roost_path, "search: init_search called");
    let generation = roost_fs.generation();
    let backend = roost_fs
        .current()
        .await
        .ok_or_else(|| "Roost backend not initialized — open a roost first".to_string())?;
    if !super::roost::same_roost_root(backend.root_id().as_str(), Path::new(&roost_path)) {
        return Err("Roost changed before search initialization began".to_string());
    }
    let db_dir = crate::paths::roost_db_dir(&app, &roost_path)?;
    let db = SearchDb::init(&db_dir)?;

    // ── Fast path: DB already warm AND a prior cold index completed ─────
    // SRX-3: gate on the completion marker, not note_count alone. A crash
    // mid-cold-index leaves rows but no marker; without this the partial index
    // would be served forever (the torn-init bug IP-5 fixed for the graph).
    let existing = db.note_count()?;
    if existing > 0 && db.index_complete() {
        tracing::info!(existing, "search: DB warm, reconciling against disk");
        // SRX-1: reconcile the warm index against disk — purge ghosts (deleted
        // off-app) and index notes added off-app.
        let db = reconcile_search_against_disk(db, backend.as_ref(), &roost_path).await;
        let final_count = db.note_count()? as u32;
        let _transition = roost_fs.transition.lock().await;
        if !roost_fs.is_generation_current(generation) {
            return Err("Roost changed during search initialization".to_string());
        }
        state.install(db, roost_path.clone(), generation)?;
        drop(_transition);
        // DB-5: warm path is where FTS tombstones accumulate — schedule the
        // (day-gated, delayed) compaction pass.
        spawn_search_maintenance(app.clone());
        return Ok(final_count);
    }

    // ── Cold path: first launch, or a torn/partial prior index ──────────
    tracing::info!("search: DB empty or index incomplete — full roost scan");
    // Clear the marker up-front so a crash mid-rebuild leaves us in the cold
    // path again next launch (never a stale "complete" over a half index).
    let _ = db.clear_index_complete();
    db.clear_index()?;

    // Same cached walk as `init_graph` — whichever fires second on cold
    // start gets the previous walk's `Arc<Vec<...>>` for free.
    let files = crate::roostfs::walk::collect_markdown_cached(backend.as_ref(), &roost_path)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "search: roost walk failed");
            e
        })?;

    // SRX-5: index the whole roost in ONE transaction, then set the completion
    // marker only after it commits (SRX-3) so a torn init re-runs cleanly.
    db.bulk_upsert(&files)?;
    if let Err(e) = db.set_index_complete() {
        tracing::warn!(error = %e, "search: failed to set index-complete marker");
    }
    let count = files.len();

    let _transition = roost_fs.transition.lock().await;
    if !roost_fs.is_generation_current(generation) {
        return Err("Roost changed during search initialization".to_string());
    }
    state.install(db, roost_path.clone(), generation)?;
    drop(_transition);

    // DB-5: stamp the maintenance clock even on a cold build (the DB was just
    // freshly written, nothing to reclaim) so the pass is skipped for a day.
    spawn_search_maintenance(app.clone());

    tracing::info!(count, "search: init_search complete (cold), notes indexed");
    Ok(count as u32)
}

/// Minimum mtime advance (seconds) that counts as "modified while closed".
/// Aliases `commands::graph::MODIFIED_MTIME_EPSILON` — one source of truth, so
/// search and graph can't drift on what counts as an external edit.
const MODIFIED_MTIME_EPSILON: f64 = super::graph::MODIFIED_MTIME_EPSILON;

/// SRX-1 + NEW-S-FTS1: reconcile a warm FTS index against disk — purge ghosts
/// (notes deleted off-app), index notes added off-app, and re-index notes
/// MODIFIED off-app. Compared by NORMALISED note id so a path-format mismatch
/// between the save path and the walker can't drop a live note (mirrors the
/// graph's `reconcile_deleted_notes` + `reconcile_added_and_modified_notes`).
///
/// NEW-S-FTS1: modify-while-closed is detected with the SAME mtime signal the
/// graph reconcile uses — the file's disk mtime vs. the mtime stamped into
/// `note_meta` at index time. Backends that can't be stat'ed (SAF `content://`)
/// have no stored mtime, so modify-detection is a no-op there, exactly as on
/// the graph side.
///
/// NEW-S-FTS3: all additions and modifications are collected and written
/// through a single `bulk_upsert` (one BEGIN/COMMIT), so a 500-note P2P sync
/// batch is one fsync instead of 500. A modified note's stale row (which may be
/// stored under a different-but-equivalent path string) is removed first so the
/// re-insert can't leave a duplicate.
///
/// Best-effort throughout: a per-note read/UTF-8/index failure logs and
/// continues. Takes `db` by value and returns it: `SearchDb` wraps a `!Sync`
/// SQLite connection, so a `&SearchDb` held across the `backend.read().await`
/// below would make the (Tauri-required-`Send`) future non-`Send`. Owning the
/// value across the await is fine — `SearchDb` is `Send`.
async fn reconcile_search_against_disk(
    db: SearchDb,
    backend: &dyn RoostFs,
    roost_path: &str,
) -> SearchDb {
    let pairs = match crate::roostfs::walk::list_markdown_path_pairs(backend).await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, "search: warm listing failed; skipping reconcile");
            return db;
        }
    };
    let id_of = |p: &str| crate::db::queries::note_id_from_abs_path(p, roost_path);
    let disk_ids: std::collections::HashSet<String> =
        pairs.iter().map(|(abs, _)| id_of(abs)).collect();

    let fts_paths = match db.all_note_paths() {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "search: could not list FTS paths; skipping reconcile");
            return db;
        }
    };
    let fts_ids: std::collections::HashSet<String> = fts_paths.iter().map(|p| id_of(p)).collect();
    // id → the verbatim path the row is stored under, so a modified note's
    // stale row can be removed even if its stored path string differs from the
    // walker's path form for the same file.
    let fts_path_by_id: HashMap<String, String> =
        fts_paths.iter().map(|p| (id_of(p), p.clone())).collect();
    // id → last-indexed mtime (NEW-S-FTS1). Absent ids (e.g. SAF paths) simply
    // never trigger modify-detection.
    let stored_mtimes: HashMap<String, f64> = db
        .all_note_mtimes()
        .unwrap_or_default()
        .into_iter()
        .map(|(p, m)| (id_of(&p), m))
        .collect();

    // Purge ghosts (deleted off-app).
    let mut purged = 0usize;
    for p in &fts_paths {
        if !disk_ids.contains(&id_of(p)) {
            if let Err(e) = db.remove_note(p) {
                tracing::warn!(error = %e, path = %p, "search: failed to purge FTS ghost");
            } else {
                purged += 1;
            }
        }
    }

    // Collect notes added or modified off-app into one batch.
    let mut batch: Vec<(String, String)> = Vec::new();
    let mut added = 0usize;
    let mut modified = 0usize;
    for (abs, rel) in &pairs {
        let id = id_of(abs);
        let is_added = !fts_ids.contains(&id);
        let is_modified = !is_added
            && match (
                crate::db::queries::file_mtime_secs(abs),
                stored_mtimes.get(&id),
            ) {
                (Some(disk), Some(&stored)) => disk > stored + MODIFIED_MTIME_EPSILON,
                _ => false,
            };
        if !is_added && !is_modified {
            continue;
        }
        match backend.read(rel).await {
            Ok(bytes) => match String::from_utf8(bytes) {
                Ok(content) => {
                    if is_modified {
                        // Drop the stale row first (keyed by whatever path it's
                        // stored under) so the re-insert can't duplicate it.
                        if let Some(old) = fts_path_by_id.get(&id) {
                            if let Err(e) = db.remove_note(old) {
                                tracing::warn!(error = %e, path = %old, "search: failed to drop stale row before re-index");
                            }
                        }
                        modified += 1;
                    } else {
                        added += 1;
                    }
                    batch.push((abs.clone(), content));
                }
                Err(_) => {
                    tracing::warn!(path = %rel, "search: added/modified note not UTF-8; skipped")
                }
            },
            Err(e) => {
                tracing::warn!(error = %e, path = %rel, "search: could not read added/modified note")
            }
        }
    }

    if !batch.is_empty() {
        if let Err(e) = db.bulk_upsert(&batch) {
            tracing::warn!(error = %e, "search: batch re-index of added/modified notes failed");
        }
    }

    if purged > 0 || added > 0 || modified > 0 {
        tracing::info!(purged, added, modified, "search: warm reconcile done");
    }
    db
}

/// Update the FTS5 index for a single note after save.
#[tauri::command]
#[specta::specta]
pub fn index_search(
    path: String,
    content: String,
    state: State<'_, SearchState>,
) -> Result<(), String> {
    let title = Path::new(&path)
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    state.with_db_for_path(&path, |db| db.upsert_note(&path, &title, &content))
}

/// Remove a note from the FTS5 index (on delete).
#[tauri::command]
#[specta::specta]
pub fn remove_from_search(path: String, state: State<'_, SearchState>) -> Result<(), String> {
    state.with_db_for_path(&path, |db| db.remove_note(&path))
}

/// Full-text search over the roost via FTS5 (BM25), ranked with
/// Reciprocal Rank Fusion.
///
/// The embeddings/HNSW semantic lane was removed in the Stage-2 teardown,
/// so this is FTS5-only. RRF degenerates to plain FTS rank order with a
/// single lane, but the helper is kept so the scoring/snippet plumbing
/// stays intact. Falls back to a grep scan if FTS5 isn't warmed yet.
#[tauri::command]
#[specta::specta]
pub async fn search_roost(
    roost_path: String,
    query: String,
    state: State<'_, SearchState>,
    roost_fs: State<'_, RoostFsState>,
) -> Result<Vec<SearchResult>, String> {
    if query.is_empty() {
        return Ok(vec![]);
    }
    let generation = roost_fs.generation();
    let backend = roost_fs
        .current()
        .await
        .ok_or_else(|| "Roost backend not initialized — open a roost first".to_string())?;
    if !super::roost::same_roost_root(backend.root_id().as_str(), Path::new(&roost_path)) {
        return Err("Search request does not match the active roost".to_string());
    }
    let total_started = std::time::Instant::now();

    // ── FTS5 (the only lane) ─────────────────────────────────────────
    let fts_started = std::time::Instant::now();
    let fts_hits: Vec<FtsHit> = run_fts(&query, &state, generation)?;
    let fts_ms = fts_started.elapsed().as_millis();

    let fused = reciprocal_rank_fusion(&fts_hits, 50);
    tracing::debug!(
        ?query,
        fts_ms = fts_ms as u64,
        fts_hits = fts_hits.len(),
        fused = fused.len(),
        "search: fts timing"
    );

    // ── Hydrate to SearchResult: re-scan files for line/col matches ─
    // SRX-4: line positions come from matching each QUERY TERM (not the whole
    // raw query as one literal substring). The old whole-string scan missed a
    // multi-word query like `rust ownership` on a line that only contained
    // `rust`, and returned `matches: []`. Tokenizing and matching any term is
    // far more robust. A purely stemmed FTS hit (e.g. index has `running`,
    // query `run`) still has no literal substring and yields `matches: []` — the
    // FTS snippet still shows the highlight, so the result isn't lost, but exact
    // in-file offsets for stem-only hits remain a follow-up (would need FTS to
    // return byte offsets, which is a schema/type change we avoid here).
    //
    // AUDIT-055: route reads through the RoostFs backend so SAF roosts
    // on Android actually get content (the previous `fs::read_to_string`
    // path silently failed with ENOENT against `content://` URIs and
    // shipped empty `matches[]`). A hit outside the active backend is rejected;
    // raw absolute-path fallback would cross the active-roost boundary.
    let terms = positive_query_terms(&query);
    let mut results = Vec::with_capacity(fused.len());
    for (path, score, match_type, snippet) in fused {
        let content_opt = read_search_hit(backend.as_ref(), &path).await;
        let Some(content) = content_opt else {
            // IP-A5: FTS still indexes this note but its file can't be read —
            // deleted/moved off-app since indexing. Skip so a ghost never
            // surfaces as a result (the warm-start reconcile purges the row;
            // this keeps it hidden until then). A readable file with no literal
            // line match is NOT dropped — that's a legit stemmed FTS hit.
            tracing::debug!(%path, "search: skipping hit with unreadable file (likely deleted off-app)");
            continue;
        };
        let matches = line_matches(&content, &terms);
        let name = Path::new(&path)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        results.push(SearchResult {
            path,
            name,
            snippet,
            matches,
            score,
            match_type,
        });
    }

    // Grep fallback — only while the FTS index isn't yet trustworthy.
    //
    // SRX-6: routes through the RoostFs backend so it works on SAF
    // `content://` roosts too (the old raw `std::fs` walk silently returned
    // nothing on Android). When no backend is registered (legacy/tests) it
    // builds a NativeFs over the roost path.
    //
    // Gated on `index_complete`: once a cold index has finished and the warm
    // reconcile (SRX-1 / NEW-S-FTS1) keeps it fresh, a zero-hit FTS result is
    // authoritative — a full-roost grep would just be wasted I/O. So the
    // fallback fires ONLY in the cold window (index not initialized, or a prior
    // cold index never completed), which is exactly when the user would
    // otherwise see nothing.
    if results.is_empty() && fts_hits.is_empty() {
        let index_trusted = {
            let guard = state.0.lock().map_err(|e| format!("Lock error: {e}"))?;
            guard
                .as_ref()
                .map(|bound| bound.generation == generation && bound.db.index_complete())
                .unwrap_or(false)
        };
        if !index_trusted {
            let grep = grep_search(backend.as_ref(), &query).await?;
            if !roost_fs.is_generation_current(generation) {
                return Err("Roost changed during search".to_string());
            }
            return Ok(grep);
        }
    }

    if !roost_fs.is_generation_current(generation) {
        return Err("Roost changed during search".to_string());
    }

    tracing::info!(
        query_len = query.len(),
        total_ms = total_started.elapsed().as_millis() as u64,
        results = results.len(),
        "search: complete"
    );
    Ok(results)
}

/// Hydrate one FTS hit strictly through the active backend. An absolute hit
/// outside `display_root` is stale/cross-roost state and must never fall back
/// to raw filesystem access.
async fn read_search_hit(backend: &dyn RoostFs, path: &str) -> Option<String> {
    let rel = to_rel(path, backend.display_root())?;
    backend
        .read(&rel)
        .await
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok())
}

// ── Retrieval helpers ───────────────────────────────────────────────

/// FTS5 hit: keeps the FTS5 snippet so the UI gets the `>>>match<<<`
/// markers and we don't lose ranking provenance during fusion.
struct FtsHit {
    path: String,
    snippet: String,
}

fn run_fts(
    query: &str,
    state: &State<'_, SearchState>,
    generation: u64,
) -> Result<Vec<FtsHit>, String> {
    let guard = state.0.lock().map_err(|e| format!("Lock error: {e}"))?;
    let Some(bound) = guard.as_ref() else {
        // Search index not warmed yet. Caller decides fallback.
        return Ok(Vec::new());
    };
    if bound.generation != generation {
        return Ok(Vec::new());
    }
    let matches = bound.db.search(query)?;
    Ok(matches
        .into_iter()
        .map(|m| FtsHit {
            path: m.path,
            snippet: m.snippet,
        })
        .collect())
}

/// Reciprocal Rank Fusion. `k = 60` is the standard parameter from
/// Cormack et al. (TREC 2009). With a single lane (FTS5) this degenerates
/// to plain FTS rank order, but the helper is kept so the scoring/snippet
/// plumbing stays intact if a second lane is ever reintroduced. Returns
/// up to `limit` results sorted by fused score descending. Each tuple is
/// `(path, score, match_type, snippet)`.
fn reciprocal_rank_fusion(fts: &[FtsHit], limit: usize) -> Vec<(String, f64, String, String)> {
    const K: f64 = 60.0;

    let mut score: HashMap<String, f64> = HashMap::new();
    let mut snippet: HashMap<String, String> = HashMap::new();

    for (i, h) in fts.iter().enumerate() {
        *score.entry(h.path.clone()).or_default() += 1.0 / (K + (i + 1) as f64);
        snippet
            .entry(h.path.clone())
            .or_insert_with(|| h.snippet.clone());
    }

    let mut fused: Vec<(String, f64, String, String)> = score
        .into_iter()
        .map(|(path, s)| {
            let snip = snippet.remove(&path).unwrap_or_default();
            (path, s, "fts".to_string(), snip)
        })
        .collect();
    fused.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    fused.truncate(limit);
    fused
}

/// Split a raw query into its positive (non-excluded) search terms, lowercased
/// and de-quoted, for in-file line/col matching (SRX-4). Mirrors the FTS5 query
/// grammar just enough for offset-finding: honours `"quoted phrases"`, drops
/// `-excluded` terms (they must NOT be highlighted), and strips a trailing `*`
/// prefix marker. Terms with no alphanumeric character are dropped.
fn positive_query_terms(raw: &str) -> Vec<String> {
    let mut terms = Vec::new();
    let chars: Vec<char> = raw.chars().collect();
    let mut i = 0;
    let mut pending_not = false;
    while i < chars.len() {
        if chars[i].is_whitespace() {
            i += 1;
            continue;
        }
        let mut negated = false;
        if chars[i] == '-' && i + 1 < chars.len() && !chars[i + 1].is_whitespace() {
            negated = true;
            i += 1;
        }
        let mut text = String::new();
        if i < chars.len() && chars[i] == '"' {
            i += 1;
            while i < chars.len() && chars[i] != '"' {
                text.push(chars[i]);
                i += 1;
            }
            if i < chars.len() {
                i += 1;
            }
            if i < chars.len() && chars[i] == '*' {
                i += 1;
            }
        } else {
            while i < chars.len() && !chars[i].is_whitespace() {
                text.push(chars[i]);
                i += 1;
            }
            if text == "NOT" && !negated {
                pending_not = true;
                continue;
            }
            if text.ends_with('*') {
                text.pop();
            }
        }
        if pending_not {
            negated = true;
            pending_not = false;
        }
        if negated || text.is_empty() || !text.chars().any(char::is_alphanumeric) {
            continue;
        }
        terms.push(text.to_lowercase());
    }
    terms
}

/// Find every occurrence of any `term` (case-insensitive) across `content`,
/// recording 1-based line, trimmed line text, and byte column. Shared by the
/// FTS hydrate path and the grep fallback so both report offsets the same way.
fn line_matches(content: &str, terms: &[String]) -> Vec<SearchMatch> {
    let mut matches = Vec::new();
    if terms.is_empty() {
        return matches;
    }
    for (line_idx, line) in content.lines().enumerate() {
        let line_lower = line.to_lowercase();
        for term in terms {
            let mut search_from = 0;
            while let Some(col) = line_lower[search_from..].find(term.as_str()) {
                matches.push(SearchMatch {
                    line: line_idx + 1,
                    text: line.trim().to_string(),
                    col: search_from + col,
                });
                search_from += col + term.len();
            }
        }
    }
    matches
}

/// Grep-based search — the fallback before FTS5 is warmed. SRX-6: walks and
/// reads through the [`RoostFs`] backend so it works uniformly on desktop and
/// on Android SAF `content://` roosts (the old raw-`std::fs` version returned
/// nothing on SAF). Best-effort: an unreadable / non-UTF-8 note is skipped.
async fn grep_search(backend: &dyn RoostFs, query: &str) -> Result<Vec<SearchResult>, String> {
    let terms = positive_query_terms(query);
    let pairs = crate::roostfs::walk::list_markdown_path_pairs(backend).await?;

    let mut results = Vec::new();
    for (abs, rel) in &pairs {
        let Ok(bytes) = backend.read(rel).await else {
            continue;
        };
        let Ok(content) = String::from_utf8(bytes) else {
            continue;
        };
        let matches = line_matches(&content, &terms);
        if !matches.is_empty() {
            let name = Path::new(abs)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            let match_count = matches.len();
            results.push(SearchResult {
                path: abs.clone(),
                name,
                snippet: String::new(),
                matches,
                // No rank to fuse in the fallback — use raw match-count as a
                // proxy score and label "fts" so the UI badge stays consistent.
                score: match_count as f64,
                match_type: "fts".to_string(),
            });
        }
    }

    results.sort_by_key(|b| std::cmp::Reverse(b.matches.len()));
    Ok(results)
}

#[cfg(test)]
mod tests {
    //! Coverage of the FTS-free retrieval helpers — RRF fusion (pure),
    //! the recursive markdown walk, and the grep fallback (both over a
    //! `tempfile::TempDir`). The async `search_roost` / `init_search`
    //! commands need a managed Tauri State stack + a warmed `SearchDb`,
    //! so they're exercised at the integration tier instead; these tests
    //! lock down the ranking + scanning logic those commands lean on.
    use super::*;
    use crate::roostfs::native::NativeFs;
    use std::fs;
    use tempfile::TempDir;

    fn hit(path: &str, snippet: &str) -> FtsHit {
        FtsHit {
            path: path.to_string(),
            snippet: snippet.to_string(),
        }
    }

    #[tokio::test]
    async fn hydration_rejects_a_hit_outside_the_active_roost() {
        let active = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let inside_note = active.path().join("inside.md");
        let outside_note = outside.path().join("secret.md");
        fs::write(&inside_note, "inside").unwrap();
        fs::write(&outside_note, "outside secret").unwrap();
        let backend = NativeFs::new(active.path());

        assert_eq!(
            read_search_hit(&backend, &inside_note.to_string_lossy()).await,
            Some("inside".to_string())
        );
        assert_eq!(
            read_search_hit(&backend, &outside_note.to_string_lossy()).await,
            None
        );
    }

    #[test]
    fn bound_search_state_rejects_cross_roost_operations() {
        let roost = TempDir::new().unwrap();
        let db_dir = TempDir::new().unwrap();
        let state = SearchState::default();
        state
            .install(
                SearchDb::init(db_dir.path()).unwrap(),
                roost.path().to_string_lossy().to_string(),
                7,
            )
            .unwrap();

        let inside = roost.path().join("inside.md").to_string_lossy().to_string();
        assert!(state.with_db_for_path(&inside, |_| Ok(())).is_ok());

        let outside = roost
            .path()
            .parent()
            .unwrap()
            .join("other-roost")
            .join("outside.md")
            .to_string_lossy()
            .to_string();
        let operation_ran = std::cell::Cell::new(false);
        let error = state
            .with_db_for_path(&outside, |_| {
                operation_ran.set(true);
                Ok(())
            })
            .unwrap_err();
        assert!(error.contains("outside the active roost"));
        assert!(
            !operation_ran.get(),
            "rejected operation must never reach DB"
        );
    }

    // ── reciprocal_rank_fusion ──────────────────────────────────────

    /// Empty input fuses to an empty result, not a panic.
    #[test]
    fn rrf_empty_input_yields_empty() {
        let fused = reciprocal_rank_fusion(&[], 50);
        assert!(fused.is_empty());
    }

    /// With a single lane, RRF preserves FTS rank order: earlier hits
    /// (lower index) get a higher RRF score and sort first. Every result
    /// is labeled `"fts"` and carries its snippet through.
    #[test]
    fn rrf_preserves_fts_order_and_carries_snippet() {
        let hits = vec![
            hit("/v/first.md", ">>>first<<<"),
            hit("/v/second.md", ">>>second<<<"),
            hit("/v/third.md", ">>>third<<<"),
        ];
        let fused = reciprocal_rank_fusion(&hits, 50);
        assert_eq!(fused.len(), 3);
        // Order preserved.
        assert_eq!(fused[0].0, "/v/first.md");
        assert_eq!(fused[1].0, "/v/second.md");
        assert_eq!(fused[2].0, "/v/third.md");
        // Scores strictly descending (1/(K+rank)).
        assert!(fused[0].1 > fused[1].1);
        assert!(fused[1].1 > fused[2].1);
        // Lane label + snippet provenance.
        assert_eq!(fused[0].2, "fts");
        assert_eq!(fused[0].3, ">>>first<<<");
    }

    /// A path that appears at two ranks accumulates both reciprocal
    /// contributions, so it can outrank a once-listed higher hit. Pin
    /// the dedup-and-sum behavior — the whole point of keeping RRF for a
    /// single lane is that the plumbing survives a second lane returning.
    #[test]
    fn rrf_sums_duplicate_paths() {
        // `dup` at ranks 2 and 3; `solo` only at rank 1.
        let hits = vec![
            hit("/v/solo.md", "s"),
            hit("/v/dup.md", "d1"),
            hit("/v/dup.md", "d2"),
        ];
        let fused = reciprocal_rank_fusion(&hits, 50);
        // Two distinct paths after fusion.
        assert_eq!(fused.len(), 2);
        // dup's summed score (1/62 + 1/63) exceeds solo's single 1/61.
        let dup = fused.iter().find(|r| r.0 == "/v/dup.md").unwrap();
        let solo = fused.iter().find(|r| r.0 == "/v/solo.md").unwrap();
        assert!(
            dup.1 > solo.1,
            "a path surfaced twice should outrank a path surfaced once"
        );
        // First-seen snippet wins (insert-if-absent).
        assert_eq!(dup.3, "d1");
    }

    /// The `limit` truncates after sorting, keeping the top-scored
    /// results, not an arbitrary slice of input order.
    #[test]
    fn rrf_truncates_to_limit() {
        let hits: Vec<FtsHit> = (0..10).map(|i| hit(&format!("/v/{i}.md"), "")).collect();
        let fused = reciprocal_rank_fusion(&hits, 3);
        assert_eq!(fused.len(), 3);
        // The 3 kept are the first-3 ranks (highest RRF scores).
        assert_eq!(fused[0].0, "/v/0.md");
        assert_eq!(fused[2].0, "/v/2.md");
    }

    // ── SRX-4: positive_query_terms + line_matches ──────────────────

    /// The query tokenizer drops exclusions and quote/prefix punctuation, so
    /// only positive, lowercased terms remain for offset-finding.
    #[test]
    fn positive_query_terms_drops_exclusions_and_punctuation() {
        assert_eq!(
            positive_query_terms("Rust Ownership"),
            vec!["rust", "ownership"]
        );
        assert_eq!(
            positive_query_terms("\"borrow checker\""),
            vec!["borrow checker"]
        );
        assert_eq!(positive_query_terms("rust*"), vec!["rust"]);
        // Excluded terms are not highlightable.
        assert_eq!(positive_query_terms("borrow -dragon"), vec!["borrow"]);
        assert_eq!(positive_query_terms("foo NOT bar"), vec!["foo"]);
        assert!(positive_query_terms("  !?.,  ").is_empty());
        assert!(positive_query_terms("\"").is_empty());
    }

    /// SRX-4 regression: a multi-word query matches a line that contains only
    /// ONE of its terms — the old whole-string scan returned `matches: []`.
    #[test]
    fn line_matches_finds_any_term_not_the_whole_query() {
        let content = "the rust language\nunrelated line\n";
        let terms = positive_query_terms("rust ownership");
        let m = line_matches(content, &terms);
        assert_eq!(m.len(), 1, "the line with only `rust` still matches");
        assert_eq!(m[0].line, 1);
        assert_eq!(m[0].col, 4);
    }

    /// Multiple hits of a term on one line each get their own entry, and the
    /// column is the byte offset within the raw (untrimmed) line.
    #[test]
    fn line_matches_records_each_hit_case_insensitive() {
        let content = "first line\n    Hello hello WORLD\n";
        let m = line_matches(content, &positive_query_terms("hello"));
        assert_eq!(m.len(), 2);
        assert_eq!(m[0].line, 2);
        assert_eq!(m[0].col, 4, "col is the byte offset within the raw line");
        assert_eq!(m[0].text, "Hello hello WORLD", "stored text is trimmed");
    }

    /// No terms (e.g. a pure-punctuation query) yields no matches, not a panic.
    #[test]
    fn line_matches_empty_terms_yields_empty() {
        assert!(line_matches("some content here", &[]).is_empty());
    }

    // ── SRX-6: grep fallback over the RoostFs backend ───────────────

    /// Grep fallback finds case-insensitive matches through the RoostFs
    /// backend and records 1-based line, byte column, and trimmed text.
    #[tokio::test]
    async fn grep_records_line_col_case_insensitive() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("a.md"),
            "first line\n    Hello World\nthird line\n",
        )
        .unwrap();

        let backend = NativeFs::new(dir.path());
        let results = grep_search(&backend, "hello").await.unwrap();
        assert_eq!(results.len(), 1);
        let m = &results[0].matches;
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].line, 2, "line is 1-based");
        assert_eq!(m[0].col, 4, "col is the byte offset within the raw line");
        assert_eq!(m[0].text, "Hello World", "stored text is trimmed");
        assert_eq!(results[0].match_type, "fts");
    }

    /// Results are ordered by descending match count so the densest hit
    /// surfaces first, and hidden/dot files are skipped by the walker.
    #[tokio::test]
    async fn grep_orders_by_match_count_and_skips_hidden() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("few.md"), "x\n").unwrap();
        fs::write(dir.path().join("many.md"), "x x x\n").unwrap();
        // A hidden dir + file the RoostFs walker must skip.
        fs::create_dir_all(dir.path().join(".corvux")).unwrap();
        fs::write(dir.path().join(".corvux").join("secret.md"), "x x x x").unwrap();

        let backend = NativeFs::new(dir.path());
        let results = grep_search(&backend, "x").await.unwrap();
        assert_eq!(results.len(), 2, "hidden note is skipped");
        assert!(results[0].name.starts_with("many"));
        assert!(results[1].name.starts_with("few"));
        assert!(!results.iter().any(|r| r.name.contains("secret")));
    }

    /// A query that matches nothing returns no results (not an error).
    #[tokio::test]
    async fn grep_no_match_returns_empty() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("a.md"), "nothing here\n").unwrap();
        let backend = NativeFs::new(dir.path());
        let results = grep_search(&backend, "absent").await.unwrap();
        assert!(results.is_empty());
    }

    /// Pointing grep at a non-existent roost errors rather than panicking.
    #[tokio::test]
    async fn grep_errors_on_missing_roost() {
        let backend = NativeFs::new(std::path::Path::new("/no/such/dir/anywhere"));
        assert!(grep_search(&backend, "q").await.is_err());
    }

    // ── NEW-S-FTS1: warm reconcile re-indexes a modify-while-closed ─

    /// A note edited while the app was closed (disk mtime advanced past the
    /// stored index mtime) is re-indexed with fresh content on the next warm
    /// reconcile — search returns the new body, not the stale one.
    #[tokio::test]
    async fn reconcile_reindexes_note_modified_while_closed() {
        use crate::db::search_db::SearchDb;

        let roost = TempDir::new().unwrap();
        let root = roost.path().to_string_lossy().to_string();
        let note = roost.path().join("a.md");
        fs::write(&note, "old body content").unwrap();
        let abs = note.to_string_lossy().to_string();

        let db_dir = TempDir::new().unwrap();
        let db = SearchDb::init(db_dir.path()).unwrap();
        db.upsert_note(&abs, "a", "old body content").unwrap();
        // Simulate having indexed it long ago so the disk mtime is clearly newer.
        db.set_note_mtime_for_test(&abs, 0.0).unwrap();

        // Edit the file "while closed".
        fs::write(&note, "fresh body content").unwrap();

        let backend = NativeFs::new(roost.path());
        let db = reconcile_search_against_disk(db, &backend, &root).await;

        assert!(
            !db.search("fresh").unwrap().is_empty(),
            "modified note re-indexed with fresh content"
        );
        assert!(
            db.search("old").unwrap().is_empty(),
            "stale content dropped — no duplicate row left behind"
        );
        assert_eq!(
            db.note_count().unwrap(),
            1,
            "exactly one row, not a duplicate"
        );
    }

    /// An unchanged note (disk mtime within the epsilon of the stored mtime) is
    /// NOT re-indexed — the warm fast path stays cheap.
    #[tokio::test]
    async fn reconcile_leaves_unchanged_note_alone() {
        use crate::db::search_db::SearchDb;

        let roost = TempDir::new().unwrap();
        let root = roost.path().to_string_lossy().to_string();
        let note = roost.path().join("a.md");
        fs::write(&note, "stable body").unwrap();
        let abs = note.to_string_lossy().to_string();

        let db_dir = TempDir::new().unwrap();
        let db = SearchDb::init(db_dir.path()).unwrap();
        db.upsert_note(&abs, "a", "stable body").unwrap();
        // upsert stamped the real file mtime, so no modify is detected.

        let backend = NativeFs::new(roost.path());
        let db = reconcile_search_against_disk(db, &backend, &root).await;
        assert_eq!(db.note_count().unwrap(), 1);
        assert!(!db.search("stable").unwrap().is_empty());
    }
}
