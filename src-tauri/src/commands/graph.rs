use cozo::DbInstance;
use serde::Serialize;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use tauri::{AppHandle, Manager, State};

use crate::commands::task_notify::NotificationScheduler;
use crate::db::queries;
use crate::db::queries::LinkResolver;

const MAX_FULL_GRAPH_NOTES: usize = 1_000;
const MAX_GRAPH_WINDOW_NODES: usize = 500;
const MAX_GRAPH_DEPTH: usize = 4;
const MAX_GRAPH_ALGO_RESULTS: usize = 1_000;
pub(crate) const MAX_AUTO_COMMUNITY_NOTES: usize = 5_000;
const MAX_BRIDGE_RESULTS: usize = 100;
const MAX_BRIDGE_COMPUTE_NOTES: usize = 2_000;
const MAX_BRIDGE_COMPUTE_LINKS: usize = 20_000;
const MAX_GRAPH_ID_BYTES: usize = 4_096;
const MAX_GRAPH_PATH_BYTES: usize = 32 * 1_024;

fn validate_graph_path(path: &str) -> Result<(), String> {
    if path.is_empty() {
        return Err("Graph path cannot be empty".to_string());
    }
    if path.len() > MAX_GRAPH_PATH_BYTES {
        return Err("Graph path is too long".to_string());
    }
    Ok(())
}

pub struct GraphDb {
    pub db: Mutex<Option<DbInstance>>,
    /// Absolute roost root path, set when `init_graph` succeeds. Needed so
    /// later commands can compute roost-relative note ids without requiring
    /// the frontend to pass the roost path on every call.
    pub roost_path: Mutex<Option<String>>,
    /// Cached wiki-link resolver. Built once at roost open and mutated
    /// incrementally as notes are added; spares every save from a
    /// full `*notes{ id }` scan (50–200ms at 100K notes).
    ///
    /// Lock ordering: always acquire `db` and `roost_path` BEFORE
    /// `link_resolver`. Never hold `link_resolver` while waiting on
    /// either of the others.
    pub link_resolver: Mutex<Option<LinkResolver>>,
    /// Task notification scheduler — Phase 2.1 slice 5. Owns the timers
    /// for `📅 + ⏰` tasks; rebuilt on `init_graph` and per-note on save.
    pub notification_scheduler: NotificationScheduler,
    /// Has the link graph changed since the algo cache was last read?
    /// Set (cheaply, no DB work) on every link-changing save; consumed by
    /// the cache-read paths, which wipe the cache lazily on the first read
    /// after a change so the next `cached_*` recomputes fresh.
    ///
    /// Supersedes the AUDIT-066 30s time-debounce: that avoided wiping the
    /// cache 1-2×/sec during active typing, but at the cost of serving up
    /// to 30s of STALE PageRank/community results to reads in that window.
    /// A dirty bit fixes both — the save path does zero DB work (just this
    /// atomic), and reads are never stale because the wipe+recompute is
    /// deferred to read time and only runs when something actually changed.
    pub algo_cache_dirty: AtomicBool,
    /// Last warm-heal outcome (IP-A8, index-health observability). Set by
    /// `spawn_warm_heal` when the reconcile finishes for the still-active
    /// roost; surfaced in the diagnostic report so a bug report shows whether
    /// the index converged and how much off-app drift the last heal absorbed.
    /// `last_heal_ran` gates the others — a 0 is only meaningful after a heal.
    pub last_heal_ran: AtomicBool,
    pub last_heal_ms: AtomicU64,
    pub last_heal_added: AtomicU64,
    pub last_heal_modified: AtomicU64,
    pub last_heal_purged: AtomicU64,
    /// NEW-S2: cached post-index snapshot of the O(roost) graph stats + full
    /// task list. Populated at the end of every real re-index
    /// (`index_note_internal_locked`); consumed by the EDT-2 unchanged-save
    /// fast path in `commands::save` so a byte-identical no-op save doesn't
    /// re-run `get_graph_stats` + `query_all_tasks` (both O(roost) at 50K
    /// notes) and contend the graph lock across N tabs' debounced autosaves.
    /// Cleared (`invalidate_stats_cache`) by every OTHER graph/task mutation
    /// so the fast path never serves stale counts.
    ///
    /// Leaf lock: acquire it alone or last, and never hold it while acquiring
    /// `db` / `roost_path` / `link_resolver` (the readers release it before
    /// touching `db`, so no ordering cycle exists).
    pub stats_cache: Mutex<Option<StatsSnapshot>>,
}

/// NEW-S2: the two O(roost) values folded into a save response, cached so the
/// unchanged-save fast path can reuse them instead of recomputing.
#[derive(Clone)]
pub struct StatsSnapshot {
    pub stats: GraphStatsResult,
    pub tasks: Vec<crate::commands::tasks::TaskRow>,
}

/// DB-3: swap one note's rows inside a cached task list that is already in
/// canonical order. Drops the note's old rows, appends its fresh ones and
/// re-sorts with the SAME comparator `query_all_tasks` uses — a stable sort over
/// two already-sorted runs is a near-linear merge, and the result is identical
/// to a from-scratch query (sort keys are unique per row: path + source line).
fn replace_note_tasks(
    tasks: &mut Vec<crate::commands::tasks::TaskRow>,
    note_id: &str,
    fresh: Vec<crate::commands::tasks::TaskRow>,
) {
    tasks.retain(|t| t.note_id != note_id);
    if fresh.is_empty() {
        return;
    }
    tasks.extend(fresh);
    tasks.sort_by(crate::commands::tasks::task_row_cmp);
}

#[cfg(test)]
thread_local! {
    /// Test counter: how many times `stats_snapshot_or_compute` had to fall
    /// back to the O(roost) recompute on THIS thread. Lets tests prove the
    /// steady-state save path never re-counts whole relations.
    pub(crate) static FULL_STATS_RECOMPUTES: std::cell::Cell<usize> =
        const { std::cell::Cell::new(0) };
}

impl GraphDb {
    pub fn new() -> Self {
        Self {
            db: Mutex::new(None),
            roost_path: Mutex::new(None),
            link_resolver: Mutex::new(None),
            notification_scheduler: NotificationScheduler::new(),
            algo_cache_dirty: AtomicBool::new(false),
            last_heal_ran: AtomicBool::new(false),
            last_heal_ms: AtomicU64::new(0),
            last_heal_added: AtomicU64::new(0),
            last_heal_modified: AtomicU64::new(0),
            last_heal_purged: AtomicU64::new(0),
            stats_cache: Mutex::new(None),
        }
    }

    /// Record the outcome of a completed warm heal (IP-A8). Called only for
    /// the active roost so a switch mid-heal doesn't stamp another roost's
    /// stats.
    pub fn record_heal(&self, elapsed_ms: u64, added: u64, modified: u64, purged: u64) {
        self.last_heal_ms.store(elapsed_ms, Ordering::Relaxed);
        self.last_heal_added.store(added, Ordering::Relaxed);
        self.last_heal_modified.store(modified, Ordering::Relaxed);
        self.last_heal_purged.store(purged, Ordering::Relaxed);
        self.last_heal_ran.store(true, Ordering::Relaxed);
    }

    /// Drop the cached resolver. The next `index_note` call will rebuild
    /// it from a fresh DB scan. Future delete/rename code paths should
    /// call this when an id disappears from the roost.
    pub fn invalidate_link_resolver(&self) {
        if let Ok(mut guard) = self.link_resolver.lock() {
            *guard = None;
        }
    }

    /// IP-A3 / TRN-7: run the full single-note (re)index for an auxiliary
    /// write path (AI roost/task edit, in-app quick-add, history revert)
    /// REUSING the cached resolver instead of rebuilding one from a full
    /// `*notes` scan. See [`Self::index_note_core`] — this is the same
    /// implementation (and the same cache maintenance) minus the result, so
    /// callers must NOT invalidate the resolver afterward (only id-removing
    /// paths do) and need no `invalidate_stats_cache` of their own either: the
    /// cached totals/task list are patched for this note before this returns.
    ///
    /// `db` must be the caller's already-locked graph DB.
    pub fn index_note_full_cached(
        &self,
        db: &DbInstance,
        roost_root: &str,
        abs_path: &str,
        content: &str,
    ) -> Result<(), String> {
        self.index_note_core(db, roost_root, abs_path, content)
            .map(|_| ())
    }

    /// The ONE single-note index implementation behind every path that
    /// (re)indexes a note through the live graph — the editor save
    /// (`index_note_internal`), folder-move re-index, AI roost/task edits,
    /// in-app quick-add, and history revert. Get-or-build the cached resolver
    /// (rebuilt only after an id-removing path invalidated it), run the atomic
    /// per-note transaction (DB-4: `queries::index_note_full_with_resolver`),
    /// then keep the cached stats snapshot exact (DB-3, see
    /// [`Self::apply_outcome_to_stats_cache`]).
    ///
    /// Lock ordering: the caller holds `db` (and, on the save path,
    /// `roost_path`) before this acquires `link_resolver`, then the leaf
    /// `stats_cache`.
    pub fn index_note_core(
        &self,
        db: &DbInstance,
        roost_root: &str,
        abs_path: &str,
        content: &str,
    ) -> Result<queries::NoteIndexOutcome, String> {
        let indexed = (|| {
            let mut resolver_guard = self
                .link_resolver
                .lock()
                .map_err(|e| format!("Lock error: {e}"))?;
            if resolver_guard.is_none() {
                *resolver_guard = Some(LinkResolver::build(db)?);
            }
            let resolver = resolver_guard.as_mut().expect("resolver populated above");
            queries::index_note_full_with_resolver(db, roost_root, abs_path, content, resolver)
        })();
        let outcome = match indexed {
            Ok(outcome) => outcome,
            Err(e) => {
                // The transaction rolls back, so the graph should be unchanged —
                // but a failed index is exactly when "should" isn't worth
                // trusting, and a recompute is cheap next to a stale snapshot
                // served as exact. (The resolver already un-adds a failed new
                // note's id.)
                self.invalidate_stats_cache();
                return Err(e);
            }
        };
        if !outcome.deltas_trusted {
            // The index COMMITTED but its measurements couldn't be read, so the
            // snapshot can't be patched and the link topology is unknown: drop
            // the cached totals/tasks and force an algo-cache refresh.
            self.invalidate_stats_cache();
            self.mark_algo_cache_dirty();
            return Ok(outcome);
        }
        self.apply_outcome_to_stats_cache(db, &outcome);
        Ok(outcome)
    }

    /// DB-3: keep the cached stats snapshot exact after a single-note index
    /// WITHOUT re-reading whole relations.
    ///
    /// The snapshot is either `None` (nothing cached — the next reader
    /// recomputes once) or EXACT; this never leaves it merely "close":
    ///
    /// - the three totals move by the exact per-note delta the index
    ///   transaction measured (`NoteIndexOutcome`);
    /// - the task list has just this note's rows replaced by a key-prefix
    ///   query (O(this note's tasks)), re-merged with the shared comparator so
    ///   the result is byte-identical to a fresh `query_all_tasks`.
    ///
    /// Any failure or arithmetic that would go negative drops the snapshot
    /// instead of guessing. Every graph mutation that does NOT pass through
    /// here (purge, task CRUD, heal, rebuild, exclusion change) invalidates.
    fn apply_outcome_to_stats_cache(&self, db: &DbInstance, outcome: &queries::NoteIndexOutcome) {
        // Cold cache: nothing to maintain.
        if self.cached_stats_is_none() {
            return;
        }
        // Computed before taking the leaf lock so the lock is never held across
        // DB work (callers already hold `db`; every mutation of the cache runs
        // under it, so nothing can invalidate in between).
        let fresh_tasks = crate::commands::tasks::query_note_tasks(db, &outcome.note_id);
        let Ok(mut guard) = self.stats_cache.lock() else {
            return;
        };
        let tasks = match fresh_tasks {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(note_id = %outcome.note_id, error = %e, "graph: stats cache patch failed; dropping snapshot");
                *guard = None;
                return;
            }
        };
        let Some(snap) = guard.as_mut() else {
            return;
        };
        let apply = |current: usize, delta: i64| -> Option<usize> {
            usize::try_from(i64::try_from(current).ok()?.checked_add(delta)?).ok()
        };
        let (Some(notes), Some(links), Some(tags)) = (
            apply(snap.stats.notes, outcome.notes_delta),
            apply(snap.stats.links, outcome.links_delta),
            apply(snap.stats.tags, outcome.tags_delta),
        ) else {
            tracing::warn!(note_id = %outcome.note_id, "graph: stats cache delta out of range; dropping snapshot");
            *guard = None;
            return;
        };
        snap.stats = GraphStatsResult { notes, links, tags };
        replace_note_tasks(&mut snap.tasks, &outcome.note_id, tasks);
    }

    fn cached_stats_is_none(&self) -> bool {
        self.stats_cache.lock().map(|g| g.is_none()).unwrap_or(true)
    }

    /// The post-save `(stats, tasks)` pair every save folds into its response.
    /// Warm cache (the steady state): returned as-is — NO relation scan, the
    /// cost is independent of roost size. Cold cache (first save of a session,
    /// or after a purge / task edit / heal invalidated it): recomputed ONCE
    /// from the DB and cached, so the next save is warm again.
    ///
    /// `db` must be the caller's already-locked graph DB (the compute then
    /// can't race a mutation).
    pub fn stats_snapshot_or_compute(
        &self,
        db: &DbInstance,
    ) -> Result<(GraphStatsResult, Vec<crate::commands::tasks::TaskRow>), String> {
        if let Some(snap) = self.cached_stats() {
            return Ok((snap.stats, snap.tasks));
        }
        #[cfg(test)]
        FULL_STATS_RECOMPUTES.with(|c| c.set(c.get() + 1));
        let counts = queries::get_graph_stats(db)?;
        let tasks = crate::commands::tasks::query_all_tasks(db)?;
        let stats = GraphStatsResult {
            notes: counts.notes,
            links: counts.links,
            tags: counts.tags,
        };
        self.store_stats_cache(stats.clone(), tasks.clone());
        Ok((stats, tasks))
    }

    /// Purge a note from the graph and drop everything cached about the old
    /// graph shape, in one place. A purge removes a node, its incoming and
    /// outgoing edges and its tasks, so it must (a) invalidate the link resolver
    /// (an id disappeared) and (b) invalidate the stats snapshot — and (b) must
    /// happen while the caller still holds the `db` lock, or a concurrent save
    /// could patch a snapshot that predates the purge. Every purge call site
    /// should use this instead of pairing `queries::purge_note` with separate
    /// invalidations.
    pub fn purge_note_cached(&self, db: &DbInstance, note_id: &str) -> Result<(), Vec<String>> {
        let result = queries::purge_note(db, note_id);
        self.invalidate_link_resolver();
        self.invalidate_stats_cache();
        result
    }

    /// Run `f` with the CACHED link resolver (get-or-build) instead of building
    /// a fresh one from a full `*notes` scan. For read-only link diagnostics /
    /// resolution that run against the live graph (AI `roost_write` /
    /// `roost_edit` link warnings, …). The cache is maintained incrementally by
    /// every indexing path and invalidated by every id-removing path, so it is
    /// as current as the graph itself.
    ///
    /// `db` must be the caller's already-locked graph DB; the resolver lock is
    /// taken after it (lock ordering), and `f` must not call back into
    /// `GraphDb`.
    pub fn with_cached_resolver<T>(
        &self,
        db: &DbInstance,
        f: impl FnOnce(&LinkResolver) -> T,
    ) -> Result<T, String> {
        let mut guard = self
            .link_resolver
            .lock()
            .map_err(|e| format!("Lock error: {e}"))?;
        if guard.is_none() {
            *guard = Some(LinkResolver::build(db)?);
        }
        Ok(f(guard.as_ref().expect("resolver populated above")))
    }

    /// Mark the algo cache stale after a link-changing save. Cheap — one
    /// atomic store, no DB work, so it's safe to call on every autosave.
    pub fn mark_algo_cache_dirty(&self) {
        self.algo_cache_dirty.store(true, Ordering::Relaxed);
    }

    /// If the graph changed since the last cache read, wipe the algo cache
    /// once so the caller's `cached_*` falls through to a fresh recompute.
    /// One-shot: the dirty bit is cleared atomically, and the wipe clears
    /// EVERY algo's rows, so a later read of a different algo still recomputes
    /// (its cache row is gone) without re-wiping. Cheap no-op when clean.
    pub fn refresh_algo_cache_if_dirty(&self, db: &DbInstance) -> Result<(), String> {
        if self.algo_cache_dirty.swap(false, Ordering::Relaxed) {
            if let Err(e) = queries::invalidate_algo_cache(db) {
                // Never lose the invalidation request. A later read retries
                // instead of serving rows we already know may be stale.
                self.algo_cache_dirty.store(true, Ordering::Relaxed);
                return Err(e);
            }
        }
        Ok(())
    }

    /// NEW-S2: stash the freshly-computed stats + task list after a real
    /// re-index so the unchanged-save fast path can serve them without
    /// re-running the two O(roost) queries. A poisoned lock just skips the
    /// cache — the fast path degrades to a recompute, never to wrong data.
    pub fn store_stats_cache(
        &self,
        stats: GraphStatsResult,
        tasks: Vec<crate::commands::tasks::TaskRow>,
    ) {
        if let Ok(mut guard) = self.stats_cache.lock() {
            *guard = Some(StatsSnapshot { stats, tasks });
        }
    }

    /// NEW-S2: return the cached stats + task list if present. `None` means the
    /// caller must recompute (either nothing has been indexed yet this session,
    /// or a mutation invalidated the snapshot).
    pub fn cached_stats(&self) -> Option<StatsSnapshot> {
        self.stats_cache.lock().ok().and_then(|g| g.clone())
    }

    /// NEW-S2: drop the cached stats snapshot. Called by every graph/task
    /// mutation that does NOT go through `index_note_internal_locked` (deletes,
    /// task CRUD, off-app heal, agent writes) so the unchanged-save fast path
    /// re-derives fresh counts on its next miss instead of serving stale ones.
    pub fn invalidate_stats_cache(&self) {
        if let Ok(mut guard) = self.stats_cache.lock() {
            *guard = None;
        }
    }

    /// Drop every singleton derived from the previously-active roost. Called
    /// under `RoostFsState::transition`, so an init commit cannot race it.
    pub fn clear_for_roost_switch(&self) {
        if let Ok(mut guard) = self.db.lock() {
            *guard = None;
        }
        if let Ok(mut guard) = self.roost_path.lock() {
            *guard = None;
        }
        if let Ok(mut guard) = self.link_resolver.lock() {
            *guard = None;
        }
        if let Ok(mut guard) = self.stats_cache.lock() {
            *guard = None;
        }
        self.algo_cache_dirty.store(false, Ordering::Release);
        self.last_heal_ran.store(false, Ordering::Release);
    }
}

