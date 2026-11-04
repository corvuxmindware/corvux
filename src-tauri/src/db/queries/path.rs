//! Path-to-id helpers shared across indexing and link resolution. Kept
//! string-pure (no DB access) so they stay testable without a Cozo
//! fixture.

/// Canonical note id: roost-relative path, lowercased, forward-slash
/// separators, file extension stripped.
///
/// Example: roost=`C:/Notes`, abs=`C:/Notes/Fit-ME/2026-W10/5th-mar-26/puli-fitness.md`
/// → `fit-me/2026-w10/5th-mar-26/puli-fitness`.
///
/// This matches Obsidian's model: every file has a unique identity based
/// on its location in the roost, not its basename. Without this, daily
/// notes that share a basename (user has 20+ `puli-fitness.md` files
/// under `Fit-ME/<week>/<day>/`) collapse into a single row.
///
/// If `abs_path` isn't under `roost_root` we fall back to the whole
/// normalized path so the function stays total — callers never handle
/// `Option`.
///
/// IP-7: the id is the ONE identity a note has across indexing, link
/// resolution, backlinks, sync, and the graph view. If two code paths mint
/// two different ids for the same file, that file ends up as two nodes (a
/// ghost that never heals). Three normalizations keep the id stable no
/// matter which layer computes it, mirroring `commands::files::to_rel`:
///   * **NFC** — macOS/SAF hand back decomposed (NFD) paths; the folder
///     picker and Windows hand back precomposed (NFC). Same file, byte-
///     distinct strings, so normalize before comparing/stripping.
///   * **case-insensitive prefix strip** — a drive-letter or folder-case
///     difference between `roost_root` and `abs_path` (`C:/Notes` vs
///     `c:/notes/a.md`) used to defeat the exact `strip_prefix`, so the
///     WHOLE absolute path leaked into the id. The id is lowercased anyway,
///     so we lowercase both sides for the strip too.
///   * **device/long-path prefix strip** — `\\?\C:\x` must equal `C:\x`.
///
/// The prefix strip is segment-boundary-aware: `roost` must not match
/// `roost-archive`.
pub fn note_id_from_abs_path(abs_path: &str, roost_root: &str) -> String {
    use unicode_normalization::UnicodeNormalization;

    fn normalize(s: &str) -> String {
        let s: String = s.nfc().collect::<String>().replace('\\', "/");
        // Strip Windows device/long-path namespaces so they don't defeat
        // the prefix match. Only these four forms exist in the wild.
        let s = s
            .strip_prefix("//?/")
            .or_else(|| s.strip_prefix("//./"))
            .map(str::to_string)
            .unwrap_or(s);
        s.to_lowercase()
    }

    let abs_n = normalize(abs_path);
    let root_n = normalize(roost_root);
    let root_n = root_n.trim_end_matches('/');

    // Segment-boundary-aware, case-insensitive strip. `rel` is the roost-
    // relative remainder when `abs` is genuinely under `root`; otherwise we
    // keep the whole normalized path (a not-under-root fallback that
    // shouldn't happen in practice).
    let rel = match abs_n.strip_prefix(root_n) {
        Some(r) if root_n.is_empty() || r.is_empty() || r.starts_with('/') => {
            r.trim_start_matches('/')
        }
        _ => abs_n.trim_start_matches('/'),
    };

    // Strip only the final extension (`.md`, `.markdown`) — never a dot in a
    // directory name. Operate segment-wise so `2026.notes/a.md` keeps its dir.
    match rel.rsplit_once('/') {
        Some((dir, file)) => {
            let stem = file.rsplit_once('.').map(|(s, _)| s).unwrap_or(file);
            if dir.is_empty() {
                stem.to_string()
            } else {
                format!("{dir}/{stem}")
            }
        }
        None => rel
            .rsplit_once('.')
            .map(|(s, _)| s)
            .unwrap_or(rel)
            .to_string(),
    }
}

