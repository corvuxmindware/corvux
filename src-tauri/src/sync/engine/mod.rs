//! Engine orchestration.
//!
//! The engine owns:
//! - The Tokio runtime tasks for the listen socket and per-peer connections.
//! - The shared `DocStore` + `PeerStore`.
//! - A channel for the file-watcher thread to push local changes into.
//! - The status-event broadcaster for the frontend UI.
//!
//! Submodules:
//! - [`engine`]  — `SyncEngine` orchestrator + lifecycle methods.
//! - [`state`]   — UI/event types (`PeerConnectionState`, `SyncEvent`, ...).
//! - [`accept`]  — TCP accept loop + per-incoming handshake task.
//! - [`util`]    — stateless helpers (time, roost-name decoding, ...).
//! - [`watcher`] — filesystem watcher that emits `LocalChange`s.
//! - [`peer`]    — per-connection state machine (handshake → sync loop).
//! - [`note_locks`] — per-note guard serializing each note's commit+disk.
//! - [`disk_seen`] — evidence a note's file was on disk (gates inferred tombstones).
//! - [`removal_guard`] — mass-missing breaker: suppresses inferred deletes when the roost looks gone.

mod accept;
mod dial;
mod disk_seen;
#[allow(clippy::module_inception)]
pub mod engine;
mod fanout;
mod note_locks;
mod outbox;
pub mod peer;
mod policy;
mod queue_ops;
mod removal_guard;
mod scan;
pub mod state;
mod util;
pub mod watcher;

pub use engine::SyncEngine;
pub use state::{PeerConnectionState, SyncEvent, SyncStatus};