/// DB-2: run a closure with the locked graph DB inside
/// `tokio::task::block_in_place`. The graph's Datalog algorithms (PageRank,
/// Louvain community detection, Brandes betweenness) and whole-roost
/// materializations (`get_full_graph`) run synchronously and can hold the DB
/// mutex for seconds—to—minutes at 50K notes. Without `block_in_place`, that
/// parks the current tokio worker thread for the whole computation, starving
/// unrelated async tasks on it — sync keepalives, the iroh accept loop, IPC
/// dispatch, timers. `block_in_place` tells the scheduler to migrate those
/// tasks off this worker while we compute, so a heavy graph query no longer
/// freezes sync or the rest of the IPC surface.
///
/// This does NOT shorten how long the DB mutex itself is held — a concurrent
/// graph query still waits. Taking the compute fully off-lock would need the
/// algorithm to run on a data snapshot, which Cozo doesn't cheaply expose;
/// tracked as a follow-up. `block_in_place` requires the multi-threaded
/// runtime (Tauri's default; `files.rs` already relies on it) and must be
/// called from inside an async task — every caller here is a `#[tauri::command]
/// async fn`.
fn with_locked_db_blocking<T>(
    db_state: &GraphDb,
    f: impl FnOnce(&DbInstance) -> Result<T, String>,
) -> Result<T, String> {
    tokio::task::block_in_place(|| {
        let guard = db_state.db.lock().map_err(|e| format!("Lock error: {e}"))?;
        let db = guard.as_ref().ok_or("Graph DB not initialized")?;
        f(db)
    })
}

#[derive(Debug, Serialize, specta::Type)]
pub struct BacklinkResult {
    pub title: String,
    pub path: String,
}

#[derive(Debug, Serialize, specta::Type)]
pub struct ForwardLinkResult {
    pub target: String,
    pub link_text: String,
}

#[derive(Debug, Clone, Serialize, specta::Type)]
pub struct GraphStatsResult {
    #[specta(type = u32)]
    pub notes: usize,
    #[specta(type = u32)]
    pub links: usize,
    #[specta(type = u32)]
    pub tags: usize,
}

/// Bundled response for [`index_note`]. Folds graph stats and the full
/// task list into one IPC round-trip so the save path doesn't fan out
/// into 3 separate Tauri invokes (`index_note`, `get_graph_stats`,
/// `list_tasks`). On Android each round-trip hops the WebView/JNI
/// bridge — collapsing the trio is a measurable mobile win.
#[derive(Debug, Serialize, specta::Type)]
pub struct IndexNoteResponse {
    pub stats: GraphStatsResult,
    pub tasks: Vec<crate::commands::tasks::TaskRow>,
}

#[derive(Debug, Serialize, specta::Type)]
pub struct FullGraphResult {
    pub nodes: Vec<GraphNodeResult>,
    pub edges: Vec<GraphEdgeResult>,
}

#[derive(Debug, Serialize, specta::Type)]
pub struct GraphNodeResult {
    pub id: String,
    pub title: String,
    pub path: String,
    pub kind: String,
    #[specta(type = u32)]
    pub link_count: usize,
}

#[derive(Debug, Serialize, specta::Type)]
pub struct GraphEdgeResult {
    pub source: String,
    pub target: String,
}

/// Initialize CozoDB for a roost and index all notes.
///
/// The DB file lives under `app_data_dir/vaults/{hash}/` — device-local,
/// never inside the roost folder. This keeps SQLite safe from file-level
/// sync (Syncthing/iCloud/Drive) and lets SAF roosts work at all (SAF
/// content:// trees have no host-writable path for SQLite to open).
///
/// The roost scan walks the backend installed by `open_roost` (NativeFs
/// or SafFs) rather than `std::fs`, so SAF roosts index the same way
/// desktop roosts do.
#[tauri::command]
#[specta::specta]
pub async fn init_graph(
    roost_path: String,
    app: AppHandle,
    roost_fs: State<'_, crate::commands::files::RoostFsState>,
    db_state: State<'_, GraphDb>,
) -> Result<GraphStatsResult, String> {
    validate_graph_path(&roost_path)?;
    tracing::info!("graph: init_graph called");

    let generation = roost_fs.generation();
    let backend = roost_fs
        .current()
        .await
        .ok_or_else(|| "Roost backend not initialized — open a roost first".to_string())?;
    if !super::roost::same_roost_root(backend.root_id().as_str(), Path::new(&roost_path)) {
        return Err("Roost changed before graph initialization began".to_string());
    }

    let db_dir = crate::paths::roost_db_dir(&app, &roost_path)?;
    let db = crate::db::schema::init_db(&db_dir).map_err(|e| {
        tracing::error!(error = %e, "graph: init_db failed");
        // AUDIT-051: a corrupt DB needs a different recovery flow
        // (rebuild from markdown) than a transient error (retry,
        // check permissions, free disk). The frontend matches the
        // `__db_corrupt__:` prefix and surfaces the Repair button
        // instead of a generic toast.
        match e {
            crate::db::schema::InitDbError::Corrupt(msg) => format!("__db_corrupt__:{msg}"),
            crate::db::schema::InitDbError::Other(msg) => msg,
        }
    })?;
    tracing::info!("graph: CozoDB initialized successfully");

    // ── Fast path: DB already has data from a previous session ──────────
    // CozoDB persists on disk. After first install, subsequent launches can
    // just load the existing DB and return stats in <100ms instead of
    // re-reading every file through SAF (~70s for 160 files).
    let existing_stats = queries::get_graph_stats(&db)?;
    // IP-5: don't treat "notes > 0" alone as "index is complete". A crash
    // between cold pass1 (notes inserted) and pass2 (links built) leaves the
    // DB with notes but no links; the old gate then took the warm path
    // FOREVER, so the graph had zero links and never healed. Gate on an
    // explicit completion marker instead. `links > 0` is a compatibility
    // escape hatch for DBs indexed before this marker existed — they're
    // known-good, so serve them warm and stamp the marker below rather than
    // forcing a needless full rescan. A torn init (notes>0, links==0, no
    // marker) correctly falls through to the cold path and rebuilds.
    let index_complete = crate::db::schema::get_kv(&db, "graph_index_complete")
        .ok()
        .flatten()
        .as_deref()
        == Some("1");
    if existing_stats.notes > 0 && (index_complete || existing_stats.links > 0) {
        tracing::info!(
            notes = existing_stats.notes,
            index_complete,
            "graph: DB warm, skipping roost content walk"
        );

        // Serve the warm DB IMMEDIATELY; heal against disk in the BACKGROUND.
        //
        // The heal (ghost purge + off-app-added indexing) used to run inline
        // here, and init_graph blocked on it — which meant the graph pane
        // showed nothing until every off-app note was read and indexed. On
        // iOS that was catastrophic after a roost switch: sync writes notes
        // to disk while another roost is open, so switching back found dozens
        // of "added off-app" notes, and scoped-storage reads + indexing took
        // ~a minute (observed on-device: 57s for 40 notes) with the UI stuck
        // on a loading graph the whole time. The DB in hand is already a
        // complete last-session index — serve it now (milliseconds), let the
        // heal converge behind it. full_graph / graph_window re-query on
        // every pane interaction, so healed notes appear organically.
        let result = GraphStatsResult {
            notes: existing_stats.notes,
            links: existing_stats.links,
            tags: existing_stats.tags,
        };

        // Pre-build the resolver cache before publishing the DB. One scan
        // here saves a scan on the very first save after roost open. The
        // background heal rebuilds + republishes it once new notes land.
        let resolver = LinkResolver::build(&db)?;

        // Commit under the same transition gate used by open/close. This turns
        // the generation check + singleton publication into one atomic action.
        let _transition = roost_fs.transition.lock().await;
        if !roost_fs.is_generation_current(generation) {
            return Err("Roost changed during graph initialization".to_string());
        }
        *db_state.db.lock().map_err(|e| format!("Lock error: {e}"))? = Some(db.clone());
        *db_state
            .roost_path
            .lock()
            .map_err(|e| format!("Lock error: {e}"))? = Some(roost_path.clone());
        *db_state
            .link_resolver
            .lock()
            .map_err(|e| format!("Lock error: {e}"))? = Some(resolver);
        // A freshly published DB invalidates whatever was cached for the one it
        // replaces (e.g. re-init after Repair on the same roost).
        db_state.invalidate_stats_cache();

        tracing::info!("graph: init_graph complete (warm), DB stored in state");

        drop(_transition);
        spawn_warm_heal(app.clone(), roost_path, backend.clone());
        // DB-5: schedule the day-gated, delayed graph-DB compaction pass.
        spawn_graph_maintenance(app.clone());
        return Ok(result);
    }

    // ── Cold path: first-ever launch, full roost scan ───────────────────
    tracing::info!("graph: DB empty — performing first-time roost scan");

    // Walk the backend that `open_roost` installed. If the frontend
    // called init_graph before open_roost (shouldn't happen, but be
    // defensive), surface a clear error rather than panic.
    // Cached walk so a follow-up `init_search` on the same roost skips a
    // full SAF re-walk. The cache is short-TTL and invalidated on roost
    // switch — see `roostfs::walk::invalidate_walk_cache`.
    let files = crate::roostfs::walk::collect_markdown_cached(backend.as_ref(), &roost_path)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "graph: roost walk failed");
            e
        })?;
    tracing::info!(files = files.len(), "graph: walked markdown files");

    // Bulk pass 1 inserts all notes + tags in 3 transactions total (notes,
    // clear-tags, insert-tags). Pass 2 resolves wiki-links against the full
    // note set and writes all edges in 2 transactions. This replaces the
    // previous per-note loop that emitted 5+ SQLite commits per note, which
    // held the DB mutex for 5–10s and caused SQLITE_BUSY errors whenever
    // auto-save called `index_note` during init.
    queries::bulk_index_pass1(&db, &roost_path, &files)?;
    let resolver = queries::LinkResolver::build(&db)?;
    queries::bulk_index_pass2(&db, &roost_path, &files, &resolver)?;

    // IP-5: mark the index complete ONLY after both passes land. If we crash
    // before this (e.g. between pass1 and pass2), the marker stays absent and
    // the next launch re-runs the cold scan instead of serving a link-less
    // graph forever.
    if let Err(e) = crate::db::schema::set_kv(&db, "graph_index_complete", "1") {
        tracing::warn!(error = %e, "graph: failed to stamp cold index-complete marker");
    }

    let stats = queries::get_graph_stats(&db)?;
    let result = GraphStatsResult {
        notes: stats.notes,
        links: stats.links,
        tags: stats.tags,
    };
    tracing::info!(
        notes = result.notes,
        links = result.links,
        tags = result.tags,
        "graph: indexed roost"
    );

    let _transition = roost_fs.transition.lock().await;
    if !roost_fs.is_generation_current(generation) {
        return Err("Roost changed during graph initialization".to_string());
    }
    *db_state.db.lock().map_err(|e| format!("Lock error: {e}"))? = Some(db);
    *db_state
        .roost_path
        .lock()
        .map_err(|e| format!("Lock error: {e}"))? = Some(roost_path);
    // Reuse the resolver we just built for pass2 — its id_set already
    // matches the post-init notes relation.
    *db_state
        .link_resolver
        .lock()
        .map_err(|e| format!("Lock error: {e}"))? = Some(resolver);
    db_state.invalidate_stats_cache();
    drop(_transition);

    // Build the notification scheduler from the freshly-populated `:task`
    // rows. Done after the locks above are released — `reschedule_all`
    // takes its own short read lock on the DB.
    {
        let guard = db_state.db.lock().map_err(|e| format!("Lock error: {e}"))?;
        if let Some(db_ref) = guard.as_ref() {
            db_state.notification_scheduler.reschedule_all(db_ref);
        }
    }

    // DB-5: schedule the day-gated, delayed graph-DB compaction pass.
    spawn_graph_maintenance(app.clone());

    tracing::info!("graph: init_graph complete (cold), DB stored in state");
    Ok(result)
}

