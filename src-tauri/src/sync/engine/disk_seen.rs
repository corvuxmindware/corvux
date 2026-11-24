//! Evidence that a note's file was really on disk during this engine's life.
//!
//! A DocStore entry whose file is missing is ambiguous. Either the user (or a
//! tool) deleted it, or it never reached disk at all: a peer-adopted note whose
//! render failed or was refused by the write policy, a note the startup
//! recovery could not materialize, a divergent doc, a path the OS cannot hold.
//! Turning the second kind into a CRDT tombstone spreads a delete the user
//! never made to every paired device (SY-2).
//!
//! The engine therefore records every note it has actually SEEN on disk (the
//! startup / overflow scan found the file, the watcher reported it modified,
//! a locked render or recovery just wrote it, or the OS reported its removal)
//! and lets a bulk-inferred tombstone fire only for notes in this set. A note
//! outside the set follows the recover (restore) policy instead: put it back
//! rather than delete it.
//!
//! The set is in-memory on purpose. After a restart, "missing at startup" is
//! decided by `recover_missing_files` (restore); only a disappearance the
//! engine WATCHED happen is eligible for a tombstone. It is bounded, and a full
//! set fails safe (an unseen note is restored, never deleted).

use std::collections::HashSet;
use std::sync::Mutex;

use crate::sync::protocol::NotePath;

/// Upper bound on tracked paths. A roost is 50K-100K notes at the realistic
/// ceiling; this only stops a pathological tree from growing the set without
/// limit. Past it a note simply counts as "not seen", which is the safe side.
const MAX_TRACKED: usize = 1 << 20;

#[derive(Default)]
pub(super) struct DiskSeen {
    seen: Mutex<HashSet<String>>,
}

impl DiskSeen {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashSet<String>> {
        // The critical sections below cannot panic mid-update, so a poisoned
        // lock still holds a consistent set.
        self.seen.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Record that the note's file existed on disk just now.
    pub(super) fn mark(&self, note: &NotePath) {
        let mut seen = self.lock();
        if seen.len() < MAX_TRACKED || seen.contains(&note.0) {
            seen.insert(note.0.clone());
        }
    }

    /// Drop the note: it is tombstoned or gone, so it no longer needs evidence.
    pub(super) fn forget(&self, note: &NotePath) {
        self.lock().remove(&note.0);
    }

    /// Was the note's file observed on disk during this engine's life?
    pub(super) fn contains(&self, note: &NotePath) -> bool {
        self.lock().contains(&note.0)
    }

    /// Every note currently recorded. A point-in-time copy for the mass-missing
    /// breaker, which diffs it against one walk of the roost.
    pub(super) fn snapshot(&self) -> Vec<String> {
        self.lock().iter().cloned().collect()
    }

    pub(super) fn len(&self) -> usize {
        self.lock().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note(p: &str) -> NotePath {
        NotePath(p.into())
    }

    #[test]
    fn mark_forget_contains_round_trip() {
        let seen = DiskSeen::default();
        assert!(!seen.contains(&note("a.md")));
        seen.mark(&note("a.md"));
        seen.mark(&note("a.md"));
        assert!(seen.contains(&note("a.md")));
        assert_eq!(seen.len(), 1, "marking twice must not duplicate");
        seen.forget(&note("a.md"));
        assert!(!seen.contains(&note("a.md")));
    }

    #[test]
    fn a_full_set_fails_safe_by_not_tracking_new_notes() {
        let seen = DiskSeen::default();
        {
            let mut set = seen.lock();
            for i in 0..MAX_TRACKED {
                set.insert(format!("n{i}.md"));
            }
        }
        seen.mark(&note("late.md"));
        assert!(
            !seen.contains(&note("late.md")),
            "past the bound a note must read as 'not seen' (restored, never tombstoned)"
        );
        seen.mark(&note("n0.md"));
        assert!(seen.contains(&note("n0.md")), "an existing entry stays");
    }
}
