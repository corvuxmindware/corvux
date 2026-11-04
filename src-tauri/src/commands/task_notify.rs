//! Task notification scheduler — Phase 2.1 slices 5 + 6.
//!
//! Watches the `:task` relation and fires OS notifications when a task
//! with `📅 + ⏰` (date + time) due_at comes due. Two backends:
//!
//! - **Desktop** — one tokio task per pending notification, sleeping
//!   until the due instant then calling the immediate-fire path of
//!   `tauri-plugin-notification`. The "never forget" promise survives
//!   app restarts via a [`DispatchLedger`] — a small JSON file in app
//!   data that records every fired (note_id, source_line, due_at)
//!   triple. On launch, `reschedule_all` re-evaluates every pending
//!   task; tasks whose due time has already passed get fired
//!   immediately as `[Missed]`-prefixed notifications when (a) they
//!   haven't been fired before per the ledger and (b) they're still
//!   inside `MISSED_GRACE_WINDOW`. Beyond the grace window we still
//!   record them as fired so a roost left closed for a month doesn't
//!   notification-storm on the next launch.
//!
//! - **Mobile (Android, iOS)** — enrolls the notification with the OS
//!   via `Notification::builder().schedule(Schedule::At { date }).id(N)
//!   .show()`, which routes through the plugin's Kotlin / Swift bridge
//!   into Android `AlarmManager` (with `SCHEDULE_EXACT_ALARM` + a
//!   `BroadcastReceiver`) or iOS `UNCalendarNotificationTrigger`. The
//!   alarm fires regardless of whether the app process is alive.
//!   TRN-3: because `id = blake3(note_id:source_line)` changes when a
//!   task line shifts, and a deleted task's alarm has no live task to
//!   cancel it against, `reschedule_all` reconciles the OS's own pending
//!   queue on launch (`pending()` → cancel the ids no live task backs)
//!   so stale/orphan alarms don't fire phantom notifications. NEW-T2:
//!   the OS can also PURGE a pending alarm before it fires (reboot,
//!   Doze, force-stop); on the next launch mobile now runs the same
//!   `[Missed]` catch-up desktop does — an immediate notification for a
//!   recently-missed task, deduped via the shared ledger.
//!
//! In both cases the timer is keyed on `(note_id, source_line)` so the
//! scheduler can cancel/replace when a task's due date changes or it
//! gets completed. The scheduler is rebuilt:
//!   - After `init_graph` / `bulk_index_pass1` (roost open: full rebuild)
//!   - After per-save `index_note_tasks` (rebuild for that note only)
//!   - After `complete_task` and `add_task_line` (their indexer call
//!     hits `reschedule_for_note` which cancels stale timers).
//!
//! Time semantics: `due_at` is stored verbatim from the markdown
//! (`YYYY-MM-DD[ HH:MM]`). We interpret it as the user's local wall-clock
//! time and convert to a UTC instant via `chrono::Local`. For mobile
//! scheduling we then re-encode that instant as `time::OffsetDateTime`
//! (the type the plugin expects). TRN-2: the wall-clock→instant
//! conversion handles DST seams — a fall-back fold picks the earlier
//! instant, and a spring-forward gap (a wall-clock time that doesn't
//! exist) is nudged +1h so the reminder still fires rather than being
//! silently dropped. The absolute instant is then fired via a monotonic
//! sleep (desktop) or an OS alarm (mobile), neither of which is perturbed
//! by a DST transition inside the wait window.

#[cfg(not(any(target_os = "android", target_os = "ios")))]
use std::cmp::Reverse;
#[cfg(not(any(target_os = "android", target_os = "ios")))]
use std::collections::BinaryHeap;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use cozo::{DbInstance, ScriptMutability};
use tauri::AppHandle;
// `Manager` is now used on every platform: `install` hydrates the missed-
// notification ledger from `app_data_dir()` on desktop AND mobile (NEW-T2 —
// mobile launch catch-up reuses the same ledger for dedup).
use tauri::Manager;
use tauri_plugin_notification::NotificationExt;

// AUDIT-054: the desktop path no longer spawns one tokio task per
// scheduled notification. A single ticker task (see `spawn_ticker`)
// polls a BinaryHeap of (deadline, TaskKey) entries and fires the
// next-soonest. The `tokio::sync::Notify` here lets `enroll_timer` /
// `abort_one` wake the ticker when a new earlier deadline is pushed
// or a cancellation tombstones the current sleep target.
#[cfg(not(any(target_os = "android", target_os = "ios")))]
use tokio::sync::Notify;

#[cfg(any(target_os = "android", target_os = "ios"))]
use tauri_plugin_notification::Schedule;

type TaskKey = (String, i32);

/// How long after a task's due time we still bother firing a missed
/// notification on launch. Beyond this window we silently mark it as
/// fired in the ledger — a roost left closed for a month should not
/// produce a 50-notification storm at restart.
///
/// Now shared by BOTH backends: desktop's launch catch-up (`try_fire_missed`)
/// and mobile's launch catch-up (`try_fire_missed_mobile`, NEW-T2) gate on the
/// same window via `should_fire_missed`.
const MISSED_GRACE_WINDOW: Duration = Duration::from_secs(24 * 60 * 60);

/// Cap on the number of missed-notification firings per launch. Stops a
/// pathological "roost was closed for 12 hours with 200 reminders due"
/// case from spamming the OS notification center. Anything beyond the
/// cap is still recorded as fired so it doesn't keep coming back.
/// Shared by both backends (see `MISSED_GRACE_WINDOW`).
const MAX_MISSED_PER_LAUNCH: usize = 10;

/// NEW-T1: the desktop ticker never sleeps longer than this in a single
/// `tokio::time::sleep`. A task due far in the future stays enrolled in the
/// heap; the ticker wakes each chunk, sees the deadline is still ahead, and
/// sleeps another chunk. This replaces the old "cap enrollment at 7 days and
/// return `None`" behavior, which silently dropped any task more than 7 days
/// out whenever the app was left open past the next roost-reopen re-enroll —
/// so a reminder 8+ days away never fired. Chunking keeps every individual
/// sleep well inside tokio's timer-driver range and makes long waits robust to
/// system clock jumps / suspend-resume. 7 days is arbitrary-but-comfortable.
#[cfg(not(any(target_os = "android", target_os = "ios")))]
const MAX_SLEEP_CHUNK: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// `actionTypeId` for task-due notifications. The frontend registers
/// a matching type at startup (`Complete` / `Snooze 15m` buttons); the
/// scheduler attaches this id to every fire so iOS shows the buttons
/// and Android renders them in the expanded notification. The string
/// is shared as a `pub(crate) const` so the frontend constant in
/// `lib/notification-actions.ts` is grep-pairable.
pub(crate) const TASK_DUE_ACTION_TYPE: &str = "task-due";

/// Build the anchor string the agent CRUD tools use to locate a task
/// line. Mirrors what `tasks::find_task_line_index` expects — the
/// first ~24 chars of the task body, trimmed and emoji-stripped just
/// enough that a quick-add line and its agent-side reference match.
/// Kept ASCII-permissive on purpose; the exact-prefix matcher is more
/// forgiving than this helper, so being slightly generous here is fine.
fn anchor_for(text: &str) -> String {
    // Strip leading whitespace; cap to a length that's unambiguous in
    // most roosts but not so long it hits the OS notification's
    // payload cap on Android.
    let trimmed = text.trim_start();
    let cap = 64usize;
    if trimmed.len() <= cap {
        trimmed.to_string()
    } else {
        // Cut at a char boundary so we never split a UTF-8 sequence.
        let mut end = cap;
        while end > 0 && !trimmed.is_char_boundary(end) {
            end -= 1;
        }
        trimmed[..end].to_string()
    }
}

/// Platform-specific handle for an enrolled timer:
///   - Desktop (post-AUDIT-054): the full task payload + deadline. The
///     single ticker task reads these when a heap entry's deadline
///     comes up. Cancel = remove from the `timers` map; the heap
///     entry becomes a tombstone that the ticker discards on pop.
///   - Mobile: the i32 notification id that the OS holds in its
///     pending-alarm queue; we cancel by id through the plugin.
#[cfg(not(any(target_os = "android", target_os = "ios")))]
#[derive(Clone)]
struct DesktopEntry {
    /// Absolute fire-time. Stored separately from `task.due_at` (which
    /// is a wall-clock string) so the ticker doesn't re-parse on every
    /// peek.
    deadline: SystemTime,
    task: PendingTask,
}
#[cfg(not(any(target_os = "android", target_os = "ios")))]
type EntryHandle = DesktopEntry;
#[cfg(any(target_os = "android", target_os = "ios"))]
type EntryHandle = i32;