/// IP-2 (added-while-closed half): index notes that exist on disk but have no
/// graph node yet. Called from the warm-open path, which otherwise never
/// reads roost content — so a note added by git pull / an external editor /
/// a backup restore while Corvux was closed would stay invisible to the graph
/// and FTS until the user opened and saved it.
///
/// Cheap by construction: we already hold the path listing; we only `read()`
/// the files whose ids aren't in the DB (usually none). Runs the same two
/// passes as a save — pass1 for all new notes first so the freshly-built
/// resolver sees their ids, then pass2 to resolve their links, then promote
/// any dangling links that were waiting for them.
///
/// Best-effort per note: a read / UTF-8 / index failure on one added note
/// logs and continues.
///
/// IP-A1: also catches notes MODIFIED while closed. `reconcile_added_and_
/// modified_notes` compares each file's disk mtime against the stored
/// `updated_at` and re-indexes the ones that advanced, so a note edited in
/// Obsidian/vim while Corvux was closed (sync off) no longer keeps stale
/// links/tags until its next in-app save. Detection uses `std::fs` mtime, so
/// it's live on native; Android SAF paths aren't stat-able by `std::fs`, so
/// there it falls back to wall-clock and modified-detection is a no-op until
/// SAF `lastModified` is wired over JNI (scoped follow-up). On mobile the
/// roost is normally the sync client and sync already re-indexes received
/// files, so the desktop-shaped sync-off scenario is the one that mattered.
/// Background half of the warm path: reconcile the served (last-session) DB
/// against what's actually on disk, then refresh the published resolver and
/// stamp the completion marker.
///
/// Runs detached so init_graph — and therefore the graph pane — never waits
/// on it (on iOS the off-app-added indexing after a roost switch took ~a
/// minute; see the warm-path comment in init_graph). Writes go to the cloned
/// per-roost DbInstance, so a roost switch mid-heal is harmless: this task
/// keeps healing the OLD roost's own DB, and the published-state swaps below
/// are guarded on the roost still being active.
/// DB-5: delay before the once-per-launch graph compaction runs, so it never
/// competes with the graph pane's first queries after a roost open.
const GRAPH_MAINTENANCE_DELAY_SECS: u64 = 150;
/// DB-5: run graph compaction at most this often (24h), tracked in `kv_meta`.
const GRAPH_MAINTENANCE_INTERVAL_SECS: i64 = 24 * 60 * 60;

/// DB-5: schedule opportunistic graph-DB compaction on a detached task so
/// `init_graph` never blocks startup. Waits out the delay, then — only if a
/// day has passed since the last run — takes the `GraphDb` mutex (so no Cozo
/// query is in flight) and runs `::compact`. See `schema::compact_graph_db`
/// for the honest note on what `::compact` does (and doesn't) reclaim on the
/// SQLite backend.
fn spawn_graph_maintenance(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(GRAPH_MAINTENANCE_DELAY_SECS)).await;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let state = app.state::<GraphDb>();
        // Compaction is synchronous; block_in_place keeps the runtime free.
        tokio::task::block_in_place(|| {
            let guard = match state.db.lock() {
                Ok(g) => g,
                Err(_) => return,
            };
            let Some(db) = guard.as_ref() else {
                return;
            };
            let due = match crate::db::schema::get_kv(db, "last_maintenance")
                .ok()
                .flatten()
                .and_then(|s| s.parse::<i64>().ok())
            {
                Some(prev) => now.saturating_sub(prev) >= GRAPH_MAINTENANCE_INTERVAL_SECS,
                None => true,
            };
            if !due {
                return;
            }
            match crate::db::schema::compact_graph_db(db) {
                Ok(()) => {
                    let _ = crate::db::schema::set_kv(db, "last_maintenance", &now.to_string());
                    tracing::info!("graph: DB compaction (::compact) complete");
                }
                Err(e) => tracing::warn!(error = %e, "graph: DB compaction failed"),
            }
        });
    });
}

fn spawn_warm_heal(
    app: AppHandle,
    roost_path: String,
    backend: std::sync::Arc<dyn crate::roostfs::RoostFs>,
) {
    // One heal per roost at a time: rapid A→B→A switching would otherwise
    // stack overlapping heals doing duplicate reads/writes. (Upserts are
    // idempotent, so overlap is wasteful rather than harmful — this is a
    // cost guard, not a correctness one.)
    static HEALS_IN_FLIGHT: std::sync::OnceLock<Mutex<std::collections::HashSet<String>>> =
        std::sync::OnceLock::new();
    let in_flight = HEALS_IN_FLIGHT.get_or_init(|| Mutex::new(std::collections::HashSet::new()));
    match in_flight.lock() {
        Ok(mut set) => {
            if !set.insert(roost_path.clone()) {
                tracing::debug!("graph: warm heal already in flight — skipping");
                return;
            }
        }
        Err(_) => return, // poisoned — skip rather than risk a double heal
    }

    tauri::async_runtime::spawn(async move {
        let started = std::time::Instant::now();
        let mut purged = 0usize;
        let mut added = 0usize;
        let mut modified = 0usize;
        match crate::roostfs::walk::list_markdown_path_pairs(backend.as_ref()).await {
            Ok(pairs) => {
                let state = app.state::<GraphDb>();
                let still_active = state
                    .roost_path
                    .lock()
                    .ok()
                    .map(|g| g.as_deref() == Some(roost_path.as_str()))
                    .unwrap_or(false);

                if still_active {
                    // (a) Purge ghosts under the same mutex used by saves and
                    // graph reads. The old cloned-DbInstance path could mutate
                    // concurrently with an editor save and publish a torn view.
                    let disk_abs: Vec<String> = pairs.iter().map(|(abs, _)| abs.clone()).collect();
                    let purge_result = state
                        .db
                        .lock()
                        .map_err(|e| format!("Lock error: {e}"))
                        .and_then(|guard| {
                            let active = state
                                .roost_path
                                .lock()
                                .map_err(|e| format!("Lock error: {e}"))?
                                .as_deref()
                                == Some(roost_path.as_str());
                            if !active {
                                return Ok(0);
                            }
                            let db = guard
                                .as_ref()
                                .ok_or("Graph DB not initialized".to_string())?;
                            let n = queries::reconcile_deleted_notes(db, &roost_path, &disk_abs)?;
                            if n > 0 {
                                // Ghosts left the graph: drop the cached
                                // resolver ids + stats snapshot while we still
                                // hold the db lock, so a save racing this heal
                                // can't patch a base that still counts them.
                                state.invalidate_link_resolver();
                                state.invalidate_stats_cache();
                            }
                            Ok(n)
                        });
                    match purge_result {
                        Ok(n) => {
                            purged = n;
                            if n > 0 {
                                tracing::info!(
                                    purged = n,
                                    "graph: purged ghost notes deleted off-app"
                                );
                            }
                        }
                        Err(e) => tracing::warn!(error = %e, "graph: warm reconcile failed"),
                    }

                    // (b) Read changed files without the DB lock, then
                    // revalidate and commit under it. A save that wins the race
                    // advances the stored mtime/id and causes the prepared stale
                    // read to be skipped instead of overwriting fresh rows.
                    let (a, m) = reconcile_added_and_modified_notes(
                        &state,
                        &roost_path,
                        backend.as_ref(),
                        &pairs,
                    )
                    .await;
                    added = a;
                    modified = m;

                    // (c) Weekly edit-history upkeep (CO-2): orphan hash rows,
                    // history of notes deleted over a year ago, the global
                    // byte budget. Starts after a delay and is gated to once
                    // per 7 days, so it never competes with the first load.
                    crate::commands::edit_history::spawn_history_maintenance(
                        app.clone(),
                        roost_path.clone(),
                        backend.clone(),
                        disk_abs,
                    );
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "graph: warm-path listing failed; skipping reconcile");
            }
        }

        // Stamp the completion marker (moved from the old inline path — same
        // best-effort semantics: stamped even when the listing failed, since
        // the served DB itself is known-good).
        {
            let state = app.state::<GraphDb>();
            if let Ok(guard) = state.db.lock() {
                let still_active = state
                    .roost_path
                    .lock()
                    .ok()
                    .map(|g| g.as_deref() == Some(roost_path.as_str()))
                    .unwrap_or(false);
                if still_active {
                    if let Some(db) = guard.as_ref() {
                        if let Err(e) = crate::db::schema::set_kv(db, "graph_index_complete", "1") {
                            tracing::warn!(error = %e, "graph: failed to stamp warm index-complete marker");
                        }
                    }
                }
            };
        }

        let elapsed_ms = started.elapsed().as_millis() as u64;

        // Record heal stats (IP-A8) and republish the resolver so saves resolve
        // links against any newly indexed notes — both only if this roost is
        // still the active one (a switch mid-heal means the published state
        // belongs to another roost now; clobbering it — or stamping this
        // roost's heal numbers over the new roost's — would be wrong).
        {
            let state = app.state::<GraphDb>();
            let still_active = state
                .roost_path
                .lock()
                .ok()
                .map(|g| g.as_deref() == Some(roost_path.as_str()))
                .unwrap_or(false);
            if still_active {
                state.record_heal(elapsed_ms, added as u64, modified as u64, purged as u64);
                // NEW-S2: the heal may have added/modified/purged notes off-app,
                // shifting note/link/tag counts and the task list. Drop the
                // cached stats snapshot so the unchanged-save fast path recomputes.
                if added + modified + purged > 0 {
                    state.invalidate_stats_cache();
                    use tauri_specta::Event;
                    if let Err(e) = (crate::events::GraphChanged {
                        added: added.min(u32::MAX as usize) as u32,
                        modified: modified.min(u32::MAX as usize) as u32,
                        purged: purged.min(u32::MAX as usize) as u32,
                    })
                    .emit(&app)
                    {
                        tracing::warn!(error = %e, "graph: failed to emit warm-heal change event");
                    }
                }
                let resolver_result = state
                    .db
                    .lock()
                    .map_err(|e| format!("Lock error: {e}"))
                    .and_then(|guard| {
                        let db = guard
                            .as_ref()
                            .ok_or("Graph DB not initialized".to_string())?;
                        LinkResolver::build(db)
                    });
                match resolver_result {
                    Ok(resolver) => {
                        if let Ok(mut lr) = state.link_resolver.lock() {
                            *lr = Some(resolver);
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "graph: post-heal resolver rebuild failed — save-path cache may lag until next open");
                    }
                }
            }
        }

        tracing::info!(
            elapsed_ms,
            added,
            modified,
            purged,
            "graph: warm heal complete"
        );
        if let Ok(mut set) = HEALS_IN_FLIGHT
            .get_or_init(|| Mutex::new(std::collections::HashSet::new()))
            .lock()
        {
            set.remove(&roost_path);
        }
    });
}

/// Minimum mtime advance (seconds) that counts as "modified while closed".
/// A small epsilon absorbs filesystem timestamp rounding (FAT is 2s-granular)
/// so an unchanged file — whose stored `updated_at` is its own last-indexed
/// mtime — never re-triggers a re-index loop. A real external edit moves the
/// mtime forward by far more (the app has to close, an editor open, and a save
/// land in between).
///
/// `pub(crate)` single source of truth — the FTS warm-heal
/// (`commands::search`) aliases this so graph and search can't drift on what
/// counts as an external edit.
pub(crate) const MODIFIED_MTIME_EPSILON: f64 = 2.0;

