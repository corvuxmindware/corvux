//! Mass-missing breaker for INFERRED deletes (SYN-I5: never delete on doubt).
//!
//! A tombstone is a CRDT change that spreads to every paired device. The engine
//! writes one without the user asking when the filesystem watcher, or a rescan
//! after an event overflow, says a note's file is gone (`commit_tombstone`,
//! `InferredFromDisk`). A single missing file is a delete. HUNDREDS of notes
//! vanishing is almost never the user: an unmounted drive, a cloud folder
//! evicting files, a permission change, a git checkout of another branch.
//! Believing it would delete the whole roost on every device.
//!
//! So before an inferred tombstone commits, the engine judges the ROOST, not the
//! note. It suppresses every inferred delete when ANY of these holds:
//!
//! 1. the roost root is missing, unreadable or empty (no visible entries and no
//!    `.corvux` marker, which the engine itself creates);
//! 2. more than `max(20, 50%)` of the notes the engine has SEEN on disk are gone
//!    in one look. Inferred tombstones committed in the last 10 minutes still
//!    count as gone: a committed delete is forgotten from `DiskSeen`, and without
//!    this a deletion that proceeds in small steps would reset the count each time;
//! 3. more than 20 inferred tombstones were committed within the last 60 s (the
//!    rolling rate). The look in (2) is a snapshot; a vanish that unfolds over
//!    minutes (git checkout on an AV-scanned disk, gradual cloud eviction) needs
//!    a counter that spans time. It is checked again, atomically, at the moment of
//!    each commit, so concurrent commits cannot overshoot it.
//!
//! A batch known up front (a folder removal, a rescan) is judged as a whole: if
//! its size exceeds what the 60 s budget still allows, NONE of it commits, rather
//! than the first 20 and then a gap.
//!
//! A genuine delete of one note, or of a few, still propagates. Deleting a note in
//! the app is not an inferred delete and is never affected.
//!
//! **Known limit (a slow vanish).** Rule 3 is a hard cap of 20 per rolling 60 s, but
//! rule 2 only remembers commits for 10 minutes. A vanish slower than 20 per minute
//! is therefore not bounded in total: with `c` = inferred commits in the last 10
//! minutes (at most 200), it is stopped only once the notes still seen on disk fall
//! to about `c + 2`. At the maximum sustained 20 per minute a 1000-note roost loses
//! about 800 notes (~40 min); at 5 per minute about 950; at 2 per minute or less
//! (`c` <= 20) nothing stops it. It is far less likely than a mass event and is a
//! decision for the owner, not a hidden property.
//!
//! **Suppression writes nothing to the roost.** No tombstone, and no restore of the
//! missing notes either: restoring is wrong in exactly these situations (it can
//! create files in an unmounted drive's mount-point stub, or next to a cloud
//! placeholder that later downloads and then conflicts). The DocStore keeps the
//! notes, so startup recovery restores them if the files are really gone. The
//! rescan, the one other place that can restore, skips its restores whenever it
//! suppresses.
//!
//! The look is one directory walk diffed against `DiskSeen`. A suppress verdict is
//! reused for 15 s. An ALLOW verdict is reused for 2 s, and only by removals that
//! were scheduled BEFORE the look started: a removal scheduled after it may belong
//! to a vanish the look never saw, so it gets a fresh look.

use std::collections::{HashSet, VecDeque};
use std::time::{Duration, Instant};

use tracing::warn;

use crate::sync::protocol::NotePath;

use super::engine::SyncEngine;
use super::state::SyncEvent;

/// At or below this many missing notes a delete is never "mass".
pub(super) const MASS_MISSING_MIN: usize = 20;
/// Rule 3: at most this many inferred tombstones commit per [`RATE_WINDOW`].
pub(super) const RATE_MAX: usize = 20;
/// Rule 3's window.
const RATE_WINDOW: Duration = Duration::from_secs(60);
/// How long a committed inferred tombstone still counts toward rule 2.
const DRIP_WINDOW: Duration = Duration::from_secs(10 * 60);
/// How long an `Allow` verdict is reused (see the module doc for who may reuse it).
const ALLOW_TTL: Duration = Duration::from_secs(2);
/// How long a suppress verdict is reused. Longer: while a drive is unmounted every
/// queued removal would otherwise repeat the walk.
const SUPPRESS_TTL: Duration = Duration::from_secs(15);
/// Minimum gap between two user notices.
const NOTICE_EVERY: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RemovalVerdict {
    /// Inferred deletes may proceed (each still verified under the note lock).
    Allow,
    /// The roost root is missing, unreadable or empty.
    RootUnhealthy,
    /// More than `max(20, 50%)` of the notes seen on disk are gone.
    MassMissing { missing: usize, seen: usize },
    /// More than [`RATE_MAX`] inferred tombstones within [`RATE_WINDOW`], or a
    /// batch larger than the budget that window still allows.
    RateLimited { recent: usize },
}

