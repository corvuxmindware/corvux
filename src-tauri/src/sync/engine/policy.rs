//! Deny-by-default write policy for remote-originated data (SYN-I2/I7, SYN-B1/B9).
//!
//! Every peer-supplied note path and attachment blob must clear one of these
//! vets *before* it is committed to the DocStore or written to the roost. The
//! policy matches what the watcher/scanner already enforce locally — markdown
//! files only, hidden/technical directories ignored — so sync can never render
//! a file the local indexer would refuse to index (the SYN-B1 divergence +
//! escape surface: `.git/hooks/pre-commit`, `.corvux/vault-id.bin`, `foo.sh`),
//! nor accept an unsolicited or content-forged attachment (SYN-B9).
//!
//! One implementation ⇒ the live and queue adopt paths cannot drift. The vets
//! are pure gates: they do NOT rewrite the path. NFC convergence is tracked
//! separately (SYN follow-up) because normalizing here without threading the
//! result through the DocStore key would desync disk vs CRDT.

use crate::sync::protocol::NotePath;

/// Why a remote-supplied path or blob was refused. Every variant is non-fatal
/// at the call site: the live frame handlers skip (`Ok(false)`) and the queue
/// handler ACK-drops (a malformed path never becomes valid on retry). Never
/// tear the session down over one — that is a remote-triggerable disconnect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RemoteReject {
    /// Traversal / absolute / backslash / NUL / Windows drive or verbatim prefix.
    Path,
    /// A dot-prefixed path component — the scanner ignores these, so a remote
    /// note there could never be indexed and is the B1 escape surface.
    Hidden,
    /// Not a `.md` / `.markdown` file — the scanner only indexes markdown.
    NotMarkdown,
    /// Blob: not scoped under `.attachments/`.
    NotAttachment,
    /// Blob: exceeds `SYNC_BLOB_MAX_BYTES`.
    TooLarge,
    /// Blob: bytes don't hash to the content-addressed name (poisoning attempt).
    ContentMismatch,
}

/// Vet a remote note path against the same policy the watcher/scanner apply.
/// Pure gate — does not modify the path.
pub(super) fn vet_remote_rel(note: &NotePath) -> Result<(), RemoteReject> {
    // 1. The original lexical gate: empty, absolute, backslash, embedded NUL,
    //    and `.`/`..`/empty components. (Also blocks NUL, which validate_rel
    //    below does not.)
    if !super::peer::is_safe_note_path(note) {
        return Err(RemoteReject::Path);
    }
    // 2. Windows drive-letter (`C:/x`) and verbatim/device prefixes — is_safe_
    //    note_path misses these, and a drive-absolute segment would REPLACE the
    //    root on a `PathBuf::join` under docs_root. validate_rel rejects them.
    let cleaned = crate::roostfs::validate_rel(&note.0).map_err(|_| RemoteReject::Path)?;
    // 3. The scanner ignores every dot-prefixed component (see NativeFs::list),
    //    so a remote note under one could never be indexed locally — and this
    //    is exactly the B1 escape surface (.git, .corvux, .corvux-trash,
    //    .attachments). Reject any hidden component, including the filename.
    if cleaned.split('/').any(|c| c.starts_with('.')) {
        return Err(RemoteReject::Hidden);
    }
    // 4. The scanner only indexes markdown.
    let last = cleaned.rsplit('/').next().unwrap_or("");
    let lower = last.to_ascii_lowercase();
    if !(lower.ends_with(".md") || lower.ends_with(".markdown")) {
        return Err(RemoteReject::NotMarkdown);
    }
    Ok(())
}

/// Vet a remote attachment blob (SYN-B9). Scoped to `.attachments/`, capped,
/// and — if the filename claims content-addressing (`sha-<hex16>.<ext>`, the
/// scheme in `commands::attachments::content_addressed_name`) — the bytes must
/// hash to it, so a peer can't overwrite a content-addressed attachment with
/// mismatched bytes (poisoning every device that later serves that name).
/// The caller MUST also confirm the blob was solicited (an in-flight request).
pub(super) fn vet_remote_blob_rel(rel: &str, bytes: &[u8]) -> Result<(), RemoteReject> {
    if bytes.len() as u64 > crate::sync::protocol::SYNC_BLOB_MAX_BYTES {
        return Err(RemoteReject::TooLarge);
    }
    if !super::peer::is_attachment_rel(rel) {
        return Err(RemoteReject::NotAttachment);
    }
    if let Some(claimed) = claimed_content_hash(rel) {
        let actual = blake3::hash(bytes);
        let hex = actual.to_hex();
        if &hex[..16] != claimed.as_str() {
            return Err(RemoteReject::ContentMismatch);
        }
    }
    Ok(())
}