/// IP-2 + IP-A1: index notes ADDED to disk while closed (no graph node yet)
/// and re-index notes MODIFIED while closed (disk mtime advanced past the
/// stored `updated_at`). Added notes are read unconditionally; modified
/// detection is skipped on backends whose paths can't be stat'ed
/// (`file_mtime_secs` → `None`, i.e. Android SAF).
///
/// Cheap by construction: one `note_updated_ats` scan yields both the presence
/// set and the stored mtimes, and we only `read()` files that are new or
/// changed (usually none). Runs the same two passes as a save — pass1 for all
/// affected notes first so the freshly-built resolver sees their ids, then
/// pass2 to resolve links, then promote any dangling links waiting on them.
/// Both passes REPLACE the note's tags/links, so a modified note's stale edges
/// are removed rather than merely augmented.
///
/// Best-effort per note: a read / UTF-8 / index failure on one note logs and
/// continues.
/// Returns `(added, modified)` counts for the caller's heal-stats record.
async fn reconcile_added_and_modified_notes(
    db_state: &GraphDb,
    roost_path: &str,
    backend: &dyn crate::roostfs::RoostFs,
    disk_pairs: &[(String, String)],
) -> (usize, usize) {
    let stored = match db_state
        .db
        .lock()
        .map_err(|e| format!("Lock error: {e}"))
        .and_then(|guard| {
            let db = guard
                .as_ref()
                .ok_or("Graph DB not initialized".to_string())?;
            queries::note_updated_ats(db)
        }) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "graph: added/modified scan could not read note mtimes; skipping");
            return (0, 0);
        }
    };

    // Pass 1 for every added or modified note (must precede the resolver build
    // so their ids are visible when links are resolved).
    let mut affected: Vec<(String, String, bool, Option<f64>)> = Vec::new();
    for (abs, rel) in disk_pairs {
        let id = queries::note_id_from_abs_path(abs, roost_path);
        let (is_added, is_modified) = match stored.get(&id) {
            None => (true, false),
            Some(&prev) => (
                false,
                matches!(queries::file_mtime_secs(abs), Some(m) if m > prev + MODIFIED_MTIME_EPSILON),
            ),
        };
        if !is_added && !is_modified {
            continue;
        }
        match backend.read(rel).await {
            Ok(bytes) => match String::from_utf8(bytes) {
                Ok(content) => {
                    affected.push((
                        abs.clone(),
                        content,
                        is_added,
                        queries::file_mtime_secs(abs),
                    ));
                }
                Err(_) => tracing::warn!(%rel, "graph: added/modified note is not UTF-8; skipped"),
            },
            Err(e) => tracing::warn!(error = %e, %rel, "graph: could not read added/modified note"),
        }
    }

    if affected.is_empty() {
        return (0, 0);
    }

    let guard = match db_state.db.lock() {
        Ok(g) => g,
        Err(e) => {
            tracing::warn!(error = %e, "graph: added/modified commit lock failed");
            return (0, 0);
        }
    };
    let still_active = db_state
        .roost_path
        .lock()
        .ok()
        .map(|g| g.as_deref() == Some(roost_path))
        .unwrap_or(false);
    if !still_active {
        return (0, 0);
    }
    let Some(db) = guard.as_ref() else {
        return (0, 0);
    };
    let current = match queries::note_updated_ats(db) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "graph: added/modified revalidation failed");
            return (0, 0);
        }
    };
    affected.retain(|(abs, _, was_added, observed_mtime)| {
        let id = queries::note_id_from_abs_path(abs, roost_path);
        if *was_added {
            return !current.contains_key(&id);
        }
        match (current.get(&id), observed_mtime) {
            (Some(prev), Some(observed)) => *observed > *prev + MODIFIED_MTIME_EPSILON,
            _ => false,
        }
    });
    if affected.is_empty() {
        return (0, 0);
    }

    // DB-4: every affected note is indexed by ONE atomic transaction (note row
    // + mtime stamp + tags + tasks + links + dangling + promotions), so a crash
    // mid-heal leaves each note either fully old — stamp included, so the next
    // heal re-detects it — or fully new. The old shape (all pass1, resolver
    // build, all pass2) wrote the stamp in pass1, so a crash between the passes
    // left notes with a fresh stamp and no links that no later heal would touch.
    //
    // The two-pass ORDER requirement still holds, it just moves: links in note A
    // to a sibling B that was added in the same batch must resolve, so the
    // resolver is built once and EVERY affected id is made visible before the
    // first note is indexed (`add_id` is idempotent; the per-note index adds
    // its own again). A note whose index fails stays in the resolver for this
    // batch only — the post-heal resolver rebuild in `spawn_warm_heal`
    // republishes one from the DB — and is re-detected by the next heal.
    let mut resolver = match LinkResolver::build(db) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "graph: added/modified resolver build failed; links may lag");
            return (0, 0);
        }
    };
    for (abs, _, _, _) in &affected {
        resolver.add_id(&queries::note_id_from_abs_path(abs, roost_path));
    }

    let mut added = 0usize;
    let mut modified = 0usize;
    for (abs, content, was_added, _) in &affected {
        let note_id = queries::note_id_from_abs_path(abs, roost_path);
        match queries::index_note_full_with_resolver(db, roost_path, abs, content, &mut resolver) {
            Err(e) => {
                tracing::warn!(error = %e, %note_id, "graph: added/modified index failed");
                // Never leave a possibly-stale snapshot behind an error that
                // `continue`s past the end-of-loop invalidation (which only
                // fires when something was counted).
                db_state.invalidate_stats_cache();
                continue;
            }
            Ok(outcome) if !outcome.deltas_trusted => {
                // Committed, but measurements unreadable: counted below, and
                // the end-of-loop invalidation covers the cache; also make sure
                // the algo cache refreshes even if the wipe below fails.
                db_state.mark_algo_cache_dirty();
            }
            Ok(_) => {}
        }
        if *was_added {
            added += 1;
        } else {
            modified += 1;
        }
    }
    if added + modified > 0 {
        // Counts and the task list moved off-app; drop the snapshot while we
        // still hold the db lock so no save can patch a stale base.
        db_state.invalidate_stats_cache();
        if let Err(e) = queries::invalidate_algo_cache(db) {
            db_state.mark_algo_cache_dirty();
            tracing::warn!(error = %e, "graph: warm-heal cache invalidation failed; queued retry");
        }
    }
    (added, modified)
}

/// IP-A8: a human-readable index-health block for the diagnostic report.
///
/// Instant DB counts (indexed notes/links/tags, dangling links) + the last
/// warm-heal's outcome, plus a best-effort live disk-drift check (a path-only
/// walk, skipped when no roost/backend is open). Pulls the state it needs off
/// `app` so `get_diagnostic_report`'s JS-facing signature is unchanged. Never
/// fails the report — every lookup degrades to a note in the text.
pub async fn index_health_text(app: &AppHandle) -> String {
    use std::fmt::Write as _;
    let db_state = app.state::<GraphDb>();
    let roost_fs = app.state::<crate::commands::files::RoostFsState>();
    let mut out = String::new();

    let roost_path = db_state.roost_path.lock().ok().and_then(|g| g.clone());

    // Instant DB metrics (brief lock — no await held across it).
    let (stats, dangling) = match db_state.db.lock() {
        Ok(guard) => match guard.as_ref() {
            Some(db) => (
                queries::get_graph_stats(db).ok(),
                queries::dangling_count(db).ok(),
            ),
            None => (None, None),
        },
        Err(_) => (None, None),
    };
    match &stats {
        Some(s) => {
            let _ = writeln!(
                out,
                "- indexed: {} notes, {} links, {} tags",
                s.notes, s.links, s.tags
            );
            let _ = writeln!(out, "- dangling links: {}", dangling.unwrap_or(0));
        }
        None => {
            let _ = writeln!(out, "- graph DB not initialized (no roost open)");
        }
    }

    // Last warm-heal outcome (IP-A8 stats recorded by spawn_warm_heal).
    if db_state.last_heal_ran.load(Ordering::Relaxed) {
        let _ = writeln!(
            out,
            "- last heal: {} ms — {} added, {} modified, {} purged",
            db_state.last_heal_ms.load(Ordering::Relaxed),
            db_state.last_heal_added.load(Ordering::Relaxed),
            db_state.last_heal_modified.load(Ordering::Relaxed),
            db_state.last_heal_purged.load(Ordering::Relaxed),
        );
    } else {
        let _ = writeln!(out, "- last heal: none this session");
    }

    // Best-effort live disk drift — a path-only walk (no content reads).
    match (roost_path.as_ref(), roost_fs.current().await) {
        (Some(rp), Some(backend)) => {
            match crate::roostfs::walk::list_markdown_path_pairs(backend.as_ref()).await {
                Ok(pairs) => {
                    let disk_notes = pairs.len();
                    let indexed = stats.as_ref().map(|s| s.notes).unwrap_or(0);
                    let _ = writeln!(
                        out,
                        "- on disk: {disk_notes} notes (drift {:+})",
                        disk_notes as i64 - indexed as i64
                    );
                    // Stale = files whose mtime is newer than the indexed
                    // value (modified-while-closed, not yet healed). Native
                    // only; SAF paths aren't stat-able so this reports 0.
                    let stored = match db_state.db.lock() {
                        Ok(guard) => guard
                            .as_ref()
                            .and_then(|db| queries::note_updated_ats(db).ok())
                            .unwrap_or_default(),
                        Err(_) => Default::default(),
                    };
                    let stale = pairs
                        .iter()
                        .filter(|(abs, _)| {
                            let id = queries::note_id_from_abs_path(abs, rp);
                            matches!(
                                (stored.get(&id), queries::file_mtime_secs(abs)),
                                (Some(&prev), Some(m)) if m > prev + MODIFIED_MTIME_EPSILON
                            )
                        })
                        .count();
                    let _ = writeln!(out, "- stale (disk newer than index): {stale}");
                }
                Err(e) => {
                    let _ = writeln!(out, "- disk walk failed: {e}");
                }
            }
        }
        _ => {
            let _ = writeln!(out, "- disk drift: unavailable (no roost/backend open)");
        }
    }

    out
}

/// Re-index a single note after save.
///
/// Hot path on every save. Uses the cached `LinkResolver` in `GraphDb`
/// to avoid the `*notes{ id }` full scan that the standalone
/// `queries::index_note` does. The cache is built lazily on the first
/// call after roost open and mutated incrementally when a previously
/// unseen note id appears.
#[tauri::command]
#[specta::specta]
pub async fn index_note(
    path: String,
    content: String,
    db_state: State<'_, GraphDb>,
) -> Result<IndexNoteResponse, String> {
    validate_graph_path(&path)?;
    index_note_internal(&path, &content, &db_state)
}

/// Purge a note from the graph DB when it was deleted on THIS device via
/// sync (a peer deleted it). The local delete IPC already does this inside
/// `delete_file` → `purge_from_graph`; this is the symmetric entry point the
/// frontend calls from its `note_deleted` sync-event handler, so a synced
/// delete doesn't leave ghost backlinks / tags / `:task` rows / a graph node
/// behind on the receiving device.
///
/// `path` may be the roost-relative form the sync event emits OR an absolute
/// path — `note_id_from_abs_path` normalizes both to the same id (it strips
/// the roost root when present and otherwise uses the string as-is).
#[tauri::command]
#[specta::specta]
pub async fn purge_note_from_graph(
    path: String,
    db_state: State<'_, GraphDb>,
) -> Result<(), String> {
    validate_graph_path(&path)?;
    with_locked_db_blocking(&db_state, |db| {
        let note_id = {
            let roost_guard = db_state
                .roost_path
                .lock()
                .map_err(|e| format!("Lock error: {e}"))?;
            let roost_root = roost_guard.as_deref().ok_or("Roost path not set")?;
            queries::note_id_from_abs_path(&path, roost_root)
        };
        // `purge_note_cached` also drops the cached resolver (an id vanished)
        // and the stats snapshot (a node, its edges and tasks just left) while
        // we still hold the db lock.
        db_state.purge_note_cached(db, &note_id).map_err(|errs| {
            for e in &errs {
                tracing::warn!(%note_id, error = %e, "purge_note_from_graph: relation failed");
            }
            format!(
                "Graph purge was incomplete for {note_id}: {}",
                errs.join("; ")
            )
        })
    })
}

/// AUDIT-053: shared internals for the graph half of a save. Used by
/// the legacy `index_note` IPC (above) AND by the bundled
/// `save_roost_note` IPC in `commands::save`. Same rationale as
/// `commands::files::write_file_internal`: one source of truth for the
/// graph re-index sequence so future post-save hooks (concept
/// extraction, episodic memory, anything Phase 3) land in one place.
///
/// Returns the `IndexNoteResponse` shape so the legacy IPC and the
/// bundled IPC return identically-shaped graph data to the frontend
/// (`save_roost_note` then adds the FTS5 + backlinks fields on top).
/// IP-A6: did a save actually change the graph's link topology?
///
/// The three cached graph algorithms — PageRank, Louvain community
/// detection, Brandes betweenness — are ALL global functions of the entire
/// `*links` set: a single edge insertion/deletion can shift every node's
/// PageRank, reassign communities, and change betweenness. So there is no
/// correct *per-algo* selective invalidation — no algorithm's cache can be
/// kept across a real link change. What we CAN skip is invalidation for a
/// save that didn't touch the graph at all: a body-only edit (prose changed,
/// no wiki-link added / removed / retargeted) leaves every cached result
/// valid.
///
/// Returns true when a new node was added OR the note's resolved outgoing
/// link set changed (compared as a set — order and duplicates don't matter).
/// Pure, so it's unit-tested without a DB harness.
fn link_topology_changed(
    is_new_node: bool,
    old_targets: &[String],
    new_targets: &[String],
) -> bool {
    if is_new_node {
        return true;
    }
    let mut a: Vec<&str> = old_targets.iter().map(String::as_str).collect();
    let mut b: Vec<&str> = new_targets.iter().map(String::as_str).collect();
    a.sort_unstable();
    a.dedup();
    b.sort_unstable();
    b.dedup();
    a != b
}

pub(crate) fn index_note_internal(
    path: &str,
    content: &str,
    db_state: &State<'_, GraphDb>,
) -> Result<IndexNoteResponse, String> {
    // DB-2/DB-3: the save path holds the graph mutex across the per-note index
    // transaction and the post-save stats/tasks fold, so block_in_place keeps a
    // slow save from parking the tokio worker and stalling sync/IPC.
    // (block_in_place is safe here: every caller — `index_note`,
    // `save_roost_note`, `reindex_moved_folder` — is an async task on the
    // multi-threaded runtime.)
    //
    // DB-3 (resolved): the graph-wide totals and the full task list are NOT
    // recomputed here any more. `GraphDb` keeps a cached snapshot that the index
    // patches per note (exact count deltas + that note's task rows — see
    // `GraphDb::apply_outcome_to_stats_cache`), and every mutation that doesn't
    // go through the index invalidates it. A save therefore costs O(this note)
    // of DB work; only the very first save after an invalidation re-reads the
    // roost, once. Residual, by IPC contract: the response still carries the
    // WHOLE task list, so serializing it scales with total tasks (not notes).
    tokio::task::block_in_place(|| index_note_on_graph(path, content, db_state.inner()))
}

