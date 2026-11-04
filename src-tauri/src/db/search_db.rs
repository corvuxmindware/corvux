use sqlite::{Connection, State};
use std::fs;
use std::path::Path;

/// SQLite-backed full-text search index.
/// Tries FTS5 first, falls back to a regular table with LIKE search.
pub struct SearchDb {
    conn: Connection,
    has_fts5: bool,
}

impl SearchDb {
    /// Open (or create) `search.db` inside `db_dir`. The caller is
    /// responsible for picking a stable, device-local directory (see
    /// `crate::paths::roost_db_dir`).
    ///
    /// SRX-2: if the file turns out to be corrupt, self-heal — `search.db` is
    /// a pure derived cache (rebuildable from disk on the next cold index), so
    /// unlike `graph.db` it's safe to delete-and-recreate automatically rather
    /// than surfacing a Repair prompt. Only the conservative `looks_corrupt`
    /// signatures trigger this, so a transient/permission error is NOT mistaken
    /// for corruption and never nukes a healthy index.
    pub fn init(db_dir: &Path) -> Result<Self, String> {
        fs::create_dir_all(db_dir)
            .map_err(|e| format!("Failed to create DB directory {}: {e}", db_dir.display()))?;
        let db_path = db_dir.join("search.db");

        match Self::open_prepared(&db_path) {
            Ok(db) => Ok(db),
            Err(e) if crate::db::schema::looks_corrupt(&e) => {
                tracing::warn!(error = %e, "search: search.db looks corrupt — deleting and rebuilding");
                for suffix in ["", "-wal", "-shm"] {
                    let _ = fs::remove_file(db_dir.join(format!("search.db{suffix}")));
                }
                Self::open_prepared(&db_path)
            }
            Err(e) => Err(e),
        }
    }

    /// Open the connection and (idempotently) create the schema. Split out so
    /// `init` can retry it once after clearing a corrupt file.
    fn open_prepared(db_path: &Path) -> Result<Self, String> {
        tracing::info!(path = %db_path.display(), "search: opening DB");
        let conn = sqlite::open(db_path.to_string_lossy().as_ref())
            .map_err(|e| format!("Failed to open search DB: {e}"))?;

        // WAL mode for better concurrent read performance
        conn.execute("PRAGMA journal_mode=WAL;")
            .map_err(|e| format!("Failed to set WAL mode: {e}"))?;

        // SRX-3: key/value metadata that survives across sessions — home of the
        // `index_complete` marker (mirrors the graph's `kv_meta`).
        conn.execute(
            "CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);",
        )
        .map_err(|e| format!("Failed to create meta table: {e}"))?;

        // NEW-S-FTS1: per-note last-indexed mtime, mirroring the graph's
        // stored `updated_at`. The FTS5 virtual table can't carry an extra
        // stat-able column, so mtimes live in this side table (keyed by the
        // same verbatim path the index stores). The warm-start reconcile
        // compares disk mtime against this to detect notes edited while the
        // app was closed and re-index them into FTS — without it, an external
        // edit left backlinks/tags fresh (the graph heals mtime) but search
        // content stale. Rows with an unstat-able path (SAF `content://`) are
        // simply absent, so modify-detection is a no-op there (same as graph).
        conn.execute(
            "CREATE TABLE IF NOT EXISTS note_meta (path TEXT PRIMARY KEY, mtime REAL NOT NULL);",
        )
        .map_err(|e| format!("Failed to create note_meta table: {e}"))?;

        // Try FTS5 first — not all SQLite builds include it.
        //
        // AUDIT-068: `porter unicode61` stems English only — Telugu /
        // Hindi / other non-Latin content falls back to substring match
        // against the unicode61-tokenized form (still works, just no
        // morphological folding like plurals/tense). Revisit only if a
        // user reports a missed hit on a non-English query. Switching
        // to a per-language stemmer would need either a runtime locale
        // detect or splitting content across multiple FTS tables.
        let has_fts5 = conn
            .execute(
                "CREATE VIRTUAL TABLE IF NOT EXISTS notes_fts USING fts5(
                    path UNINDEXED,
                    title,
                    content,
                    tokenize = 'porter unicode61'
                );",
            )
            .is_ok();

