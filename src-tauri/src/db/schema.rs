use cozo::{DbInstance, ScriptMutability};
use std::path::{Path, PathBuf};

/// AUDIT-051: structured error for `init_db`. `Corrupt` means the
/// underlying SQLite file is unreadable as a database — the only
/// reliable recovery is to delete `graph.db` and re-walk the roost
/// (handled by the `rebuild_database` IPC). `Other` is everything
/// else (permission denied, disk full, schema/migration bug) and
/// the caller should NOT auto-rebuild on it — destroying a working
/// DB to "fix" a transient permission error would be a much worse
/// failure mode.
#[derive(Debug)]
pub enum InitDbError {
    Corrupt(String),
    Other(String),
}

impl std::fmt::Display for InitDbError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InitDbError::Corrupt(msg) => write!(f, "{msg}"),
            InitDbError::Other(msg) => write!(f, "{msg}"),
        }
    }
}

impl From<String> for InitDbError {
    fn from(s: String) -> Self {
        InitDbError::Other(s)
    }
}

/// AUDIT-051: heuristic match on SQLite/Cozo error strings that
/// signal "this file is not a usable database." We match on the
/// substrings SQLite emits for the three corruption codes
/// (SQLITE_NOTADB / SQLITE_CORRUPT / SQLITE_FULL-during-open) so a
/// caller can branch on `InitDbError::Corrupt` and offer Repair
/// without forcing the user to read the raw error.
///
/// `pub(crate)` so the FTS search DB (`db::search_db`) can reuse the same
/// signatures for its own corrupt-and-rebuild self-heal (SRX-2).
pub(crate) fn looks_corrupt(msg: &str) -> bool {
    let lower = msg.to_ascii_lowercase();
    lower.contains("not a database")
        || lower.contains("file is not a database")
        || lower.contains("disk image is malformed")
        || lower.contains("database is corrupt")
        || lower.contains("malformed database schema")
}

/// Initialize CozoDB with SQLite storage at the given directory.
/// Creates `graph.db` inside `db_dir`. The caller is responsible for
/// picking a stable, device-local `db_dir` (see `crate::paths`).
pub fn init_db(db_dir: &Path) -> Result<DbInstance, InitDbError> {
    std::fs::create_dir_all(db_dir).map_err(|e| {
        InitDbError::Other(format!(
            "Failed to create DB directory {}: {e}",
            db_dir.display()
        ))
    })?;

    let db_path = db_dir.join("graph.db");

    // Enable WAL journal mode BEFORE CozoDB opens the file. WAL is a
    // persistent header-level setting, so this one-shot PRAGMA carries over
    // to every subsequent CozoDB connection. Without it, SQLite defaults to
    // the rollback journal, where every write takes an exclusive lock and
    // blocks all readers — which is exactly what was producing the
    // "database is locked (code 5)" errors whenever auto-save races with
    // init_graph.
    enable_wal_mode(&db_path).map_err(|e| {
        if looks_corrupt(&e) {
            InitDbError::Corrupt(e)
        } else {
            InitDbError::Other(e)
        }
    })?;

    let db = DbInstance::new(
        "sqlite",
        db_path.to_string_lossy().as_ref(),
        Default::default(),
    )
    .map_err(|e| {
        let msg = format!("Failed to initialize CozoDB: {e}");
        if looks_corrupt(&msg) {
            InitDbError::Corrupt(msg)
        } else {
            InitDbError::Other(msg)
        }
    })?;

    create_schema(&db).map_err(|e| {
        if looks_corrupt(&e) {
            InitDbError::Corrupt(e)
        } else {
            InitDbError::Other(e)
        }
    })?;

    // DB-3: snapshot the DB file(s) before a version-advancing migration so
    // a botched migration can be rolled back at the file level. `Repair`
    // (rebuild_database) reconstructs derived state from the markdown roost,
    // but `:edit_event` / `:edit_event_after_hash` / `:approval_log` /
    // `:ai_signals` are NOT derivable from files — a half-applied,
    // table-rebuilding migration would be silent, unrecoverable data loss.
    // `create_schema` above only ever adds relations idempotently (never
    // drops/rebuilds), so it's safe to snapshot after it and before the
    // migration bodies, which are the only place a destructive step can live.
    let backups = if migration_pending(&db) {
        match backup_db_files(db_dir) {
            Ok(b) => b,
            Err(e) => {
                // NEW-DB2: do NOT proceed without a snapshot. A migration is
                // pending and can rebuild tables; without the snapshot the
                // rollback net is gone, so a mid-migration failure would lose
                // exactly the non-derivable data (:edit_event / :approval_log)
                // DB-3 exists to protect. Abort startup instead — the next
                // launch retries once the transient cause (AV lock, disk
                // full) clears. `Other`, not `Corrupt`: the DB is fine, we
                // just couldn't snapshot it, so we must not trigger a rebuild.
                return Err(InitDbError::Other(format!(
                    "Refusing to migrate the database without a pre-migration \
                     backup (snapshot failed: {e}). This is usually transient \
                     (disk full, antivirus lock). Free space / retry and reopen."
                )));
            }
        }
    } else {
        Vec::new()
    };

    match run_migrations(&db) {
        Ok(()) => {
            cleanup_db_backup(&backups);
            Ok(db)
        }
        Err(e) => {
            // Release the CozoDB handle before restoring — Windows won't let
            // us overwrite a SQLite file another handle still holds open.
            drop(db);
            if !backups.is_empty() {
                tracing::warn!("DB-3: migration to v{CURRENT_SCHEMA_VERSION} failed, rolling back to pre-migration snapshot");
                restore_db_backup(db_dir);
            }
            Err(if looks_corrupt(&e) {
                InitDbError::Corrupt(e)
            } else {
                InitDbError::Other(e)
            })
        }
    }
}

/// DB-3: the SQLite files that together make up `graph.db` under WAL
/// journaling. We snapshot all three so a restore recovers a consistent
/// state — the `-wal` file can hold committed frames not yet checkpointed
/// into the main file, so backing up `graph.db` alone could drop writes.
const DB_FILE_STEMS: [&str; 3] = ["graph.db", "graph.db-wal", "graph.db-shm"];

const PREMIGRATION_BAK_SUFFIX: &str = ".premigration-bak";

/// True when the on-disk schema version is behind this binary, i.e. a
/// version-advancing migration is about to run. A missing key (fresh DB)
/// or an unparseable value → `false`: fresh DBs have no data to protect,
/// and `run_migrations` surfaces the parse error itself.
fn migration_pending(db: &DbInstance) -> bool {
    match get_kv(db, "schema_version") {
        Ok(Some(s)) => s
            .parse::<i64>()
            .map(|v| v < CURRENT_SCHEMA_VERSION)
            .unwrap_or(false),
        _ => false,
    }
}

