//! Corvux sync engine.
//!
//! End-to-end CRDT-based peer sync over an authenticated TLS transport.
//! See `Corvux/_sync-mvp-plan.md` in the Obsidian roost for the full design
//! document and each submodule's header for layer-specific rationale.
//!
//! # Layering
//!
//! ```text
//!   mod.rs (public API)             ← Tauri commands call this
//!     └── engine/                   ← orchestrator, peer FSM
//!         ├── protocol/             ← Message enum + handshake FSM + automerge::sync wrapper
//!         ├── transport/            ← framed TLS channel (`Transport` trait)
//!         ├── crdt/                 ← NoteDoc lifecycle + markdown round-trip
//!         ├── store/                ← on-disk persistence
//!         ├── crypto/               ← pairing + fingerprints
//!         └── identity.rs           ← device keypair
//! ```
//!
//! Nothing outside `sync/` should reach into submodules directly — the
//! public surface is whatever `mod.rs` re-exports plus the Tauri commands
//! in `commands::sync`.

pub mod crdt;
pub mod crypto;
pub mod discovery;
pub mod engine;
pub mod error;
pub mod identity;
pub mod protocol;
pub mod queue;
pub mod store;
pub mod transport;
pub mod vault_id;