/// The save-path index against a plain `&GraphDb` (so tests can drive the real
/// hot path without a Tauri `State`).
fn index_note_on_graph(
    path: &str,
    content: &str,
    graph: &GraphDb,
) -> Result<IndexNoteResponse, String> {
    let started = std::time::Instant::now();
    let db_guard = graph.db.lock().map_err(|e| format!("Lock error: {e}"))?;
    let db = db_guard.as_ref().ok_or("Graph DB not initialized")?;
    let roost_guard = graph
        .roost_path
        .lock()
        .map_err(|e| format!("Lock error: {e}"))?;
    let roost_root = roost_guard.as_deref().ok_or("Roost path not set")?;
    crate::commands::files::to_rel(path, roost_root)
        .ok_or_else(|| "Note path is outside the active roost".to_string())?;

    // One atomic transaction for the whole note (note row + stamp, tags, tasks,
    // links, dangling rows, and — for a brand-new note — the promotion of links
    // that were waiting on it, so their backlinks appear immediately). Also
    // adds this note's id to the cached resolver and patches the stats cache.
    let outcome = graph.index_note_core(db, roost_root, path, content)?;
    let new_id = outcome.note_id.clone();

    // IP-A6: only invalidate the algo cache when the save ACTUALLY changed the
    // link topology. PageRank / community / betweenness are all global
    // functions of the full link set, so any edge change can shift all three —
    // there is no correct per-algo split (see `link_topology_changed`). But a
    // body-only edit leaves the graph identical, so it must not force a
    // recompute. When topology did change we just flip the dirty bit (one
    // atomic, no DB work) — the cache-read paths wipe and recompute lazily on
    // the next read (never serving stale results; see
    // `refresh_algo_cache_if_dirty`). This refines the old always-mark behavior
    // that recomputed all three algorithms after every prose-only autosave.
    if link_topology_changed(
        outcome.was_new,
        &outcome.link_targets_before,
        &outcome.link_targets_after,
    ) {
        graph.mark_algo_cache_dirty();
    }

    // (Embeddings subsystem removed — notes are no longer enqueued for
    // async embedding on save. Retrieval is FTS-tool-driven now.)

    // Reschedule notification timers for this note's tasks. Cheap —
    // typical note has 0–10 tasks, and the scheduler scopes to note_id.
    graph
        .notification_scheduler
        .reschedule_for_note(db, &new_id);

    // Fold both the post-save graph stats AND the full task list into
    // one response. Without this the save path fires three IPCs:
    // index_note + get_graph_stats + list_tasks. On Android each
    // round-trip hops the WebView/JNI bridge, so collapsing the trio is
    // a measurable mobile win. Both come from the patched cache (O(note));
    // a cold cache is recomputed once under the lock we already hold.
    let (stats, tasks) = graph.stats_snapshot_or_compute(db)?;

    tracing::info!(
        note_id = %new_id,
        tasks = tasks.len(),
        elapsed_ms = started.elapsed().as_millis() as u64,
        "graph: index_note done"
    );

    Ok(IndexNoteResponse { stats, tasks })
}

/// Get backlinks for a note (notes that link TO this note).
#[tauri::command]
#[specta::specta]
pub async fn get_backlinks(
    note_path: String,
    db_state: State<'_, GraphDb>,
) -> Result<Vec<BacklinkResult>, String> {
    validate_graph_path(&note_path)?;
    with_locked_db_blocking(&db_state, |db| {
        let Some(note_id) = queries::lookup_note_id_by_path(db, &note_path)? else {
            return Ok(Vec::new());
        };
        let entries = queries::get_backlinks(db, &note_id)?;
        Ok(entries
            .into_iter()
            .map(|e| BacklinkResult {
                title: e.title,
                path: e.path,
            })
            .collect())
    })
}

/// Get forward links for a note (notes this note links TO).
#[tauri::command]
#[specta::specta]
pub async fn get_note_links(
    note_path: String,
    db_state: State<'_, GraphDb>,
) -> Result<Vec<ForwardLinkResult>, String> {
    validate_graph_path(&note_path)?;
    with_locked_db_blocking(&db_state, |db| {
        let Some(note_id) = queries::lookup_note_id_by_path(db, &note_path)? else {
            return Ok(Vec::new());
        };
        let entries = queries::get_note_links(db, &note_id)?;
        Ok(entries
            .into_iter()
            .map(|e| ForwardLinkResult {
                target: e.target,
                link_text: e.link_text,
            })
            .collect())
    })
}

/// PageRank — top hub notes ordered by rank descending. Returns from
/// the cache when warm; recomputes the full graph and rewrites the
/// cache when cold (after roost open or a save invalidation).
#[tauri::command]
#[specta::specta]
pub async fn graph_page_rank(
    limit: u32,
    db_state: State<'_, GraphDb>,
) -> Result<Vec<queries::PageRankEntry>, String> {
    let limit = (limit as usize).min(MAX_GRAPH_ALGO_RESULTS);
    if limit == 0 {
        return Ok(Vec::new());
    }
    with_locked_db_blocking(&db_state, |db| {
        db_state.refresh_algo_cache_if_dirty(db)?;
        let cached = queries::cached_page_rank(db, limit)?;
        if !cached.is_empty() {
            return Ok(cached);
        }
        queries::recompute_page_rank(db, limit)
    })
}

/// Louvain community labels for every note. Cluster coloring in the
/// graph view consumes this directly — same caching pattern as
/// `graph_page_rank`.
#[tauri::command]
#[specta::specta]
pub async fn graph_communities(
    limit: u32,
    db_state: State<'_, GraphDb>,
) -> Result<Vec<queries::CommunityEntry>, String> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    with_locked_db_blocking(&db_state, |db| {
        db_state.refresh_algo_cache_if_dirty(db)?;
        let limit = (limit as usize).min(MAX_GRAPH_ALGO_RESULTS);
        let mut cached = queries::cached_community_detection(db)?;
        if !cached.is_empty() {
            cached.truncate(limit);
            return Ok(cached);
        }
        let stats = queries::get_graph_stats(db)?;
        if stats.notes > MAX_AUTO_COMMUNITY_NOTES {
            tracing::info!(
                notes = stats.notes,
                max = MAX_AUTO_COMMUNITY_NOTES,
                "graph: skipping cold Louvain auto-compute for large roost"
            );
            return Ok(Vec::new());
        }
        let mut computed = queries::recompute_community_detection(db)?;
        computed.truncate(limit);
        Ok(computed)
    })
}

/// BetweennessCentrality — bridge notes ordered by score descending.
/// Cozo's docs warn this is expensive on medium/large graphs, so the
/// cache is even more important than for the others. Limit clamps the
/// returned count, but the cache stores the full result so the next
/// call with a higher limit is cheap.
///
/// AUDIT-025: cache MISS no longer auto-recomputes. Brandes is O(V·E) —
/// billions of operations at 50K notes, which froze the graph view for
/// minutes on a cold cache. The opening flow returns an empty cache hit.
/// `compute_bridges_now` remains an explicit, size-gated IPC for a future
/// bridge-inspection UI; Graph View does not invoke it during normal load.
#[tauri::command]
#[specta::specta]
pub async fn graph_bridges(
    limit: u32,
    db_state: State<'_, GraphDb>,
) -> Result<Vec<queries::CentralityEntry>, String> {
    let limit = (limit as usize).min(MAX_BRIDGE_RESULTS);
    if limit == 0 {
        return Ok(Vec::new());
    }
    with_locked_db_blocking(&db_state, |db| {
        db_state.refresh_algo_cache_if_dirty(db)?;
        queries::cached_betweenness_centrality(db, limit)
    })
}

/// AUDIT-025: explicit user-triggered bridges recompute. Blocking — the
/// frontend should surface a busy indicator until this returns. Emits a
/// `graph:bridges-progress` event so the UI can render a percentage if
/// the implementation later grows incremental support; today the event
/// fires once at start (so the spinner can latch) and once at completion.
#[tauri::command]
#[specta::specta]
pub async fn compute_bridges_now(
    limit: u32,
    app: AppHandle,
    db_state: State<'_, GraphDb>,
) -> Result<Vec<queries::CentralityEntry>, String> {
    let limit = (limit as usize).min(MAX_BRIDGE_RESULTS);
    if limit == 0 {
        return Ok(Vec::new());
    }
    use tauri_specta::Event;
    let _ = crate::events::GraphBridgesProgress("started".to_string()).emit(&app);
    let result = with_locked_db_blocking(&db_state, |db| {
        let stats = queries::get_graph_stats(db)?;
        if stats.notes > MAX_BRIDGE_COMPUTE_NOTES || stats.links > MAX_BRIDGE_COMPUTE_LINKS {
            return Err(format!(
                "Bridge computation is limited to {MAX_BRIDGE_COMPUTE_NOTES} notes / {MAX_BRIDGE_COMPUTE_LINKS} links; this graph has {} notes / {} links",
                stats.notes, stats.links
            ));
        }
        queries::recompute_betweenness_centrality(db, limit)
    });
    let _ = crate::events::GraphBridgesProgress("done".to_string()).emit(&app);
    result
}

/// Shortest path between two notes in the wiki-link graph. Treated as
/// undirected. Returns `None` when the two notes are unreachable from
/// each other. Per-query (not cached) — paths are short-lived.
#[tauri::command]
#[specta::specta]
pub async fn graph_shortest_path(
    from_id: String,
    to_id: String,
    db_state: State<'_, GraphDb>,
) -> Result<Option<queries::ShortestPathResult>, String> {
    if from_id.len() > MAX_GRAPH_ID_BYTES || to_id.len() > MAX_GRAPH_ID_BYTES {
        return Err("Graph note id is too long".to_string());
    }
    with_locked_db_blocking(&db_state, |db| queries::shortest_path(db, &from_id, &to_id))
}

/// Frontend-friendly variant of `graph_shortest_path`: takes absolute
/// note paths (what the frontend already has in `roostStore`) instead
/// of canonical note ids, and returns rows hydrated with `{id, title,
/// path}` so the renderer doesn't need a second round-trip to label
/// each step. Includes `elapsed_ms` for the path-render UI.
#[derive(Debug, Serialize, Clone, specta::Type)]
pub struct ShortestPathBetweenResult {
    pub cost: f64,
    pub path: Vec<queries::NoteIndexEntry>,
    #[specta(type = u32)]
    pub elapsed_ms: u64,
}

#[tauri::command]
#[specta::specta]
pub async fn graph_shortest_path_between(
    from_path: String,
    to_path: String,
    db_state: State<'_, GraphDb>,
) -> Result<Option<ShortestPathBetweenResult>, String> {
    validate_graph_path(&from_path)?;
    validate_graph_path(&to_path)?;
    let started = std::time::Instant::now();
    let outcome = with_locked_db_blocking(&db_state, |db| {
        let Some(from_id) = queries::lookup_note_id_by_path(db, &from_path)? else {
            return Ok(None);
        };
        let Some(to_id) = queries::lookup_note_id_by_path(db, &to_path)? else {
            return Ok(None);
        };
        let Some(raw) = queries::shortest_path(db, &from_id, &to_id)? else {
            return Ok(None);
        };
        if raw.path.is_empty() {
            return Ok(None);
        }
        let hydrated = queries::lookup_notes_by_ids(db, &raw.path)?;
        Ok(Some((raw.cost, hydrated)))
    })?;
    let Some((cost, hydrated)) = outcome else {
        return Ok(None);
    };
    let elapsed_ms = started.elapsed().as_millis() as u64;
    tracing::debug!(
        "graph_shortest_path_between: {} hops, cost={}, elapsed={}ms",
        hydrated.len(),
        cost,
        elapsed_ms
    );
    Ok(Some(ShortestPathBetweenResult {
        cost,
        path: hydrated,
        elapsed_ms,
    }))
}

/// Get all tags for a note.
#[tauri::command]
#[specta::specta]
pub async fn get_note_tags(
    note_path: String,
    db_state: State<'_, GraphDb>,
) -> Result<Vec<String>, String> {
    validate_graph_path(&note_path)?;
    with_locked_db_blocking(&db_state, |db| {
        let Some(note_id) = queries::lookup_note_id_by_path(db, &note_path)? else {
            return Ok(Vec::new());
        };
        queries::get_note_tags(db, &note_id)
    })
}

/// Get graph-wide statistics.
#[tauri::command]
#[specta::specta]
pub async fn get_graph_stats(db_state: State<'_, GraphDb>) -> Result<GraphStatsResult, String> {
    with_locked_db_blocking(&db_state, |db| {
        let stats = queries::get_graph_stats(db)?;
        Ok(GraphStatsResult {
            notes: stats.notes,
            links: stats.links,
            tags: stats.tags,
        })
    })
}

// AUDIT-024: the original `get_full_graph` IPC sent the entire roost with
// no bound — full-roost payloads don't survive force layout past ~10K
// notes, so it was removed in favor of `get_graph_window`.
//
// Restored below for Graph View's "Full" mode, but now **capped** at
// `max_nodes`: the window query can't render disconnected notes (a BFS
// from one node only reaches that node's connected component), so on a
// roost whose notes aren't linked yet "Full" collapsed to a single node.
// The cap (driven by `FULL_MAX_NODES` on the frontend, 1000) keeps the
// scale guarantee that motivated AUDIT-024 while still showing orphans.

// Whole-roost graph for "Full" mode: every note as a node (orphans
// included), capped at `max_nodes` so large roosts stay renderable. Use
// `get_graph_window` for focused/neighborhood views (Local + search).
//
// NOTE: plain `//` (not `///`) on purpose. tauri-specta renders a `///`
// doc comment into a `/** */` block in bindings.ts; this file's binding
// line was hand-written (the gen_bindings example can't run on the Windows
// dev box — DLL load error), so a doc comment here would desync the
// committed bindings.ts from the generated one and fail
// `bindings_are_up_to_date`. Keep it `//` until bindings can be regenerated.
#[tauri::command]
#[specta::specta]
pub async fn get_full_graph(
    max_nodes: u32,
    db_state: State<'_, GraphDb>,
) -> Result<FullGraphResult, String> {
    let max_nodes = (max_nodes as usize).min(MAX_FULL_GRAPH_NOTES);
    let started = std::time::Instant::now();
    let graph = with_locked_db_blocking(&db_state, |db| queries::get_full_graph(db, max_nodes))?;
    tracing::info!(
        max_nodes,
        nodes = graph.nodes.len(),
        edges = graph.edges.len(),
        elapsed_ms = started.elapsed().as_millis() as u64,
        "full_graph: done"
    );
    Ok(FullGraphResult {
        nodes: graph
            .nodes
            .into_iter()
            .map(|n| GraphNodeResult {
                id: n.id,
                title: n.title,
                path: n.path,
                kind: n.kind,
                link_count: n.link_count,
            })
            .collect(),
        edges: graph
            .edges
            .into_iter()
            .map(|e| GraphEdgeResult {
                source: e.source,
                target: e.target,
            })
            .collect(),
    })
}

/// Viewport-bounded graph fetch. Returns `focus_id` plus its `depth`-hop
/// neighborhood, capped at `max_nodes`. Use this for graph rendering at
/// any scale — `get_full_graph` does not survive past ~10K notes.
///
/// Sensible defaults from the frontend: `depth = 2`, `max_nodes = 300`.
#[tauri::command]
#[specta::specta]
pub async fn get_graph_window(
    focus_id: String,
    depth: u32,
    max_nodes: u32,
    db_state: State<'_, GraphDb>,
) -> Result<FullGraphResult, String> {
    if focus_id.len() > MAX_GRAPH_ID_BYTES {
        return Err("Graph focus id is too long".to_string());
    }
    let depth = (depth as usize).min(MAX_GRAPH_DEPTH);
    let max_nodes = (max_nodes as usize).min(MAX_GRAPH_WINDOW_NODES);
    let started = std::time::Instant::now();
    let graph = with_locked_db_blocking(&db_state, |db| {
        queries::get_graph_window(db, &focus_id, depth, max_nodes)
    })?;
    tracing::info!(
        focus = %focus_id,
        depth,
        max_nodes,
        nodes = graph.nodes.len(),
        edges = graph.edges.len(),
        elapsed_ms = started.elapsed().as_millis() as u64,
        "graph_window: done"
    );
    Ok(FullGraphResult {
        nodes: graph
            .nodes
            .into_iter()
            .map(|n| GraphNodeResult {
                id: n.id,
                title: n.title,
                path: n.path,
                kind: n.kind,
                link_count: n.link_count,
            })
            .collect(),
        edges: graph
            .edges
            .into_iter()
            .map(|e| GraphEdgeResult {
                source: e.source,
                target: e.target,
            })
            .collect(),
    })
}