        if has_fts5 {
            tracing::info!("search: FTS5 available — using full-text index");
        } else {
            tracing::warn!(
                "search: FTS5 not available — falling back to LIKE search. \
                 Bundled SQLite was compiled without FTS5; set SQLITE_ENABLE_FTS5=1 \
                 in src-tauri/.cargo/config.toml and `cargo clean -p sqlite3-src` \
                 to rebuild with FTS5."
            );
            // Fallback: regular table with content for LIKE search
            conn.execute(
                "CREATE TABLE IF NOT EXISTS notes_content (
                    path TEXT PRIMARY KEY,
                    title TEXT NOT NULL,
                    content TEXT NOT NULL
                );",
            )
            .map_err(|e| format!("Failed to create fallback table: {e}"))?;
        }

        Ok(Self { conn, has_fts5 })
    }

    /// Read a `meta` value by key, or `None` if absent/unreadable.
    fn get_meta(&self, key: &str) -> Option<String> {
        let mut stmt = self
            .conn
            .prepare("SELECT value FROM meta WHERE key = ?")
            .ok()?;
        stmt.bind((1, key)).ok()?;
        if let Ok(State::Row) = stmt.next() {
            stmt.read::<String, _>(0).ok()
        } else {
            None
        }
    }

    fn set_meta(&self, key: &str, value: &str) -> Result<(), String> {
        let mut stmt = self
            .conn
            .prepare("INSERT OR REPLACE INTO meta (key, value) VALUES (?, ?)")
            .map_err(|e| format!("meta set prepare: {e}"))?;
        stmt.bind((1, key))
            .map_err(|e| format!("meta bind key: {e}"))?;
        stmt.bind((2, value))
            .map_err(|e| format!("meta bind value: {e}"))?;
        stmt.next().map_err(|e| format!("meta set exec: {e}"))?;
        Ok(())
    }

    /// SRX-3: has a cold index previously completed? The warm-start fast path
    /// trusts the persisted index only when this is set. A crash mid-cold-index
    /// leaves rows but no marker, so the next launch re-indexes instead of
    /// serving a partial index forever (the torn-init bug IP-5 fixed for the
    /// graph).
    pub fn index_complete(&self) -> bool {
        self.get_meta("index_complete").as_deref() == Some("1")
    }

    /// Mark the index complete — call ONLY after a full cold index commits.
    pub fn set_index_complete(&self) -> Result<(), String> {
        self.set_meta("index_complete", "1")
    }

    /// Clear the completion marker before a rebuild so a crash mid-rebuild
    /// can't leave a stale "complete" flag over a half-cleared index.
    pub fn clear_index_complete(&self) -> Result<(), String> {
        self.set_meta("index_complete", "0")
    }

    /// SRX-5: index many notes in ONE transaction. Collapses N implicit
    /// auto-commits (an fsync each) into a single commit, and makes the cold
    /// index atomic — either every note lands (and the caller sets the
    /// completion marker) or a crash rolls back to empty and the next launch
    /// re-indexes. `files` is `(abs_path, content)`; the title is derived here.
    pub fn bulk_upsert(&self, files: &[(String, String)]) -> Result<(), String> {
        self.conn
            .execute("BEGIN")
            .map_err(|e| format!("bulk begin: {e}"))?;
        for (path, content) in files {
            let title = Path::new(path)
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();
            if let Err(e) = self.upsert_note(path, &title, content) {
                let _ = self.conn.execute("ROLLBACK");
                return Err(e);
            }
        }
        self.conn
            .execute("COMMIT")
            .map_err(|e| format!("bulk commit: {e}"))?;
        Ok(())
    }

    /// Clear every row from the active index table. Used by `init_search`
    /// to start fresh before the RoostFs walker repopulates. Index-roost
    /// scanning used to live here but now lives in the command layer so
    /// it can reach the same RoostFs backend the graph indexer walks.
    pub fn clear_index(&self) -> Result<(), String> {
        let table = if self.has_fts5 {
            "notes_fts"
        } else {
            "notes_content"
        };
        self.conn
            .execute(format!("DELETE FROM {table}"))
            .map_err(|e| format!("Failed to clear {table}: {e}"))?;
        // NEW-S-FTS1: keep the mtime side table in lockstep with the index.
        self.conn
            .execute("DELETE FROM note_meta")
            .map_err(|e| format!("Failed to clear note_meta: {e}"))
    }

    /// Insert or replace a single note in the search index.
    ///
    /// Also stamps the note's current on-disk mtime into `note_meta`
    /// (NEW-S-FTS1) so the warm-start reconcile can later spot an edit made
    /// while the app was closed. Mirrors what the graph does in
    /// `index_note_pass1` (`updated_at = file_mtime_secs`).
    pub fn upsert_note(&self, path: &str, title: &str, content: &str) -> Result<(), String> {
        if self.has_fts5 {
            self.upsert_fts5(path, title, content)?;
        } else {
            self.upsert_plain(path, title, content)?;
        }
        self.record_mtime(path)
    }

    /// Stamp `path`'s current filesystem mtime into `note_meta`. No-op (not an
    /// error) when the path can't be stat'ed — SAF `content://` roosts return
    /// `None`, exactly like the graph's mtime probe, so modify-while-closed
    /// detection is simply skipped there rather than storing a bogus value.
    fn record_mtime(&self, path: &str) -> Result<(), String> {
        let Some(mtime) = crate::db::queries::file_mtime_secs(path) else {
            return Ok(());
        };
        let mut stmt = self
            .conn
            .prepare("INSERT OR REPLACE INTO note_meta (path, mtime) VALUES (?, ?)")
            .map_err(|e| format!("note_meta upsert prepare: {e}"))?;
        stmt.bind((1, path))
            .map_err(|e| format!("note_meta bind path: {e}"))?;
        stmt.bind((2, mtime))
            .map_err(|e| format!("note_meta bind mtime: {e}"))?;
        stmt.next()
            .map_err(|e| format!("note_meta upsert exec: {e}"))?;
        Ok(())
    }

    fn upsert_fts5(&self, path: &str, title: &str, content: &str) -> Result<(), String> {
        let mut del = self
            .conn
            .prepare("DELETE FROM notes_fts WHERE path = ?")
            .map_err(|e| format!("FTS5 delete prepare: {e}"))?;
        del.bind((1, path))
            .map_err(|e| format!("FTS5 delete bind: {e}"))?;
        del.next().map_err(|e| format!("FTS5 delete exec: {e}"))?;

        let mut ins = self
            .conn
            .prepare("INSERT INTO notes_fts (path, title, content) VALUES (?, ?, ?)")
            .map_err(|e| format!("FTS5 insert prepare: {e}"))?;
        ins.bind((1, path))
            .map_err(|e| format!("FTS5 bind 1: {e}"))?;
        ins.bind((2, title))
            .map_err(|e| format!("FTS5 bind 2: {e}"))?;
        ins.bind((3, content))
            .map_err(|e| format!("FTS5 bind 3: {e}"))?;
        ins.next().map_err(|e| format!("FTS5 insert exec: {e}"))?;
        Ok(())
    }

    fn upsert_plain(&self, path: &str, title: &str, content: &str) -> Result<(), String> {
        let mut stmt = self
            .conn
            .prepare("INSERT OR REPLACE INTO notes_content (path, title, content) VALUES (?, ?, ?)")
            .map_err(|e| format!("Plain insert prepare: {e}"))?;
        stmt.bind((1, path))
            .map_err(|e| format!("Plain bind 1: {e}"))?;
        stmt.bind((2, title))
            .map_err(|e| format!("Plain bind 2: {e}"))?;
        stmt.bind((3, content))
            .map_err(|e| format!("Plain bind 3: {e}"))?;
        stmt.next().map_err(|e| format!("Plain insert exec: {e}"))?;
        Ok(())
    }

    /// Return the number of indexed notes. Used to detect whether the DB
    /// already has data from a previous session so `init_search` can skip
    /// the expensive full roost re-scan.
    pub fn note_count(&self) -> Result<usize, String> {
        let table = if self.has_fts5 {
            "notes_fts"
        } else {
            "notes_content"
        };
        let mut stmt = self
            .conn
            .prepare(format!("SELECT COUNT(*) FROM {table}"))
            .map_err(|e| format!("Count prepare: {e}"))?;
        if let Ok(sqlite::State::Row) = stmt.next() {
            Ok(stmt.read::<i64, _>(0).unwrap_or(0) as usize)
        } else {
            Ok(0)
        }
    }

    /// Every indexed note path, verbatim as stored. The warm-start reconcile
    /// (`commands::search::init_search`) normalises these to note ids to find
    /// rows whose file was deleted off-app and purges them — mirroring the
    /// graph side's `reconcile_deleted_notes` so a deleted note doesn't keep
    /// surfacing as a search hit (IP-A5).
    pub fn all_note_paths(&self) -> Result<Vec<String>, String> {
        let table = if self.has_fts5 {
            "notes_fts"
        } else {
            "notes_content"
        };
        let mut stmt = self
            .conn
            .prepare(format!("SELECT path FROM {table}"))
            .map_err(|e| format!("all_note_paths prepare: {e}"))?;
        let mut paths = Vec::new();
        while let Ok(State::Row) = stmt.next() {
            if let Ok(p) = stmt.read::<String, _>(0) {
                paths.push(p);
            }
        }
        Ok(paths)
    }

    /// The last-indexed verbatim content for `path`, or `None` if the note
    /// isn't in the index yet. This is the "before" snapshot for external-edit
    /// history: the watcher only ever sees the post-write file, so the prior
    /// state has to come from what search last stored. It lags reality if the
    /// note was edited externally since the last index — which is exactly why
    /// `record_external_edit` re-`upsert`s the new content right after reading
    /// this, so the baseline advances and the NEXT external edit diffs against
    /// the right point.
    ///
    /// PERF: on the FTS5 path `path` is UNINDEXED, so `WHERE path = ?` is an
    /// O(rows) scan (the `notes_content` fallback path IS PK-indexed). Same
    /// pattern as `remove_note`, but this runs per external write, so a chatty
    /// MCP agent / sync storm pays it repeatedly under the search lock. Fine at
    /// today's roost sizes; if it shows up in a profile, back it with a
    /// path→content shadow table (or a rowid map) rather than scanning FTS5.
    pub fn content_for(&self, path: &str) -> Result<Option<String>, String> {
        // `table` is one of two string literals chosen by the internal
        // `has_fts5` bool — never user input — so the format! interpolation
        // below carries no injection risk. Keep it that way.
        let table = if self.has_fts5 {
            "notes_fts"
        } else {
            "notes_content"
        };
        let mut stmt = self
            .conn
            .prepare(format!("SELECT content FROM {table} WHERE path = ?"))
            .map_err(|e| format!("content_for prepare: {e}"))?;
        stmt.bind((1, path))
            .map_err(|e| format!("content_for bind: {e}"))?;
        if let Ok(State::Row) = stmt.next() {
            Ok(stmt.read::<String, _>(0).ok())
        } else {
            Ok(None)
        }
    }

    /// Remove a note from the search index.
    pub fn remove_note(&self, path: &str) -> Result<(), String> {
        let table = if self.has_fts5 {
            "notes_fts"
        } else {
            "notes_content"
        };
        let mut stmt = self
            .conn
            .prepare(format!("DELETE FROM {table} WHERE path = ?"))
            .map_err(|e| format!("Delete prepare: {e}"))?;
        stmt.bind((1, path))
            .map_err(|e| format!("Delete bind: {e}"))?;
        stmt.next().map_err(|e| format!("Delete exec: {e}"))?;
        // NEW-S-FTS1: drop the paired mtime row so a re-created note doesn't
        // inherit a stale timestamp.
        let mut mstmt = self
            .conn
            .prepare("DELETE FROM note_meta WHERE path = ?")
            .map_err(|e| format!("note_meta delete prepare: {e}"))?;
        mstmt
            .bind((1, path))
            .map_err(|e| format!("note_meta delete bind: {e}"))?;
        mstmt
            .next()
            .map_err(|e| format!("note_meta delete exec: {e}"))?;
        Ok(())
    }

    /// Every indexed note's stored mtime, as `(path, mtime)` pairs — the
    /// NEW-S-FTS1 counterpart to `all_note_paths`. The warm-start reconcile
    /// normalises these paths to note ids and compares each against the live
    /// disk mtime to re-index notes edited while the app was closed.
    pub fn all_note_mtimes(&self) -> Result<Vec<(String, f64)>, String> {
        let mut stmt = self
            .conn
            .prepare("SELECT path, mtime FROM note_meta")
            .map_err(|e| format!("all_note_mtimes prepare: {e}"))?;
        let mut out = Vec::new();
        while let Ok(State::Row) = stmt.next() {
            if let (Ok(p), Ok(m)) = (stmt.read::<String, _>(0), stmt.read::<f64, _>(1)) {
                out.push((p, m));
            }
        }
        Ok(out)
    }

    /// Test-only: force a note's stored mtime, so a reconcile test can simulate
    /// "this note was indexed a while ago" without waiting out the real
    /// `MODIFIED_MTIME_EPSILON` window or manipulating filesystem timestamps.
    #[cfg(test)]
    pub(crate) fn set_note_mtime_for_test(&self, path: &str, mtime: f64) -> Result<(), String> {
        let mut stmt = self
            .conn
            .prepare("INSERT OR REPLACE INTO note_meta (path, mtime) VALUES (?, ?)")
            .map_err(|e| format!("test set mtime prepare: {e}"))?;
        stmt.bind((1, path)).map_err(|e| e.to_string())?;
        stmt.bind((2, mtime)).map_err(|e| e.to_string())?;
        stmt.next().map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Search for notes matching the query.
    pub fn search(&self, query: &str) -> Result<Vec<FtsMatch>, String> {
        if query.trim().is_empty() {
            return Ok(vec![]);
        }

        if self.has_fts5 {
            self.search_fts5(query)
        } else {
            self.search_plain(query)
        }
    }

    /// FTS5 search with BM25 ranking and snippet extraction.
    ///
    /// - SRX-7: the user query is parsed into a real FTS5 MATCH expression that
    ///   honours `"quoted phrases"`, `-exclusion` / `NOT`, and `prefix*`, while
    ///   every term is quote-escaped so a stray `"` can never break the MATCH
    ///   (see [`build_fts5_query`]). The assembled grammar goes in via a bound
    ///   parameter — no raw user string is ever concatenated into SQL.
    /// - SRX-8: ranked by `bm25()` with the title column weighted far above the
    ///   body, so a title hit outranks a body-only hit.
    /// - NEW-S-FTS2: the snippet prefers the column the match landed in — a
    ///   title-only hit falls back to the title snippet (then the raw title)
    ///   instead of returning a marker-less slice of unrelated body text.
    fn search_fts5(&self, query: &str) -> Result<Vec<FtsMatch>, String> {
        let match_query = build_fts5_query(query);
        if match_query.is_empty() {
            return Ok(vec![]);
        }

        // Column order in the CREATE VIRTUAL TABLE is `path (UNINDEXED),
        // title, content`. bm25() takes one weight per column, positionally,
        // including the UNINDEXED one (which never contributes tokens, so its
        // weight is inert — kept at 0.0 for clarity).
        let mut stmt = self
            .conn
            .prepare(
                "SELECT path, title,
                        snippet(notes_fts, 1, '>>>', '<<<', '...', 16) AS title_snip,
                        snippet(notes_fts, 2, '>>>', '<<<', '...', 48) AS content_snip
                 FROM notes_fts
                 WHERE notes_fts MATCH ?
                 ORDER BY bm25(notes_fts, 0.0, 10.0, 1.0)
                 LIMIT 50",
            )
            .map_err(|e| format!("FTS5 search prepare: {e}"))?;

        stmt.bind((1, match_query.as_str()))
            .map_err(|e| format!("FTS5 search bind: {e}"))?;

        let mut results = Vec::new();
        while let Ok(State::Row) = stmt.next() {
            let title = stmt.read::<String, _>("title").unwrap_or_default();
            let title_snip = stmt.read::<String, _>("title_snip").unwrap_or_default();
            let content_snip = stmt.read::<String, _>("content_snip").unwrap_or_default();
            // Prefer the snippet that actually contains a highlight marker.
            // Content wins when both match; a title-only hit falls back to the
            // title snippet, then the bare title, so it's never an empty or
            // misleading body slice.
            let snippet = if content_snip.contains(">>>") {
                content_snip
            } else if title_snip.contains(">>>") {
                title_snip
            } else if !title.is_empty() {
                title
            } else {
                content_snip
            };
            results.push(FtsMatch {
                path: stmt.read::<String, _>("path").unwrap_or_default(),
                snippet,
            });
        }
        Ok(results)
    }

    /// Fallback: LIKE-based search on the plain content table.
    ///
    /// NEW-S-FTS2: when the term is only in the title (not the body), the
    /// content substring window would return a leading body slice unrelated to
    /// the match, so the snippet falls back to the title in that case.
    fn search_plain(&self, query: &str) -> Result<Vec<FtsMatch>, String> {
        let pattern = format!("%{}%", query.replace('%', "\\%").replace('_', "\\_"));

        let mut stmt = self
            .conn
            .prepare(
                "SELECT path, title,
                    CASE WHEN instr(lower(content), lower(?)) > 0
                         THEN substr(content,
                                max(1, instr(lower(content), lower(?)) - 40),
                                120)
                         ELSE title
                    END AS snip
                 FROM notes_content
                 WHERE lower(content) LIKE lower(?) ESCAPE '\\'
                    OR lower(title) LIKE lower(?) ESCAPE '\\'
                 LIMIT 50",
            )
            .map_err(|e| format!("Plain search prepare: {e}"))?;

        stmt.bind((1, query))
            .map_err(|e| format!("Plain bind 1: {e}"))?;
        stmt.bind((2, query))
            .map_err(|e| format!("Plain bind 2: {e}"))?;
        stmt.bind((3, pattern.as_str()))
            .map_err(|e| format!("Plain bind 3: {e}"))?;
        stmt.bind((4, pattern.as_str()))
            .map_err(|e| format!("Plain bind 4: {e}"))?;

        let mut results = Vec::new();
        while let Ok(State::Row) = stmt.next() {
            results.push(FtsMatch {
                path: stmt.read::<String, _>("path").unwrap_or_default(),
                snippet: stmt.read::<String, _>("snip").unwrap_or_default(),
            });
        }
        Ok(results)
    }

    /// DB-5: opportunistic maintenance. FTS5 accumulates b-tree segment
    /// tombstones on every delete/update (each `upsert_note` is a
    /// DELETE+INSERT), and `search.db` never shrinks after mass deletes.
    /// This merges the FTS5 index (`'optimize'`, reclaiming tombstones),
    /// refreshes the query planner stats (`PRAGMA optimize`), then `VACUUM`s
    /// to return freed pages to the OS.
    ///
    /// Safe by construction: `SearchDb` owns its single SQLite connection and
    /// the caller holds the `SearchState` mutex for the duration, so no
    /// concurrent upsert/search runs on this connection while VACUUM (which
    /// needs no other open statement) executes. Runs off the hot path — see
    /// the scheduler in `commands::search` (once per launch, gated to ~daily).
    pub fn run_maintenance(&self) -> Result<(), String> {
        if self.has_fts5 {
            self.conn
                .execute("INSERT INTO notes_fts(notes_fts) VALUES('optimize');")
                .map_err(|e| format!("FTS5 optimize failed: {e}"))?;
        }
        self.conn
            .execute("PRAGMA optimize;")
            .map_err(|e| format!("PRAGMA optimize failed: {e}"))?;
        self.conn
            .execute("VACUUM;")
            .map_err(|e| format!("VACUUM failed: {e}"))?;
        Ok(())
    }

    /// DB-5: has it been at least `MAINTENANCE_INTERVAL_SECS` since the last
    /// `run_maintenance`? A missing marker (fresh DB) → due. Persisted in the
    /// `meta` table so the cadence survives restarts and multiple roost opens
    /// in one session don't each trigger a VACUUM.
    pub fn maintenance_due(&self, now_unix: i64) -> bool {
        match self
            .get_meta("last_maintenance")
            .and_then(|s| s.parse::<i64>().ok())
        {
            Some(prev) => now_unix.saturating_sub(prev) >= MAINTENANCE_INTERVAL_SECS,
            None => true,
        }
    }

    /// DB-5: stamp the last-maintenance time (unix seconds).
    pub fn mark_maintenance_done(&self, now_unix: i64) -> Result<(), String> {
        self.set_meta("last_maintenance", &now_unix.to_string())
    }
}

