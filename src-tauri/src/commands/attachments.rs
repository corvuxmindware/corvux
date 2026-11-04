//! Image paste / load flow. Images are base64-shuttled over the IPC
//! boundary (Tauri's JSON transport is unreliable for binaries), stored
//! under `<roost>/.attachments/`, and referenced from markdown as
//! `![alt](.attachments/filename)`. Desktop and SAF both share the same
//! surface; the NativeFs / SafFs backend handles write-atomicity.

use std::collections::HashSet;

use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use serde::Serialize;
use tauri::State;

use super::files::RoostFsState;

/// ATT-1: derive a content-addressed attachment filename, `sha-<hex>.<ext>`.
/// blake3 of the bytes (first 16 hex chars = 64 bits — collision-safe for a
/// per-roost attachment set, short enough to stay a readable filename).
/// Content addressing means pasting the same image twice yields the same
/// name, so the caller can dedup instead of writing `paste-{ms}` twice and
/// bloating the roost + sync traffic.
fn content_addressed_name(bytes: &[u8], ext: &str) -> String {
    let hash = blake3::hash(bytes);
    let hex = hash.to_hex();
    format!("sha-{}.{ext}", &hex[..16])
}

/// S-06: hard cap on attachment size (read AND save). Tauri's JSON
/// IPC transport is already inefficient for binaries; a multi-hundred-MB
/// attachment would also blow up the renderer heap as a base64 string
/// (~1.33× the raw size). 25 MB is generous for screenshots, PDFs,
/// and pasted images while still bounding the worst case. Tightening
/// later is easier than loosening, so start conservative.
const ATTACHMENT_MAX_BYTES: u64 = 25 * 1024 * 1024;
const ATTACHMENT_MAX_BASE64_BYTES: usize = (ATTACHMENT_MAX_BYTES as usize).div_ceil(3) * 4;

/// Returns `Err` with a human-readable message when `len` exceeds the
/// cap. Pure — extracted so the cap can be unit-tested without spinning
/// up a RoostFs backend.
fn check_attachment_size(len: usize, op: &str) -> Result<(), String> {
    if (len as u64) > ATTACHMENT_MAX_BYTES {
        Err(format!(
            "attachment too large to {op}: {} bytes > {} byte cap (25 MB)",
            len, ATTACHMENT_MAX_BYTES
        ))
    } else {
        Ok(())
    }
}

fn check_encoded_attachment_size(len: usize) -> Result<(), String> {
    if len > ATTACHMENT_MAX_BASE64_BYTES {
        Err(format!(
            "attachment too large to save: encoded payload exceeds {} bytes (25 MB decoded cap)",
            ATTACHMENT_MAX_BASE64_BYTES
        ))
    } else {
        Ok(())
    }
}

/// AUDIT-080: cap on extension length + character set. The extension flows
/// into a filename built by `format!`, so a slash, dot-dot, or NUL would
/// let a caller escape `.attachments/`. Anything that doesn't match this
/// shape is replaced with `"bin"` (callers always pass a real ext, this
/// is purely a hard floor for hostile IPC input).
fn sanitize_extension(raw: &str) -> &'static str {
    fn is_safe(ext: &str) -> bool {
        !ext.is_empty() && ext.len() <= 8 && ext.chars().all(|c| c.is_ascii_alphanumeric())
    }
    // Static table — avoids allocating per call and gives an exhaustive
    // allowlist of formats we actually paste/save. Audio formats added for
    // the voice-memo feature: webm/opus from Android Chrome WebView,
    // m4a/aac from iOS WKWebView, plus the legacy formats users may drop in.
    const ALLOWED: &[&str] = &[
        "png", "jpg", "jpeg", "gif", "webp", "bmp", "svg", "pdf", "heic", "heif", "avif", "webm",
        "opus", "m4a", "mp4", "mp3", "wav", "ogg", "aac",
    ];
    if !is_safe(raw) {
        return "bin";
    }
    let lower = raw.to_ascii_lowercase();
    ALLOWED
        .iter()
        .find(|a| **a == lower.as_str())
        .copied()
        .unwrap_or("bin")
}