/// DB-3: copy the live DB files to `<name>.premigration-bak` siblings.
/// Returns the backup paths created so the caller can restore or clean
/// them up. Safe to run while CozoDB holds the files open: this executes
/// at startup before the migration writes anything, and snapshotting the
/// `-wal`/`-shm` sidecars alongside the main file preserves consistency.
fn backup_db_files(db_dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut created = Vec::new();
    for stem in DB_FILE_STEMS {
        let src = db_dir.join(stem);
        if src.exists() {
            let dst = db_dir.join(format!("{stem}{PREMIGRATION_BAK_SUFFIX}"));
            std::fs::copy(&src, &dst)?;
            created.push(dst);
        }
    }
    Ok(created)
}

/// DB-3: roll the DB files back to the pre-migration snapshot after a
/// failed migration. Removes the (possibly half-migrated) live files —
/// including any stale `-wal`/`-shm` that would otherwise be replayed onto
/// the restored main file — then moves each backup into place. Best-effort:
/// a failure is logged, not fatal; the `.premigration-bak` files stay on
/// disk for manual recovery. MUST be called after the CozoDB handle is
/// dropped.
fn restore_db_backup(db_dir: &Path) {
    for stem in DB_FILE_STEMS {
        let _ = std::fs::remove_file(db_dir.join(stem));
    }
    for stem in DB_FILE_STEMS {
        let bak = db_dir.join(format!("{stem}{PREMIGRATION_BAK_SUFFIX}"));
        if bak.exists() {
            let dst = db_dir.join(stem);
            if let Err(e) = std::fs::rename(&bak, &dst) {
                tracing::error!("DB-3: failed to restore {} from backup: {e}", dst.display());
            }
        }
    }
}

/// DB-3: drop the pre-migration snapshot after a successful migration.
fn cleanup_db_backup(backups: &[PathBuf]) {
    for b in backups {
        let _ = std::fs::remove_file(b);
    }
}

/// One stepwise schema upgrade. The runner applies migrations whose
/// `to_version` is greater than the DB's recorded `schema_version` and
/// less-than-or-equal to [`CURRENT_SCHEMA_VERSION`], in ascending order.
///
/// `apply` returns `Err` to abort startup. On error the DB stays at the
/// previous version so the next launch can retry the same migration.
/// Mid-migration data corruption is the migration author's problem to
/// avoid (Cozo doesn't expose user-level transactions across multiple
/// `run_script` calls); when in doubt, write idempotent migrations that
/// can survive being interrupted and rerun.
#[allow(dead_code)] // fields are read by run_migrations once a real migration lands
struct Migration {
    to_version: i64,
    description: &'static str,
    apply: fn(&DbInstance) -> Result<(), String>,
}

/// Ordered list of all migrations the runner knows about. Append new
/// entries at the end. `to_version` must equal the version the migration
/// produces (matching one of the bumped `CURRENT_*_VERSION` constants
/// above), and entries must be strictly increasing.
///
/// Append new entries at the end. Each migration's `to_version` matches
/// the `CURRENT_SCHEMA_VERSION` produced after it runs.
const MIGRATIONS: &[Migration] = &[
    Migration {
        to_version: 2,
        description: "Add :edit_event relation (Phase 2.2 — Edit History)",
        apply: migrate_v2_edit_events,
    },
    Migration {
        to_version: 3,
        description: "(no-op; episodic memory removed — formerly :episode relations)",
        apply: migrate_v3_episodes,
    },
    Migration {
        to_version: 4,
        description: "Add :edit_event_after_hash sidecar (revert divergence detection)",
        apply: migrate_v4_edit_event_after_hash,
    },
    Migration {
        to_version: 5,
        description: "Add :approval_log relation (agent-hardening § 3.4.4 — approval audit trail)",
        apply: migrate_v5_approval_log,
    },
    Migration {
        to_version: 6,
        description: "Add :dangling relation (promote backlinks when their target note is created)",
        apply: migrate_v6_dangling,
    },
];

/// v1 → v2: create the `:edit_event` relation. Idempotent — `create_schema`
/// also tries to create it on every startup; this migration is just the
/// version bump that records "this DB now has it" so a future schema
/// change can rely on it being present without re-checking.
fn migrate_v2_edit_events(db: &DbInstance) -> Result<(), String> {
    create_edit_event_relation(db)
}

/// v2 → v3 (historical): originally created the `:episode` +
/// `:episode_embedding` relations + HNSW index for episodic memory.
/// Episodic memory was removed in the Stage-2 teardown, so this is now a
/// no-op — it stays in the MIGRATIONS table only to keep the version
/// chain (and `to_version: 3`) contiguous. Existing user DBs keep their
/// orphaned `:episode*` relations; we simply stop creating new ones.
fn migrate_v3_episodes(db: &DbInstance) -> Result<(), String> {
    // episodic memory removed; historical migration is now a no-op
    let _ = db;
    Ok(())
}

/// v3 → v4: create the `:edit_event_after_hash` sidecar. Sidecar instead
/// of a column on `:edit_event` because Cozo doesn't support ALTER and
/// rebuilding the main table risks losing edit history. Old events have
/// no entry in the sidecar; the revert command treats a sidecar miss as
/// "no divergence info available" and falls back to the existing
/// later-events check. Idempotent — `create_edit_event_after_hash`
/// swallows already-exists errors via `:create` semantics.
fn migrate_v4_edit_event_after_hash(db: &DbInstance) -> Result<(), String> {
    create_edit_event_after_hash_relation(db)
}

/// v4 → v5: create the `:approval_log` relation. Records every decision
/// the approval gate (`ai/tools/approval.rs`) emits — Approve,
/// AlwaysApprove, Reject, or Forgotten (panel-unmount / agent-cancel).
/// Prerequisite for the §4.4 tool reliability scorecard and the
/// Settings → AI → Approval History page. Idempotent via shared
/// `run_script` "already exists" handling.
fn migrate_v5_approval_log(db: &DbInstance) -> Result<(), String> {
    create_approval_log_relation(db)
}

/// v5 → v6: create the `:dangling` relation. Existing notes that reference a
/// not-yet-created note keep their dropped link until they're re-saved; once
/// this relation exists, future saves record those unresolved links and a
/// new note promotes them to real `:links` edges immediately. Back-filling
/// existing dangling links would need a full content re-scan, so we don't —
/// the relation starts empty and fills as notes are saved (the next full
/// `rebuild_database` populates it for the whole roost). Idempotent via the
/// shared `run_script` "already exists" handling.
fn migrate_v6_dangling(db: &DbInstance) -> Result<(), String> {
    create_dangling_relation(db)
}