/// Roost-relative directory of a note (forward slashes, no trailing `/`).
/// Used as the anchor for nearest-folder wiki-link resolution.
pub fn note_rel_dir(abs_path: &str, roost_root: &str) -> String {
    let abs = std::path::Path::new(abs_path);
    let root = std::path::Path::new(roost_root);
    let rel = abs.strip_prefix(root).unwrap_or(abs);
    let rel_dir = rel.parent().unwrap_or(std::path::Path::new(""));
    rel_dir
        .to_string_lossy()
        .replace('\\', "/")
        .trim_start_matches('/')
        .to_lowercase()
}

/// Pretty display title — file stem with original casing preserved.
pub(super) fn title_from_path(path: &str) -> String {
    std::path::Path::new(path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(path)
        .to_string()
}

/// Count matching leading path segments between a candidate note id's
/// folder portion and the linking note's folder. Higher = closer in the
/// roost tree.
pub(super) fn shared_dir_prefix(candidate_id: &str, source_rel_dir: &str) -> usize {
    let candidate_dir = candidate_id.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
    let a = candidate_dir.split('/').filter(|s| !s.is_empty());
    let b = source_rel_dir.split('/').filter(|s| !s.is_empty());
    a.zip(b).take_while(|(x, y)| x == y).count()
}

#[cfg(test)]
mod id_tests {
    use super::note_id_from_abs_path;

    #[test]
    fn basic_relative_lowercased_no_ext() {
        assert_eq!(
            note_id_from_abs_path("C:/Notes/Fit-ME/2026-W10/Puli.md", "C:/Notes"),
            "fit-me/2026-w10/puli"
        );
    }

    #[test]
    fn backslash_paths_normalize_like_forward_slash() {
        assert_eq!(
            note_id_from_abs_path(r"C:\Notes\A\B.md", r"C:\Notes"),
            "a/b"
        );
    }

    #[test]
    fn markdown_extension_is_stripped() {
        assert_eq!(note_id_from_abs_path("/r/note.markdown", "/r"), "note");
    }

    #[test]
    fn dot_in_directory_name_is_preserved() {
        // Only the final file extension is stripped, never a dot in a folder.
        assert_eq!(
            note_id_from_abs_path("/r/2026.notes/a.md", "/r"),
            "2026.notes/a"
        );
    }

    #[test]
    fn case_mismatch_between_root_and_abs_yields_one_identity() {
        // IP-7: a drive/folder-case difference used to defeat the exact
        // strip_prefix and leak the whole path into the id, minting a SECOND
        // node identity for the same file. Both forms must now collapse to
        // the same relative id.
        let a = note_id_from_abs_path("C:/Notes/A.md", "C:/Notes");
        let b = note_id_from_abs_path("c:/notes/a.md", "C:/Notes");
        assert_eq!(a, "a");
        assert_eq!(a, b);
    }

    #[test]
    fn segment_boundary_is_respected() {
        // `/roost` must NOT be treated as a prefix of `/roost-archive`.
        // Not under root → whole (normalized) path is the id, so it can't
        // collide with a real note under the roost.
        assert_eq!(
            note_id_from_abs_path("/roost-archive/a.md", "/roost"),
            "roost-archive/a"
        );
    }

    #[test]
    fn windows_long_path_prefix_matches_plain_root() {
        // `\\?\C:\...` and `C:\...` are the same file; the device-namespace
        // prefix must not mint a divergent id.
        let plain = note_id_from_abs_path("C:/Notes/a.md", "C:/Notes");
        let devns = note_id_from_abs_path(r"\\?\C:\Notes\a.md", "C:/Notes");
        assert_eq!(plain, "a");
        assert_eq!(plain, devns);
    }

    #[test]
    fn nfd_and_nfc_paths_collapse_to_one_id() {
        // macOS/SAF hand back decomposed (NFD) unicode; the folder picker
        // hands back precomposed (NFC). Same file → must be one id.
        let nfd = note_id_from_abs_path("/r/cafe\u{0301}.md", "/r"); // e + combining acute
        let nfc = note_id_from_abs_path("/r/caf\u{00e9}.md", "/r"); // é
        assert_eq!(nfd, nfc);
        assert_eq!(nfc, "caf\u{00e9}");
    }
}