impl RemovalVerdict {
    pub(super) fn is_allow(&self) -> bool {
        matches!(self, RemovalVerdict::Allow)
    }

    /// Short, user-facing notice, shown as a toast (the frontend relies on the
    /// "Sync paused deleting notes:" prefix being a full sentence). It says what
    /// the user needs to know and would otherwise guess wrong: nothing was deleted
    /// on the other devices, AND the notes now missing from this disk are not
    /// "deleted": they come back here at the next start unless the user restores
    /// the folder or deletes them inside Corvux. Each stays under ~300 chars
    /// (`notices_say_what_happens_next_and_stay_short`).
    fn notice(&self) -> Option<String> {
        match self {
            RemovalVerdict::Allow => None,
            RemovalVerdict::RootUnhealthy => Some(
                "Sync paused deleting notes: the notes folder looks missing, empty or unreadable \
                 (drive unmounted or cloud offline?). Nothing was deleted on your other devices, \
                 but notes missing from disk will come back here at the next start unless you \
                 restore the folder or delete them inside Corvux."
                    .into(),
            ),
            RemovalVerdict::MassMissing { missing, seen } => Some(format!(
                "Sync paused deleting notes: {missing} of {seen} notes vanished from disk (drive \
                 unmounted or cloud eviction?). Nothing further was deleted on your other \
                 devices, but notes missing from disk will come back here at the next start \
                 unless you restore the folder or delete them inside Corvux."
            )),
            RemovalVerdict::RateLimited { recent } => Some(format!(
                "Sync paused deleting notes: {recent} notes left the disk in a minute (drive \
                 unmounted, cloud eviction or branch switch?). Nothing further was deleted on \
                 your other devices, but notes missing from disk will come back here at the next \
                 start unless you restore the folder or delete them inside Corvux."
            )),
        }
    }
}

/// `missing` of `seen` notes gone is a mass event iff it is above BOTH 20 and
/// 50%: more than `max(20, 50%)`.
pub(super) fn is_mass_missing(missing: usize, seen: usize) -> bool {
    missing > MASS_MISSING_MIN && missing * 2 > seen
}

/// The rolling log of committed inferred tombstones and the notice limiter. Every
/// method takes `now` explicitly so the rules are testable without sleeping.
#[derive(Default)]
struct CommitLog {
    commits: VecDeque<Instant>,
    last_notice: Option<Instant>,
}

impl CommitLog {
    fn prune(&mut self, now: Instant) {
        while self
            .commits
            .front()
            .is_some_and(|at| now.saturating_duration_since(*at) > DRIP_WINDOW)
        {
            self.commits.pop_front();
        }
    }

    fn within(&self, window: Duration, now: Instant) -> usize {
        self.commits
            .iter()
            .filter(|at| now.saturating_duration_since(**at) <= window)
            .count()
    }
}

#[derive(Default)]
pub(super) struct RemovalGuard {
    /// The last look: when it STARTED and what it found. The async mutex is held
    /// across a look, so concurrent callers share one walk.
    look: tokio::sync::Mutex<Option<(Instant, RemovalVerdict)>>,
    /// Held only for instants; never across an await.
    log: std::sync::Mutex<CommitLog>,
    /// Inferred removals scheduled (holding a verification slot) but not yet
    /// resolved. They are not committed yet, so the log cannot see them, but a
    /// second batch scheduled right behind the first must count them, or two
    /// batches of 15 would both pass a budget of 20 and then collide at the commit.
    pending: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

/// Counts one scheduled inferred removal until it resolves (commit, skip,
/// suppress or cancel).
pub(super) struct PendingInferred(std::sync::Arc<std::sync::atomic::AtomicUsize>);

impl Drop for PendingInferred {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

impl RemovalGuard {
    fn log(&self) -> std::sync::MutexGuard<'_, CommitLog> {
        // The critical sections cannot panic mid-update.
        self.log.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Inferred tombstones committed within the rate window.
    pub(super) fn rate_count(&self, now: Instant) -> usize {
        let mut log = self.log();
        log.prune(now);
        log.within(RATE_WINDOW, now)
    }