/// Heap entry: `Reverse<SystemTime>` so `BinaryHeap` pops the earliest
/// deadline first. The `TaskKey` is what we look up in `timers` to
/// recover the payload and to confirm the entry hasn't been cancelled.
#[cfg(not(any(target_os = "android", target_os = "ios")))]
type HeapItem = (Reverse<SystemTime>, TaskKey);

#[derive(Default)]
pub struct NotificationScheduler {
    inner: Arc<Mutex<SchedulerState>>,
    /// AUDIT-054: signal handle the ticker waits on. Push/cancel paths
    /// call `notify_one()` so the ticker re-peeks instead of sleeping
    /// to its previously-known deadline. Lives outside the mutex so
    /// the ticker doesn't need to hold the lock while waiting.
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    wake: Arc<Notify>,
}

#[derive(Default)]
struct SchedulerState {
    timers: HashMap<TaskKey, EntryHandle>,
    /// AUDIT-054: min-heap of pending deadlines for the ticker. May
    /// contain entries whose `TaskKey` has been removed from `timers`
    /// (cancellations) — those are tombstones and the ticker drops
    /// them on pop.
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    heap: BinaryHeap<HeapItem>,
    /// AUDIT-054: set true after the first `install` so we only spawn
    /// the ticker once across multiple `install` calls (test reentry,
    /// roost re-open).
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    ticker_spawned: bool,
    app_handle: Option<AppHandle>,
    /// Persisted across restarts. Keyed on the `dispatch_key` triple so
    /// rescheduling a task to a new due time is treated as a new event.
    /// Used by BOTH backends' missed-notification catch-up: desktop fires
    /// missed tasks on launch, and (NEW-T2) mobile now also fires missed
    /// tasks on launch when the OS dropped an alarm (reboot / Doze /
    /// force-stop). The ledger dedupes so a recently-missed task isn't
    /// re-shown on every subsequent launch.
    ledger: DispatchLedger,
    /// Bounded counter so MAX_MISSED_PER_LAUNCH actually caps firings
    /// across all `enroll_timer` calls during a single launch (the
    /// scheduler may rebuild several times — once on init_graph, once
    /// per per-note save — and we don't want each rebuild's first 10
    /// to fire fresh).
    missed_fired_this_launch: usize,
}

/// Persistent ledger of missed notifications we've already shown. Lives
/// at `<app_data>/missed-notifications.json` as a flat object mapping
/// `dispatch_key → fired_unix_secs`. Pruned to entries fired within the
/// last 30 days on every save so the file stays bounded.
#[derive(Default)]
struct DispatchLedger {
    entries: HashSet<String>,
    path: Option<PathBuf>,
}

impl DispatchLedger {
    fn load(path: PathBuf) -> Self {
        let entries = match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str::<HashMap<String, u64>>(&text)
                .map(|m| {
                    let cutoff = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0)
                        .saturating_sub(30 * 24 * 60 * 60);
                    m.into_iter()
                        .filter(|(_, fired_at)| *fired_at >= cutoff)
                        .map(|(k, _)| k)
                        .collect::<HashSet<String>>()
                })
                .unwrap_or_default(),
            Err(_) => HashSet::new(),
        };
        Self {
            entries,
            path: Some(path),
        }
    }

    fn contains(&self, key: &str) -> bool {
        self.entries.contains(key)
    }

    fn record(&mut self, key: String) {
        if !self.entries.insert(key) {
            return;
        }
        let Some(path) = self.path.as_ref() else {
            return;
        };
        let now_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let map: HashMap<&String, u64> = self.entries.iter().map(|k| (k, now_secs)).collect();
        if let Ok(json) = serde_json::to_string(&map) {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(path, json);
        }
    }
}

fn dispatch_key(note_id: &str, source_line: i32, due_at: &str) -> String {
    format!("{note_id}:{source_line}:{due_at}")
}

/// Shared missed-notification gate (desktop launch catch-up + NEW-T2 mobile
/// launch catch-up). Given a past-due task's `dispatch_key`, decides whether
/// the caller should fire a `[Missed]` notification *now*, and records the
/// decision so it never repeats on a later launch. Returns `true` only when:
///   1. we haven't already fired this exact (note, line, due) triple, AND
///   2. the miss is still inside `MISSED_GRACE_WINDOW` (recent enough to be
///      worth surfacing — a month-old task is silently marked, not shown), AND
///   3. we're under `MAX_MISSED_PER_LAUNCH` for this launch.
///
/// Recording happens unconditionally on the first sight of a key (even when
/// grace/cap suppress the actual fire) so a stale or capped task doesn't keep
/// re-qualifying every launch. Platform-agnostic + no AppHandle so both
/// backends share it and it's unit-testable without a device.
fn should_fire_missed(
    state: &mut SchedulerState,
    key: String,
    deadline: SystemTime,
    now: SystemTime,
) -> bool {
    if state.ledger.contains(&key) {
        return false;
    }
    // Always record so a 30-day-old (or capped) task doesn't re-fire on every
    // launch.
    state.ledger.record(key);

    let elapsed = now.duration_since(deadline).unwrap_or(Duration::ZERO);
    if elapsed > MISSED_GRACE_WINDOW {
        return false;
    }
    if state.missed_fired_this_launch >= MAX_MISSED_PER_LAUNCH {
        return false;
    }
    state.missed_fired_this_launch += 1;
    true
}

/// On-device QA helper — fires an immediate notification shaped exactly
/// like a real task-due fire (same `action_type_id`, same four extras)
/// so testers can verify the Complete / Snooze 15m buttons render and
/// the action-routing wire reaches `planDispatch`. The sentinel
/// `note_path` value (`__corvux_test__`) means the dispatcher's
/// downstream `complete_task` / `reschedule_task` IPC will fail soft
/// with NotFound — that's expected; the QA signal is whether the
/// buttons appear on the device's lock screen at all (Android #2968 +
/// iOS `UNNotificationAction` are the open verification items).
#[tauri::command]
#[specta::specta]
pub async fn send_test_notification(app: AppHandle) -> Result<(), String> {
    let body = "Tap Complete or Snooze 15m to test button routing. Safe — won't touch your roost.";
    app.notification()
        .builder()
        .title("Corvux — test notification")
        .body(body)
        .action_type_id(TASK_DUE_ACTION_TYPE)
        .extra("note_id", "__corvux_test__")
        .extra("note_path", "__corvux_test__")
        // All notification `extra` values MUST be strings: iOS decodes the extra
        // map as [String: String], so a numeric value (e.g. source_line) throws a
        // Swift DecodingError(typeMismatch) on schedule/fire. Desktop/Android
        // tolerate numbers; iOS does not. The frontend (notification-actions.ts)
        // parses source_line back to a number. Keep every `.extra(...)` a string.
        .extra("source_line", "0")
        .extra("text_anchor", anchor_for(body))
        .show()
        .map_err(|e| e.to_string())?;
    Ok(())
}

impl NotificationScheduler {
    pub fn new() -> Self {
        Self::default()
    }

    /// Wire the scheduler to a Tauri app handle. Called once during
    /// `setup` in `lib.rs::run`. Without this the scheduler has nowhere
    /// to send notifications and silently no-ops.
    ///
    /// This is also where we hydrate the missed-notification ledger from disk
    /// so the very first `reschedule_all` call (fired from `init_graph` on
    /// roost open) can correctly skip tasks that were already fired in a prior
    /// launch. Done on desktop AND mobile — NEW-T2 gave mobile a launch
    /// catch-up path that needs the same dedup persistence.
    pub fn install(&self, app: AppHandle) {
        let ledger_path = app
            .path()
            .app_data_dir()
            .ok()
            .map(|d| d.join("missed-notifications.json"));

        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        let needs_ticker = {
            if let Ok(mut state) = self.inner.lock() {
                if let Some(path) = ledger_path {
                    state.ledger = DispatchLedger::load(path);
                }
                state.app_handle = Some(app);
                if !state.ticker_spawned {
                    state.ticker_spawned = true;
                    true
                } else {
                    false
                }
            } else {
                false
            }
        };

        // Mobile install: stash the AppHandle + hydrate the ledger, no ticker
        // (the OS holds the alarms; we only need the ledger for the NEW-T2
        // launch catch-up).
        #[cfg(any(target_os = "android", target_os = "ios"))]
        {
            if let Ok(mut state) = self.inner.lock() {
                if let Some(path) = ledger_path {
                    state.ledger = DispatchLedger::load(path);
                }
                state.app_handle = Some(app);
            }
        }

        // AUDIT-054: spawn the single ticker task that owns ALL
        // sleeping. Replaces the prior N-tasks-per-N-notifications
        // shape. Called once per process; the `ticker_spawned` flag
        // makes re-invocations of `install` (test reentry, roost
        // re-open) safe.
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        if needs_ticker {
            let state = self.inner.clone();
            let wake = self.wake.clone();
            crate::background_runtime::handle().spawn(async move {
                run_ticker(state, wake).await;
            });
        }
    }