/// Apply pending migrations to bring the DB up to [`CURRENT_SCHEMA_VERSION`].
///
/// Behavior:
/// - Recorded version equals current → no-op.
/// - Recorded version greater than current → abort. We refuse to open a
///   newer DB with an older binary; downgrading silently risks data
///   corruption.
/// - Recorded version less than current → run every migration with
///   `from < to_version <= current`, in ascending order, bumping
///   `schema_version` after each one so a crash mid-batch resumes from
///   the right place on the next launch.
fn run_migrations(db: &DbInstance) -> Result<(), String> {
    let from = match get_kv(db, "schema_version")? {
        Some(s) => s.parse::<i64>().map_err(|_| {
            "kv_meta.schema_version is not a valid integer — DB may be corrupt".to_string()
        })?,
        // Missing key: `seed_kv_meta` runs before us and stamps fresh DBs
        // with CURRENT, so this branch is mostly defensive. Treat as
        // current to avoid running a migration against unknown state.
        None => CURRENT_SCHEMA_VERSION,
    };

    if from == CURRENT_SCHEMA_VERSION {
        return Ok(());
    }
    if from > CURRENT_SCHEMA_VERSION {
        return Err(format!(
            "Database schema is newer (v{from}) than this build (v{CURRENT_SCHEMA_VERSION}). \
             Refusing to open. Update Corvux to a newer build, or open a different roost."
        ));
    }

    let mut applicable: Vec<&Migration> = MIGRATIONS
        .iter()
        .filter(|m| m.to_version > from && m.to_version <= CURRENT_SCHEMA_VERSION)
        .collect();
    applicable.sort_by_key(|m| m.to_version);

    tracing::info!(
        from,
        to = CURRENT_SCHEMA_VERSION,
        count = applicable.len(),
        "schema: running migrations"
    );

    for m in applicable {
        tracing::info!(
            to_version = m.to_version,
            desc = m.description,
            "schema: applying migration"
        );
        (m.apply)(db).map_err(|e| format!("Migration to v{} failed: {e}", m.to_version))?;
        set_kv(db, "schema_version", &m.to_version.to_string())?;
    }

    Ok(())
}

/// Insert/overwrite a single `kv_meta` row. Used by the migration runner
/// to bump `schema_version`, and by device-local settings the backend must
/// read at query time (e.g. the Tasks pane's excluded-folders list — see
/// `commands::tasks`). `kv_meta` is documented at its `:create` as the home
/// for "anything that needs to survive schema changes"; adding a key never
/// needs a migration. For settings only the frontend reads, prefer
/// `<roost>/.corvux/settings.json` (roost-portable) instead.
pub fn set_kv(db: &DbInstance, key: &str, value: &str) -> Result<(), String> {
    use cozo::DataValue;
    use std::collections::BTreeMap;
    let mut params = BTreeMap::new();
    params.insert("key".to_string(), DataValue::Str(key.to_string().into()));
    params.insert(
        "value".to_string(),
        DataValue::Str(value.to_string().into()),
    );
    db.run_script(
        r#"?[key, value] <- [[$key, $value]]
           :put kv_meta { key => value }"#,
        params,
        ScriptMutability::Mutable,
    )
    .map_err(|e| format!("Failed to write kv_meta[{key}]: {e}"))?;
    Ok(())
}

/// DB-5: opportunistic graph-DB compaction. Runs CozoDB's `::compact`
/// system op. The caller must hold the `GraphDb` mutex so no Cozo query is
/// in flight while this runs (see the scheduler in `commands::graph`).
///
/// HONEST LIMITATION (verified in cozo-0.7.6 `src/storage/sqlite.rs`): on
/// the SQLite backend `::compact` maps to `range_compact`, which only drains
/// Cozo's connection pool — it does NOT issue a SQLite `VACUUM`, so
/// `graph.db`'s file size is not reclaimed after mass deletes. A true
/// file-level VACUUM would need either the RocksDB backend or VACUUMing the
/// live file from a foreign connection; the latter is deliberately avoided
/// here because it can't be verified safe against Cozo's open handles in
/// this environment, and `:edit_event` / `:approval_log` are not rebuildable
/// from disk. The real space win in DB-5 lands on `search.db` (owned
/// connection, safe VACUUM) where the FTS5 tombstones actually accumulate.
/// Deferred user scenario: after deleting thousands of notes, `search.db`
/// shrinks but `graph.db` stays at its high-water mark until a Repair.
pub fn compact_graph_db(db: &DbInstance) -> Result<(), String> {
    db.run_script("::compact", Default::default(), ScriptMutability::Mutable)
        .map(|_| ())
        .map_err(|e| format!("graph compact failed: {e}"))
}

