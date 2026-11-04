//! On-disk persistence for the sync layer.
//!
//! Two stores sit behind this module:
//!
//! - [`DocStore`] — per-note Automerge binaries under `<state_dir>/docs/`.
//!   One `.automerge` file per note, atomically written (temp + rename).
//! - [`PeerStore`] — known-peer registry persisted as a single
//!   `<state_dir>/peers.bin` file. Small (one entry per ever-paired device)
//!   so full-file rewrites on update are fine.
//!
//! `state_dir` is an **app-private** directory (derived from
//! `app_data_dir/vaults/<hash>/sync/` — see `paths::roost_db_dir`). It is
//! deliberately NOT inside the user's roost: Android SAF URIs are not
//! writable via `std::fs`, and in any case sync state is device-local
//! (should not Syncthing-replicate between machines).
//!
//! Identity material lives elsewhere (`identity.rs`, in the app data dir).
//!
//! Both stores validate and normalize every path before touching the
//! filesystem; see `normalize_note_path` for the rules.

pub mod doc_store;
pub mod outbox_store;
pub mod peer_store;
pub mod pending_trash_store;

use std::path::{Component, Path, PathBuf};

use unicode_normalization::UnicodeNormalization;

use crate::sync::error::StoreError;
use crate::sync::protocol::NotePath;

/// NFC-normalize a note-path string so Unicode-equivalent spellings resolve to
/// one identity. A `café.md` typed in an editor (NFC) and one created via
/// macOS Finder (NFD) are byte-different but the same logical note; without
/// this they become two never-converging docs. Applied at the DocStore
/// filename boundary. See C-1.
pub fn nfc_normalize(s: &str) -> String {
    s.nfc().collect()
}

/// Clamp an untrusted tombstone `deleted_at` (unix millis) to a plausible
/// window around our local `now`. Both the legacy `NoteDelete` frame AND the
/// CRDT `deleted_at` register carry the *deleting* peer's wall clock, which a
/// hostile or badly-skewed peer controls — so neither can be trusted at a
/// reap decision (M-1). `0` (unset) would be un-reapable; a future value would
/// dodge GC forever (a tombstone-accumulation DoS); an implausibly ancient
/// value would reap on the next sweep and resurrect the note. Anything outside
/// the window is stamped with our own clock (`now`) so the retention timer
/// starts from when *we* observed the delete, not from an attacker's number.
///
/// Lives in the store layer because the tombstone GC ([`doc_store`]) is the
/// primary security consumer; the engine's legacy-frame path calls it too.
pub fn sanitize_tombstone_millis(peer_at: u64, now: u64) -> u64 {
    const MAX_FUTURE_SKEW_MS: u64 = 5 * 60 * 1000; // 5 minutes of clock skew
    const MAX_AGE_MS: u64 = 400 * 24 * 60 * 60 * 1000; // > tombstone retention
    let too_new = peer_at > now.saturating_add(MAX_FUTURE_SKEW_MS);
    let too_old = peer_at < now.saturating_sub(MAX_AGE_MS);
    if peer_at == 0 || too_new || too_old {
        now
    } else {
        peer_at
    }
}

/// Docs subdirectory inside the sync state root: `<state_dir>/docs/`.
pub fn docs_root(state_dir: &Path) -> PathBuf {
    state_dir.join("docs")
}

/// Peers file inside the sync state root: `<state_dir>/peers.bin`.
pub fn peers_file(state_dir: &Path) -> PathBuf {
    state_dir.join("peers.bin")
}

/// Durable outbox file inside the sync state root: `<state_dir>/outbox.bin`.
pub fn outbox_file(state_dir: &Path) -> PathBuf {
    state_dir.join("outbox.bin")
}

/// Convert an absolute path inside the roost to a normalized roost-relative
/// `NotePath` with forward slashes. Rejects:
///
/// - Paths with `..` or absolute anchors (path traversal).
/// - Paths not inside the roost.
/// - Paths containing a null byte or platform-reserved names.
///
/// Called on every write and read — this is the security boundary that
/// prevents a malicious peer's `NotePath` payload from writing outside the
/// roost.
pub fn to_note_path(roost: &Path, abs: &Path) -> Result<NotePath, StoreError> {
    let roost = roost.canonicalize().map_err(StoreError::Io)?;
    let abs = abs.canonicalize().map_err(StoreError::Io)?;
    let rel = abs
        .strip_prefix(&roost)
        .map_err(|_| StoreError::PathTraversal(abs.to_string_lossy().into()))?;
    let mut parts = Vec::new();
    for c in rel.components() {
        match c {
            Component::Normal(s) => {
                let s = s.to_string_lossy();
                if s.contains('\0') || s.contains('/') || s.contains('\\') {
                    return Err(StoreError::InvalidPath(s.into()));
                }
                // L-1: symmetry with `from_note_path` — on Windows, refuse to
                // import a local file whose name Windows can't round-trip
                // (reserved device name, reserved char, trailing/leading dot or
                // space). No-op off Windows, where such names are legal.
                #[cfg(windows)]
                if is_windows_unsafe_component(&s) {
                    return Err(StoreError::InvalidPath(s.into()));
                }
                parts.push(s.into_owned());
            }
            _ => {
                return Err(StoreError::InvalidPath(rel.to_string_lossy().into()));
            }
        }
    }
    Ok(NotePath(parts.join("/")))
}