    /// Cancel every timer for `note_id`. Used when a note is purged or
    /// before reindexing its tasks.
    fn cancel_for_note(state: &mut SchedulerState, note_id: &str) {
        let stale: Vec<TaskKey> = state
            .timers
            .keys()
            .filter(|(nid, _)| nid == note_id)
            .cloned()
            .collect();
        let app = state.app_handle.clone();
        for key in stale {
            if let Some(handle) = state.timers.remove(&key) {
                abort_one(app.as_ref(), handle);
            }
        }
    }

    /// Reschedule timers for a single note based on the current `:task`
    /// rows. Cheap — typical note has 0–10 tasks.
    pub fn reschedule_for_note(&self, db: &DbInstance, note_id: &str) {
        let pending = match query_pending_for_note(db, note_id) {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!(error = %e, %note_id, "task_notify: query failed");
                return;
            }
        };

        if let Ok(mut state) = self.inner.lock() {
            if state.app_handle.is_none() {
                return;
            }
            Self::cancel_for_note(&mut state, note_id);
            for row in pending {
                let key = (row.note_id.clone(), row.source_line);
                if let Some(handle) = enroll_timer(&mut state, row) {
                    // AUDIT-054: desktop also pushes onto the ticker
                    // heap; mobile path's EntryHandle is the i32 OS
                    // notification id and doesn't need a heap entry.
                    #[cfg(not(any(target_os = "android", target_os = "ios")))]
                    state.heap.push((Reverse(handle.deadline), key.clone()));
                    state.timers.insert(key, handle);
                }
            }
        }
        // AUDIT-054: wake the desktop ticker so it re-peeks. A new
        // task may have an earlier deadline than the one it's
        // currently sleeping toward.
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        self.wake.notify_one();
    }

    /// Reschedule every timer from a fresh full-roost query. Called once
    /// per roost open. Avoid calling per-save — it scales O(total_tasks).
    pub fn reschedule_all(&self, db: &DbInstance) {
        let pending = match query_pending_all(db) {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!(error = %e, "task_notify: full query failed");
                return;
            }
        };

        if let Ok(mut state) = self.inner.lock() {
            if state.app_handle.is_none() {
                return;
            }
            // TRN-3: on mobile, OS alarms armed in a PRIOR launch are NOT in
            // our in-memory `timers` map (it starts empty each process), so
            // draining `timers` cancels nothing and stale alarms leak — a task
            // whose line shifted, or a deleted task/note, keeps its old
            // `blake3(note_id:source_line)` alarm armed and fires a phantom
            // notification. Reconcile against the OS's own pending-alarm queue:
            // cancel every armed alarm that no longer maps to a live task. The
            // valid tasks are re-armed by the enroll loop below (scheduling the
            // same id just replaces the alarm — idempotent). `reschedule_all`
            // is the full-roost rebuild (roost open / settings change), so this
            // is the right place for the sweep.
            #[cfg(any(target_os = "android", target_os = "ios"))]
            if let Some(app) = state.app_handle.as_ref() {
                let valid: HashSet<i32> = pending
                    .iter()
                    .map(|p| task_notification_id(&p.note_id, p.source_line))
                    .collect();
                match app.notification().pending() {
                    Ok(armed) => {
                        let armed_ids: Vec<i32> = armed.iter().map(|n| n.id()).collect();
                        let orphans = orphan_alarm_ids(&armed_ids, &valid);
                        let orphan_count = orphans.len();
                        if orphan_count > 0 {
                            if let Err(e) = app.notification().cancel(orphans) {
                                tracing::warn!(error = %e, "task_notify: failed to cancel orphan OS alarms");
                            } else {
                                tracing::info!(
                                    count = orphan_count,
                                    "task_notify: cancelled orphan OS alarms"
                                );
                            }
                        }
                    }
                    // If we can't enumerate the OS queue, fall back to the
                    // coarser cancel-all: the enroll loop below re-arms every
                    // live task, so we still end clean, just with more churn.
                    Err(e) => {
                        tracing::warn!(error = %e, "task_notify: pending() failed, falling back to cancel_all");
                        let _ = app.notification().cancel_all();
                    }
                }
            }
            let drained: Vec<EntryHandle> = state.timers.drain().map(|(_, v)| v).collect();
            // AUDIT-054: clear the heap on full rebuild. Old entries
            // would still tombstone correctly (drained from `timers`)
            // but draining the heap avoids the ticker churning through
            // a pile of tombstones before reaching live entries.
            #[cfg(not(any(target_os = "android", target_os = "ios")))]
            state.heap.clear();
            let app_for_abort = state.app_handle.clone();
            for handle in drained {
                abort_one(app_for_abort.as_ref(), handle);
            }
            for row in pending {
                let key = (row.note_id.clone(), row.source_line);
                if let Some(handle) = enroll_timer(&mut state, row) {
                    #[cfg(not(any(target_os = "android", target_os = "ios")))]
                    state.heap.push((Reverse(handle.deadline), key.clone()));
                    state.timers.insert(key, handle);
                }
            }
        }
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        self.wake.notify_one();
        tracing::info!("task_notify: scheduler rebuilt");
    }
}

#[derive(Debug, Clone)]
struct PendingTask {
    note_id: String,
    source_line: i32,
    text: String,
    /// Local-time string `YYYY-MM-DD HH:MM` from `:task.due_at`.
    due_at: String,
    note_title: String,
    /// Absolute path from `:notes.path`. Used as the notification
    /// payload's `note_path` extra so the frontend's `onAction` handler
    /// can call `complete_task` / `reschedule_task` directly (their
    /// IPCs take a `notePath`, not the path-derived `note_id`).
    note_path: String,
}

fn query_pending_all(db: &DbInstance) -> Result<Vec<PendingTask>, String> {
    let script = r#"
        ?[note_id, source_line, text, due_at, note_title, note_path] :=
            *task{ note_id, source_line, text, due_at, completed },
            *notes{ id: note_id, title: note_title, path: note_path },
            !completed,
            due_at != ""
    "#;
    let result = db
        .run_script(
            script,
            std::collections::BTreeMap::new(),
            ScriptMutability::Immutable,
        )
        .map_err(|e| e.to_string())?;
    let mut pending: Vec<PendingTask> = result
        .rows
        .iter()
        .filter_map(|row| extract_pending(row))
        .collect();
    // Layer 2: don't arm notifications for tasks in excluded folders. Same
    // filter the Tasks pane + agent use (`query_all_tasks`), so an excluded
    // folder's timed reminders never fire. `set_excluded_task_folders`
    // calls `reschedule_all`, so a settings change rebuilds through here.
    retain_unexcluded(db, &mut pending);
    Ok(pending)
}

/// Drop pending tasks whose note lives in a user-excluded folder. Shared
/// by the full and per-note pending queries so notifications honor the
/// same exclusions as the pane and the agent.
fn retain_unexcluded(db: &DbInstance, pending: &mut Vec<PendingTask>) {
    let excludes = crate::commands::tasks::excluded_task_folders(db);
    if !excludes.is_empty() {
        pending.retain(|p| crate::commands::tasks::task_row_visible(&p.note_id, &excludes));
    }
}

fn query_pending_for_note(db: &DbInstance, note_id: &str) -> Result<Vec<PendingTask>, String> {
    use cozo::DataValue;
    let mut params = std::collections::BTreeMap::new();
    params.insert("id".to_string(), DataValue::Str(note_id.to_string().into()));
    let script = r#"
        ?[note_id, source_line, text, due_at, note_title, note_path] :=
            *task{ note_id, source_line, text, due_at, completed },
            *notes{ id: note_id, title: note_title, path: note_path },
            note_id == $id,
            !completed,
            due_at != ""
    "#;
    let result = db
        .run_script(script, params, ScriptMutability::Immutable)
        .map_err(|e| e.to_string())?;
    let mut pending: Vec<PendingTask> = result
        .rows
        .iter()
        .filter_map(|row| extract_pending(row))
        .collect();
    // Per-note reschedule (after a save) must also respect exclusions —
    // otherwise saving a note inside an excluded folder would re-arm its
    // timers even though the pane hides it.
    retain_unexcluded(db, &mut pending);
    Ok(pending)
}

fn extract_pending(row: &[cozo::DataValue]) -> Option<PendingTask> {
    if row.len() < 6 {
        return None;
    }
    let note_id = row[0].get_str()?.to_string();
    let source_line = match &row[1] {
        cozo::DataValue::Num(cozo::Num::Int(i)) => *i as i32,
        _ => return None,
    };
    let text = row[2].get_str()?.to_string();
    let due_at = row[3].get_str()?.to_string();
    let note_title = row[4].get_str()?.to_string();
    let note_path = row[5].get_str()?.to_string();
    Some(PendingTask {
        note_id,
        source_line,
        text,
        due_at,
        note_title,
        note_path,
    })
}

// =====================================================================
// Desktop path — tokio sleep + immediate show()
// =====================================================================