/// Open the graph.db SQLite file briefly via the `sqlite` crate just to
/// flip `journal_mode` to WAL. Closing this connection is fine — WAL
/// persists in the DB header.
///
/// AUDIT-020: two concurrent `init_graph` calls during cold start used to
/// race here: each opened its own SQLite connection and both tried to
/// take the exclusive lock journal-mode changes require, with the loser
/// getting "database is locked". The 5s busy_timeout didn't help because
/// `PRAGMA journal_mode` doesn't invoke the busy handler.
///
/// Mitigation: serialise the whole helper behind a process-wide mutex,
/// and probe `PRAGMA journal_mode` first so the common (already-wal)
/// path is a single read query that never takes the exclusive lock at
/// all. Per-process mutex is fine here: the cross-process case is
/// handled by the roost lock (`roostfs::lock`), which is acquired before
/// any DB initialisation.
fn enable_wal_mode(db_path: &Path) -> Result<(), String> {
    static WAL_INIT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = WAL_INIT_LOCK.lock().unwrap_or_else(|p| p.into_inner());

    let conn = sqlite::open(db_path.to_string_lossy().as_ref())
        .map_err(|e| format!("Failed to open graph.db for PRAGMA: {e}"))?;

    // Read the current journal_mode. If it's already wal, do nothing —
    // this is the common case once the DB has been initialised once.
    let mut current = String::new();
    conn.iterate("PRAGMA journal_mode;", |pairs| {
        for &(_, v) in pairs {
            if let Some(v) = v {
                current = v.to_string();
            }
        }
        true
    })
    .map_err(|e| format!("Failed to read journal_mode on graph.db: {e}"))?;

    if current.eq_ignore_ascii_case("wal") {
        return Ok(());
    }

    conn.execute("PRAGMA journal_mode=WAL;")
        .map_err(|e| format!("Failed to set journal_mode=WAL on graph.db: {e}"))?;
    // 5-second busy-wait before returning SQLITE_BUSY. Cheap insurance in
    // the rare case two writers still contend under WAL (long reader + new
    // writer on the same page).
    conn.execute("PRAGMA busy_timeout=5000;")
        .map_err(|e| format!("Failed to set busy_timeout on graph.db: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod wal_tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;
    use tempfile::TempDir;

    /// AUDIT-020 regression: two threads racing `enable_wal_mode` against
    /// the same fresh database must both succeed. Pre-fix, the second
    /// caller would hit "database is locked" because both tried to take an
    /// exclusive lock to flip journal_mode at the same time.
    #[test]
    fn concurrent_enable_wal_does_not_race() {
        let dir = TempDir::new().unwrap();
        let path: Arc<Path> = Arc::from(dir.path().join("graph.db").as_path());

        // Initialise the file once so SQLite has somewhere to read from.
        sqlite::open(path.to_string_lossy().as_ref()).expect("create");

        let mut handles = Vec::new();
        for _ in 0..4 {
            let p = path.clone();
            handles.push(thread::spawn(move || enable_wal_mode(&p)));
        }
        for h in handles {
            h.join()
                .unwrap()
                .expect("enable_wal_mode must not fail under contention");
        }
    }

    /// AUDIT-051: write garbage bytes where `graph.db` lives and assert
    /// `init_db` classifies the failure as `InitDbError::Corrupt`. The
    /// frontend will then show the Repair flow instead of a generic toast.
    #[test]
    fn init_db_reports_corrupt_on_garbage_file() {
        use std::io::Write;
        let dir = TempDir::new().unwrap();
        // Write something that's definitely not a SQLite database file
        // header (which starts with the magic "SQLite format 3\0").
        let mut f = std::fs::File::create(dir.path().join("graph.db")).unwrap();
        f.write_all(b"not a real database, just bytes the user pasted by accident")
            .unwrap();
        drop(f);

        match init_db(dir.path()) {
            Err(InitDbError::Corrupt(msg)) => {
                // Message comes from SQLite/Cozo — we don't pin the exact
                // text, but it should be non-empty and obviously about
                // the bad file rather than empty/silent.
                assert!(
                    !msg.is_empty(),
                    "Corrupt variant must carry diagnostic text"
                );
            }
            Err(InitDbError::Other(msg)) => {
                panic!("expected InitDbError::Corrupt for garbage file, got Other({msg})");
            }
            Ok(_) => panic!("init_db must reject a garbage-bytes graph.db"),
        }
    }

    /// AUDIT-051: a healthy DB must open cleanly — guards against the
    /// `looks_corrupt` heuristic accidentally false-positive-ing on
    /// legitimate Cozo/SQLite startup chatter.
    #[test]
    fn init_db_succeeds_on_fresh_directory() {
        let dir = TempDir::new().unwrap();
        if let Err(e) = init_db(dir.path()) {
            panic!("fresh dir must initialize cleanly, got: {e}");
        }
    }

    /// AUDIT-051: end-to-end recovery shape — `init_db` reports a
    /// corrupt fixture, the fixture gets nuked (mimicking the body
    /// of `rebuild_database`), and the next `init_db` succeeds.
    /// Exercises the round-trip the IPC actually guarantees, without
    /// pulling in an AppHandle.
    #[test]
    fn delete_then_reinit_recovers_from_corruption() {
        use std::io::Write;
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("graph.db");

        // Plant a corrupt fixture.
        let mut f = std::fs::File::create(&db_path).unwrap();
        f.write_all(b"definitely not a SQLite file").unwrap();
        drop(f);
        assert!(matches!(init_db(dir.path()), Err(InitDbError::Corrupt(_))));

        // The rebuild path: remove graph.db (and any sidecars). After
        // that, init_db is the same fresh-roost path init_graph runs.
        std::fs::remove_file(&db_path).unwrap();
        // Sidecars may not exist on a never-opened file — best-effort.
        let _ = std::fs::remove_file(dir.path().join("graph.db-wal"));
        let _ = std::fs::remove_file(dir.path().join("graph.db-shm"));

        if let Err(e) = init_db(dir.path()) {
            panic!("init_db must succeed after the fixture is removed, got: {e}");
        }
    }

    /// DB-3: backup then restore must recover the exact pre-migration bytes,
    /// including the `-wal` sidecar (a backup of `graph.db` alone could drop
    /// committed-but-not-checkpointed frames).
    #[test]
    fn backup_and_restore_roundtrip_recovers_original_bytes() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("graph.db");
        std::fs::write(&db_path, b"ORIGINAL-DB-CONTENT").unwrap();
        std::fs::write(dir.path().join("graph.db-wal"), b"ORIGINAL-WAL").unwrap();

        let backups = backup_db_files(dir.path()).expect("backup");
        assert_eq!(backups.len(), 2, "graph.db + -wal must both be snapshotted");

        // Simulate a destructive half-migration mangling the live files.
        std::fs::write(&db_path, b"CORRUPTED-HALF-MIGRATION").unwrap();
        std::fs::write(dir.path().join("graph.db-wal"), b"CORRUPTED-WAL").unwrap();

        restore_db_backup(dir.path());

        assert_eq!(std::fs::read(&db_path).unwrap(), b"ORIGINAL-DB-CONTENT");
        assert_eq!(
            std::fs::read(dir.path().join("graph.db-wal")).unwrap(),
            b"ORIGINAL-WAL"
        );
        // Restore consumes the snapshot (rename), leaving a clean dir.
        assert!(!dir.path().join("graph.db.premigration-bak").exists());
    }

    /// DB-3: a successful migration must clean up its snapshot so backups
    /// don't accumulate on every version bump.
    #[test]
    fn cleanup_removes_backups_after_success() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("graph.db"), b"db").unwrap();
        let backups = backup_db_files(dir.path()).expect("backup");
        assert!(dir.path().join("graph.db.premigration-bak").exists());
        cleanup_db_backup(&backups);
        assert!(!dir.path().join("graph.db.premigration-bak").exists());
    }

    /// DB-3: if a migration created a fresh `-wal` that wasn't in the
    /// snapshot, restore must delete it — otherwise SQLite would replay the
    /// stale sidecar onto the restored main file and re-corrupt it.
    #[test]
    fn restore_removes_stale_wal_absent_from_backup() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("graph.db"), b"ORIG").unwrap();
        let _ = backup_db_files(dir.path()).expect("backup"); // graph.db only
        std::fs::write(dir.path().join("graph.db"), b"HALF").unwrap();
        std::fs::write(dir.path().join("graph.db-wal"), b"STALE-WAL").unwrap();

        restore_db_backup(dir.path());

        assert_eq!(std::fs::read(dir.path().join("graph.db")).unwrap(), b"ORIG");
        assert!(
            !dir.path().join("graph.db-wal").exists(),
            "stale wal not present in the snapshot must be removed on restore"
        );
    }

    /// NEW-DB2: a pre-migration snapshot failure must be FATAL when a
    /// migration is pending — `init_db` must abort rather than run the
    /// migration with no rollback net (which would risk losing the
    /// non-derivable `:edit_event` / `:approval_log` data DB-3 protects).
    /// Regression guard against reverting to the old `warn + Vec::new()`.
    #[test]
    fn migration_aborts_when_pre_migration_snapshot_fails() {
        let dir = TempDir::new().unwrap();
        // 1. Create a healthy DB, then stamp it at an OLD version so the next
        //    open sees a pending migration.
        {
            let db = init_db(dir.path()).expect("initial init");
            set_kv(&db, "schema_version", "1").expect("downgrade stamp");
        } // drop releases the file handle
          // 2. Obstruct the snapshot: a DIRECTORY where backup_db_files wants to
          //    write graph.db.premigration-bak makes std::fs::copy fail.
        std::fs::create_dir(dir.path().join("graph.db.premigration-bak")).unwrap();
        // 3. Re-open: migration pending + snapshot fails → must abort, and as
        //    `Other` (the DB is fine — must NOT trigger a corrupt-rebuild).
        match init_db(dir.path()) {
            Err(InitDbError::Other(msg)) => assert!(
                msg.contains("without a pre-migration backup"),
                "expected the snapshot-fatal error, got: {msg}"
            ),
            Err(InitDbError::Corrupt(m)) => {
                panic!("snapshot failure must be Other, not Corrupt: {m}")
            }
            Ok(_) => panic!("init_db must abort when the pre-migration snapshot fails"),
        }
    }

    /// AUDIT-051: `looks_corrupt` must match the strings SQLite actually
    /// emits for the corruption codes; otherwise the Repair flow never
    /// triggers.
    #[test]
    fn looks_corrupt_matches_sqlite_corruption_strings() {
        assert!(looks_corrupt("file is not a database"));
        assert!(looks_corrupt("database disk image is malformed"));
        assert!(looks_corrupt("malformed database schema (notes)"));
        // Case-insensitive — Cozo wraps the message and may re-cap.
        assert!(looks_corrupt("ERROR: Disk Image Is Malformed"));
        // Non-corruption errors must NOT match — otherwise a transient
        // permission denied would trigger an auto-rebuild and wipe the
        // user's embeddings.
        assert!(!looks_corrupt("permission denied"));
        assert!(!looks_corrupt("database is locked"));
        assert!(!looks_corrupt("disk full"));
    }
}