/// DB-5: run opportunistic maintenance at most this often (24h). Keeps the
/// VACUUM off the common path when a user opens/switches roosts repeatedly.
pub const MAINTENANCE_INTERVAL_SECS: i64 = 24 * 60 * 60;

/// SRX-7: turn a raw user query into a safe FTS5 MATCH expression.
///
/// Supported grammar, mapped onto FTS5's own operators:
///   * `"quoted phrase"` → an FTS5 phrase (adjacent tokens in order).
///   * `-term` or `NOT term` → excluded via the FTS5 `NOT` operator.
///   * `term*` (or `"phrase"*`) → an FTS5 prefix query.
///   * bare words → AND-ed together (all must appear).
///
/// Every term is emitted as a double-quoted FTS5 string with any embedded `"`
/// doubled, so a stray quote in the input can never break out of the string and
/// corrupt the MATCH grammar (a lone `"` collapses to an empty term and is
/// dropped). The result is bound as a parameter by the caller — this only
/// assembles the FTS5 query language, never raw SQL.
///
/// Returns an empty string when there's nothing searchable (blank input, pure
/// punctuation, or only exclusions — FTS5 has no "everything except X" form, so
/// a query with no positive terms yields no results rather than an error).
fn build_fts5_query(raw: &str) -> String {
    /// Wrap a term as an FTS5 string literal, escaping embedded quotes.
    fn quote(term: &str) -> String {
        format!("\"{}\"", term.replace('"', "\"\""))
    }

    let mut positives: Vec<String> = Vec::new();
    let mut negatives: Vec<String> = Vec::new();
    // Set by a standalone `NOT` token; forces the *next* term to be excluded.
    let mut pending_not = false;

    let chars: Vec<char> = raw.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        // Skip whitespace between tokens.
        if chars[i].is_whitespace() {
            i += 1;
            continue;
        }

        // A leading '-' marks an exclusion (FTS5 `NOT`) — only when it's
        // immediately followed by more of the token, not a lone dash.
        let mut negated = false;
        if chars[i] == '-' && i + 1 < chars.len() && !chars[i + 1].is_whitespace() {
            negated = true;
            i += 1;
        }

        // Collect the raw term text, honouring a `"quoted phrase"`.
        let mut text = String::new();
        let mut prefix = false;
        if i < chars.len() && chars[i] == '"' {
            i += 1; // consume opening quote
            while i < chars.len() && chars[i] != '"' {
                text.push(chars[i]);
                i += 1;
            }
            if i < chars.len() {
                i += 1; // consume closing quote
            }
            // A `*` right after the closing quote makes it a prefix query.
            if i < chars.len() && chars[i] == '*' {
                prefix = true;
                i += 1;
            }
        } else {
            while i < chars.len() && !chars[i].is_whitespace() {
                text.push(chars[i]);
                i += 1;
            }
            // Bareword `NOT` (uppercase) excludes the FOLLOWING term. Consume
            // it as a connector and remember to negate the next token.
            if text == "NOT" && !negated {
                pending_not = true;
                continue;
            }
            // A trailing `*` on a bareword is a prefix query.
            if text.ends_with('*') {
                prefix = true;
                text.pop();
            }
        }

        // Fold in a pending standalone-`NOT` from the previous token.
        if pending_not {
            negated = true;
            pending_not = false;
        }

        if text.is_empty() {
            continue; // lone quote, lone dash, or empty phrase → drop
        }
        // Drop terms with no searchable character. The unicode61 tokenizer
        // ignores punctuation, so a pure-symbol term (`!?.,`) can never match
        // anyway, and feeding an all-punctuation phrase into MATCH risks a
        // picky FTS5 syntax error. `is_alphanumeric` is Unicode-aware, so
        // Telugu/Hindi/CJK terms are kept.
        if !text.chars().any(char::is_alphanumeric) {
            continue;
        }

        let mut term = quote(&text);
        if prefix {
            term.push('*');
        }
        if negated {
            negatives.push(term);
        } else {
            positives.push(term);
        }
    }

    // FTS5 can't express a pure-negative query ("everything but X"), so with no
    // positive terms there's nothing to return.
    if positives.is_empty() {
        return String::new();
    }

    let mut out = format!("({})", positives.join(" AND "));
    if !negatives.is_empty() {
        out.push_str(&format!(" NOT ({})", negatives.join(" OR ")));
    }
    out
}

