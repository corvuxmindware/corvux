// known_paths exposed for engine diagnostics
#![allow(dead_code)]

//! Per-note sync exchanges, wrapping `automerge::sync`.
//!
//! Automerge ships a Merkle-head-based sync protocol out of the box:
//!
//! - Each peer holds a `sync::State` per `(doc, remote_peer)` pair.
//! - `generate_sync_message(&mut state)` inspects what we know the peer has
//!   and returns the minimal set of changes they're missing — or `None` if
//!   nothing is pending.
//! - `receive_sync_message(&mut state, msg)` applies remote changes and
//!   updates our model of what the peer has.
//! - The loop terminates when both sides return `None` from `generate_*`
//!   in response to each other's messages.
//!
//! This module is a thin adapter over that API. Two responsibilities:
//!
//! 1. Encode/decode `automerge::sync::Message` bytes for transport.
//! 2. Maintain a `HashMap<NotePath, sync::State>` so the engine can drive
//!    many notes over one transport.

use std::collections::HashMap;

use automerge::sync as am_sync;
use automerge::sync::SyncDoc;

use super::NotePath;
use crate::sync::crdt::NoteDoc;
use crate::sync::error::{CrdtError, SyncResult};

/// Per-peer, per-note sync state. Indexed by `NotePath` because in the
/// normal case a peer has one engine and one transport; all notes share
/// the same "other end", so one map per transport is sufficient.
#[derive(Default)]
pub struct PeerSyncState {
    notes: HashMap<NotePath, am_sync::State>,
}

impl PeerSyncState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Produce the next sync message for a note, if any. `None` means "we
    /// have nothing new for them and are caught up with what they told us".
    pub fn generate(&mut self, path: &NotePath, doc: &mut NoteDoc) -> SyncResult<Option<Vec<u8>>> {
        let state = self.notes.entry(path.clone()).or_default();
        let inner = doc.inner_mut();
        match inner.generate_sync_message(state) {
            Some(msg) => Ok(Some(msg.encode())),
            None => Ok(None),
        }
    }

    /// Apply an incoming sync message to our local doc and sync state.
    pub fn receive(&mut self, path: &NotePath, doc: &mut NoteDoc, bytes: &[u8]) -> SyncResult<()> {
        let state = self.notes.entry(path.clone()).or_default();
        let msg =
            am_sync::Message::decode(bytes).map_err(|e| CrdtError::Automerge(e.to_string()))?;
        let inner = doc.inner_mut();
        inner
            .receive_sync_message(state, msg)
            .map_err(CrdtError::from)?;
        Ok(())
    }

    /// Drop the state for a note (e.g. because we just deleted it). Future
    /// exchanges about that path will start fresh.
    pub fn forget(&mut self, path: &NotePath) {
        self.notes.remove(path);
    }

    pub fn known_paths(&self) -> impl Iterator<Item = &NotePath> {
        self.notes.keys()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::identity::DeviceId;

    fn dev(n: u8) -> DeviceId {
        DeviceId([n; 16])
    }

    /// End-to-end property: two peers with divergent docs converge after
    /// a finite number of sync-message exchanges.
    #[test]
    fn sync_converges_two_peers() {
        let mut a = NoteDoc::new(dev(1)).unwrap();
        a.apply_markdown("alpha").unwrap();
        let mut b = NoteDoc::new(dev(2)).unwrap();
        b.apply_markdown("beta").unwrap();

        let path = NotePath("note.md".into());
        let mut a_state = PeerSyncState::new();
        let mut b_state = PeerSyncState::new();

        // Drive the sync loop manually with a generous iteration cap.
        for _ in 0..20 {
            let from_a = a_state.generate(&path, &mut a).unwrap();
            let from_b = b_state.generate(&path, &mut b).unwrap();
            if from_a.is_none() && from_b.is_none() {
                break;
            }
            if let Some(m) = from_a {
                b_state.receive(&path, &mut b, &m).unwrap();
            }
            if let Some(m) = from_b {
                a_state.receive(&path, &mut a, &m).unwrap();
            }
        }

        // After convergence, both docs have the same heads.
        let heads_a = a.inner_mut().get_heads();
        let heads_b = b.inner_mut().get_heads();
        assert_eq!(heads_a, heads_b);
    }
}