/// Current schema version. Bump when adding/changing relations in a way
/// that requires a migration. New databases are stamped with this; old
/// databases retain whatever they were last stamped with until a
/// migration runs.
pub const CURRENT_SCHEMA_VERSION: i64 = 6;

/// Create the core graph schema relations if they don't exist.
/// Uses `:create` with idempotent error handling — ignores "already exists" errors.
fn create_schema(db: &DbInstance) -> Result<(), String> {
    // Key-value metadata. Single source of truth for "what version is this
    // DB at" — `schema_version` drives the migration runner. Anything that
    // needs to survive schema changes goes here; adding a key never requires
    // a migration, since missing keys default in the read helper.
    run_script(
        db,
        r#"
        :create kv_meta {
            key: String
            =>
            value: String
        }
    "#,
    )?;
    seed_kv_meta(db)?;

    // Notes relation: one row per markdown file
    run_script(
        db,
        r#"
        :create notes {
            id: String
            =>
            title: String,
            path: String,
            updated_at: Float
        }
    "#,
    )?;

    // Links relation: from_id → to_id (wiki-links between notes)
    run_script(
        db,
        r#"
        :create links {
            from_id: String,
            to_id: String
            =>
            link_text: String
        }
    "#,
    )?;

    // Tags relation: note_id + tag (inline #tags)
    run_script(
        db,
        r#"
        :create tags {
            note_id: String,
            tag: String
        }
    "#,
    )?;

    // Reverse traversal is on the interactive graph-window hot path. The
    // primary `links` key starts with `from_id`, so incoming-edge lookups by
    // `to_id` otherwise scans the entire relation. Cozo's planner can select
    // this covering index for base-relation queries, and the hot path names it
    // explicitly to prevent a poor join-order choice at large scale.
    run_script(db, r#"::index create links:by_to {to_id}"#)?;

    // ai_signals: agentic-loop signal log. Records the agent loop's
    // feedback — every tool call and every result acceptance/dismissal —
    // and powers the §4.4 tool reliability scorecard. (The old
    // `ai_feedback` relation backed the link/tag suggestion-card UX, which
    // was removed with the embeddings subsystem; existing DBs keep the
    // orphaned relation, we simply stop creating it.)
    //
    // `query_hash` is a blake3 hash of the user's natural-language
    // question; rows for the same query group together for ranking.
    // `result_rank` is the position in the tool result list when the
    // user acted — needed to subtract rank-bias from accept signals.
    // `signal`: +1 accept, -1 dismiss, 0 implicit ignore.
    run_script(
        db,
        r#"
        :create ai_signals {
            signal_id: String
            =>
            query_hash: String,
            tool_name: String,
            suggestion_kind: String,
            suggestion_id: String,
            signal: Int,
            timestamp: Float,
            context_note_id: String,
            tool_iteration: Int,
            result_rank: Int,
            roost_path: String
        }
    "#,
    )?;

    // Graph algorithm result cache. Stores the output of expensive
    // algorithms (PageRank, CommunityDetectionLouvain,
    // BetweennessCentrality) so the graph view doesn't recompute on
    // every paint. Invalidated on every note save — `index_note` clears
    // entries for all algos because any link change can shift hub
    // ranks and community boundaries.
    //
    // Single shared schema for all algos: `value` carries the score
    // (PageRank/betweenness) or the community label (Louvain) cast to
    // f64. Discriminator is `algo`. ShortestPath is NOT cached here —
    // it's per-query and short-lived.
    run_script(
        db,
        r#"
        :create graph_algo_cache {
            algo: String,
            note_id: String
            =>
            value: Float,
            computed_at: Float
        }
    "#,
    )?;

    // Tasks relation: one row per `- [ ]` / `- [x]` checkbox in the roost,
    // plus rows derived from frontmatter `tasks:` arrays.
    //
    // Source of truth is the markdown line — this row is DERIVED.
    // Indexer (`db::queries::notes::index_note_tasks`) clears all rows
    // for a note then reinserts on every save. `purge_note` drops them
    // when the file is deleted.
    //
    // Composite key is (note_id, source_line). Frontmatter tasks use
    // `source_line = 0`. `due_at` / `completed_at` use empty-string
    // sentinel for "not set" because the schema convention here is
    // String (no Option type in stored relations).
    //
    // `linked_person` / `linked_project` are convenience denorms — the
    // first wikilink and first `#project/X` tag in the task text — used
    // by the Tasks pane (slice 2) for grouping without a JOIN. The full
    // tag/link sets continue to live in `tags` and `links` relations
    // populated by the existing pass-1 indexer.
    //
    // Phase 2.2 will add a stable UUID column when agent CRUD lands
    // (the agent needs a stable handle across rewrites). For Phase 2.1
    // the (note_id, source_line) tuple is enough for the pane UX.
    run_script(
        db,
        r#"
        :create task {
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
        }
    "#,
    )?;

    // Edit history relation (Phase 2.2). One row per recorded mutation
    // (file write, task toggle, task add, file rename, file delete).
    // Powers the History pane and `revert_edit`. See create_edit_event_relation
    // for the schema rationale.
    create_edit_event_relation(db)?;
    create_edit_event_after_hash_relation(db)?;

    // Approval audit log (agent-hardening § 3.4.4). One row per
    // decision emitted by the approval gate. Feeds the §4.4 tool
    // reliability scorecard and the Settings → AI → Approval History UI.
    create_approval_log_relation(db)?;

    // Dangling wiki-links (links whose target note doesn't exist yet), so a
    // backlink appears the moment the target is created — not only after the
    // referencing note is next re-saved. See create_dangling_relation.
    create_dangling_relation(db)?;

    Ok(())
}

/// Create the `:edit_event` relation. Called from both `create_schema`
/// (fresh DBs) and the v2 migration (existing DBs). Idempotent via the
/// shared `run_script` helper that swallows "already exists" errors.
///
/// Schema:
/// - `note_id` + `event_id` is the composite key. `event_id` is a
///   monotonically-increasing-per-note string (`<timestamp_ms>-<rand>`),
///   sortable lexicographically so `:order` works without a Float cast.
/// - `actor_type` ∈ {`user`, `agent`, `system`}. `actor_id` is an opaque
///   handle (e.g. `gemini-2.5-flash`, `user`, `watcher`); for the user
///   bucket it's just `"user"` until per-user identity lands.
/// - `intent` ∈ {`edit`, `create`, `delete`, `toggle_task`, `add_task`,
///   `rename`, `agent_write`} — the human label shown in the History pane.
/// - `diff` is the unified-diff text produced by `similar`. Stored as
///   String (Cozo has no Bytes type at this storage version); typical
///   diffs are <2 KB so the overhead is negligible. `before_content` is
///   kept verbatim to power exact reverts even after subsequent edits;
///   `after_content` is implicit (= what's on disk right after the event).
/// - `lines_added` / `lines_removed` are pre-computed for the pane row
///   summary so we don't have to re-parse the diff for every list call.
/// - `reverted_by` chains revert events. Empty string when not reverted;
///   set to the `event_id` of the revert when this event has been undone.
///   Cozo stored relations don't expose Optional types so empty-string
///   sentinels are the project convention (same as `:task.completed_at`).
fn create_edit_event_relation(db: &DbInstance) -> Result<(), String> {
    run_script(
        db,
        r#"
        :create edit_event {
            note_id: String,
            event_id: String,
            =>
            actor_type: String,
            actor_id: String,
            intent: String,
            timestamp: Float,
            diff: String,
            before_content: String,
            lines_added: Int,
            lines_removed: Int,
            reverted_by: String,
            note_path: String
        }
    "#,
    )
}

/// Create the `:dangling` relation. Called from both `create_schema` (fresh
/// DBs) and the v6 migration (existing DBs). Idempotent via the shared
/// `run_script` "already exists" handling.
///
/// One row per (source note, unresolved wiki-link target) — a link whose
/// target note doesn't exist yet, so pass-2 couldn't make a `:links` edge.
/// Keyed by `(from_id, target)` so pass-2 can range-clear a note's rows by
/// `from_id` cheaply on every save. `target` is the resolver-normalized link
/// target; `target_base` is its basename, the key the create path matches on
/// when promoting (`promote_dangling_links`). `link_text` is the display text
/// to carry onto the promoted edge.
fn create_dangling_relation(db: &DbInstance) -> Result<(), String> {
    run_script(
        db,
        r#"
        :create dangling {
            from_id: String,
            target: String
            =>
            link_text: String,
            target_base: String
        }
    "#,
    )
}

/// Sidecar relation that stores a content hash of the post-state for each
/// edit event. Without it, `revert_edit` couldn't tell whether the file
/// on disk diverged from the recorded chain — an external editor could
/// silently overwrite changes the agent never saw, and a revert would
/// blindly nuke them. With it, the revert command compares the current
/// file hash to the most recent event's recorded `after_hash`; mismatch
/// → external divergence detected, require explicit `force=true`.
///
/// Stored as a sidecar (separate relation) instead of a column on
/// `:edit_event` because Cozo doesn't support ALTER. Entries written for
/// every event from v4 onward; events recorded before this migration have
/// no row here, in which case the divergence check falls back to the
/// existing later-events heuristic.
fn create_edit_event_after_hash_relation(db: &DbInstance) -> Result<(), String> {
    run_script(
        db,
        r#"
        :create edit_event_after_hash {
            note_id: String,
            event_id: String,
            =>
            after_hash: String
        }
    "#,
    )
}

/// Approval audit log (agent-hardening § 3.4.4). One row per decision
/// the approval gate produces. Schema:
/// - `request_id` is the same UUID `approval::request()` mints and emits
///   on `ai:tool-pending`, so a row in this table can be cross-referenced
///   against frontend logs / tool-card renders.
/// - `decision` is one of `approve` | `always_approve` | `reject` |
///   `forgotten`. The last one fires when the receiver was dropped
///   without an explicit user decision — panel-unmount (§3.4.1),
///   Stop-Generation, app shutdown.
/// - `tier` mirrors the `SafetyTier` of the tool at the time of the
///   request (`read` | `write` | `destructive`). Stored as a string
///   because Cozo enums add migration cost for negligible benefit at
///   this volume.
/// - `args_hash` is `blake3(args_json)` truncated to 16 hex chars —
///   enough entropy to group identical calls without storing the full
///   args (which may contain PII). The full args content-addressed
///   sidecar is a follow-up if we ever need raw-args replay.
/// - `args_summary` is the same one-liner the tool card title used
///   (`"Edit projects/foo.md"`, `"Delete task #42"`), capped at 200
///   chars by the caller. Searchable in the History UI.
///
/// Idempotent — `run_script` swallows already-exists errors.
fn create_approval_log_relation(db: &DbInstance) -> Result<(), String> {
    run_script(
        db,
        r#"
        :create approval_log {
            request_id: String,
            =>
            timestamp: Float,
            tool: String,
            tier: String,
            decision: String,
            args_hash: String,
            args_summary: String
        }
    "#,
    )
}

/// Seed `kv_meta` with the current version stamps on first creation.
/// Subsequent runs hit the `:put` no-op path because the keys already
/// exist with non-stale values. We never overwrite an existing
/// `schema_version` value here — that's a migration's job.
fn seed_kv_meta(db: &DbInstance) -> Result<(), String> {
    use cozo::DataValue;
    use std::collections::BTreeMap;

    // Read what's there. Missing keys mean a fresh DB (or a pre-versioning
    // DB created before this commit landed). Either way, stamp the current
    // values. We only insert MISSING keys — we never clobber an existing
    // version number, since that would defeat migration logic.
    let existing = db
        .run_script(
            r#"?[key, value] := *kv_meta{ key, value }"#,
            BTreeMap::new(),
            ScriptMutability::Immutable,
        )
        .map_err(|e| format!("Failed to read kv_meta: {e}"))?;

    let mut have: std::collections::HashSet<String> = std::collections::HashSet::new();
    for row in &existing.rows {
        if let Some(k) = row.first().and_then(|v| v.get_str()) {
            have.insert(k.to_string());
        }
    }

    let seeds: &[(&str, String)] = &[("schema_version", CURRENT_SCHEMA_VERSION.to_string())];

    for (key, value) in seeds {
        if have.contains(*key) {
            continue;
        }
        let mut params = BTreeMap::new();
        params.insert("key".to_string(), DataValue::Str((*key).to_string().into()));
        params.insert(
            "value".to_string(),
            DataValue::Str(value.to_string().into()),
        );
        db.run_script(
            r#"?[key, value] <- [[$key, $value]]
               :put kv_meta { key => value }"#,
            params,
            ScriptMutability::Mutable,
        )
        .map_err(|e| format!("Failed to seed kv_meta[{key}]: {e}"))?;
    }
    Ok(())
}

/// Read a `kv_meta` value by key. Returns `None` for missing keys — the
/// caller decides what default to use. Cheap (single-row indexed lookup).
#[allow(dead_code)] // first consumer is the not-yet-written migration runner
pub fn get_kv(db: &DbInstance, key: &str) -> Result<Option<String>, String> {
    use cozo::DataValue;
    use std::collections::BTreeMap;

    let mut params = BTreeMap::new();
    params.insert("key".to_string(), DataValue::Str(key.to_string().into()));
    let result = db
        .run_script(
            r#"?[value] := *kv_meta{ key, value }, key == $key"#,
            params,
            ScriptMutability::Immutable,
        )
        .map_err(|e| format!("kv_meta lookup failed: {e}"))?;
    Ok(result
        .rows
        .first()
        .and_then(|row| row.first())
        .and_then(|v| v.get_str())
        .map(String::from))
}

/// Run a CozoScript, ignoring errors when relations already exist (idempotent schema creation).
fn run_script(db: &DbInstance, script: &str) -> Result<(), String> {
    match db.run_script(script, Default::default(), ScriptMutability::Mutable) {
        Ok(_) => Ok(()),
        Err(e) => {
            let msg = e.to_string();
            // Idempotent: ignore if relation already exists (CozoDB says "conflicts with")
            if msg.contains("already exists")
                || msg.contains("conflicts with")
                || msg.contains("Stored relation")
            {
                Ok(())
            } else {
                Err(format!("Schema error: {msg}"))
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod tests {
    use super::*;

    /// Helper: open a fresh in-memory DB, manually create + seed kv_meta.
    /// Avoids the SQLite-specific `init_db` path so the test stays hermetic.
    fn fresh_kv_db() -> DbInstance {
        let db = DbInstance::new("mem", "", Default::default()).expect("mem db");
        db.run_script(
            r#":create kv_meta { key: String => value: String }"#,
            Default::default(),
            ScriptMutability::Mutable,
        )
        .expect("create kv_meta");
        seed_kv_meta(&db).expect("seed");
        db
    }

    #[test]
    fn kv_meta_seeds_current_versions_on_fresh_db() {
        let db = fresh_kv_db();
        assert_eq!(
            get_kv(&db, "schema_version").unwrap(),
            Some(CURRENT_SCHEMA_VERSION.to_string())
        );
    }

    #[test]
    fn compact_graph_db_runs_without_error() {
        // DB-5: `::compact` must succeed against a live (writable) DB. On the
        // mem/sqlite backend it's a pool-flush no-op for space (documented on
        // the fn), but it must never error — the scheduler treats an error as
        // a skipped maintenance cycle.
        let db = fresh_kv_db();
        compact_graph_db(&db).expect("compact should succeed on a writable DB");
    }

    #[test]
    fn graph_reverse_lookup_indices_are_created_idempotently() {
        let db = bare_mem_db();
        create_schema(&db).expect("create schema");
        create_schema(&db).expect("re-create schema");
        assert!(relation_is_queryable(
            &db,
            r#"?[from_id] := *links:by_to{ to_id: "target", from_id }"#
        ));
    }

    #[test]
    fn kv_meta_does_not_overwrite_existing_values() {
        let db = fresh_kv_db();
        // Simulate an older DB that has already been migrated to v7.
        use cozo::DataValue;
        use std::collections::BTreeMap;
        let mut params = BTreeMap::new();
        params.insert("key".to_string(), DataValue::Str("schema_version".into()));
        params.insert("value".to_string(), DataValue::Str("7".into()));
        db.run_script(
            r#"?[key, value] <- [[$key, $value]]
               :put kv_meta { key => value }"#,
            params,
            ScriptMutability::Mutable,
        )
        .expect("override");

        // Re-running the seeder must NOT clobber the existing 7. Migrations
        // own the version field; the seeder only fills missing keys.
        seed_kv_meta(&db).expect("re-seed");
        assert_eq!(
            get_kv(&db, "schema_version").unwrap(),
            Some("7".to_string())
        );
    }

    #[test]
    fn kv_meta_returns_none_for_missing_key() {
        let db = fresh_kv_db();
        assert_eq!(get_kv(&db, "nonexistent").unwrap(), None);
    }

    /// Force a known schema_version onto a fresh kv_meta-only DB, then
    /// run the migration runner. Used by the migration tests below to
    /// simulate older/newer DBs.
    fn stamp_version(db: &DbInstance, v: i64) {
        set_kv(db, "schema_version", &v.to_string()).expect("stamp version");
    }

    #[test]
    fn migrations_no_op_when_db_is_current() {
        let db = fresh_kv_db();
        // Fresh seed already stamps CURRENT.
        run_migrations(&db).expect("no-op should succeed");
        assert_eq!(
            get_kv(&db, "schema_version").unwrap(),
            Some(CURRENT_SCHEMA_VERSION.to_string())
        );
    }

    #[test]
    fn migrations_refuse_to_downgrade() {
        let db = fresh_kv_db();
        // Pretend a future build wrote a newer version.
        stamp_version(&db, CURRENT_SCHEMA_VERSION + 1);
        let err = run_migrations(&db).expect_err("downgrade must abort");
        assert!(
            err.contains("newer") && err.contains("Refusing"),
            "expected a downgrade-refusal error, got: {err}"
        );
    }

    #[test]
    fn migrations_reject_non_integer_version() {
        let db = fresh_kv_db();
        // Corrupt-looking value — runner must surface it, not panic.
        set_kv(&db, "schema_version", "not-a-number").expect("write garbage");
        let err = run_migrations(&db).expect_err("non-integer must fail");
        assert!(
            err.contains("not a valid integer"),
            "expected integer-parse error, got: {err}"
        );
    }

    // ── DB-1: migration *shape* tests ────────────────────────────────────
    // The tests above exercise the runner's control flow. These assert that
    // each migration's `apply` body actually produces the relation it claims
    // — the "build v(N-1), apply, assert v(N) shape" round-trip that was
    // missing. A migration that silently created the wrong relation (or
    // nothing) would now fail here instead of corrupting a user DB.

    /// Bare in-memory DB with no relations at all — the blank slate a
    /// migration body must be able to create its relation against.
    fn bare_mem_db() -> DbInstance {
        DbInstance::new("mem", "", Default::default()).expect("mem db")
    }

    /// A relation exists and is well-formed iff a query naming its columns
    /// runs without error.
    fn relation_is_queryable(db: &DbInstance, query: &str) -> bool {
        db.run_script(query, Default::default(), ScriptMutability::Immutable)
            .is_ok()
    }

    #[test]
    fn migrate_v2_creates_edit_event_relation() {
        let db = bare_mem_db();
        migrate_v2_edit_events(&db).expect("v2 apply");
        assert!(
            relation_is_queryable(&db, r#"?[note_id] := *edit_event{ note_id }"#),
            ":edit_event must be queryable after the v2 migration"
        );
        // Idempotent: re-applying (as create_schema does every startup) must
        // not error.
        migrate_v2_edit_events(&db).expect("v2 re-apply must be idempotent");
    }

    #[test]
    fn migrate_v4_creates_after_hash_relation() {
        let db = bare_mem_db();
        migrate_v4_edit_event_after_hash(&db).expect("v4 apply");
        assert!(
            relation_is_queryable(
                &db,
                r#"?[note_id, after_hash] := *edit_event_after_hash{ note_id, after_hash }"#
            ),
            ":edit_event_after_hash must expose note_id + after_hash after v4"
        );
    }

    #[test]
    fn migrate_v5_creates_approval_log_relation() {
        let db = bare_mem_db();
        migrate_v5_approval_log(&db).expect("v5 apply");
        assert!(
            relation_is_queryable(&db, r#"?[request_id] := *approval_log{ request_id }"#),
            ":approval_log must be queryable after the v5 migration"
        );
    }

    #[test]
    fn migrate_v6_creates_dangling_relation() {
        let db = bare_mem_db();
        migrate_v6_dangling(&db).expect("v6 apply");
        assert!(
            relation_is_queryable(&db, r#"?[from_id, target] := *dangling{ from_id, target }"#),
            ":dangling must expose from_id + target after the v6 migration"
        );
    }

    #[test]
    fn migrate_v3_is_a_noop_and_never_errors() {
        // Historical episodic-memory migration; must stay a clean no-op so
        // the version chain remains contiguous.
        let db = bare_mem_db();
        migrate_v3_episodes(&db).expect("v3 no-op must succeed on any DB");
    }

    /// The headline DB-1 test: a v1-era DB (only kv_meta) taken through the
    /// full chain ends up at CURRENT with every migration-owned relation
    /// present and queryable.
    #[test]
    fn full_migration_chain_from_v1_creates_all_relations() {
        let db = fresh_kv_db();
        stamp_version(&db, 1);
        run_migrations(&db).expect("migration chain from v1");

        assert_eq!(
            get_kv(&db, "schema_version").unwrap(),
            Some(CURRENT_SCHEMA_VERSION.to_string()),
            "chain must land on CURRENT_SCHEMA_VERSION"
        );
        for q in [
            r#"?[note_id] := *edit_event{ note_id }"#,
            r#"?[note_id] := *edit_event_after_hash{ note_id }"#,
            r#"?[request_id] := *approval_log{ request_id }"#,
            r#"?[from_id] := *dangling{ from_id }"#,
        ] {
            assert!(
                relation_is_queryable(&db, q),
                "relation missing after full chain: {q}"
            );
        }
    }

    #[test]
    fn migration_chain_is_idempotent_on_rerun() {
        let db = fresh_kv_db();
        stamp_version(&db, 1);
        run_migrations(&db).expect("first run");
        // Second run must be a clean no-op (relations already exist; the
        // shared run_script swallows "already exists").
        run_migrations(&db).expect("re-run must be a no-op");
        assert_eq!(
            get_kv(&db, "schema_version").unwrap(),
            Some(CURRENT_SCHEMA_VERSION.to_string())
        );
    }

    #[test]
    fn migration_pending_true_when_behind_false_otherwise() {
        let db = fresh_kv_db(); // seeded at CURRENT
        assert!(!migration_pending(&db), "current DB has nothing pending");
        stamp_version(&db, 1);
        assert!(migration_pending(&db), "v1 DB is behind CURRENT");
        stamp_version(&db, CURRENT_SCHEMA_VERSION + 1);
        assert!(
            !migration_pending(&db),
            "a newer-than-binary DB is not 'pending' (run_migrations aborts it separately)"
        );
    }
}