/// Whole-roost indexer fixture:
///  1. Walk every `.md` under the roost and make EVERY note id visible to the
///     link resolver.
///  2. Index each note (one atomic transaction per note) resolving its
///     wiki-links with nearest-folder match (Obsidian semantics).
///
/// Seeing every candidate id before the first note is indexed is the only
/// reliable way to resolve `[[shared-meals]]` when a roost contains many files
/// with the same basename across daily folders — the resolver needs every
/// candidate before it can pick the closest one.
///
/// TEST-ONLY (sharp-edges F6, 2026-11-11). This was `#[allow(dead_code)]`
/// "future bulk re-index entry point": a complete raw-`std::fs` walker +
/// reader sitting in `commands/`, one `git revert` away from reintroducing
/// unchecked reads on desktop and a hard break on Android SAF. The shipped
/// indexer goes through `RoostFs`; this remains only as the disk-backed
/// fixture its tests round-trip through, where raw `std::fs` on a `TempDir`
/// is fine and cannot reach production.
#[cfg(test)]
fn index_roost_two_pass(db: &DbInstance, roost_root: &str) -> Result<(), String> {
    let mut md_files: Vec<String> = Vec::new();
    collect_md_files(roost_root, &mut md_files)?;

    // Every id is visible to the resolver BEFORE any note is indexed (the same
    // ordering the warm heal uses), so a link to a sibling that sorts later in
    // the walk still resolves; each note is then ONE atomic transaction.
    let mut resolver = queries::LinkResolver::build(db)?;
    for abs_path in &md_files {
        resolver.add_id(&queries::note_id_from_abs_path(abs_path, roost_root));
    }
    for abs_path in &md_files {
        let content = std::fs::read_to_string(abs_path)
            .map_err(|e| format!("Failed to read {abs_path}: {e}"))?;
        queries::index_note_full_with_resolver(db, roost_root, abs_path, &content, &mut resolver)?;
    }

    Ok(())
}

/// AUDIT-051: nuke the on-disk `graph.db` and re-walk the roost to
/// rebuild it from scratch. The user's markdown files are the source
/// of truth — every relation in CozoDB is derivable from them — so a
/// rebuild is non-destructive to user data. Embeddings ARE lost
/// (cost: one re-embed pass), but stale embeddings on a corrupt DB
/// would be worse.
///
/// Frontend should only call this after surfacing a confirmation
/// dialog. The IPC itself trusts the call.
#[tauri::command]
#[specta::specta]
pub async fn rebuild_database(
    roost_path: String,
    app: AppHandle,
    roost_fs: State<'_, crate::commands::files::RoostFsState>,
    db_state: State<'_, GraphDb>,
    search_state: State<'_, crate::commands::search::SearchState>,
) -> Result<GraphStatsResult, String> {
    validate_graph_path(&roost_path)?;
    // Renderer paths are identity assertions, never authority. Validate the
    // complete repair target against the mounted backend before clearing a
    // single live handle or deleting an app-data artifact.
    let generation = roost_fs.generation();
    let backend = roost_fs
        .current()
        .await
        .ok_or_else(|| "No roost is open".to_string())?;
    let active_roost = backend.display_root().to_string();
    if !crate::commands::roost::same_roost_root(&active_roost, Path::new(&roost_path)) {
        return Err("Repair request does not match the active roost".to_string());
    }
    let db_dir = crate::paths::roost_db_dir(&app, &active_roost)?;
    if !roost_fs.is_generation_current(generation) {
        return Err("Roost changed before database repair started".to_string());
    }
    tracing::warn!("graph: rebuild_database requested — nuking graph.db + search.db");

    // 1. Drop the in-memory handle first so the file isn't held open
    //    when we try to delete it. Same for the cached resolver and
    //    roost_path — they reference the soon-to-die DB.
    {
        let mut db_guard = db_state.db.lock().map_err(|e| format!("Lock error: {e}"))?;
        *db_guard = None;
    }
    {
        let mut resolver_guard = db_state
            .link_resolver
            .lock()
            .map_err(|e| format!("Lock error: {e}"))?;
        *resolver_guard = None;
    }
    // The cached totals/task list describe the graph being nuked.
    db_state.invalidate_stats_cache();
    // IP-A5: drop the FTS handle too so `search.db` isn't held open when we
    // delete it below. Repair must clear FTS ghosts (a note deleted off-app
    // leaves a stale search row the graph rebuild alone wouldn't touch), not
    // just the graph. The frontend resets search-readiness after a repair so
    // the next search re-runs a cold FTS index over the rebuilt notes.
    search_state.clear();

    // 2. Delete the on-disk artifacts. CozoDB's SQLite backend also
    //    writes `-wal` and `-shm` sidecar files; leaving them behind
    //    after deleting `graph.db` produces "database disk image is
    //    malformed" on the next open. Remove all three best-effort.
    //    `search.db` (+ its WAL/SHM) is deleted alongside so Repair fully
    //    rebuilds the FTS index instead of serving stale rows.
    let db_path = db_dir.join("graph.db");
    let wal_path = db_dir.join("graph.db-wal");
    let shm_path = db_dir.join("graph.db-shm");
    let search_db_path = db_dir.join("search.db");
    let search_wal_path = db_dir.join("search.db-wal");
    let search_shm_path = db_dir.join("search.db-shm");
    let mut removal_errors = Vec::new();
    for p in [
        &db_path,
        &wal_path,
        &shm_path,
        &search_db_path,
        &search_wal_path,
        &search_shm_path,
    ] {
        if p.exists() {
            if let Err(e) = std::fs::remove_file(p) {
                tracing::warn!(path = %p.display(), error = %e, "rebuild_database: remove failed");
                removal_errors.push(format!("{}: {e}", p.display()));
            }
        }
    }

    // 3. Re-run init_graph against the now-empty directory. Reuses
    //    the same cold-path machinery — bulk_index_pass1/2 — so we
    //    don't duplicate the walk logic.
    let restored = init_graph(active_roost, app, roost_fs, db_state).await;
    if !removal_errors.is_empty() {
        let restore_note = match &restored {
            Ok(_) => "The previous index was reopened so the app remains usable.".to_string(),
            Err(e) => format!("Reopening the index also failed: {e}"),
        };
        return Err(format!(
            "Database repair could not remove every index artifact: {}. {restore_note}",
            removal_errors.join("; ")
        ));
    }
    let stats = restored?;
    tracing::info!(
        notes = stats.notes,
        links = stats.links,
        "graph: rebuild_database complete"
    );
    Ok(stats)
}