    /// How many more inferred tombstones the rate window allows right now:
    /// what it holds already, plus what is scheduled and not yet resolved.
    pub(super) fn budget(&self, now: Instant) -> usize {
        let pending = self.pending.load(std::sync::atomic::Ordering::SeqCst);
        RATE_MAX.saturating_sub(self.rate_count(now) + pending)
    }

    /// Count a scheduled inferred removal until the returned guard drops.
    pub(super) fn begin_pending(&self) -> PendingInferred {
        self.pending
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        PendingInferred(self.pending.clone())
    }

    /// Inferred tombstones committed within the drip window (rule 2's carry-over).
    fn drip_count(&self, now: Instant) -> usize {
        let mut log = self.log();
        log.prune(now);
        log.commits.len()
    }

    /// Reserve one inferred commit, atomically. `Err(n)`: the rate window already
    /// holds `n >= RATE_MAX` commits. Reserve only once the commit is certain to be
    /// attempted (file confirmed absent under the note lock), and
    /// [`Self::release`] if it then fails.
    pub(super) fn reserve(&self, now: Instant) -> Result<(), usize> {
        let mut log = self.log();
        log.prune(now);
        let recent = log.within(RATE_WINDOW, now);
        if recent >= RATE_MAX {
            return Err(recent);
        }
        log.commits.push_back(now);
        Ok(())
    }

    /// Undo the latest [`Self::reserve`] (the commit it was for failed).
    pub(super) fn release(&self) {
        self.log().commits.pop_back();
    }

    /// `true` at most once per [`NOTICE_EVERY`].
    fn notice_due(&self, now: Instant) -> bool {
        let mut log = self.log();
        if log
            .last_notice
            .is_none_or(|at| now.saturating_duration_since(at) >= NOTICE_EVERY)
        {
            log.last_notice = Some(now);
            true
        } else {
            false
        }
    }
}

impl SyncEngine {
    /// Judge the roost before an inferred delete of a note whose removal was
    /// scheduled at `since`. Pass `Instant::now()` to force a fresh look.
    /// Concurrent callers share one look: the first holds the lock while it
    /// walks, the rest reuse its result when the rules above allow.
    pub(super) async fn removal_verdict(&self, since: Instant) -> RemovalVerdict {
        let guard = &self.inner.removal_guard;
        // Rule 3 needs no walk.
        let recent = guard.rate_count(Instant::now());
        if recent >= RATE_MAX {
            let verdict = RemovalVerdict::RateLimited { recent };
            self.announce_removal_suppressed(verdict);
            return verdict;
        }
        let mut cached = guard.look.lock().await;
        if let Some((at, verdict)) = *cached {
            let reusable = if verdict.is_allow() {
                at >= since && at.elapsed() < ALLOW_TTL
            } else {
                at.elapsed() < SUPPRESS_TTL
            };
            if reusable {
                return verdict;
            }
        }
        let started = Instant::now();
        let verdict = self.assess_removals().await;
        *cached = Some((started, verdict));
        self.announce_removal_suppressed(verdict);
        verdict
    }

    /// Log a suppress verdict and, at most once per minute, raise the user notice
    /// on the existing per-note failure channel with the roost root as the path:
    /// no new IPC type. The frontend store keeps it in `noteSyncFailures[""]`.
    /// Never blocks and never awaits, so it is safe under the doc-store lock.
    pub(super) fn announce_removal_suppressed(&self, verdict: RemovalVerdict) {
        let Some(notice) = verdict.notice() else {
            return;
        };
        warn!(?verdict, "{notice}");
        if self.inner.removal_guard.notice_due(Instant::now()) {
            let _ = self.inner.events.send(SyncEvent::NoteSyncFailed {
                path: String::new(),
                reason: notice,
            });
        }
    }

