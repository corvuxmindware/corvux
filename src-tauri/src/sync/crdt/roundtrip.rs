//! Markdown ↔ Automerge round-trip.
//!
//! When the file watcher detects that a `.md` file on disk diverges from the
//! Automerge doc's rendered text, we have to translate the disk content into
//! a minimal sequence of Automerge splice ops. Doing this naively ("delete
//! everything, insert new text") would:
//!
//! 1. Destroy per-character provenance in the CRDT history.
//! 2. Create gigantic merge conflicts against other devices' concurrent
//!    edits — replacing characters that happened to be identical on both
//!    sides of the edit.
//!
//! Instead we compute a **Myers edit script** against the current Automerge
//! text and apply only the differing regions as splice operations. The
//! `similar` crate gives us this at character granularity in one call.
//!
//! # Why character-level, not line-level?
//!
//! Line-level is cheaper to compute but merges badly: if two devices add a
//! word on the same line, a line-level diff records "line X changed on both
//! sides" which forces a CRDT conflict. Character-level preserves the
//! structure Automerge's text CRDT was designed for — insertions and
//! deletions that commute when they target disjoint regions.
//!
//! # Cost
//!
//! Myers is `O((N+M)*D)` where N+M is total length and D is edit distance.
//! For small external edits on a typical note (under a few KB, edit
//! distance <100), this is microseconds. For a 1 MB paste, it's tens of
//! milliseconds — still fast enough to run on the file-watcher thread.

use automerge::transaction::Transactable;
use automerge::{AutomergeError, ObjId};
use similar::{ChangeTag, TextDiff};

/// Compute a Myers diff between `current` (Automerge rendering) and `target`
/// (disk content), then apply the diff to the `text` object inside `tx`
/// using splice operations.
///
/// Generic over `Transactable` so this works both inside an explicit
/// `Automerge::transact` and against an `AutoCommit` — the former is what
/// `NoteDoc` uses today, the latter might show up in tests.
///
/// The implementation walks the change-list once, tracking a position in
/// the Automerge text object. It coalesces runs of consecutive insertions
/// or deletions into a single `splice_text` call (important for CRDT
/// quality and for cheaper Automerge change records).
pub fn apply_diff<T: Transactable>(
    tx: &mut T,
    text_id: &ObjId,
    current: &str,
    target: &str,
) -> Result<(), AutomergeError> {
    let diff = TextDiff::from_chars(current, target);

    // Automerge indexes text by character position (unicode scalar value).
    // We maintain our own cursor mirroring the Automerge position as we
    // apply ops — every delete shrinks it, every insert grows it.
    let mut cursor: usize = 0;
    let mut pending_insert = String::new();
    let mut pending_delete: usize = 0;

    for change in diff.iter_all_changes() {
        match change.tag() {
            ChangeTag::Equal => {
                // Flush any pending splice before advancing over equal chars.
                flush(
                    tx,
                    text_id,
                    &mut cursor,
                    &mut pending_insert,
                    &mut pending_delete,
                )?;
                cursor += change.value().chars().count();
            }
            ChangeTag::Delete => {
                // Coalesce deletes — but first flush a pending insert so we
                // don't mix the ordering (inserts should happen at current
                // cursor *before* we delete ahead of it, otherwise the
                // semantics flip).
                if !pending_insert.is_empty() {
                    flush(
                        tx,
                        text_id,
                        &mut cursor,
                        &mut pending_insert,
                        &mut pending_delete,
                    )?;
                }
                pending_delete += change.value().chars().count();
            }
            ChangeTag::Insert => {
                pending_insert.push_str(change.value());
            }
        }
    }
    flush(
        tx,
        text_id,
        &mut cursor,
        &mut pending_insert,
        &mut pending_delete,
    )?;

    Ok(())
}

fn flush<T: Transactable>(
    tx: &mut T,
    text_id: &ObjId,
    cursor: &mut usize,
    pending_insert: &mut String,
    pending_delete: &mut usize,
) -> Result<(), AutomergeError> {
    if *pending_delete == 0 && pending_insert.is_empty() {
        return Ok(());
    }
    // `splice_text` semantics: at position `cursor`, delete `del` chars,
    // then insert the new string. Atomic in one Automerge op.
    let del = *pending_delete;
    let ins: &str = pending_insert;
    tx.splice_text(text_id, *cursor, del as isize, ins)?;
    // After the splice, cursor advances past the inserted chars; deleted
    // chars were in front of cursor and are now gone, so cursor stays put
    // minus-plus: `cursor + inserted_count` (we never moved over the deletes).
    *cursor += pending_insert.chars().count();
    pending_insert.clear();
    *pending_delete = 0;
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::sync::crdt::NoteDoc;
    use crate::sync::identity::DeviceId;

    fn dev(n: u8) -> DeviceId {
        DeviceId([n; 16])
    }

    #[test]
    fn diff_applies_insertion_at_end() {
        let mut doc = NoteDoc::new(dev(1)).unwrap();
        doc.apply_markdown("hello").unwrap();
        doc.apply_markdown("hello world").unwrap();
        assert_eq!(doc.render().unwrap(), "hello world");
    }

    #[test]
    fn diff_applies_deletion_at_start() {
        let mut doc = NoteDoc::new(dev(1)).unwrap();
        doc.apply_markdown("abcdef").unwrap();
        doc.apply_markdown("def").unwrap();
        assert_eq!(doc.render().unwrap(), "def");
    }

    #[test]
    fn diff_applies_middle_replacement() {
        let mut doc = NoteDoc::new(dev(1)).unwrap();
        doc.apply_markdown("the quick brown fox").unwrap();
        doc.apply_markdown("the slow brown fox").unwrap();
        assert_eq!(doc.render().unwrap(), "the slow brown fox");
    }

    #[test]
    fn diff_handles_unicode() {
        let mut doc = NoteDoc::new(dev(1)).unwrap();
        doc.apply_markdown("café").unwrap();
        doc.apply_markdown("cafés 🥐").unwrap();
        assert_eq!(doc.render().unwrap(), "cafés 🥐");
    }

    #[test]
    fn diff_preserves_concurrent_edits_from_other_device() {
        // The whole point of the Myers-based diff: if device B concurrently
        // appends to the same base, and device A's round-trip is triggered
        // by an external edit that *doesn't touch B's region*, B's content
        // must survive the merge.
        let mut a = NoteDoc::new(dev(1)).unwrap();
        a.apply_markdown("base text.").unwrap();
        let base = a.save();

        // Device A: external editor inserts at the start.
        let mut a = NoteDoc::load(&base, dev(1)).unwrap();
        a.apply_markdown("PREFIX base text.").unwrap();

        // Device B (loaded from same base): appends at the end.
        let mut b = NoteDoc::load(&base, dev(2)).unwrap();
        b.apply_markdown("base text. SUFFIX").unwrap();

        a.merge(&mut b).unwrap();
        let merged = a.render().unwrap();
        assert!(
            merged.contains("PREFIX") && merged.contains("SUFFIX"),
            "merged text '{merged}' should preserve both edits"
        );
    }

    #[test]
    fn empty_to_nonempty() {
        let mut doc = NoteDoc::new(dev(1)).unwrap();
        doc.apply_markdown("hello").unwrap();
        assert_eq!(doc.render().unwrap(), "hello");
    }

    #[test]
    fn nonempty_to_empty() {
        let mut doc = NoteDoc::new(dev(1)).unwrap();
        doc.apply_markdown("hello").unwrap();
        doc.apply_markdown("").unwrap();
        assert_eq!(doc.render().unwrap(), "");
    }
}