/// Inverse of `to_note_path` — builds a roost-absolute path from a `NotePath`,
/// re-validating each component.
///
/// Retained as the tested reference for the lexical component rules that
/// `peer::is_safe_note_path` mirrors at the message boundary. It has **no
/// production callers**: since the Class-A hardening, every sync disk op
/// resolves its path through `RoostFs::safe_resolve` (canonicalizing, so it
/// also rejects symlink escapes a lexical check can't see), which strictly
/// supersedes this. Kept `#[cfg(test)]` so the rules stay under test without
/// carrying dead code into the binary.
#[cfg(test)]
pub fn from_note_path(roost: &Path, note: &NotePath) -> Result<PathBuf, StoreError> {
    if note.0.is_empty() || note.0.starts_with('/') {
        return Err(StoreError::InvalidPath(note.0.clone()));
    }
    let mut out = roost.to_path_buf();
    for part in note.0.split('/') {
        if part.is_empty() || part == "." || part == ".." || part.contains('\0') {
            return Err(StoreError::InvalidPath(note.0.clone()));
        }
        // L-1: on Windows, a name using a reserved char/device name (or a
        // trailing dot/space) can't be created — `std::fs::write` fails EINVAL
        // deep in the render path and the error is swallowed, so the note
        // silently never appears. Reject it at the boundary with a clear
        // InvalidPath instead. Non-Windows platforms accept these names (they
        // are legal there), so this is a no-op off Windows.
        #[cfg(windows)]
        if is_windows_unsafe_component(part) {
            return Err(StoreError::InvalidPath(note.0.clone()));
        }
        out.push(part);
    }
    Ok(out)
}

/// True if `name` is a filename Windows cannot create: it contains a reserved
/// character (`< > : " / \ | ? *` or a control char), matches a reserved
/// device name (CON, PRN, AUX, NUL, COM1-9, LPT1-9 — with or without an
/// extension), or ends in a dot or space. See L-1. Always compiled (so it can
/// be unit-tested on any platform); only *enforced* on Windows.
#[cfg_attr(not(windows), allow(dead_code))]
fn is_windows_unsafe_component(name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    if name.chars().any(|c| {
        matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') || (c as u32) < 0x20
    }) {
        return true;
    }
    // A leading OR trailing dot/space is mishandled by Windows (Explorer + a
    // number of APIs trim them) → the on-disk name diverges from the note path
    // and the two never reconcile. See L-1.
    if name.starts_with(' ') || name.ends_with('.') || name.ends_with(' ') {
        return true;
    }
    // Reserved device names, case-insensitive, matched on the stem before the
    // first dot (`NUL.md` is just as reserved as `NUL`).
    let stem = name.split('.').next().unwrap_or(name);
    let stem_upper = stem.to_ascii_uppercase();
    if matches!(stem_upper.as_str(), "CON" | "PRN" | "AUX" | "NUL") {
        return true;
    }
    if let Some(rest) = stem_upper
        .strip_prefix("COM")
        .or_else(|| stem_upper.strip_prefix("LPT"))
    {
        // COM1-9 / LPT1-9 (a single digit 1-9).
        if rest.len() == 1 && matches!(rest.as_bytes()[0], b'1'..=b'9') {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn note_path_round_trip() {
        let roost = tempdir().unwrap();
        let sub = roost.path().join("notes");
        std::fs::create_dir_all(&sub).unwrap();
        let file = sub.join("idea.md");
        std::fs::write(&file, "x").unwrap();
        let np = to_note_path(roost.path(), &file).unwrap();
        assert_eq!(np.0, "notes/idea.md");
        let back = from_note_path(roost.path(), &np).unwrap();
        assert_eq!(back.canonicalize().unwrap(), file.canonicalize().unwrap());
    }

    #[test]
    fn from_note_path_rejects_traversal() {
        let roost = tempdir().unwrap();
        assert!(from_note_path(roost.path(), &NotePath("../etc/passwd".into())).is_err());
        assert!(from_note_path(roost.path(), &NotePath("./foo".into())).is_err());
        assert!(from_note_path(roost.path(), &NotePath("/abs".into())).is_err());
        assert!(from_note_path(roost.path(), &NotePath("a//b".into())).is_err());
        assert!(from_note_path(roost.path(), &NotePath("".into())).is_err());
    }

    #[test]
    fn windows_unsafe_component_detection() {
        // Reserved chars.
        for bad in [
            "a:b.md",
            "q?.md",
            "a<b>.md",
            "pipe|.md",
            "star*.md",
            "quote\".md",
        ] {
            assert!(is_windows_unsafe_component(bad), "should be unsafe: {bad}");
        }
        // Reserved device names, with or without extension, any case.
        for bad in ["CON", "con.md", "NUL.txt", "Aux", "com1", "LPT9.md"] {
            assert!(is_windows_unsafe_component(bad), "should be unsafe: {bad}");
        }
        // Leading / trailing dot or space (Windows trims → silent divergence).
        assert!(is_windows_unsafe_component("note."));
        assert!(is_windows_unsafe_component("note "));
        assert!(is_windows_unsafe_component(" note.md"));
        // Perfectly ordinary names (incl. legal-everywhere ampersand) are safe.
        for ok in [
            "note.md",
            "Q&A.md",
            "café.md",
            "com10.md",
            "console.md",
            "a-b_c.md",
        ] {
            assert!(!is_windows_unsafe_component(ok), "should be safe: {ok}");
        }
    }
}