    /// The cached suppress verdict, if there is a fresh one (or the rate window is
    /// full). Never walks and never waits (a look in progress counts as "no
    /// answer"), so the watcher loop can use it to skip scheduling work that would
    /// only be suppressed.
    pub(super) fn cached_suppression(&self) -> Option<RemovalVerdict> {
        let guard = &self.inner.removal_guard;
        let now = Instant::now();
        // No budget left: what is committed plus what is already scheduled fills
        // the window. (Dropping a removal here is the same refusal the commit
        // would give it, minus the timer and the slot.)
        if guard.budget(now) == 0 {
            let recent = guard.rate_count(now);
            return Some(RemovalVerdict::RateLimited {
                recent: recent.max(RATE_MAX),
            });
        }
        let cached = guard.look.try_lock().ok()?;
        let (at, verdict) = (*cached)?;
        (!verdict.is_allow() && at.elapsed() < SUPPRESS_TTL).then_some(verdict)
    }

    /// One look at the roost. Cheap checks first; the doc loads only happen when
    /// the number missing is already above the floor.
    async fn assess_removals(&self) -> RemovalVerdict {
        let fsh = super::peer::resolve_fs(&self.inner.roost, self.inner.roost_fs.as_deref());
        let fs = fsh.as_fs();

        // 1. Root health. `list` skips hidden entries, so an empty listing is
        //    only "empty" if the engine's own `.corvux` marker is gone too.
        match fs.list("").await {
            Err(e) => {
                warn!(error = %e, "removal guard: roost root unreadable");
                return RemovalVerdict::RootUnhealthy;
            }
            Ok(entries) if entries.is_empty() => {
                if !matches!(fs.exists(".corvux").await, Ok(true)) {
                    return RemovalVerdict::RootUnhealthy;
                }
            }
            Ok(_) => {}
        }

        // 2. One walk, diffed against what the engine has seen on disk.
        let on_disk: HashSet<String> = match super::watcher::list_watchable_paths(fs).await {
            Ok(paths) => paths.into_iter().map(|(_, rel)| rel).collect(),
            Err(e) => {
                warn!(error = %e, "removal guard: could not walk the roost");
                return RemovalVerdict::RootUnhealthy;
            }
        };
        let missing: Vec<NotePath> = self
            .inner
            .disk_seen
            .snapshot()
            .into_iter()
            .filter(|rel| !on_disk.contains(rel))
            .map(NotePath)
            .collect();
        // Inferred tombstones committed recently were forgotten from `DiskSeen`;
        // they are still notes that vanished, so they stay in the count.
        let carried = self.inner.removal_guard.drip_count(Instant::now());
        if missing.len() + carried <= MASS_MISSING_MIN {
            return RemovalVerdict::Allow;
        }

        // 3. Above the floor: drop entries that are already tombstones (a note a
        //    peer deleted stays in `DiskSeen` and is missing from disk by
        //    definition), which also cleans the set. A doc that cannot be read
        //    counts as missing: doubt pushes toward suppression.
        let mut live_missing = 0usize;
        for note in missing {
            match self.note_is_tombstoned(&note).await {
                Ok(true) => self.inner.disk_seen.forget(&note),
                Ok(false) | Err(_) => live_missing += 1,
            }
        }
        let missing = live_missing + carried;
        let seen = self.inner.disk_seen.len() + carried;
        if is_mass_missing(missing, seen) {
            RemovalVerdict::MassMissing { missing, seen }
        } else {
            RemovalVerdict::Allow
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mass_missing_needs_both_more_than_20_and_more_than_half() {
        // Floor: never "mass" at or below 20, however large the share.
        assert!(!is_mass_missing(5, 5));
        assert!(!is_mass_missing(20, 20));
        assert!(!is_mass_missing(20, 30));
        // Above the floor but not above half.
        assert!(!is_mass_missing(21, 100));
        assert!(!is_mass_missing(50, 100));
        assert!(!is_mass_missing(500, 1000));
        // Both.
        assert!(is_mass_missing(21, 30));
        assert!(is_mass_missing(51, 100));
        assert!(is_mass_missing(60, 100));
        assert!(is_mass_missing(25, 40));
        assert!(is_mass_missing(501, 1000));
    }

    #[test]
    fn only_a_non_allow_verdict_has_a_notice() {
        assert!(RemovalVerdict::Allow.notice().is_none());
        assert!(RemovalVerdict::RootUnhealthy.notice().is_some());
        let mass = RemovalVerdict::MassMissing {
            missing: 60,
            seen: 100,
        };
        let text = mass.notice().unwrap();
        assert!(text.contains("60 of 100"), "{text}");
        let rate = RemovalVerdict::RateLimited { recent: 21 };
        assert!(rate.notice().unwrap().contains("21 notes"));
    }

    /// N2: the user must learn what happens next. Every suppressing notice says
    /// nothing was deleted on the other devices, that the missing notes come back
    /// on this device at the next start, and the two ways to avoid it, and none
    /// is long enough to get cut off as a toast (even with 6-digit counts).
    #[test]
    fn notices_say_what_happens_next_and_stay_short() {
        let verdicts = [
            RemovalVerdict::RootUnhealthy,
            RemovalVerdict::MassMissing {
                missing: 100_000,
                seen: 100_000,
            },
            RemovalVerdict::RateLimited { recent: 100_000 },
        ];
        for verdict in verdicts {
            let text = verdict.notice().unwrap();
            assert!(text.starts_with("Sync paused deleting notes:"), "{text}");
            assert!(text.contains("other devices"), "{text}");
            assert!(text.contains("come back"), "{text}");
            assert!(text.contains("next start"), "{text}");
            assert!(text.contains("restore the folder"), "{text}");
            assert!(text.contains("delete them inside Corvux"), "{text}");
            assert!(text.chars().count() < 300, "{} chars: {text}", text.len());
        }
    }

    /// Rule 3: the 21st inferred commit within 60 s is refused, a commit that has
    /// aged out of the window frees a slot, and `release` gives one back.
    #[test]
    fn the_rate_window_admits_twenty_commits_per_minute() {
        let guard = RemovalGuard::default();
        let t0 = Instant::now();
        for i in 0..RATE_MAX {
            assert!(
                guard.reserve(t0 + Duration::from_millis(i as u64)).is_ok(),
                "commit {i} must be admitted"
            );
        }
        let t1 = t0 + Duration::from_secs(1);
        assert_eq!(guard.reserve(t1), Err(RATE_MAX), "the 21st within 60 s");
        assert_eq!(guard.budget(t1), 0);
        // 61 s later the first twenty have left the rate window.
        let later = t0 + Duration::from_secs(61);
        assert_eq!(guard.budget(later), RATE_MAX);
        assert!(guard.reserve(later).is_ok());
        // A failed commit gives its slot back.
        guard.release();
        assert_eq!(guard.rate_count(later), 0);
    }

    /// Rule 2's carry-over outlives the rate window but not the drip window.
    #[test]
    fn committed_tombstones_carry_into_the_look_for_ten_minutes() {
        let guard = RemovalGuard::default();
        let t0 = Instant::now();
        for _ in 0..5 {
            guard.reserve(t0).unwrap();
        }
        assert_eq!(guard.drip_count(t0 + Duration::from_secs(120)), 5);
        assert_eq!(guard.rate_count(t0 + Duration::from_secs(120)), 0);
        assert_eq!(guard.drip_count(t0 + Duration::from_secs(601)), 0);
    }

    /// Scheduled-but-unresolved removals count against the budget, so a batch
    /// scheduled behind another sees what is already in flight.
    #[test]
    fn pending_removals_reduce_the_budget_until_they_resolve() {
        let guard = RemovalGuard::default();
        let now = Instant::now();
        assert_eq!(guard.budget(now), RATE_MAX);
        let held: Vec<PendingInferred> = (0..15).map(|_| guard.begin_pending()).collect();
        assert_eq!(guard.budget(now), RATE_MAX - 15);
        guard.reserve(now).unwrap();
        assert_eq!(guard.budget(now), RATE_MAX - 16);
        drop(held);
        assert_eq!(guard.budget(now), RATE_MAX - 1);
    }

    #[test]
    fn the_notice_is_rate_limited() {
        let guard = RemovalGuard::default();
        let t0 = Instant::now();
        assert!(guard.notice_due(t0));
        assert!(!guard.notice_due(t0 + Duration::from_secs(30)));
        assert!(guard.notice_due(t0 + Duration::from_secs(61)));
    }
}