/// If `rel`'s basename is a content-addressed name `sha-<16 lowercase hex>.<ext>`,
/// return the 16-hex claim. Non-content-addressed names (user-imported
/// attachments) return `None` — there is nothing to verify against.
fn claimed_content_hash(rel: &str) -> Option<String> {
    let base = rel.rsplit('/').next()?;
    let stem = base.strip_prefix("sha-")?;
    let hex = stem.split('.').next()?;
    if hex.len() == 16 && hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        Some(hex.to_ascii_lowercase())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn np(s: &str) -> NotePath {
        NotePath(s.into())
    }

    #[test]
    fn accepts_ordinary_markdown_notes() {
        assert!(vet_remote_rel(&np("note.md")).is_ok());
        assert!(vet_remote_rel(&np("folder/note.md")).is_ok());
        assert!(vet_remote_rel(&np("a/b/c.markdown")).is_ok());
        assert!(vet_remote_rel(&np("资料/笔记.md")).is_ok());
    }

    #[test]
    fn rejects_reserved_and_hidden_dirs() {
        // The B1 payloads: overwrite git / corvux internals.
        assert_eq!(
            vet_remote_rel(&np(".git/config")),
            Err(RemoteReject::Hidden)
        );
        assert_eq!(
            vet_remote_rel(&np(".git/hooks/pre-commit")),
            Err(RemoteReject::Hidden)
        );
        assert_eq!(
            vet_remote_rel(&np(".corvux/vault-id.bin")),
            Err(RemoteReject::Hidden)
        );
        assert_eq!(
            vet_remote_rel(&np("notes/.corvux/x.md")),
            Err(RemoteReject::Hidden)
        );
        assert_eq!(
            vet_remote_rel(&np(".corvux-trash/old.md")),
            Err(RemoteReject::Hidden)
        );
        // Even a hidden *filename* — the scanner would ignore it, so it can't sync.
        assert_eq!(vet_remote_rel(&np(".hidden.md")), Err(RemoteReject::Hidden));
    }

    #[test]
    fn rejects_non_markdown() {
        assert_eq!(
            vet_remote_rel(&np("evil.sh")),
            Err(RemoteReject::NotMarkdown)
        );
        assert_eq!(
            vet_remote_rel(&np("folder/config.yaml")),
            Err(RemoteReject::NotMarkdown)
        );
        assert_eq!(vet_remote_rel(&np("noext")), Err(RemoteReject::NotMarkdown));
    }

    #[test]
    fn rejects_traversal_absolute_and_drive() {
        assert_eq!(vet_remote_rel(&np("../escape.md")), Err(RemoteReject::Path));
        assert_eq!(vet_remote_rel(&np("/abs.md")), Err(RemoteReject::Path));
        assert_eq!(vet_remote_rel(&np("a\\b.md")), Err(RemoteReject::Path));
        assert_eq!(vet_remote_rel(&np("")), Err(RemoteReject::Path));
        // Windows drive letter — is_safe_note_path misses this; validate_rel catches it.
        assert_eq!(vet_remote_rel(&np("C:/win.md")), Err(RemoteReject::Path));
    }

    #[test]
    fn blob_accepts_matching_content_address() {
        let bytes = b"hello attachment world";
        let hex = blake3::hash(bytes).to_hex();
        let rel = format!(".attachments/sha-{}.png", &hex[..16]);
        assert!(vet_remote_blob_rel(&rel, bytes).is_ok());
    }

    #[test]
    fn blob_accepts_non_content_addressed_name() {
        // User-imported attachment — nothing to verify against, allow.
        assert!(vet_remote_blob_rel(".attachments/my pic.webp", b"anything").is_ok());
    }

    #[test]
    fn blob_rejects_poisoned_content_address() {
        let hex = blake3::hash(b"the real bytes").to_hex();
        let rel = format!(".attachments/sha-{}.png", &hex[..16]);
        // Same name, different bytes → mismatch (B9 poisoning).
        assert_eq!(
            vet_remote_blob_rel(&rel, b"forged bytes"),
            Err(RemoteReject::ContentMismatch)
        );
    }

    #[test]
    fn blob_rejects_non_attachment_and_oversize() {
        assert_eq!(
            vet_remote_blob_rel("notes/x.png", b"x"),
            Err(RemoteReject::NotAttachment)
        );
        let big = vec![0u8; (crate::sync::protocol::SYNC_BLOB_MAX_BYTES + 1) as usize];
        assert_eq!(
            vet_remote_blob_rel(".attachments/big.bin", &big),
            Err(RemoteReject::TooLarge)
        );
    }
}