/// Save a pasted image to `{roost}/.attachments/sha-{hash}.{ext}`.
/// Returns the filename (not full path) for use in
/// `![alt](.attachments/filename)` markdown image syntax.
///
/// ATT-1: content-addressed. Identical bytes hash to the same name, so a
/// re-paste of the same image reuses the existing file instead of writing a
/// second copy — dedup that keeps the roost and sync traffic lean.
#[tauri::command]
#[specta::specta]
pub async fn save_attachment(
    _roost_path: String,
    data: String,
    extension: String,
    state: State<'_, RoostFsState>,
) -> Result<String, String> {
    // Bound the renderer-controlled string before allocating the decoded
    // Vec. Checking only `bytes.len()` after decode allowed a huge base64
    // request to exhaust backend memory before the 25 MB policy ran.
    check_encoded_attachment_size(data.len())?;
    let bytes = B64
        .decode(data.as_bytes())
        .map_err(|e| format!("Invalid base64 payload: {e}"))?;
    // S-06: reject oversized payloads before they hit disk. base64
    // decoding above already runs in memory but is cheap compared to
    // sustained writes — surfacing the cap as a user-readable error
    // beats silently truncating or running the box out of RAM.
    check_attachment_size(bytes.len(), "save")?;
    // AUDIT-080: never trust the caller-supplied extension verbatim — it
    // would otherwise become path-traversal once spliced into the filename.
    let ext = sanitize_extension(&extension);
    // ATT-1: name by content hash so the same image can't be stored twice.
    let filename = content_addressed_name(&bytes, ext);
    let rel = format!(".attachments/{filename}");

    // AUDIT-080: removed the no-backend `std::fs` fallback. It used the
    // caller-supplied `roost_path` directly, which bypassed the active
    // roost scope (any IPC caller could write anywhere on disk).
    let backend = state
        .current()
        .await
        .ok_or_else(|| "No roost is open — cannot save attachment".to_string())?;
    if let Err(e) = backend.mkdir(".attachments").await {
        if !matches!(e, crate::roostfs::RoostError::AlreadyExists(_)) {
            return Err(format!("Failed to create .attachments dir: {e}"));
        }
    }
    // ATT-1: dedup — if a blob with this exact content already exists, reuse
    // it. Content addressing guarantees same name ⇒ same bytes, so this is a
    // safe skip, not a clobber. (Errors from the existence probe are
    // non-fatal: fall through to the write, which is idempotent for
    // identical content anyway.)
    if backend.exists(&rel).await.unwrap_or(false) {
        return Ok(filename);
    }
    backend
        .write_atomic(&rel, &bytes)
        .await
        .map_err(|e| format!("Failed to save attachment: {e}"))?;
    Ok(filename)
}

/// Read a roost-relative attachment and return its bytes as a base64 string.
/// The frontend wraps the result in `data:image/...;base64,...` so it can be
/// embedded directly in the editor without the asset protocol (which needs
/// per-path capabilities and doesn't work in `pnpm dev` browser mode).
#[tauri::command]
#[specta::specta]
pub async fn read_attachment(
    _roost_path: String,
    relative_path: String,
    state: State<'_, RoostFsState>,
) -> Result<String, String> {
    let rel = relative_path.trim_start_matches("./");
    // AUDIT-080: removed the no-backend `std::fs` fallback. Like the save
    // path, it used caller-supplied `roost_path` directly and so wasn't
    // scoped to the active roost. The mounted backend (NativeFs / SafFs)
    // is the only legitimate reader.
    let backend = state
        .current()
        .await
        .ok_or_else(|| "No roost is open — cannot read attachment".to_string())?;
    let bytes = backend
        .read(rel)
        .await
        .map_err(|e| format!("Attachment not found ({relative_path}): {e}"))?;
    // S-06: same cap on the read path. An attachment that grew past
    // the cap (e.g. saved before this cap existed, or copied in from
    // outside) must not be base64-shuttled to the renderer wholesale.
    check_attachment_size(bytes.len(), "read")?;
    Ok(B64.encode(&bytes))
}

/// ATT-4: report of attachments no note references. The analog of
/// `find_broken_links` for the `.attachments/` folder — deleting a note
/// orphans its embedded images, and nothing else ever cleans them up, so
/// `.attachments/` grows forever. This surfaces the orphans for the user to
/// review; it never deletes (matching `find_broken_links`, which also only
/// reports — the user decides).
#[derive(Debug, Serialize, specta::Type)]
pub struct OrphanAttachmentReport {
    /// Roost-relative `.attachments/<name>` paths referenced by no note.
    pub orphans: Vec<String>,
    /// Total files scanned in `.attachments/` (for the UI summary line).
    #[specta(type = u32)]
    pub total_attachments: usize,
    /// Markdown notes scanned for references.
    #[specta(type = u32)]
    pub notes_scanned: usize,
}

/// Local markdown predicate. (A shared `is_markdown` helper lands with the
/// EDT-3 save-hardening work; inlined here to keep this change self-contained.)
fn is_markdown_note(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.ends_with(".md") || lower.ends_with(".markdown")
}