/// A single search match — file-level with context snippet.
pub struct FtsMatch {
    pub path: String,
    pub snippet: String,
}

#[cfg(test)]
mod tests {
    //! Regression net for the `sqlite` crate version bump from 0.32 to 0.37.
    //! Locks down FTS5 schema creation, upsert/delete semantics, BM25 ranking,
    //! and snippet emission. Any of these silently changing across the bump
    //! is a data-format risk we want to catch here before users have indexes
    //! on disk.
    //!
    //! These tests assume the bundled SQLite was built with FTS5 (set via
    //! `SQLITE_ENABLE_FTS5=1` in `src-tauri/.cargo/config.toml`). On a build
    //! without FTS5 the LIKE fallback path is exercised by the
    //! `*_falls_back_when_fts5_disabled`-style tests we don't have yet
    //! — out of scope for this migration.

    use super::*;
    use tempfile::tempdir;

    fn fresh_db() -> SearchDb {
        let dir = tempdir().unwrap();
        // Leak the tempdir so the connection outlives this helper. The DB
        // file is cleaned up on process exit; that's fine for a unit test.
        let path = dir.keep();
        SearchDb::init(&path).expect("SearchDb::init should succeed in a fresh tempdir")
    }

    fn populate(db: &SearchDb) {
        db.upsert_note(
            "notes/rust.md",
            "Rust ownership",
            "Rust enforces memory safety via the borrow checker.",
        )
        .unwrap();
        db.upsert_note(
            "notes/python.md",
            "Python iterators",
            "Generators in Python lazily yield values.",
        )
        .unwrap();
        db.upsert_note(
            "ideas/dragon.md",
            "Dragon dreams",
            "A dragon flies over the borrow checker without complaint.",
        )
        .unwrap();
    }