/// NEW-T1: the disposition of a to-be-enrolled task, decided purely from its
/// `due_at` + the current time. Extracted from `enroll_timer` (which needs an
/// AppHandle we can't build in a unit test) so the "far-future tasks still
/// enroll, past tasks go to the missed path" rule is directly testable.
#[cfg(not(any(target_os = "android", target_os = "ios")))]
enum EnrollPlan {
    /// Deadline is in the future — enroll this entry in the heap. There is NO
    /// upper cap: a far-future entry costs one heap slot (O(1)) and the ticker
    /// chunks its sleep (see `MAX_SLEEP_CHUNK`), so it stays armed even across
    /// weeks/months of uptime.
    Enroll(DesktopEntry),
    /// Deadline already passed — caller runs the missed-notification path.
    Missed {
        task: PendingTask,
        deadline: SystemTime,
    },
    /// `due_at` didn't parse — nothing to arm.
    Invalid,
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
fn plan_enroll(task: PendingTask, now: SystemTime) -> EnrollPlan {
    let Some(deadline) = parse_local_due_to_system_time(&task.due_at) else {
        return EnrollPlan::Invalid;
    };
    // NEW-T1: any future deadline enrolls, no matter how far out. The old code
    // capped at 7 days and returned `None` past it, relying on a roost-open
    // re-enroll that may never come if the app is left running — so an 8-day
    // reminder never fired. The ticker's chunked sleep makes far-future waits
    // safe, so the cap is gone.
    if deadline > now {
        EnrollPlan::Enroll(DesktopEntry { deadline, task })
    } else {
        EnrollPlan::Missed { task, deadline }
    }
}

/// AUDIT-054: enroll a pending notification into the heap-backed ticker.
/// **Does not spawn** a per-task tokio task anymore — returns a `DesktopEntry`
/// snapshot of the payload + deadline that the ticker reads when the heap entry
/// comes up. Caller stores the entry in `state.timers`; the ticker checks
/// `timers` on pop to distinguish live entries from tombstones.
///
/// Cancellation = remove the row from `state.timers`. The heap may still hold a
/// `HeapItem` pointing at the cancelled key; the ticker drops it as a tombstone
/// on pop. This avoids the O(n) heap-scan that a real removal would need.
///
/// Caller responsibilities (kept inside `reschedule_for_note` /
/// `reschedule_all`):
/// 1. Insert the returned `Some(entry)` into `state.timers`.
/// 2. Push `(Reverse(entry.deadline), key)` onto `state.heap`.
/// 3. Call `self.wake.notify_one()` so the ticker re-peeks (the new deadline
///    may be earlier than its current sleep target).
///
/// Step 3 is the only reason the caller can't see the wake handle from this
/// function — `enroll_timer` only takes `&mut SchedulerState` to keep the
/// signature in lockstep with the mobile branch.
#[cfg(not(any(target_os = "android", target_os = "ios")))]
fn enroll_timer(state: &mut SchedulerState, task: PendingTask) -> Option<EntryHandle> {
    let app = state.app_handle.clone()?;
    let now = SystemTime::now();
    match plan_enroll(task, now) {
        EnrollPlan::Enroll(entry) => Some(entry),
        EnrollPlan::Missed { task, deadline } => {
            // Deadline has already passed. If the ledger says we already fired
            // this exact (note, line, due) triple in some prior launch, do
            // nothing — repeat-fires are noise. Otherwise, catch the user up if
            // the miss is still recent.
            try_fire_missed(state, &app, task, deadline, now);
            None
        }
        EnrollPlan::Invalid => None,
    }
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
fn try_fire_missed(
    state: &mut SchedulerState,
    app: &AppHandle,
    task: PendingTask,
    deadline: SystemTime,
    now: SystemTime,
) {
    let key = dispatch_key(&task.note_id, task.source_line, &task.due_at);
    if !should_fire_missed(state, key, deadline, now) {
        return;
    }
    let _ = app
        .notification()
        .builder()
        .title(format!("[Missed] {}", task.note_title))
        .body(&task.text)
        .sound("default")
        .action_type_id(TASK_DUE_ACTION_TYPE)
        .extra("note_id", task.note_id.clone())
        .extra("note_path", task.note_path.clone())
        .extra("source_line", task.source_line.to_string())
        .extra("text_anchor", anchor_for(&task.text))
        .show();
    tracing::info!(
        note_id = %task.note_id,
        source_line = task.source_line,
        due_at = %task.due_at,
        "task_notify: fired missed-notification catch-up"
    );
}

/// AUDIT-054: cancel an enrolled timer.
///
/// Removing the entry from `state.timers` is the cancel — no spawn to
/// abort. The corresponding `HeapItem` is left in `state.heap` as a
/// tombstone; the ticker drops it on pop after seeing that the
/// `TaskKey` is no longer in `timers`. This O(1) cancel + lazy heap
/// cleanup is the whole reason cancellation is cheap; the alternative
/// (real heap removal) would be O(n) per cancel for the same
/// observable effect.
///
/// The signature stays in lockstep with the mobile branch so call
/// sites (`reschedule_for_note`, `reschedule_all`) can be platform-
/// agnostic.
#[cfg(not(any(target_os = "android", target_os = "ios")))]
fn abort_one(_app: Option<&AppHandle>, _handle: EntryHandle) {
    // No-op: the caller already removed the entry from `state.timers`
    // before calling us (see `cancel_for_note` / the drain in
    // `reschedule_all`). The heap tombstone gets cleaned at pop time.
}

/// AUDIT-054: the single ticker task that owns ALL desktop notification
/// sleeping. Spawned once per process from `install()`.
///
/// Loop:
/// 1. Peek heap. If empty, wait for `wake` (a new enroll will fire it).
/// 2. Pop the next-soonest `(deadline, key)`. If `key` isn't in
///    `state.timers`, it's a tombstone — discard and loop.
/// 3. If `deadline > now`, sleep until `deadline` OR until `wake`,
///    whichever first. On wake, push the entry back onto the heap and
///    loop (a new earlier deadline may have been pushed).
/// 4. If `deadline <= now`, fire the notification, remove from
///    `timers`, loop.
///
/// All state mutation happens behind the `state` mutex but the actual
/// async `.show()` call runs without the lock held — the OS notification
/// plugin's IO is fast in practice but blocking the scheduler on it
/// would delay other due notifications.
#[cfg(not(any(target_os = "android", target_os = "ios")))]
async fn run_ticker(state: Arc<Mutex<SchedulerState>>, wake: Arc<Notify>) {
    loop {
        // Phase 1: pop the next live entry off the heap. Skip
        // tombstones (entries cancelled since they were pushed).
        // Delegates to `pop_next_live` so the same pop-loop
        // semantics get exercised in unit tests. Split-borrow on
        // `heap` and `timers` lets us avoid cloning the timers map.
        let next: Option<(SystemTime, TaskKey, DesktopEntry)> = {
            let mut guard = match state.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            let SchedulerState { heap, timers, .. } = &mut *guard;
            pop_next_live(heap, timers)
        };

        let Some((deadline, key, entry)) = next else {
            // No pending timers. Wait for an enroll to wake us.
            wake.notified().await;
            continue;
        };

        // Phase 2: sleep until deadline, but bail early if `wake`
        // fires (means a new earlier deadline was pushed and we
        // should re-peek). On wake-early, push the entry back onto
        // the heap so we don't lose it.
        let now = SystemTime::now();
        let wait = deadline
            .duration_since(now)
            .unwrap_or(Duration::from_millis(0));
        if !wait.is_zero() {
            // NEW-T1: never sleep the whole of a far-future wait in one go —
            // clamp to `MAX_SLEEP_CHUNK`. If the wait was longer than a chunk
            // (`partial`), we MUST re-peek instead of firing when the chunk
            // ends: the real deadline is still ahead. This is what lets a task
            // due weeks/months out stay armed without a pathologically long
            // `tokio::time::sleep`.
            let (sleep_for, partial) = ticker_sleep_chunk(wait);
            let woke_early = tokio::select! {
                _ = tokio::time::sleep(sleep_for) => false,
                _ = wake.notified() => true,
            };
            if woke_early || partial {
                // Re-push and re-peek. Either a new (possibly earlier) deadline
                // was pushed, or we only slept a chunk of a longer wait.
                if let Ok(mut guard) = state.lock() {
                    // Only re-push if the entry is still live (not
                    // cancelled while we were sleeping).
                    if guard.timers.contains_key(&key) {
                        guard.heap.push((Reverse(deadline), key));
                    }
                }
                continue;
            }
        }

        // Phase 3: fire. Pull the AppHandle + remove the live entry
        // under the lock; release before the async `.show()` call so
        // we don't serialize the notification plugin on the scheduler
        // lock.
        let app = {
            let mut guard = match state.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            // Re-check liveness (could have been cancelled while we
            // slept-to-deadline). If gone, skip — caller's intent.
            if guard.timers.remove(&key).is_none() {
                continue;
            }
            guard.app_handle.clone()
        };
        let Some(app) = app else { continue };

        let task = entry.task;
        let title = format!("Task due: {}", task.note_title);
        // macOS: post via the modern UserNotifications API (prompts for
        // permission + delivers reliably on macOS 26, where the plugin's legacy
        // path is silent). Returns false in dev (no app bundle) → fall back.
        #[cfg(target_os = "macos")]
        let posted_native = crate::notification_macos::post(
            &dispatch_key(&task.note_id, task.source_line, &task.due_at),
            &title,
            &task.text,
            true,
        );
        #[cfg(not(target_os = "macos"))]
        let posted_native = false;
        if !posted_native {
            let _ = app
                .notification()
                .builder()
                .title(title)
                .body(&task.text)
                // Default tone. Desktop maps this through notify-rust's
                // sound_name (freedesktop "default" / Windows default); without
                // it the desktop notification is silent.
                .sound("default")
                .action_type_id(TASK_DUE_ACTION_TYPE)
                .extra("note_id", task.note_id.clone())
                .extra("note_path", task.note_path.clone())
                .extra("source_line", task.source_line.to_string())
                .extra("text_anchor", anchor_for(&task.text))
                .show();
        }
    }
}

// =====================================================================
// Mobile path — enroll with the OS via Schedule::At, cancel by id
// =====================================================================

/// Enroll the task with the OS scheduler. `Schedule::At` routes through
/// `tauri-plugin-notification`'s Kotlin/Swift bridge, which on Android
/// uses `AlarmManager.setExactAndAllowWhileIdle` + a `BroadcastReceiver`
/// to fire the notification regardless of app process state. The
/// returned id is what we use to cancel.
/// Relabel a true UTC instant so its wall-clock digits equal the LOCAL
/// wall-clock, by shifting by the local UTC offset and re-expressing as UTC.
/// The iOS notification plugin reads a schedule date's digits as local time
/// (see `enroll_timer`), so handing it `local_wall_clock_as_utc(instant,
/// offset)` makes it fire at the intended local time. Pure + offset-injected
/// so it's testable without a device clock.
#[cfg(any(test, target_os = "ios"))]
fn local_wall_clock_as_utc(secs: i64, local_offset_secs: i32) -> Option<time::OffsetDateTime> {
    time::OffsetDateTime::from_unix_timestamp(secs + local_offset_secs as i64).ok()
}

#[cfg(any(target_os = "android", target_os = "ios"))]
fn enroll_timer(state: &mut SchedulerState, task: PendingTask) -> Option<EntryHandle> {
    let app = state.app_handle.clone()?;
    let deadline = parse_local_due_to_system_time(&task.due_at)?;
    let now = SystemTime::now();
    let secs = deadline.duration_since(UNIX_EPOCH).ok()?.as_secs() as i64;
    let now_secs = now.duration_since(UNIX_EPOCH).ok()?.as_secs() as i64;
    // Already past — the OS would reject a backdated alarm anyway.
    if secs <= now_secs {
        // NEW-T2: the OS may have PURGED this alarm (device reboot, Doze, or a
        // force-stop) before it fired, and if the app was closed nothing caught
        // it — the user gets zero notification. Desktop catches missed tasks up
        // on launch via the ledger + grace window; mirror that on mobile by
        // firing an immediate `[Missed]` notification when the miss is still
        // recent. Shares the desktop grace/dedup/cap gate (`should_fire_missed`)
        // so it can't storm and won't repeat on the next launch.
        try_fire_missed_mobile(state, &app, &task, deadline, now);
        return None;
    }
    // iOS: `tauri-plugin-notification`'s Swift layer parses the schedule
    // date with a format whose trailing `Z` is a *literal* (not a UTC
    // marker) on a DateFormatter left in the device's local timezone — so it
    // reads the serialized clock digits as LOCAL time. Our instant serializes
    // as UTC digits, so iOS misreads the fire time, shifted by the local UTC
    // offset; east of UTC (e.g. +05:30) the shift lands it in the past, the
    // plugin throws `pastScheduledTime`, and the alarm is silently dropped —
    // no notification ever fires. Relabel so the UTC digits equal the local
    // wall-clock (shift by the local offset at the due instant, serialize as
    // UTC). Android keeps the true instant — its AlarmManager path uses
    // absolute epoch millis and is timezone-correct as-is.
    #[cfg(target_os = "ios")]
    let date = {
        use chrono::{Local, TimeZone};
        let local_offset_secs = Local
            .timestamp_opt(secs, 0)
            .single()
            .map(|dt| dt.offset().local_minus_utc())
            .unwrap_or(0);
        local_wall_clock_as_utc(secs, local_offset_secs)?
    };
    #[cfg(target_os = "android")]
    let date = time::OffsetDateTime::from_unix_timestamp(secs).ok()?;
    let id = task_notification_id(&task.note_id, task.source_line);
    let builder = app
        .notification()
        .builder()
        .id(id)
        .title(format!("Task due: {}", task.note_title))
        .body(&task.text);
    // Sound is platform-specific. On iOS, no sound = a silent banner, and the
    // plugin only takes a custom file name — so we send the "default" sentinel
    // that our vendored patch maps to UNNotificationSound.default. On Android,
    // the sound string is resolved as a raw resource (res/raw/<name>); there's
    // no "default" resource, and setting a bad URI silences the channel, so we
    // leave it unset and let the notification channel's own default tone play.
    #[cfg(target_os = "ios")]
    let builder = builder.sound("default");
    let result = builder
        .action_type_id(TASK_DUE_ACTION_TYPE)
        .extra("note_id", task.note_id.clone())
        .extra("note_path", task.note_path.clone())
        .extra("source_line", task.source_line.to_string())
        .extra("text_anchor", anchor_for(&task.text))
        .schedule(Schedule::At {
            date,
            repeating: false,
            allow_while_idle: true,
        })
        .show();
    if let Err(e) = result {
        tracing::warn!(
            error = %e,
            note_id = %task.note_id,
            source_line = task.source_line,
            "task_notify: schedule failed"
        );
        return None;
    }
    tracing::info!(
        id,
        note_id = %task.note_id,
        source_line = task.source_line,
        due_at = %task.due_at,
        "task_notify: enrolled OS alarm"
    );
    Some(id)
}

#[cfg(any(target_os = "android", target_os = "ios"))]
fn abort_one(app: Option<&AppHandle>, handle: EntryHandle) {
    let Some(app) = app else { return };
    let _ = app.notification().cancel(vec![handle]);
}

/// NEW-T2: mobile launch catch-up for a task whose OS alarm was purged before
/// it could fire (reboot / Doze / force-stop). Fires an IMMEDIATE `[Missed]`
/// notification (no `Schedule::At` — the due time is already in the past)
/// gated by the shared `should_fire_missed` (grace window + per-launch cap +
/// ledger dedup), so it neither storms nor repeats across launches. Called
/// from the past-due branch of the mobile `enroll_timer`, which runs on every
/// `reschedule_all` (roost open) — i.e. once per launch.
#[cfg(any(target_os = "android", target_os = "ios"))]
fn try_fire_missed_mobile(
    state: &mut SchedulerState,
    app: &AppHandle,
    task: &PendingTask,
    deadline: SystemTime,
    now: SystemTime,
) {
    let key = dispatch_key(&task.note_id, task.source_line, &task.due_at);
    if !should_fire_missed(state, key, deadline, now) {
        return;
    }
    let builder = app
        .notification()
        .builder()
        .title(format!("[Missed] {}", task.note_title))
        .body(&task.text);
    // Same platform sound rationale as the live-fire path in `enroll_timer`:
    // iOS needs the "default" sentinel; Android leaves the channel's own tone.
    #[cfg(target_os = "ios")]
    let builder = builder.sound("default");
    let result = builder
        .action_type_id(TASK_DUE_ACTION_TYPE)
        .extra("note_id", task.note_id.clone())
        .extra("note_path", task.note_path.clone())
        .extra("source_line", task.source_line.to_string())
        .extra("text_anchor", anchor_for(&task.text))
        .show();
    if let Err(e) = result {
        tracing::warn!(
            error = %e,
            note_id = %task.note_id,
            source_line = task.source_line,
            "task_notify: mobile missed catch-up failed"
        );
    } else {
        tracing::info!(
            note_id = %task.note_id,
            source_line = task.source_line,
            due_at = %task.due_at,
            "task_notify: fired mobile missed-notification catch-up"
        );
    }
}

/// Deterministic, non-negative i32 id derived from `(note_id, source_line)`.
/// `Notification::id` is i32 — Android wants a stable per-task id so
/// cancellation works after process restarts even if our in-memory
/// `timers` map is empty. Stripping the sign bit keeps the id positive
/// (the plugin's mobile bridge serializes via JSON; negative ints are
/// fine in theory but some OEM Android skins have shown up with weird
/// behavior on negative notification ids).
#[cfg(any(target_os = "android", target_os = "ios"))]
fn task_notification_id(note_id: &str, source_line: i32) -> i32 {
    let s = format!("{note_id}:{source_line}");
    let hash = blake3::hash(s.as_bytes());
    let bytes = hash.as_bytes();
    i32::from_be_bytes([bytes[0] & 0x7f, bytes[1], bytes[2], bytes[3]])
}

// =====================================================================
// Local-time parsing — shared by both backends
// =====================================================================

/// TRN-1: default time-of-day for a date-only task (a `📅` date with no `⏰`
/// time). Without this the scheduler returned `None` for anything shorter than
/// "YYYY-MM-DD HH:MM", so a `📅 2026-07-04` task showed in the pane but
/// silently never fired — the "never forget" promise broken for the most
/// common reminder shape. A dated to-do IS an intentional reminder, so we arm
/// it at a sensible morning default. Only date-BEARING checkboxes are tasks
/// (convention 0.3), so this doesn't arm stray checkboxes, and the existing
/// missed-fire caps (`MAX_MISSED_PER_LAUNCH` + the grace window) keep a backlog
/// of past date-only tasks from storming on the first launch after this shipped.
/// If a per-roost "default reminder time" setting is later wired up, read it in
/// place of this constant.
const DATE_ONLY_DEFAULT_HHMM: (u32, u32) = (9, 0);

/// Parse `YYYY-MM-DD[ HH:MM]` interpreted as local wall-clock and convert to a
/// `SystemTime`. A date-only string is armed at [`DATE_ONLY_DEFAULT_HHMM`]
/// (TRN-1); DST seams are handled rather than dropped (TRN-2). Returns `None`
/// only for a missing/unparseable date.
fn parse_local_due_to_system_time(due_at: &str) -> Option<SystemTime> {
    let s = due_at.trim();
    // A date ("YYYY-MM-DD", 10 chars) is mandatory; a time is optional (TRN-1).
    if s.len() < 10 {
        return None;
    }
    let date = &s[..10];
    let mut date_parts = date.split('-');
    let y: i32 = date_parts.next()?.parse().ok()?;
    let m: u32 = date_parts.next()?.parse().ok()?;
    let d: u32 = date_parts.next()?.parse().ok()?;

    // Use the note's time-of-day when present ("YYYY-MM-DD HH:MM"), else the
    // date-only default (TRN-1) so a dated task still fires.
    let (hh, mm) = if s.len() >= 16 {
        let mut time_parts = s[11..16].split(':');
        let hh: u32 = time_parts.next()?.parse().ok()?;
        let mm: u32 = time_parts.next()?.parse().ok()?;
        (hh, mm)
    } else {
        DATE_ONLY_DEFAULT_HHMM
    };

    use chrono::{
        Duration as ChronoDuration, Local, LocalResult, NaiveDate, NaiveDateTime, TimeZone,
    };
    let date = NaiveDate::from_ymd_opt(y, m, d)?;
    let naive = NaiveDateTime::new(date, chrono::NaiveTime::from_hms_opt(hh, mm, 0)?);
    // TRN-2: resolve the wall-clock time to a UTC instant, handling DST seams
    // instead of silently dropping the task (the old `.single()?` returned
    // `None` for a spring-forward-gap time → the reminder never armed):
    //  - Single    → that instant.
    //  - Ambiguous → fall-back fold (an hour repeats); pick the earlier one.
    //  - None      → spring-forward gap (this wall-clock time doesn't exist);
    //                nudge +1h out of the gap so it still fires (~1h late)
    //                rather than never.
    let local = match Local.from_local_datetime(&naive) {
        LocalResult::Single(dt) => dt,
        LocalResult::Ambiguous(earliest, _latest) => earliest,
        LocalResult::None => Local
            .from_local_datetime(&(naive + ChronoDuration::hours(1)))
            .earliest()?,
    };
    let epoch_secs = local.timestamp();
    if epoch_secs < 0 {
        return None;
    }
    Some(UNIX_EPOCH + Duration::from_secs(epoch_secs as u64))
}

/// AUDIT-054: pop the next live entry from a heap, skipping tombstones
/// (heap entries whose `TaskKey` is no longer in `timers`). Pure
/// function so the ticker's pop-loop semantics are unit-testable
/// without spinning up the async ticker + an AppHandle.
///
/// Returns `Some((deadline, key, entry))` for a live entry, `None` if
/// the heap is empty (or held only tombstones).
/// NEW-T1: compute how long the ticker sleeps for a given remaining wait, and
/// whether that only covers a *chunk* of a longer wait. Pure so the "never
/// sleep the whole of a far-future wait, never fire early" rule is unit-
/// testable. Returns `(sleep_for, is_partial)`: when `is_partial` is true the
/// caller must re-peek (the deadline is still ahead) rather than fire.
#[cfg(not(any(target_os = "android", target_os = "ios")))]
fn ticker_sleep_chunk(wait: Duration) -> (Duration, bool) {
    if wait > MAX_SLEEP_CHUNK {
        (MAX_SLEEP_CHUNK, true)
    } else {
        (wait, false)
    }
}

/// TRN-3: compute which currently-armed OS alarm ids are orphans — armed on
/// the device but no longer backed by a live task (`valid`). Pure set-
/// difference so the launch-time reconcile is unit-testable without a device.
/// Gated to `test` too so it compiles + is exercised on the desktop CI target
/// even though its production caller is mobile-only.
#[cfg(any(test, target_os = "android", target_os = "ios"))]
fn orphan_alarm_ids(armed: &[i32], valid: &HashSet<i32>) -> Vec<i32> {
    armed
        .iter()
        .copied()
        .filter(|id| !valid.contains(id))
        .collect()
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
fn pop_next_live(
    heap: &mut BinaryHeap<HeapItem>,
    timers: &HashMap<TaskKey, EntryHandle>,
) -> Option<(SystemTime, TaskKey, EntryHandle)> {
    loop {
        let (Reverse(deadline), key) = heap.pop()?;
        let Some(entry) = timers.get(&key).cloned() else {
            continue; // tombstone
        };
        let live_deadline = entry.deadline.max(deadline);
        return Some((live_deadline, key, entry));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_local_datetime() {
        let t = parse_local_due_to_system_time("2030-05-08 10:00").unwrap();
        let now = SystemTime::now();
        assert!(t > now);
    }

    #[test]
    fn date_only_task_arms_at_default_time() {
        // TRN-1: a date-only task used to return None (never fired). It now
        // arms at the date-only default time — i.e. it resolves to the same
        // instant as spelling that default time out explicitly (tz-independent).
        let date_only = parse_local_due_to_system_time("2030-05-08");
        assert!(date_only.is_some(), "date-only task must now arm");
        let explicit = parse_local_due_to_system_time("2030-05-08 09:00");
        assert_eq!(date_only, explicit, "date-only arms at the 09:00 default");
    }

    #[test]
    fn requires_a_parseable_date() {
        // A missing/too-short date still yields None — nothing to arm.
        assert!(parse_local_due_to_system_time("").is_none());
        assert!(parse_local_due_to_system_time("2030").is_none());
    }

    // Regression: iOS scheduled notifications never fired east of UTC because
    // the plugin reads the schedule date's digits as local time, but we sent
    // UTC — shifting the alarm into the past (→ pastScheduledTime → dropped).
    // `local_wall_clock_as_utc` must re-express the instant so its UTC digits
    // equal the local wall-clock.
    #[test]
    fn ios_relabel_makes_utc_digits_match_local_wall_clock() {
        use time::{Date, Month, Time, UtcOffset};
        // 14:20:00 UTC; +05:30 (IST) local offset.
        let secs = Date::from_calendar_date(2026, Month::June, 14)
            .unwrap()
            .with_time(Time::from_hms(14, 20, 0).unwrap())
            .assume_offset(UtcOffset::UTC)
            .unix_timestamp();
        let offset = 5 * 3600 + 30 * 60; // +05:30 in seconds

        let relabeled = local_wall_clock_as_utc(secs, offset).unwrap();
        // The relabeled value's clock digits are the local wall-clock (19:50),
        // which is exactly what iOS will (mis)read as local — i.e. correctly.
        assert_eq!((relabeled.hour(), relabeled.minute()), (19, 50));
        // And it stays after the true instant, so the plugin's
        // pastScheduledTime guard no longer rejects it.
        assert!(relabeled.unix_timestamp() > secs);
    }

    #[test]
    fn ios_relabel_is_identity_at_utc() {
        use time::{Date, Month, Time, UtcOffset};
        let secs = Date::from_calendar_date(2026, Month::June, 14)
            .unwrap()
            .with_time(Time::from_hms(14, 20, 0).unwrap())
            .assume_offset(UtcOffset::UTC)
            .unix_timestamp();
        let relabeled = local_wall_clock_as_utc(secs, 0).unwrap();
        assert_eq!((relabeled.hour(), relabeled.minute()), (14, 20));
    }

    #[test]
    fn returns_none_for_garbage() {
        assert!(parse_local_due_to_system_time("not a date").is_none());
        assert!(parse_local_due_to_system_time("2030-99-99 99:99").is_none());
    }

    #[test]
    fn anchor_for_passes_short_strings_through() {
        assert_eq!(anchor_for("buy milk"), "buy milk");
        assert_eq!(anchor_for("   buy milk"), "buy milk");
    }

    #[test]
    fn anchor_for_caps_long_strings_at_char_boundary() {
        let long = "a".repeat(100);
        let out = anchor_for(&long);
        assert!(out.len() <= 64);
        assert!(out.chars().all(|c| c == 'a'));
    }

    #[test]
    fn anchor_for_does_not_split_multibyte_chars() {
        // 21 ✅ emojis = 63 bytes (each ✅ is 3 bytes). 22 emojis = 66 bytes
        // which would land just past the 64-byte cap if naive byte slicing
        // were used — but the boundary backoff loop must align to a char.
        let s = "✅".repeat(22);
        let out = anchor_for(&s);
        // Whatever the truncation point, the output must be valid UTF-8 —
        // which is implicit since `&str`, but we also assert it parses
        // round-trip and contains a whole number of ✅ chars.
        let n = out.chars().count();
        assert_eq!(out, "✅".repeat(n));
        assert!(out.len() <= 64);
    }

    #[test]
    fn pending_queries_skip_excluded_folders() {
        // Layer 2: a timed task in an excluded folder must NOT be enrolled
        // for a notification. Drives the same `excluded_task_folders` /
        // `task_row_visible` path the scheduler uses at reschedule time.
        use cozo::{DataValue, DbInstance, ScriptMutability};
        use std::collections::BTreeMap;

        let db = DbInstance::new("mem", "", Default::default()).unwrap();
        for script in [
            r#":create kv_meta { key: String => value: String }"#,
            r#":create notes { id: String => title: String, path: String, updated_at: Float }"#,
            r#":create task {
                note_id: String, source_line: Int =>
                text: String, content_anchor: String, due_at: String, completed: Bool,
                completed_at: String, linked_person: String, linked_project: String, updated_at: Float
            }"#,
        ] {
            db.run_script(script, Default::default(), ScriptMutability::Mutable)
                .unwrap();
        }

        // Two notes, each with one open, timed task.
        for (id, path, title) in [
            ("templates/skeleton", "Templates/skeleton.md", "Skeleton"),
            (
                "daily notes/2026-06-10",
                "Daily Notes/2026-06-10.md",
                "2026-06-10",
            ),
        ] {
            let mut p = BTreeMap::new();
            p.insert("id".to_string(), DataValue::Str(id.into()));
            p.insert("t".to_string(), DataValue::Str(title.into()));
            p.insert("path".to_string(), DataValue::Str(path.into()));
            db.run_script(
                r#"?[id, title, path, updated_at] <- [[$id, $t, $path, 0.0]]
                   :put notes { id => title, path, updated_at }"#,
                p,
                ScriptMutability::Mutable,
            )
            .unwrap();

            let mut tp = BTreeMap::new();
            tp.insert("id".to_string(), DataValue::Str(id.into()));
            db.run_script(
                r#"?[note_id, source_line, text, content_anchor, due_at, completed, completed_at, linked_person, linked_project, updated_at]
                   <- [[$id, 1, "do it", "do it", "2030-01-01 09:00", false, "", "", "", 0.0]]
                   :put task { note_id, source_line => text, content_anchor, due_at, completed, completed_at, linked_person, linked_project, updated_at }"#,
                tp,
                ScriptMutability::Mutable,
            )
            .unwrap();
        }

        // No exclusions → both tasks are pending.
        assert_eq!(query_pending_all(&db).unwrap().len(), 2);

        // Exclude Templates/ → only the daily-note task remains pending.
        let json = serde_json::to_string(&["Templates"]).unwrap();
        crate::db::schema::set_kv(&db, crate::commands::tasks::EXCLUDED_FOLDERS_KEY, &json)
            .unwrap();
        let all = query_pending_all(&db).unwrap();
        assert_eq!(all.len(), 1, "excluded folder's task must not be pending");
        assert_eq!(all[0].note_id, "daily notes/2026-06-10");

        // Per-note path honors it too: the excluded note returns nothing.
        assert!(query_pending_for_note(&db, "templates/skeleton")
            .unwrap()
            .is_empty());
        assert_eq!(
            query_pending_for_note(&db, "daily notes/2026-06-10")
                .unwrap()
                .len(),
            1
        );
    }

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    #[test]
    fn ledger_persists_and_dedupes() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("missed.json");

        let mut l = DispatchLedger::load(path.clone());
        assert!(!l.contains("a:1:2026-05-08 09:00"));
        l.record("a:1:2026-05-08 09:00".to_string());
        assert!(l.contains("a:1:2026-05-08 09:00"));

        // Reload from disk: prior entries survive.
        let l2 = DispatchLedger::load(path.clone());
        assert!(l2.contains("a:1:2026-05-08 09:00"));
        assert!(!l2.contains("never-fired:0:x"));
    }

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    #[test]
    fn ledger_is_safe_when_path_absent() {
        // No path → no persistence, but in-memory contains/record still
        // work so test paths and headless contexts don't crash.
        let mut l = DispatchLedger::default();
        assert!(!l.contains("k"));
        l.record("k".to_string());
        assert!(l.contains("k"));
    }

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    #[test]
    fn ledger_prunes_entries_older_than_thirty_days() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("missed.json");
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        // Hand-craft a JSON file with one fresh + one ancient entry.
        let stale = now.saturating_sub(60 * 24 * 60 * 60);
        let body = format!(r#"{{"fresh:0:t":{now},"stale:0:t":{stale}}}"#);
        std::fs::write(&path, body).unwrap();

        let l = DispatchLedger::load(path);
        assert!(l.contains("fresh:0:t"));
        assert!(
            !l.contains("stale:0:t"),
            "ancient entry should be pruned during load"
        );
    }

    #[cfg(any(target_os = "android", target_os = "ios"))]
    #[test]
    fn notification_id_is_deterministic_and_positive() {
        let a = task_notification_id("Daily Notes/2026-05-02", 7);
        let b = task_notification_id("Daily Notes/2026-05-02", 7);
        assert_eq!(a, b);
        assert!(a >= 0);

        // Different keys → different ids (almost always — collision is
        // 2^-31 probability).
        let c = task_notification_id("Daily Notes/2026-05-02", 8);
        assert_ne!(a, c);
    }

    // ── AUDIT-054 tests for the single-ticker heap mechanics ─────
    //
    // The full ticker requires a real AppHandle (notifications need
    // the plugin's IO bridge). What's worth pinning at the unit-test
    // tier is the `pop_next_live` pop-loop: heap ordering + the
    // tombstone-skip semantics that make cancellation O(1). End-to-
    // end "the OS notification actually fires" lives in manual
    // verification (see commit message).

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    fn fake_entry(deadline_offset_secs: u64, note_id: &str, line: i32) -> DesktopEntry {
        DesktopEntry {
            deadline: SystemTime::now() + Duration::from_secs(deadline_offset_secs),
            task: PendingTask {
                note_id: note_id.to_string(),
                source_line: line,
                text: "test task".to_string(),
                due_at: "2030-05-08 10:00".to_string(),
                note_title: "test note".to_string(),
                note_path: "/v/test.md".to_string(),
            },
        }
    }

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    #[test]
    fn pop_next_live_returns_earliest_deadline_first() {
        // Heap should pop entries in deadline order regardless of
        // insert order — the BinaryHeap + Reverse pairing is what
        // makes the ticker's "sleep until next soonest" cheap.
        let mut heap: BinaryHeap<HeapItem> = BinaryHeap::new();
        let mut timers: HashMap<TaskKey, EntryHandle> = HashMap::new();

        let later = fake_entry(120, "note-late", 1);
        let sooner = fake_entry(10, "note-soon", 1);
        let middle = fake_entry(60, "note-mid", 1);

        heap.push((Reverse(later.deadline), ("note-late".into(), 1)));
        heap.push((Reverse(sooner.deadline), ("note-soon".into(), 1)));
        heap.push((Reverse(middle.deadline), ("note-mid".into(), 1)));
        timers.insert(("note-late".into(), 1), later);
        timers.insert(("note-soon".into(), 1), sooner);
        timers.insert(("note-mid".into(), 1), middle);

        let (_, key1, _) = pop_next_live(&mut heap, &timers).expect("first");
        let (_, key2, _) = pop_next_live(&mut heap, &timers).expect("second");
        let (_, key3, _) = pop_next_live(&mut heap, &timers).expect("third");
        assert_eq!(key1.0, "note-soon");
        assert_eq!(key2.0, "note-mid");
        assert_eq!(key3.0, "note-late");
        assert!(pop_next_live(&mut heap, &timers).is_none(), "exhausted");
    }

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    #[test]
    fn pop_next_live_skips_tombstones() {
        // The O(1)-cancel invariant: a cancelled entry stays in the
        // heap (no expensive removal) but `timers` no longer has it.
        // The pop loop must walk past tombstones until it finds a
        // live entry — otherwise the ticker would "fire" stale work.
        let mut heap: BinaryHeap<HeapItem> = BinaryHeap::new();
        let mut timers: HashMap<TaskKey, EntryHandle> = HashMap::new();

        let cancelled = fake_entry(5, "cancelled", 1);
        let live = fake_entry(30, "live", 1);

        heap.push((Reverse(cancelled.deadline), ("cancelled".into(), 1)));
        heap.push((Reverse(live.deadline), ("live".into(), 1)));
        // Only the "live" entry is in `timers` — the cancelled one
        // was removed by `cancel_for_note`.
        timers.insert(("live".into(), 1), live);

        let (_, key, _) = pop_next_live(&mut heap, &timers).expect("found live");
        assert_eq!(
            key.0, "live",
            "tombstone must be skipped, live entry returned"
        );
        assert!(pop_next_live(&mut heap, &timers).is_none(), "exhausted");
    }

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    #[test]
    fn pop_next_live_uses_latest_deadline_when_rescheduled() {
        // Realistic reschedule: the same task got re-enrolled with a
        // LATER deadline (user pushed the due time out). The heap
        // still holds the old (earlier) `HeapItem`; `timers` has the
        // new entry. The pop loop should report the timers entry's
        // current deadline, not the stale heap one — otherwise the
        // ticker would fire early.
        let mut heap: BinaryHeap<HeapItem> = BinaryHeap::new();
        let mut timers: HashMap<TaskKey, EntryHandle> = HashMap::new();

        let original_deadline = SystemTime::now() + Duration::from_secs(5);
        let new_entry = fake_entry(60, "x", 1); // 60s out, vs heap's 5s
        heap.push((Reverse(original_deadline), ("x".into(), 1)));
        timers.insert(("x".into(), 1), new_entry.clone());

        let (deadline, key, _) = pop_next_live(&mut heap, &timers).expect("live");
        assert_eq!(key.0, "x");
        // Must use the new (later) deadline so the ticker doesn't
        // fire ~55s early.
        assert!(
            deadline >= new_entry.deadline,
            "deadline must reflect the rescheduled (later) time, not the stale heap entry"
        );
    }

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    #[test]
    fn pop_next_live_empty_heap_returns_none() {
        let mut heap: BinaryHeap<HeapItem> = BinaryHeap::new();
        let timers: HashMap<TaskKey, EntryHandle> = HashMap::new();
        assert!(pop_next_live(&mut heap, &timers).is_none());
    }

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    #[test]
    fn pop_next_live_only_tombstones_returns_none() {
        // All heap entries are cancelled. The loop drains the heap
        // and returns None — the ticker then waits on `Notify` for
        // the next enroll.
        let mut heap: BinaryHeap<HeapItem> = BinaryHeap::new();
        let timers: HashMap<TaskKey, EntryHandle> = HashMap::new();
        for i in 0..3 {
            heap.push((
                Reverse(SystemTime::now() + Duration::from_secs(i)),
                (format!("cancelled-{i}"), 1),
            ));
        }
        assert!(pop_next_live(&mut heap, &timers).is_none());
        assert!(heap.is_empty(), "all tombstones consumed");
    }

    // ── NEW-T1: far-future desktop tasks stay enrolled (no 7-day drop) ──

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    fn task_due(due_at: &str) -> PendingTask {
        PendingTask {
            note_id: "n".to_string(),
            source_line: 1,
            text: "do it".to_string(),
            due_at: due_at.to_string(),
            note_title: "note".to_string(),
            note_path: "/v/n.md".to_string(),
        }
    }

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    #[test]
    fn plan_enroll_arms_far_future_tasks() {
        // NEW-T1 regression: a task decades out MUST still enroll. The old code
        // capped enrollment at 7 days and returned None past it, so a reminder
        // 8+ days away silently never fired if the app stayed open. Would fail
        // against the buggy code, which produced no enroll entry for this task.
        let now = SystemTime::now();
        let plan = plan_enroll(task_due("2099-01-01 09:00"), now);
        assert!(
            matches!(plan, EnrollPlan::Enroll(ref e) if e.deadline > now),
            "a far-future task must still enroll (NEW-T1: no 7-day cap)"
        );
    }

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    #[test]
    fn plan_enroll_flags_past_as_missed() {
        let now = SystemTime::now();
        let plan = plan_enroll(task_due("2000-01-01 09:00"), now);
        assert!(matches!(plan, EnrollPlan::Missed { .. }));
    }

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    #[test]
    fn plan_enroll_invalid_due_yields_invalid() {
        let now = SystemTime::now();
        assert!(matches!(
            plan_enroll(task_due("garbage"), now),
            EnrollPlan::Invalid
        ));
    }

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    #[test]
    fn ticker_sleep_chunk_caps_far_future_and_signals_partial() {
        // A 30-day wait sleeps only one chunk and reports partial=true, so the
        // ticker re-peeks instead of firing early — the mechanism that keeps a
        // far-future task armed without a pathologically long tokio sleep.
        let (dur, partial) = ticker_sleep_chunk(Duration::from_secs(30 * 24 * 60 * 60));
        assert_eq!(dur, MAX_SLEEP_CHUNK);
        assert!(partial, "a wait longer than a chunk must signal re-peek");
    }

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    #[test]
    fn ticker_sleep_chunk_passes_short_wait_through() {
        let (dur, partial) = ticker_sleep_chunk(Duration::from_secs(3600));
        assert_eq!(dur, Duration::from_secs(3600));
        assert!(
            !partial,
            "a within-chunk wait fires on completion, no re-peek"
        );
    }

    // ── NEW-T2 / shared missed-notification gate (guards mobile catch-up
    //    AND desktop launch catch-up) ─────────────────────────────────────

    #[test]
    fn should_fire_missed_fires_recent_then_dedupes() {
        // A recently-missed task fires once; the same key is deduped on a
        // second sighting (i.e. the next launch), so it never re-storms.
        let mut state = SchedulerState::default();
        let now = SystemTime::now();
        let recent = now - Duration::from_secs(3600); // 1h ago, within grace
        let key = "n:1:2030-01-01 09:00".to_string();
        assert!(should_fire_missed(&mut state, key.clone(), recent, now));
        assert!(
            !should_fire_missed(&mut state, key, recent, now),
            "same key must dedupe on a later launch"
        );
    }

    #[test]
    fn should_fire_missed_skips_stale_beyond_grace() {
        // NEW-T2: a task missed by more than the grace window is NOT surfaced —
        // it's silently recorded so it also won't re-qualify next launch.
        let mut state = SchedulerState::default();
        let now = SystemTime::now();
        let stale = now - (MISSED_GRACE_WINDOW + Duration::from_secs(60));
        assert!(!should_fire_missed(
            &mut state,
            "old".to_string(),
            stale,
            now
        ));
    }

    #[test]
    fn should_fire_missed_caps_per_launch() {
        let mut state = SchedulerState::default();
        let now = SystemTime::now();
        let recent = now - Duration::from_secs(60);
        for i in 0..MAX_MISSED_PER_LAUNCH {
            assert!(
                should_fire_missed(&mut state, format!("k{i}"), recent, now),
                "the first MAX_MISSED_PER_LAUNCH misses fire"
            );
        }
        assert!(
            !should_fire_missed(&mut state, "over".to_string(), recent, now),
            "the (cap+1)th missed fire must be suppressed"
        );
    }

    // ── TRN-3: orphan OS-alarm reconcile (mobile) ────────────────────────

    #[test]
    fn orphan_alarm_ids_cancels_only_unbacked_alarms() {
        // Given the OS's armed alarm ids and the ids of live tasks, only the
        // armed ids with no live task behind them are cancelled. This is the
        // core of the launch-time sweep that clears phantom notifications for
        // shifted/deleted tasks.
        let valid: HashSet<i32> = [2, 3, 4].into_iter().collect();
        assert_eq!(orphan_alarm_ids(&[1, 2, 3], &valid), vec![1]);
        // Nothing armed → nothing to cancel.
        assert!(orphan_alarm_ids(&[], &valid).is_empty());
        // No live tasks at all → every armed alarm is an orphan.
        let empty: HashSet<i32> = HashSet::new();
        assert_eq!(orphan_alarm_ids(&[1, 2, 3], &empty), vec![1, 2, 3]);
    }
}