/// Extract the attachment filenames a note references. Anchors on the
/// `.attachments/` prefix (both `![alt](.attachments/x.png)` and
/// `![[.attachments/x.png]]` render that way) and reads to the first
/// terminator, mirroring `sync::engine::peer::parse_attachment_refs`.
/// Returns basenames (the part after `.attachments/`), deduped.
fn extract_attachment_names(md: &str) -> Vec<String> {
    const MARK: &str = ".attachments/";
    let mut out: Vec<String> = Vec::new();
    let mut from = 0;
    while let Some(idx) = md[from..].find(MARK) {
        let start = from + idx + MARK.len();
        let mut end = md.len();
        for (i, c) in md[start..].char_indices() {
            if matches!(
                c,
                ')' | ']' | '"' | '\'' | '|' | '#' | '\n' | '\r' | '\t' | '<' | '>'
            ) {
                end = start + i;
                break;
            }
        }
        from = end.max(start);
        let name = md[start..end].trim();
        if !name.is_empty() && !name.contains('/') && !out.iter().any(|n| n == name) {
            out.push(name.to_string());
        }
    }
    out
}

/// ATT-4: find attachments in `.attachments/` that no note references.
/// Report only — never deletes. The frontend shows the list so the user can
/// decide (same contract as `find_broken_links`).
#[tauri::command]
#[specta::specta]
pub async fn find_orphan_attachments(
    state: State<'_, RoostFsState>,
) -> Result<OrphanAttachmentReport, String> {
    let backend = state
        .current()
        .await
        .ok_or_else(|| "No roost is open".to_string())?;

    // Files currently in .attachments/. A missing folder = no attachments.
    let att_files: Vec<String> = match backend.list(".attachments").await {
        Ok(entries) => entries
            .into_iter()
            .filter(|e| !e.is_dir)
            .map(|e| e.name)
            .collect(),
        Err(crate::roostfs::RoostError::NotFound(_)) => {
            return Ok(OrphanAttachmentReport {
                orphans: Vec::new(),
                total_attachments: 0,
                notes_scanned: 0,
            });
        }
        Err(e) => return Err(format!("Failed to list .attachments: {e}")),
    };

    // Every attachment name any note references.
    let notes = backend
        .list_recursive("")
        .await
        .map_err(|e| format!("Failed to walk roost: {e}"))?;
    let mut referenced: HashSet<String> = HashSet::new();
    let mut notes_scanned = 0usize;
    for entry in notes {
        if entry.is_dir || !is_markdown_note(&entry.name) {
            continue;
        }
        notes_scanned += 1;
        // A note we can't read/decode is skipped for reference-collection —
        // but we must NOT then report its attachments as orphans on that
        // basis. Since orphan = "referenced by NO note", a skipped note only
        // risks a false orphan if it was the sole referencer. The report is
        // advisory (no auto-delete), so this is acceptable; note it.
        if let Ok(bytes) = backend.read(&entry.rel_path).await {
            if let Ok(md) = String::from_utf8(bytes) {
                for name in extract_attachment_names(&md) {
                    referenced.insert(name);
                }
            }
        }
    }

    let orphans: Vec<String> = att_files
        .iter()
        .filter(|name| !referenced.contains(*name))
        .map(|name| format!(".attachments/{name}"))
        .collect();

    Ok(OrphanAttachmentReport {
        total_attachments: att_files.len(),
        notes_scanned,
        orphans,
    })
}

#[cfg(test)]
mod tests {
    use super::{check_encoded_attachment_size, sanitize_extension, ATTACHMENT_MAX_BASE64_BYTES};

    #[test]
    fn oversized_base64_is_rejected_before_decode() {
        assert!(check_encoded_attachment_size(ATTACHMENT_MAX_BASE64_BYTES).is_ok());
        assert!(check_encoded_attachment_size(ATTACHMENT_MAX_BASE64_BYTES + 1).is_err());
    }

    /// AUDIT-080: extension comes from the frontend IPC layer and was
    /// previously interpolated straight into the filename. A path
    /// separator or `..` would let a caller escape `.attachments/`.
    #[test]
    fn sanitize_extension_strips_path_traversal() {
        assert_eq!(sanitize_extension("png/../../../etc/foo"), "bin");
        assert_eq!(sanitize_extension("../escape"), "bin");
        assert_eq!(sanitize_extension("png/sub"), "bin");
        assert_eq!(sanitize_extension(".."), "bin");
    }

    #[test]
    fn sanitize_extension_rejects_special_chars() {
        for bad in ["", "png ", "p<g", "png\0", "p\\g", "p:g", "\nng", "p\tg"] {
            assert_eq!(
                sanitize_extension(bad),
                "bin",
                "non-alphanumeric extension `{bad}` must collapse to bin"
            );
        }
    }