    #[test]
    fn init_creates_fts5_table_and_reports_zero_notes() {
        let db = fresh_db();
        assert!(db.has_fts5, "expected FTS5 in the bundled SQLite build");
        assert_eq!(db.note_count().unwrap(), 0);
    }

    #[test]
    fn upsert_and_count_track_inserted_notes() {
        let db = fresh_db();
        populate(&db);
        assert_eq!(db.note_count().unwrap(), 3);
    }

    #[test]
    fn upsert_is_idempotent_on_same_path() {
        // FTS5 doesn't support a native ON CONFLICT for content-table rows,
        // so the upsert path uses DELETE-then-INSERT. Confirms that re-inserting
        // the same path replaces (not duplicates) the row.
        let db = fresh_db();
        db.upsert_note("notes/x.md", "v1", "first body").unwrap();
        db.upsert_note("notes/x.md", "v2", "second body").unwrap();
        assert_eq!(db.note_count().unwrap(), 1);

        let hits = db.search("second").unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path, "notes/x.md");
        assert!(
            hits[0].snippet.contains("second"),
            "snippet should reflect the updated content, got: {:?}",
            hits[0].snippet
        );
    }

    #[test]
    fn remove_note_drops_row_from_index() {
        let db = fresh_db();
        populate(&db);
        db.remove_note("notes/python.md").unwrap();
        assert_eq!(db.note_count().unwrap(), 2);
        assert!(db.search("Generators").unwrap().is_empty());
    }

    #[test]
    fn search_returns_matches_with_snippets() {
        let db = fresh_db();
        populate(&db);

        let hits = db.search("borrow").unwrap();
        // "borrow checker" appears in both rust.md and dragon.md.
        assert_eq!(hits.len(), 2);
        let paths: Vec<&str> = hits.iter().map(|h| h.path.as_str()).collect();
        assert!(paths.contains(&"notes/rust.md"));
        assert!(paths.contains(&"ideas/dragon.md"));
        for h in &hits {
            assert!(
                h.snippet.contains(">>>borrow<<<") || h.snippet.contains(">>>Borrow<<<"),
                "expected FTS5 snippet markers around the match, got: {:?}",
                h.snippet
            );
        }
    }

    #[test]
    fn search_empty_query_returns_empty() {
        let db = fresh_db();
        populate(&db);
        assert!(db.search("").unwrap().is_empty());
        assert!(db.search("   ").unwrap().is_empty());
    }

    #[test]
    fn search_with_only_punctuation_returns_empty() {
        // The safe-query builder strips non-alphanumeric chars; a query of
        // pure punctuation must short-circuit to empty rather than emit a
        // malformed FTS5 MATCH clause.
        let db = fresh_db();
        populate(&db);
        assert!(db.search("!?.,").unwrap().is_empty());
    }

    #[test]
    fn clear_index_wipes_all_rows() {
        let db = fresh_db();
        populate(&db);
        db.clear_index().unwrap();
        assert_eq!(db.note_count().unwrap(), 0);
        assert!(db.search("borrow").unwrap().is_empty());
    }

    #[test]
    fn index_complete_marker_roundtrips() {
        // SRX-3: a fresh DB is not "complete"; set/clear flip the marker so the
        // warm-start fast path can tell a finished index from a torn one.
        let db = fresh_db();
        assert!(!db.index_complete(), "fresh DB has no completion marker");
        db.set_index_complete().unwrap();
        assert!(db.index_complete(), "marker set after a full index");
        db.clear_index_complete().unwrap();
        assert!(!db.index_complete(), "marker cleared before a rebuild");
    }

    #[test]
    fn content_for_returns_last_indexed_verbatim_content() {
        // content_for is the "before" source for external-edit history: it must
        // return None before a note is indexed, the exact verbatim body after,
        // and the LATEST body after a re-upsert (so the external-edit diff
        // baseline advances rather than diffing against a stale snapshot).
        let db = fresh_db();
        assert_eq!(
            db.content_for("notes/x.md").unwrap(),
            None,
            "unindexed note has no before-snapshot"
        );
        db.upsert_note("notes/x.md", "x", "line one\nline two\n")
            .unwrap();
        assert_eq!(
            db.content_for("notes/x.md").unwrap().as_deref(),
            Some("line one\nline two\n"),
            "verbatim content is recoverable"
        );
        db.upsert_note("notes/x.md", "x", "line one\nline two\nline three\n")
            .unwrap();
        assert_eq!(
            db.content_for("notes/x.md").unwrap().as_deref(),
            Some("line one\nline two\nline three\n"),
            "content_for reflects the latest upsert (baseline advanced)"
        );
    }

    #[test]
    fn bulk_upsert_indexes_all_notes_in_one_txn() {
        // SRX-5: one transaction indexes every note; titles derive from the
        // path stem (same as the single-note upsert).
        let db = fresh_db();
        db.bulk_upsert(&[
            (
                "notes/rust.md".to_string(),
                "borrow checker safety".to_string(),
            ),
            (
                "notes/py.md".to_string(),
                "generators yield lazily".to_string(),
            ),
        ])
        .unwrap();
        assert_eq!(db.note_count().unwrap(), 2);
        let hits = db.search("borrow").unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path, "notes/rust.md");
    }

    #[test]
    fn init_self_heals_a_corrupt_db() {
        // SRX-2: a non-SQLite (corrupt) search.db is deleted and rebuilt rather
        // than surfacing a hard error — it's a pure derived cache, safe to nuke.
        let dir = tempdir().unwrap();
        let path = dir.keep();
        std::fs::write(
            path.join("search.db"),
            b"this is not a sqlite database at all",
        )
        .unwrap();
        let db = SearchDb::init(&path).expect("init should self-heal a corrupt search.db");
        assert_eq!(db.note_count().unwrap(), 0, "rebuilt index starts empty");
        db.upsert_note("a.md", "a", "hello").unwrap();
        assert_eq!(db.note_count().unwrap(), 1, "usable after self-heal");
    }

    #[test]
    fn search_respects_porter_stemming() {
        // Schema uses `tokenize = 'porter unicode61'`. Indexing "enforces"
        // should match a query for "enforce" — both share the Porter stem
        // "enforc". Pins the stemmer behavior so a sqlite-crate bump that
        // bundles a SQLite with a different FTS5 tokenizer would surface
        // here.
        let db = fresh_db();
        populate(&db);
        let hits = db.search("enforce").unwrap();
        assert!(
            hits.iter().any(|h| h.path == "notes/rust.md"),
            "porter stemmer should match 'enforce' -> 'enforces', got: {:?}",
            hits.iter().map(|h| &h.path).collect::<Vec<_>>()
        );
    }

    #[test]
    fn all_note_paths_lists_every_indexed_path() {
        // IP-A5: the warm-start search reconcile normalises these to note ids
        // to purge rows whose file was deleted off-app, so `all_note_paths`
        // must report every indexed path verbatim.
        let db = fresh_db();
        populate(&db);
        let mut paths = db.all_note_paths().unwrap();
        paths.sort();
        assert_eq!(
            paths,
            vec![
                "ideas/dragon.md".to_string(),
                "notes/python.md".to_string(),
                "notes/rust.md".to_string(),
            ]
        );
    }

    #[test]
    fn all_note_paths_drops_a_removed_note() {
        let db = fresh_db();
        populate(&db);
        db.remove_note("notes/python.md").unwrap();
        let paths = db.all_note_paths().unwrap();
        assert!(
            !paths.contains(&"notes/python.md".to_string()),
            "a removed note must not appear in all_note_paths, got {paths:?}"
        );
        assert_eq!(paths.len(), 2);
    }

    // ── DB-5: maintenance ───────────────────────────────────────────

    #[test]
    fn run_maintenance_preserves_the_index() {
        // FTS optimize + VACUUM must not lose rows or break search — it only
        // reclaims space and merges segments. Exercise it after a delete
        // (which leaves FTS tombstones) to mimic the real trigger.
        let db = fresh_db();
        populate(&db);
        db.remove_note("notes/python.md").unwrap();
        db.run_maintenance().expect("maintenance should succeed");
        assert_eq!(db.note_count().unwrap(), 2, "surviving rows intact");
        // Still searchable after compaction.
        let hits = db.search("borrow").unwrap();
        assert_eq!(hits.len(), 2);
        // Idempotent — a second pass on a clean index is fine.
        db.run_maintenance().expect("second maintenance pass ok");
    }

    #[test]
    fn maintenance_due_respects_the_interval_marker() {
        let db = fresh_db();
        // Fresh DB has never run maintenance → always due.
        assert!(db.maintenance_due(1_000_000));
        db.mark_maintenance_done(1_000_000).unwrap();
        // Immediately after, not due.
        assert!(!db.maintenance_due(1_000_000));
        assert!(!db.maintenance_due(1_000_000 + MAINTENANCE_INTERVAL_SECS - 1));
        // Once the full interval elapses, due again.
        assert!(db.maintenance_due(1_000_000 + MAINTENANCE_INTERVAL_SECS));
    }

    // ── SRX-7: build_fts5_query grammar ─────────────────────────────

    #[test]
    fn query_builder_handles_grammar_and_lone_quotes() {
        // Bare words → AND-ed positives.
        assert_eq!(build_fts5_query("rust"), "(\"rust\")");
        assert_eq!(build_fts5_query("rust lang"), "(\"rust\" AND \"lang\")");
        // Phrase preserved as a single FTS5 phrase.
        assert_eq!(
            build_fts5_query("\"borrow checker\""),
            "(\"borrow checker\")"
        );
        // Prefix marker survives, bareword and quoted forms.
        assert_eq!(build_fts5_query("rust*"), "(\"rust\"*)");
        // Exclusion via `-` and via bareword NOT.
        assert_eq!(
            build_fts5_query("borrow -dragon"),
            "(\"borrow\") NOT (\"dragon\")"
        );
        assert_eq!(build_fts5_query("foo NOT bar"), "(\"foo\") NOT (\"bar\")");
        // Nothing searchable → empty (caller short-circuits to no results).
        assert_eq!(build_fts5_query(""), "");
        assert_eq!(build_fts5_query("   "), "");
        assert_eq!(build_fts5_query("!?.,"), "");
        assert_eq!(build_fts5_query("\""), "", "lone quote must not error");
        // Pure-negative has no positive anchor → empty (FTS5 can't do it).
        assert_eq!(build_fts5_query("-dragon"), "");
        // A stray quote mid-token is escaped (doubled), never breaks the grammar.
        assert_eq!(build_fts5_query("ab\"cd"), "(\"ab\"\"cd\")");
    }

    #[test]
    fn search_phrase_query_respects_word_order() {
        // NEW-S-FTS: "borrow checker" as a phrase matches both notes that
        // contain the words adjacently; reversed order matches neither.
        let db = fresh_db();
        populate(&db);
        assert_eq!(db.search("\"borrow checker\"").unwrap().len(), 2);
        assert!(
            db.search("\"checker borrow\"").unwrap().is_empty(),
            "phrase order matters — reversed phrase must not match"
        );
    }

    #[test]
    fn search_exclusion_drops_the_excluded_note() {
        // `borrow -dragon` keeps rust.md but drops the dragon note.
        let db = fresh_db();
        populate(&db);
        let hits = db.search("borrow -dragon").unwrap();
        assert!(hits.iter().any(|h| h.path == "notes/rust.md"));
        assert!(
            !hits.iter().any(|h| h.path == "ideas/dragon.md"),
            "-dragon must exclude the dragon note, got {:?}",
            hits.iter().map(|h| &h.path).collect::<Vec<_>>()
        );
    }

    #[test]
    fn search_prefix_query_matches_partial_token() {
        // `borro*` prefix-matches the stored token `borrow`; the non-prefix
        // `borro` does not (proving it's a real prefix query, not substring).
        let db = fresh_db();
        populate(&db);
        assert!(
            db.search("borro*")
                .unwrap()
                .iter()
                .any(|h| h.path == "notes/rust.md"),
            "prefix borro* should match borrow"
        );
        assert!(
            db.search("borro").unwrap().is_empty(),
            "non-prefix borro must not match borrow"
        );
    }

    #[test]
    fn search_lone_quote_does_not_error() {
        // Regression: a raw `"` (or a stray quote mid-query) must never produce
        // a malformed MATCH that errors — it degrades to a safe query.
        let db = fresh_db();
        populate(&db);
        assert!(db.search("\"").unwrap().is_empty());
        // Stray quote alongside a real term still returns hits, no error.
        assert!(!db.search("borrow \" checker").unwrap().is_empty());
    }

    // ── SRX-8: title boost ──────────────────────────────────────────

    #[test]
    fn title_hit_outranks_body_only_hit() {
        let db = fresh_db();
        db.upsert_note("a.md", "alpha topic beacon", "unrelated body text")
            .unwrap();
        db.upsert_note(
            "b.md",
            "unrelated heading",
            "the beacon appears in the body only",
        )
        .unwrap();
        let hits = db.search("beacon").unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(
            hits[0].path,
            "a.md",
            "a title hit must rank above a body-only hit, got {:?}",
            hits.iter().map(|h| &h.path).collect::<Vec<_>>()
        );
    }

    // ── NEW-S-FTS2: title-only snippet fallback ─────────────────────

    #[test]
    fn title_only_match_snippet_falls_back_to_title() {
        let db = fresh_db();
        db.upsert_note("z.md", "zebra", "horses run fast").unwrap();
        let hits = db.search("zebra").unwrap();
        assert_eq!(hits.len(), 1);
        assert!(
            hits[0].snippet.contains("zebra"),
            "a title-only match must snippet the title, not an empty/body slice, got {:?}",
            hits[0].snippet
        );
    }

    // ── NEW-S-FTS1: per-note mtime side table ───────────────────────

    #[test]
    fn upsert_records_real_file_mtime_and_remove_clears_it() {
        // mtime is only recorded for a stat-able path, so use a real temp file.
        let dir = tempdir().unwrap();
        let file = dir.path().join("real.md");
        std::fs::write(&file, "hello world").unwrap();
        let abs = file.to_string_lossy().to_string();

        let db = fresh_db();
        db.upsert_note(&abs, "real", "hello world").unwrap();

        let mtimes = db.all_note_mtimes().unwrap();
        assert_eq!(mtimes.len(), 1, "a real-file upsert records one mtime row");
        assert_eq!(mtimes[0].0, abs);
        assert!(mtimes[0].1 > 0.0, "recorded mtime is a real timestamp");

        db.remove_note(&abs).unwrap();
        assert!(
            db.all_note_mtimes().unwrap().is_empty(),
            "removing a note clears its mtime row"
        );
    }

    #[test]
    fn fake_path_upsert_records_no_mtime() {
        // A path that can't be stat'ed (SAF `content://`, or a fake test path)
        // records no mtime — modify-detection is simply a no-op there.
        let db = fresh_db();
        db.upsert_note("content://roost/x.md", "x", "body").unwrap();
        assert!(db.all_note_mtimes().unwrap().is_empty());
    }
}