/// Collect all `.md` files under the roost, recursing into subdirectories.
/// Skips hidden entries (anything starting with `.`).
/// Test-only, like `index_roost_two_pass` (its only caller).
#[cfg(test)]
fn collect_md_files(dir: &str, out: &mut Vec<String>) -> Result<(), String> {
    let entries = std::fs::read_dir(dir).map_err(|e| format!("Read dir error: {e}"))?;
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        if path.is_dir() {
            collect_md_files(&path.to_string_lossy(), out)?;
        } else if crate::roostfs::walk::is_markdown_path(&path) {
            // EDT-3: shared predicate (was `.md`-only, dropped `.markdown`).
            out.push(path.to_string_lossy().to_string());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    //! The Tauri command handlers in this file are thin wrappers over
    //! `db::queries::*` (PageRank, communities, shortest-path, window),
    //! each of which already has its own unit tests in `db/queries/`, and
    //! they require a managed `GraphDb` State stack to call directly. What
    //! lives ONLY here is the standalone two-pass disk indexer
    //! (`index_roost_two_pass` + its `collect_md_files` walk), so that's
    //! what these tests lock down: the walk's filtering rules and a real
    //! note→link round-trip through CozoDB.
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn graph_path_validation_rejects_empty_and_oversized_inputs() {
        assert!(validate_graph_path("").is_err());
        assert!(validate_graph_path(&"x".repeat(MAX_GRAPH_PATH_BYTES + 1)).is_err());
        assert!(validate_graph_path("notes/example.md").is_ok());
    }

    #[test]
    fn algo_cache_dirty_bit_is_one_shot() {
        // The fix for the audit's H finding hinges on this: the dirty bit is
        // consumed (cleared) by the FIRST cache read after a change, so a
        // later read of a different algo doesn't re-wipe — it relies on the
        // wipe having already cleared every algo's cache rows. `swap` gives us
        // that atomic check-and-clear.
        let g = GraphDb::new();
        assert!(
            !g.algo_cache_dirty.swap(false, Ordering::Relaxed),
            "starts clean"
        );
        g.mark_algo_cache_dirty();
        assert!(
            g.algo_cache_dirty.swap(false, Ordering::Relaxed),
            "first read sees dirty"
        );
        assert!(
            !g.algo_cache_dirty.swap(false, Ordering::Relaxed),
            "subsequent reads see clean (one-shot)"
        );
    }

    // ── IP-A6: link_topology_changed ────────────────────────────────

    #[test]
    fn topology_change_new_node_always_dirties() {
        // A brand-new note is a new graph node — always a topology change,
        // regardless of its (empty) old link set.
        assert!(link_topology_changed(true, &[], &[]));
        assert!(link_topology_changed(true, &[], &["a".to_string()]));
    }

    #[test]
    fn topology_unchanged_when_links_identical() {
        // Body-only edit: same outgoing links before and after → no wipe.
        let old = vec!["a".to_string(), "b".to_string()];
        let new = vec!["a".to_string(), "b".to_string()];
        assert!(!link_topology_changed(false, &old, &new));
    }

    #[test]
    fn topology_unchanged_ignores_order_and_dupes() {
        // Set semantics: reordering or a duplicate edge row is not a change.
        let old = vec!["b".to_string(), "a".to_string()];
        let new = vec!["a".to_string(), "b".to_string(), "a".to_string()];
        assert!(!link_topology_changed(false, &old, &new));
    }

    #[test]
    fn topology_changed_on_added_removed_or_retargeted_link() {
        // Added edge.
        assert!(link_topology_changed(
            false,
            &["a".to_string()],
            &["a".to_string(), "b".to_string()]
        ));
        // Removed edge.
        assert!(link_topology_changed(
            false,
            &["a".to_string(), "b".to_string()],
            &["a".to_string()]
        ));
        // Retargeted edge (X → Y).
        assert!(link_topology_changed(
            false,
            &["x".to_string()],
            &["y".to_string()]
        ));
    }

    #[test]
    fn index_note_full_cached_reuses_warm_resolver_and_rebuilds_after_invalidation() {
        // IP-A3 / TRN-7: the auxiliary write path must reuse GraphDb's cached
        // resolver, rebuilding ONLY when it has been invalidated. We prove both
        // halves by feeding notes the cached resolver was never told about.
        let roost = TempDir::new().unwrap();
        let db_dir = TempDir::new().unwrap();
        let db = crate::db::schema::init_db(db_dir.path()).expect("init_db");
        let root = roost.path().to_string_lossy().to_string();
        let abs = |name: &str| roost.path().join(name).to_string_lossy().to_string();

        let graph = GraphDb::new();

        // Cold cache (None): the first cached index builds the resolver fresh.
        // Seed a target via the fresh-build path (which does NOT touch GraphDb's
        // cached resolver) so the cold build has something to resolve.
        queries::index_note_full(&db, &root, &abs("target.md"), "# Target\n").unwrap();
        graph
            .index_note_full_cached(&db, &root, &abs("src.md"), "see [[target]]\n")
            .unwrap();
        assert_eq!(
            link_count(&db, "src", "target"),
            1,
            "cold cached index builds the resolver and resolves the link"
        );

        // Warm cache: add target2 straight to the DB WITHOUT teaching the
        // cached resolver (index_note_full is the fresh-build path; it leaves
        // GraphDb's cached resolver alone). A warm reuse cannot resolve it.
        queries::index_note_full(&db, &root, &abs("target2.md"), "# Target2\n").unwrap();
        graph
            .index_note_full_cached(&db, &root, &abs("src2.md"), "see [[target2]]\n")
            .unwrap();
        assert_eq!(
            link_count(&db, "src2", "target2"),
            0,
            "warm cached resolver was NOT rebuilt → cannot resolve target2 (proves reuse)"
        );

        // Invalidation forces a rebuild on the next cached index → resolves it.
        graph.invalidate_link_resolver();
        graph
            .index_note_full_cached(&db, &root, &abs("src2.md"), "see [[target2]]\n")
            .unwrap();
        assert_eq!(
            link_count(&db, "src2", "target2"),
            1,
            "after invalidation the cached index rebuilds and resolves target2"
        );
    }

    // ── collect_md_files (Result-returning, roost-init variant) ──────

    /// Walk recurses into subdirs, keeps only `.md`, and skips hidden
    /// dot-entries (both files and whole directories).
    #[test]
    fn collect_md_files_recurses_and_skips_hidden() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        std::fs::write(root.join("a.md"), "x").unwrap();
        std::fs::write(root.join("note.txt"), "skip").unwrap();
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("sub").join("b.md"), "y").unwrap();
        std::fs::create_dir_all(root.join(".hidden")).unwrap();
        std::fs::write(root.join(".hidden").join("c.md"), "z").unwrap();

        let mut out = Vec::new();
        collect_md_files(&root.to_string_lossy(), &mut out).expect("walk ok");
        out.sort();

        assert_eq!(out.len(), 2, "got {out:?}");
        assert!(out.iter().any(|p| p.ends_with("a.md")));
        assert!(out.iter().any(|p| p.ends_with("b.md")));
        assert!(!out.iter().any(|p| p.contains("c.md")));
    }

    /// A missing directory surfaces an error rather than silently
    /// returning nothing.
    #[test]
    fn collect_md_files_errors_on_missing_dir() {
        let mut out = Vec::new();
        let err = collect_md_files("/no/such/dir/anywhere", &mut out).unwrap_err();
        assert!(err.contains("Read dir error"));
    }

    // ── index_roost_two_pass (notes + resolved wiki-links) ──────────

    /// Build a real on-disk roost + a freshly-migrated CozoDB, run the
    /// two-pass indexer, and assert that (a) every note becomes a graph
    /// node and (b) a `[[wiki-link]]` resolves to a backlink edge. This
    /// is the one bit of graph logic that lives in this file rather than
    /// `db::queries`, so it's worth an end-to-end pin.
    #[test]
    fn two_pass_indexes_notes_and_resolves_links() {
        let roost = TempDir::new().unwrap();
        let db_dir = TempDir::new().unwrap();

        // Note A links to B by title; B is a leaf.
        std::fs::write(
            roost.path().join("alpha.md"),
            "# Alpha\n\nSee [[beta]] for details.\n",
        )
        .unwrap();
        std::fs::write(roost.path().join("beta.md"), "# Beta\n\nLeaf note.\n").unwrap();

        let db = crate::db::schema::init_db(db_dir.path()).expect("init_db");
        index_roost_two_pass(&db, &roost.path().to_string_lossy()).expect("two-pass");

        // Both notes indexed.
        let stats = queries::get_graph_stats(&db).expect("stats");
        assert_eq!(stats.notes, 2, "both notes should be nodes");
        assert!(stats.links >= 1, "the [[beta]] link should be an edge");

        // The link from alpha → beta must surface as a backlink ON beta.
        let beta_id = queries::note_id_from_abs_path(
            &roost.path().join("beta.md").to_string_lossy(),
            &roost.path().to_string_lossy(),
        );
        let backlinks = queries::get_backlinks(&db, &beta_id).expect("backlinks");
        assert_eq!(backlinks.len(), 1, "beta should have one backlink (alpha)");
        assert!(
            backlinks[0].path.to_lowercase().contains("alpha"),
            "backlink should originate from alpha, got {:?}",
            backlinks[0]
        );
    }

    /// An empty roost indexes to an empty graph — no notes, no links, no
    /// panic on the resolver build over zero rows.
    #[test]
    fn two_pass_empty_roost_is_empty_graph() {
        let roost = TempDir::new().unwrap();
        let db_dir = TempDir::new().unwrap();
        let db = crate::db::schema::init_db(db_dir.path()).expect("init_db");
        index_roost_two_pass(&db, &roost.path().to_string_lossy()).expect("two-pass");
        let stats = queries::get_graph_stats(&db).expect("stats");
        assert_eq!(stats.notes, 0);
        assert_eq!(stats.links, 0);
    }

    // ── warm-heal internals (IP-A8 stats + IP-A1 modified reconcile) ────
    //
    // `GraphDb::record_heal` / the `last_heal_*` atomics and the private
    // `reconcile_added_and_modified_notes` live ONLY in this file, so they're
    // covered here rather than in `db::queries`.

    /// Count `links` edges from `from` → `to`.
    fn link_count(db: &DbInstance, from: &str, to: &str) -> usize {
        let mut p = std::collections::BTreeMap::new();
        p.insert("f".to_string(), cozo::DataValue::Str(from.into()));
        p.insert("t".to_string(), cozo::DataValue::Str(to.into()));
        let r = db
            .run_script(
                r#"?[count(from_id)] := *links{from_id, to_id}, from_id == $f, to_id == $t"#,
                p,
                cozo::ScriptMutability::Immutable,
            )
            .expect("link_count");
        r.rows
            .first()
            .and_then(|row| row.first())
            .and_then(|v| match v {
                cozo::DataValue::Num(cozo::Num::Int(i)) => Some(*i as usize),
                _ => None,
            })
            .unwrap_or(0)
    }

    /// Force a note's stored `updated_at` to a fixed value via a direct
    /// `:put` — used to make disk look "newer than the index" without any
    /// real-time dependency.
    fn force_updated_at(db: &DbInstance, id: &str, val: f64) {
        let mut p = std::collections::BTreeMap::new();
        p.insert("id".to_string(), cozo::DataValue::Str(id.into()));
        p.insert("u".to_string(), cozo::DataValue::from(val));
        db.run_script(
            r#"?[id, title, path, updated_at] <- [[$id, "t", "/p", $u]]
               :put notes { id => title, path, updated_at }"#,
            p,
            cozo::ScriptMutability::Mutable,
        )
        .expect("force updated_at");
    }

    #[test]
    fn record_heal_sets_last_heal_atomics() {
        // IP-A8: before any heal, `last_heal_ran` is false and gates the
        // (meaningless) zeros; record_heal flips it true and stores the four
        // counters read back by the diagnostic report.
        let g = GraphDb::new();
        assert!(
            !g.last_heal_ran.load(Ordering::Relaxed),
            "no heal has run yet"
        );
        g.record_heal(12, 3, 1, 2);
        assert!(g.last_heal_ran.load(Ordering::Relaxed), "heal recorded");
        assert_eq!(g.last_heal_ms.load(Ordering::Relaxed), 12);
        assert_eq!(g.last_heal_added.load(Ordering::Relaxed), 3);
        assert_eq!(g.last_heal_modified.load(Ordering::Relaxed), 1);
        assert_eq!(g.last_heal_purged.load(Ordering::Relaxed), 2);
    }

    /// NEW-S2: the stats snapshot cache is empty until a real index stores it,
    /// returns what was stored, and goes empty again on invalidate — so the
    /// unchanged-save fast path only ever serves current data (or recomputes).
    #[test]
    fn stats_cache_store_read_invalidate() {
        use crate::commands::tasks::TaskRow;
        let g = GraphDb::new();
        assert!(g.cached_stats().is_none(), "empty before any index");

        let stats = GraphStatsResult {
            notes: 7,
            links: 3,
            tags: 2,
        };
        let tasks = vec![TaskRow {
            note_id: "n1".into(),
            note_path: "/r/n1.md".into(),
            note_title: "N1".into(),
            source_line: 1,
            text: "do it".into(),
            due_at: String::new(),
            completed: false,
            completed_at: String::new(),
            linked_person: String::new(),
            linked_project: String::new(),
        }];
        g.store_stats_cache(stats, tasks);

        let snap = g.cached_stats().expect("cache populated");
        assert_eq!(snap.stats.notes, 7);
        assert_eq!(snap.stats.links, 3);
        assert_eq!(snap.stats.tags, 2);
        assert_eq!(snap.tasks.len(), 1);
        assert_eq!(snap.tasks[0].note_id, "n1");

        g.invalidate_stats_cache();
        assert!(
            g.cached_stats().is_none(),
            "invalidate drops the snapshot so the fast path recomputes"
        );
    }

    /// IP-A1: the warm heal must re-index a note edited while Corvux was
    /// closed, so its STALE links are removed — not merely augmented. We
    /// simulate "disk is newer than the index" by forcing the stored
    /// `updated_at` old (0.0), sidestepping mtime timing entirely.
    #[tokio::test]
    async fn reconcile_reindexes_modified_note_and_drops_stale_link() {
        use crate::roostfs::native::NativeFs;

        let roost = TempDir::new().unwrap();
        let db_dir = TempDir::new().unwrap();
        let db = crate::db::schema::init_db(db_dir.path()).expect("init_db");
        let root = roost.path().to_string_lossy().to_string();
        let graph_state = GraphDb::new();
        *graph_state.db.lock().expect("graph lock") = Some(db.clone());
        *graph_state.roost_path.lock().expect("roost lock") = Some(root.clone());
        let fs = NativeFs::new(roost.path());
        // NativeFs::new stores this verbatim as its display_root, which the
        // walkers use to build the abs paths — so ids stay consistent.
        // Cold roost: a → b, b is a leaf. Index the cold path exactly like
        // init_graph does (pass1 → resolver → pass2).
        std::fs::write(roost.path().join("a.md"), "[[b]]\n").unwrap();
        std::fs::write(roost.path().join("b.md"), "x").unwrap();
        let files = crate::roostfs::walk::collect_markdown(&fs)
            .await
            .expect("cold walk");
        queries::bulk_index_pass1(&db, &root, &files).expect("pass1");
        let resolver = LinkResolver::build(&db).expect("resolver");
        queries::bulk_index_pass2(&db, &root, &files, &resolver).expect("pass2");
        assert_eq!(
            link_count(&db, "a", "b"),
            1,
            "precondition: cold index links a→b"
        );
        let seeded_rank = queries::recompute_page_rank(&db, 10).expect("seed PageRank cache");
        assert!(!seeded_rank.is_empty());

        // Off-app edit while closed: a now points at a brand-new c; b untouched.
        std::fs::write(roost.path().join("a.md"), "[[c]]\n").unwrap();
        std::fs::write(roost.path().join("c.md"), "y").unwrap();
        // Force a's stored updated_at OLD so file_mtime(a.md) > 0.0 + epsilon
        // fires the "modified while closed" branch with no real-time sleep.
        force_updated_at(&db, "a", 0.0);

        let pairs = crate::roostfs::walk::list_markdown_path_pairs(&fs)
            .await
            .expect("disk listing");
        let (added, modified) =
            reconcile_added_and_modified_notes(&graph_state, &root, &fs, &pairs).await;

        assert!(
            modified >= 1,
            "a was edited off-app → counted as modified, got {modified}"
        );
        assert!(
            added >= 1,
            "c is new on disk → counted as added, got {added}"
        );

        // The whole point of A1: a→c is live AND the stale a→b edge is gone.
        assert_eq!(link_count(&db, "a", "c"), 1, "re-index creates a→c");
        assert_eq!(
            link_count(&db, "a", "b"),
            0,
            "stale a→b edge must be removed by the re-index"
        );
        assert!(
            queries::cached_page_rank(&db, 10)
                .expect("read PageRank cache")
                .is_empty(),
            "warm-heal topology changes must invalidate persisted algorithms"
        );
    }

    // ── DB-4 / DB-3: atomic per-note index + incrementally exact stats cache ──

    /// A graph state wired like a freshly-opened roost: real SQLite schema, the
    /// roost root published, caches cold.
    struct Fixture {
        roost: TempDir,
        _db_dir: TempDir,
        graph: GraphDb,
        db: DbInstance,
        root: String,
    }

    fn fixture() -> Fixture {
        let roost = TempDir::new().unwrap();
        let db_dir = TempDir::new().unwrap();
        let db = crate::db::schema::init_db(db_dir.path()).expect("init_db");
        let root = roost.path().to_string_lossy().to_string();
        let graph = GraphDb::new();
        *graph.db.lock().unwrap() = Some(db.clone());
        *graph.roost_path.lock().unwrap() = Some(root.clone());
        FULL_STATS_RECOMPUTES.with(|c| c.set(0));
        Fixture {
            roost,
            _db_dir: db_dir,
            graph,
            db,
            root,
        }
    }

    impl Fixture {
        fn abs(&self, rel: &str) -> String {
            self.roost.path().join(rel).to_string_lossy().to_string()
        }

        /// Write the note to disk (so its mtime is real) and run the save-path
        /// index on it.
        fn save(&self, rel: &str, content: &str) -> IndexNoteResponse {
            let abs = self.abs(rel);
            if let Some(parent) = std::path::Path::new(&abs).parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&abs, content).unwrap();
            index_note_on_graph(&abs, content, &self.graph).expect("index")
        }

        fn id(&self, rel: &str) -> String {
            queries::note_id_from_abs_path(&self.abs(rel), &self.root)
        }

        fn recomputes(&self) -> usize {
            FULL_STATS_RECOMPUTES.with(|c| c.get())
        }
    }

    type TaskKey = (String, i32, String, String, bool, String, String);

    fn task_keys(tasks: &[crate::commands::tasks::TaskRow]) -> Vec<TaskKey> {
        tasks
            .iter()
            .map(|t| {
                (
                    t.note_id.clone(),
                    t.source_line,
                    t.text.clone(),
                    t.due_at.clone(),
                    t.completed,
                    t.note_title.clone(),
                    t.note_path.clone(),
                )
            })
            .collect()
    }

    /// The cached snapshot and a from-scratch recount must agree exactly.
    fn assert_cache_exact(f: &Fixture, label: &str) {
        let snap = f
            .graph
            .cached_stats()
            .unwrap_or_else(|| panic!("{label}: cache should be warm"));
        let fresh = queries::get_graph_stats(&f.db).unwrap();
        assert_eq!(
            (snap.stats.notes, snap.stats.links, snap.stats.tags),
            (fresh.notes, fresh.links, fresh.tags),
            "{label}: cached totals drifted from a recount"
        );
        let fresh_tasks = crate::commands::tasks::query_all_tasks(&f.db).unwrap();
        assert_eq!(
            task_keys(&snap.tasks),
            task_keys(&fresh_tasks),
            "{label}: cached task list drifted from a fresh query_all_tasks"
        );
    }

    fn assert_response_matches_db(f: &Fixture, resp: &IndexNoteResponse, label: &str) {
        let fresh = queries::get_graph_stats(&f.db).unwrap();
        assert_eq!(
            (resp.stats.notes, resp.stats.links, resp.stats.tags),
            (fresh.notes, fresh.links, fresh.tags),
            "{label}: save response totals"
        );
        let fresh_tasks = crate::commands::tasks::query_all_tasks(&f.db).unwrap();
        assert_eq!(
            task_keys(&resp.tasks),
            task_keys(&fresh_tasks),
            "{label}: save response tasks"
        );
    }

    /// DB-3: the steady-state save never re-counts relations or re-reads the
    /// task list, yet the totals / task list it returns are EXACT — across
    /// index, edit, identical re-save, purge, rename and folder move.
    #[test]
    fn hot_save_keeps_totals_and_tasks_exact_without_rescanning() {
        let f = fixture();

        // First save of the session: cache is cold → ONE recompute.
        let r = f.save("a.md", "see [[b]] #x\n- [ ] one 📅 2026-12-01\n");
        assert_eq!(f.recomputes(), 1, "cold first save recomputes once");
        assert_response_matches_db(&f, &r, "new a");
        assert_cache_exact(&f, "new a");

        // From here on: patched in place, never recomputed.
        let r = f.save("b.md", "# b #x #y\n- [ ] two 📅 2026-12-02\n");
        assert_response_matches_db(&f, &r, "new b (promotes a→b)");
        let r = f.save(
            "a.md",
            "no links #z\n- [x] one 📅 2026-12-01\n- [ ] extra 📅 2026-12-03\n",
        );
        assert_response_matches_db(&f, &r, "edit a (drop link, retag, retask)");
        let r = f.save(
            "a.md",
            "no links #z\n- [x] one 📅 2026-12-01\n- [ ] extra 📅 2026-12-03\n",
        );
        assert_response_matches_db(&f, &r, "identical re-save");
        let r = f.save(
            "sub/c.md",
            "[[a]] [[b]] [[b]] #x\n- [ ] deep 📅 2026-11-30\n",
        );
        assert_response_matches_db(&f, &r, "new nested note");
        assert_cache_exact(&f, "after edits");
        assert_eq!(
            f.recomputes(),
            1,
            "steady-state saves must not fall back to the O(roost) recount"
        );

        // Purge: an id-removing path invalidates; the next save recomputes once.
        {
            let guard = f.graph.db.lock().unwrap();
            let db = guard.as_ref().unwrap();
            f.graph.purge_note_cached(db, &f.id("b.md")).unwrap();
        }
        assert!(f.graph.cached_stats().is_none(), "purge drops the snapshot");
        let r = f.save("a.md", "back to [[sub/c]] #z\n");
        assert_eq!(f.recomputes(), 2);
        assert_response_matches_db(&f, &r, "save after purge");
        assert_cache_exact(&f, "save after purge");

        // Rename = purge old id + index the new path.
        {
            let guard = f.graph.db.lock().unwrap();
            let db = guard.as_ref().unwrap();
            f.graph.purge_note_cached(db, &f.id("sub/c.md")).unwrap();
        }
        let r = f.save("renamed/c2.md", "[[a]] #x\n- [ ] deep 📅 2026-11-30\n");
        assert_response_matches_db(&f, &r, "rename");
        assert_cache_exact(&f, "rename");

        // Folder move: purge every nested old id, re-index at the new prefix.
        f.save("proj/n1.md", "[[a]] #m\n- [ ] n1 📅 2026-12-05\n");
        f.save("proj/n2.md", "[[n1]] #m #n\n");
        for old in ["proj/n1.md", "proj/n2.md"] {
            let guard = f.graph.db.lock().unwrap();
            let db = guard.as_ref().unwrap();
            f.graph.purge_note_cached(db, &f.id(old)).unwrap();
        }
        let r1 = f.save("moved/n1.md", "[[a]] #m\n- [ ] n1 📅 2026-12-05\n");
        assert_response_matches_db(&f, &r1, "folder move: first note");
        let r2 = f.save("moved/n2.md", "[[n1]] #m #n\n");
        assert_response_matches_db(&f, &r2, "folder move: second note");
        assert_cache_exact(&f, "folder move");
        assert_eq!(
            f.recomputes(),
            4,
            "one recompute per invalidation burst, none per save"
        );
    }

    /// The patched cache must also agree with the DB after other mutators that
    /// touch graph rows without the index — they invalidate, so a following
    /// save recomputes instead of patching a stale base.
    #[test]
    fn task_and_exclusion_edits_invalidate_instead_of_patching_a_stale_base() {
        let f = fixture();
        f.save("a.md", "#x\n- [ ] one 📅 2026-12-01\n");
        assert!(f.graph.cached_stats().is_some());
        // `complete_task`-style path: tasks of a note rewritten outside the
        // index, then the cache dropped by that path.
        queries::index_note_tasks(&f.db, "a", "#x\n- [x] one 📅 2026-12-01\n").unwrap();
        f.graph.invalidate_stats_cache();
        let before = f.recomputes();
        let r = f.save("b.md", "# b\n");
        assert_eq!(f.recomputes(), before + 1);
        assert_response_matches_db(&f, &r, "after task rewrite");
        assert!(
            r.tasks.iter().any(|t| t.completed),
            "sees the completed task"
        );
    }

    #[test]
    fn aux_write_path_patches_the_cache_so_it_is_never_stale() {
        // AI edit / quick-add / history revert go through `index_note_full_cached`
        // and (roost_edit, edit_history) used to leave the snapshot stale.
        let f = fixture();
        f.save("a.md", "# a\n");
        assert!(f.graph.cached_stats().is_some());
        let abs = f.abs("agent.md");
        std::fs::write(&abs, "[[a]] #ai\n- [ ] agent task 📅 2026-12-09\n").unwrap();
        {
            let guard = f.graph.db.lock().unwrap();
            let db = guard.as_ref().unwrap();
            f.graph
                .index_note_full_cached(
                    db,
                    &f.root,
                    &abs,
                    "[[a]] #ai\n- [ ] agent task 📅 2026-12-09\n",
                )
                .unwrap();
        }
        assert_cache_exact(&f, "aux index patches the cache");
        assert_eq!(f.recomputes(), 1, "no recount for the aux path either");
    }

    /// DB-4: a save that dies mid-transaction leaves the note exactly as it was
    /// — stamp included — so the next warm reconcile re-detects and restores it.
    #[tokio::test]
    async fn interrupted_save_is_restored_by_the_next_warm_reconcile() {
        use crate::roostfs::native::NativeFs;
        use std::time::{Duration, SystemTime};

        let f = fixture();
        let fs = NativeFs::new(f.roost.path());
        f.save("b.md", "# b\n");
        f.save("c.md", "# c\n");
        f.save("a.md", "see [[b]] #old\n- [ ] first 📅 2026-12-01\n");
        let id_a = f.id("a.md");
        let stamp = |f: &Fixture| {
            let guard = f.graph.db.lock().unwrap();
            let db = guard.as_ref().unwrap();
            queries::note_updated_ats(db).unwrap()[&id_a]
        };
        let rows = |f: &Fixture| {
            let links = queries::get_note_links(&f.db, &id_a).unwrap();
            let tags = queries::get_note_tags(&f.db, &id_a).unwrap();
            let mut links: Vec<String> = links.into_iter().map(|l| l.target).collect();
            links.sort();
            (links, tags)
        };
        let v1_stamp = stamp(&f);
        assert_eq!(rows(&f), (vec!["b".to_string()], vec!["old".to_string()]));

        // The user's next edit hits disk (mtime clearly newer than the stamp).
        let v2 = "now [[c]] #new\n";
        let abs_a = f.abs("a.md");
        std::fs::write(&abs_a, v2).unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&abs_a)
            .unwrap()
            .set_modified(SystemTime::now() + Duration::from_secs(120))
            .unwrap();

        // ...but the index of that save is interrupted after its writes ran.
        {
            let _fp = crate::db::queries::tx_fail_point::Guard::arm();
            assert!(index_note_on_graph(&abs_a, v2, &f.graph).is_err());
        }
        assert_eq!(
            rows(&f),
            (vec!["b".to_string()], vec!["old".to_string()]),
            "no torn state: links and tags are still the pre-save pair"
        );
        assert_eq!(stamp(&f), v1_stamp, "stamp not advanced → heal can see it");

        // A warm reconcile that is ITSELF interrupted changes nothing either.
        let pairs = crate::roostfs::walk::list_markdown_path_pairs(&fs)
            .await
            .unwrap();
        {
            let _fp = crate::db::queries::tx_fail_point::Guard::arm();
            let (added, modified) =
                reconcile_added_and_modified_notes(&f.graph, &f.root, &fs, &pairs).await;
            assert_eq!((added, modified), (0, 0), "nothing committed");
        }
        assert_eq!(stamp(&f), v1_stamp);

        // The next (uninterrupted) reconcile restores the note.
        let (added, modified) =
            reconcile_added_and_modified_notes(&f.graph, &f.root, &fs, &pairs).await;
        assert_eq!((added, modified), (0, 1), "a.md re-detected as modified");
        assert_eq!(rows(&f), (vec!["c".to_string()], vec!["new".to_string()]));
        assert!(stamp(&f) > v1_stamp + MODIFIED_MTIME_EPSILON);
        // And it is now up to date: a further reconcile is a no-op.
        let (added, modified) =
            reconcile_added_and_modified_notes(&f.graph, &f.root, &fs, &pairs).await;
        assert_eq!((added, modified), (0, 0));
    }

    /// A brand-new note must show up immediately (and promote the links that
    /// were waiting on it) — the atomic path must not defer anything.
    #[test]
    fn brand_new_note_is_visible_and_promotes_waiting_links_immediately() {
        let f = fixture();
        f.save("a.md", "waiting on [[late]]\n");
        assert!(queries::lookup_note_id_by_path(&f.db, &f.abs("late.md"))
            .unwrap()
            .is_none());
        let r = f.save("late.md", "# late\n");
        assert_eq!(
            queries::lookup_note_id_by_path(&f.db, &f.abs("late.md"))
                .unwrap()
                .as_deref(),
            Some("late"),
            "the new note is queryable right after the save"
        );
        let backlinks = queries::get_backlinks(&f.db, "late").unwrap();
        assert_eq!(backlinks.len(), 1, "a's dangling link promoted at once");
        assert_response_matches_db(&f, &r, "new note + promotion");
    }

    #[test]
    fn with_cached_resolver_borrows_the_cache_and_rebuilds_after_invalidation() {
        // IP-A3 for AI link diagnostics: no full `*notes` scan per call.
        let f = fixture();
        queries::index_note_full(&f.db, &f.root, &f.abs("x.md"), "# x\n").unwrap();
        // Cold → built from the DB.
        assert!(f
            .graph
            .with_cached_resolver(&f.db, |r| r.contains_id("x"))
            .unwrap());
        // A note the CACHED resolver was never told about (fresh-build path
        // leaves GraphDb's cache alone): a warm borrow can't see it.
        queries::index_note_full(&f.db, &f.root, &f.abs("y.md"), "# y\n").unwrap();
        assert!(
            !f.graph
                .with_cached_resolver(&f.db, |r| r.contains_id("y"))
                .unwrap(),
            "warm cache is reused, not rebuilt"
        );
        f.graph.invalidate_link_resolver();
        assert!(f
            .graph
            .with_cached_resolver(&f.db, |r| r.contains_id("y"))
            .unwrap());
    }

    #[test]
    fn purge_note_cached_drops_resolver_and_stats_snapshot_together() {
        let f = fixture();
        f.save("a.md", "# a\n");
        assert!(f.graph.cached_stats().is_some());
        assert!(f.graph.link_resolver.lock().unwrap().is_some());
        {
            let guard = f.graph.db.lock().unwrap();
            let db = guard.as_ref().unwrap();
            f.graph.purge_note_cached(db, "a").unwrap();
        }
        assert!(f.graph.cached_stats().is_none());
        assert!(f.graph.link_resolver.lock().unwrap().is_none());
    }

    // ── N1: errors AFTER the commit point must never leave a trusted-stale cache ──

    /// A failed index (rolled back) still drops the snapshot: an error is the
    /// moment "the graph should be unchanged" is least worth trusting.
    #[test]
    fn failed_index_drops_the_stats_snapshot() {
        let f = fixture();
        f.save("a.md", "# a\n");
        assert!(f.graph.cached_stats().is_some());
        let abs = f.abs("b.md");
        {
            let guard = f.graph.db.lock().unwrap();
            let db = guard.as_ref().unwrap();
            let _fp = crate::db::queries::tx_fail_point::Guard::arm();
            assert!(f.graph.index_note_core(db, &f.root, &abs, "# b\n").is_err());
        }
        assert!(
            f.graph.cached_stats().is_none(),
            "a failed index must not leave a snapshot served as exact"
        );
    }

    /// The index COMMITS but its post-commit measurements can't be read: the
    /// call still succeeds (a committed index never reports Err), the snapshot
    /// is DROPPED rather than patched with zeros, the algo cache is forced
    /// stale, and the next reader recomputes the exact truth.
    #[test]
    fn committed_index_with_unreadable_post_commit_state_drops_the_snapshot() {
        let f = fixture();
        f.save("a.md", "# a #x\n- [ ] t 📅 2026-12-01\n");
        assert!(f.graph.cached_stats().is_some());
        let abs = f.abs("b.md");
        let outcome = {
            let guard = f.graph.db.lock().unwrap();
            let db = guard.as_ref().unwrap();
            let _fp = crate::db::queries::tx_fail_point::PostCommitGuard::arm();
            f.graph
                .index_note_core(db, &f.root, &abs, "[[a]] #y\n- [ ] u 📅 2026-12-02\n")
                .expect("a committed index must never report Err")
        };
        assert!(
            !outcome.deltas_trusted,
            "outcome admits its deltas are unknown"
        );
        assert!(
            f.graph.cached_stats().is_none(),
            "stale-but-trusted snapshot must be dropped"
        );
        assert!(
            f.graph.algo_cache_dirty.load(Ordering::Relaxed),
            "link topology unknown → algo cache forced stale"
        );
        assert!(
            queries::lookup_note_id_by_path(&f.db, &abs)
                .unwrap()
                .is_some(),
            "the index really did commit"
        );

        // Next reader recomputes once and is exact.
        let before = f.recomputes();
        {
            let guard = f.graph.db.lock().unwrap();
            let db = guard.as_ref().unwrap();
            f.graph.stats_snapshot_or_compute(db).unwrap();
        }
        assert_eq!(f.recomputes(), before + 1);
        assert_cache_exact(&f, "after post-commit failure");

        // And through the real save path the response is exact too.
        let abs2 = f.abs("c.md");
        let r = {
            let _fp = crate::db::queries::tx_fail_point::PostCommitGuard::arm();
            index_note_on_graph(&abs2, "[[b]] #z\n", &f.graph)
                .expect("save path must not fail after commit")
        };
        assert_response_matches_db(&f, &r, "save with unreadable post-commit state");
    }

    /// Warm heal: a per-note index failure (which `continue`s) must still drop
    /// a snapshot that user saves warmed during the heal; a committed-but-
    /// unmeasured note is still counted and the cache dropped.
    #[tokio::test]
    async fn warm_heal_failure_paths_drop_the_stats_snapshot() {
        use crate::roostfs::native::NativeFs;
        use std::time::{Duration, SystemTime};

        let f = fixture();
        let fs = NativeFs::new(f.roost.path());
        f.save("a.md", "see [[b]]\n");
        f.save("b.md", "# b\n");
        assert!(f.graph.cached_stats().is_some());

        let abs_a = f.abs("a.md");
        std::fs::write(&abs_a, "now [[b]] #new\n").unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&abs_a)
            .unwrap()
            .set_modified(SystemTime::now() + Duration::from_secs(120))
            .unwrap();
        let pairs = crate::roostfs::walk::list_markdown_path_pairs(&fs)
            .await
            .unwrap();

        // Every per-note index fails → nothing counted, yet the snapshot goes.
        {
            let _fp = crate::db::queries::tx_fail_point::Guard::arm();
            let (added, modified) =
                reconcile_added_and_modified_notes(&f.graph, &f.root, &fs, &pairs).await;
            assert_eq!((added, modified), (0, 0));
        }
        assert!(
            f.graph.cached_stats().is_none(),
            "an all-failures heal must not leave the old snapshot trusted"
        );

        // Re-warm, then a heal whose index commits but can't measure.
        f.save("c.md", "# c\n");
        assert!(f.graph.cached_stats().is_some());
        {
            let _fp = crate::db::queries::tx_fail_point::PostCommitGuard::arm();
            let (added, modified) =
                reconcile_added_and_modified_notes(&f.graph, &f.root, &fs, &pairs).await;
            assert_eq!((added, modified), (0, 1), "committed → counted");
        }
        assert!(f.graph.cached_stats().is_none());
        assert!(f.graph.algo_cache_dirty.load(Ordering::Relaxed));
    }
}