    #[test]
    fn sanitize_extension_caps_length() {
        // 9 chars — over our 8-char cap.
        assert_eq!(sanitize_extension("toolongext"), "bin");
        // Exactly at the cap is fine if it's in the allowlist; "12345678"
        // isn't, so still collapses to bin.
        assert_eq!(sanitize_extension("12345678"), "bin");
    }

    #[test]
    fn sanitize_extension_preserves_known_formats() {
        for good in ["png", "jpg", "jpeg", "gif", "webp", "svg", "pdf", "heic"] {
            assert_eq!(
                sanitize_extension(good),
                good,
                "allowlisted extension `{good}` must pass through verbatim"
            );
        }
    }

    #[test]
    fn sanitize_extension_allows_audio_formats() {
        for good in ["webm", "opus", "m4a", "mp4", "mp3", "wav", "ogg", "aac"] {
            assert_eq!(
                sanitize_extension(good),
                good,
                "audio extension `{good}` must pass through for voice memos"
            );
        }
    }

    #[test]
    fn sanitize_extension_normalizes_case() {
        assert_eq!(sanitize_extension("PNG"), "png");
        assert_eq!(sanitize_extension("JPEG"), "jpeg");
        assert_eq!(sanitize_extension("Webp"), "webp");
    }

    /// S-06: oversized payloads must fail fast with a readable error,
    /// not silently truncate or run the box out of RAM.
    #[test]
    fn attachment_size_cap_rejects_oversize() {
        use super::{check_attachment_size, ATTACHMENT_MAX_BYTES};
        let oversize = (ATTACHMENT_MAX_BYTES + 1) as usize;
        let err = check_attachment_size(oversize, "save").unwrap_err();
        assert!(
            err.contains("too large"),
            "error message should explain why: {err}"
        );
        assert!(
            err.contains("25 MB"),
            "error message should include the cap: {err}"
        );
    }

    /// S-06: legitimately-sized attachments must pass through. The
    /// boundary value (exactly at the cap) is fine — only `>` is
    /// rejected.
    #[test]
    fn attachment_size_cap_allows_legitimate_sizes() {
        use super::{check_attachment_size, ATTACHMENT_MAX_BYTES};
        assert!(check_attachment_size(0, "save").is_ok());
        assert!(check_attachment_size(1024, "save").is_ok()); // 1 KB
        assert!(check_attachment_size(ATTACHMENT_MAX_BYTES as usize, "save").is_ok());
    }

    // ── ATT-1: content-addressed dedup ───────────────────────────────────

    #[test]
    fn content_addressed_name_is_stable_and_content_derived() {
        use super::content_addressed_name;
        let a = content_addressed_name(b"the same image bytes", "png");
        let b = content_addressed_name(b"the same image bytes", "png");
        assert_eq!(a, b, "identical bytes must yield identical names (dedup)");
        assert!(a.starts_with("sha-") && a.ends_with(".png"));
        // Different content ⇒ different name.
        let c = content_addressed_name(b"different bytes", "png");
        assert_ne!(a, c);
        // Same bytes, different ext ⇒ ext honored.
        assert!(content_addressed_name(b"x", "webp").ends_with(".webp"));
    }

    // ── ATT-4: orphan reference extraction ───────────────────────────────

    #[test]
    fn extract_attachment_names_handles_both_markdown_forms() {
        use super::extract_attachment_names;
        let md = "\
            intro ![shot](.attachments/sha-abc123.png) middle\n\
            embed ![[.attachments/paste-999.webp]] end\n\
            audio ![rec](.attachments/recording-1.webm)\n";
        let mut names = extract_attachment_names(md);
        names.sort();
        assert_eq!(
            names,
            vec![
                "paste-999.webp".to_string(),
                "recording-1.webm".to_string(),
                "sha-abc123.png".to_string(),
            ]
        );
    }

    #[test]
    fn extract_attachment_names_dedups_and_ignores_traversal() {
        use super::extract_attachment_names;
        let md = "![a](.attachments/x.png) again ![b](.attachments/x.png) \
                  bad ![c](.attachments/sub/y.png)";
        let names = extract_attachment_names(md);
        // Deduped, and the nested-path form (contains `/`) is not treated as
        // a direct attachment name.
        assert_eq!(names, vec!["x.png".to_string()]);
    }

    #[test]
    fn extract_attachment_names_empty_when_no_refs() {
        use super::extract_attachment_names;
        assert!(extract_attachment_names("a plain note with no images").is_empty());
    }
}
