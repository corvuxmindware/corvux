//! Roost access: settings, safe path resolution, atomic writes, and a
//! recursive markdown scan.
//!
//! The server deliberately does NOT take the app's exclusive
//! `<roost>/.corvux/.lock`. The whole point is to run while Corvux is open,
//! and that lock is held for the app's whole session. Instead we write
//! atomically (temp file + rename, see `atomic_write`) so the running app's
//! file watcher only ever sees a complete file, then re-indexes it.
//! `write_atomic_checked` additionally refuses to commit if the destination
//! changed since the caller last read it (mirroring the main app's own
//! `EDT-1` save-conflict guard), so a concurrent editor/MCP-process write
//! is reported as a conflict instead of silently discarded — see
//! `server.rs`'s `checked_read_modify_write`.

use std::fs;
use std::path::{Component, Path, PathBuf};

use crate::atomic_write::{self, WriteError};

/// Cap on note content accepted by a single write — keeps a pathological or
/// adversarial `content` argument from costing unbounded memory/disk before
/// any I/O happens. Generous for real markdown; the largest legitimate
/// roost notes are nowhere near this.
pub const MAX_NOTE_BYTES: usize = 10 * 1024 * 1024;
const MAX_SCANNED_NOTES: usize = 100_000;

/// Roost-level config read from `<roost>/.corvux/settings.json`, with the
/// same defaults the app falls back to.
#[derive(Debug, Clone)]
pub struct Settings {
    pub daily_note_folder: String,
    pub inbox_path: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            daily_note_folder: "Daily Notes".to_string(),
            inbox_path: "Inbox.md".to_string(),
        }
    }
}

pub struct Roost {
    pub root: PathBuf,
    /// Symlink-resolved roost root, cached at open. Every resolved path is
    /// checked to live under this (see `resolve`) so an in-roost symlink
    /// that escapes (`<roost>/link -> /etc`) can't be read/written through.
    canonical_root: PathBuf,
    pub settings: Settings,
}

impl Roost {
    /// Open a roost at `root`. Errors only if the path isn't a directory;
    /// the `.corvux/` marker check is deferred to per-tool calls so the
    /// server still starts (and can report a helpful error) on a bad path.
    pub fn open(root: PathBuf) -> Result<Self, String> {
        if !root.is_dir() {
            return Err(format!("Roost path is not a directory: {}", root.display()));
        }
        // Resolve the root once so the containment check in `resolve` has a
        // stable, symlink-free anchor to compare against.
        let canonical_root = fs::canonicalize(&root)
            .map_err(|e| format!("Failed to resolve roost root {}: {e}", root.display()))?;
        let settings = Self::load_settings(&root);
        Ok(Self {
            root,
            canonical_root,
            settings,
        })
    }

    /// A real Corvux roost has a `.corvux/` directory. Used to refuse
    /// writing notes into an arbitrary folder.
    pub fn is_corvux_roost(&self) -> bool {
        self.root.join(".corvux").is_dir()
    }

    fn load_settings(root: &Path) -> Settings {
        // Settings file lives at `<roost>/.corvux/<name>-settings.json`. The
        // filename is migrating with the vault→roost rename: the app will
        // write `roost-settings.json` once renamed, but until that pass ships
        // it still writes `vault-settings.json` — and existing installs have
        // that file on disk. Earlier builds of THIS server read a bare
        // `settings.json`. Try them in order so a custom daily-note folder
        // configured in the app reaches MCP captures regardless of which
        // build wrote it. Drop the older fallbacks once the app rename has
        // shipped and 0.1.x is no longer in the wild.
        let dir = root.join(".corvux");
        // `settings.json` is the file the current app writes, therefore it is
        // authoritative whenever present. The rename candidates are fallback
        // compatibility inputs only; preferring them allowed an abandoned
        // migration artifact to route new captures into a stale folder.
        for name in [
            "settings.json",
            "roost-settings.json",
            "vault-settings.json",
        ] {
            if let Some(s) = Self::parse_settings(&dir.join(name)) {
                return s;
            }
        }
        Settings::default()
    }

    /// Parse a `.corvux/*-settings.json` file into [`Settings`], applying
    /// the `tasks.daily_note_folder` / `tasks.inbox_path` overrides over the
    /// defaults. Returns `None` when the file is absent or unreadable so the
    /// caller can fall through to the next candidate filename.
    fn parse_settings(path: &Path) -> Option<Settings> {
        let raw = fs::read_to_string(path).ok()?;
        let v = serde_json::from_str::<serde_json::Value>(&raw).ok()?;
        let mut s = Settings::default();
        if let Some(tasks) = v.get("tasks") {
            if let Some(f) = tasks.get("daily_note_folder").and_then(|x| x.as_str()) {
                if !f.trim().is_empty() {
                    s.daily_note_folder = f.to_string();
                }
            }
            if let Some(i) = tasks.get("inbox_path").and_then(|x| x.as_str()) {
                if !i.trim().is_empty() {
                    s.inbox_path = i.to_string();
                }
            }
        }
        Some(s)
    }

    /// Resolve a roost-relative path, rejecting absolute paths and any `..`
    /// traversal so a tool argument can never escape the roost. Normalizes
    /// `\` to `/` so Windows-style inputs work too.
    ///
    /// The textual checks below only catch *lexical* escapes. A symlink
    /// planted inside the roost that points outside (`<roost>/link -> /etc`,
    /// then `read("link/passwd")`) has no `..` in the argument and slips
    /// past them — so we additionally resolve symlinks and verify the
    /// result stays inside the roost (`contain_within_roost`). This mirrors
    /// the app's `roostfs::native` AUDIT-076 hardening.
    pub fn resolve(&self, rel: &str) -> Result<PathBuf, String> {
        let normalized = rel.replace('\\', "/");
        let p = Path::new(&normalized);
        if p.is_absolute() {
            return Err(format!("Absolute paths are not allowed: {rel}"));
        }
        let mut out = PathBuf::new();
        for comp in p.components() {
            match comp {
                Component::Normal(c) => out.push(c),
                Component::CurDir => {}
                Component::ParentDir => {
                    return Err(format!("Path traversal (`..`) is not allowed: {rel}"))
                }
                Component::RootDir | Component::Prefix(_) => {
                    return Err(format!("Absolute paths are not allowed: {rel}"))
                }
            }
        }
        if out.as_os_str().is_empty() {
            return Err("Empty path".to_string());
        }
        self.contain_within_roost(self.root.join(out), rel)
    }

    /// Resolve symlinks at every *existing* ancestor of `target` and verify
    /// the result still lives under `canonical_root`. For not-yet-created
    /// paths (writes/creates) we canonicalize the deepest existing ancestor
    /// and re-append the missing tail, so write targets are covered too. A
    /// component that exists as a symlink but can't be canonicalized is a
    /// *dangling* symlink — we fail closed rather than treat its name as a
    /// plain creatable file (which would let `note.md -> /outside/x` slip
    /// through before its target exists).
    ///
    /// Residual (documented, not closed): this check and the read/write
    /// syscall that follows it are two separate operations, not one — a
    /// symlink swapped into place in the gap between them would slip
    /// through. `read`/`check_writable` re-verify with a cheap
    /// `symlink_metadata` check immediately before the syscall to shrink
    /// that window, but full closure would need per-platform `openat`-style
    /// directory-descent APIs (Linux-only `openat2(RESOLVE_NO_SYMLINKS)`, no
    /// clean Windows equivalent) — disproportionate given the threat model
    /// requires an attacker who can already plant filesystem symlinks
    /// winning a sub-millisecond race against a specific tool call.
    fn contain_within_roost(&self, target: PathBuf, rel: &str) -> Result<PathBuf, String> {
        let mut probe = target;
        let mut tail: Vec<std::ffi::OsString> = Vec::new();
        let canonical_existing = loop {
            match fs::canonicalize(&probe) {
                Ok(c) => break c,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    if fs::symlink_metadata(&probe)
                        .map(|m| m.file_type().is_symlink())
                        .unwrap_or(false)
                    {
                        return Err(format!("Path component is a dangling symlink: {rel}"));
                    }
                    let name = probe.file_name().map(|n| n.to_os_string());
                    let parent = probe.parent().map(|p| p.to_path_buf());
                    match (name, parent) {
                        (Some(n), Some(p)) if !p.as_os_str().is_empty() => {
                            tail.push(n);
                            probe = p;
                        }
                        _ => return Err(format!("Path has no resolvable ancestor: {rel}")),
                    }
                }
                Err(e) => return Err(format!("Failed to resolve {rel}: {e}")),
            }
        };
        let mut final_path = canonical_existing;
        for name in tail.iter().rev() {
            final_path.push(name);
        }
        if !final_path.starts_with(&self.canonical_root) {
            return Err(format!("Path resolves outside the roost: {rel}"));
        }
        Ok(final_path)
    }

    pub fn exists(&self, rel: &str) -> bool {
        self.resolve(rel).map(|p| p.exists()).unwrap_or(false)
    }

    /// Read a user-visible note. MCP tools must never use this to cross into
    /// `.corvux`, another hidden directory, or a non-Markdown file.
    pub fn read(&self, rel: &str) -> Result<String, String> {
        if !is_user_markdown_rel(rel) {
            return Err(format!(
                "Only non-hidden Markdown notes can be read through MCP: {rel}"
            ));
        }
        self.read_resolved(rel)
    }

    /// Read a Corvux-owned file for a narrowly-scoped internal operation such
    /// as loading the daily-note template. Never call this with an MCP tool
    /// argument.
    pub(crate) fn read_internal(&self, rel: &str) -> Result<String, String> {
        self.read_resolved(rel)
    }

    fn read_resolved(&self, rel: &str) -> Result<String, String> {
        let p = self.resolve(rel)?;
        // Shrinks (does not close) the TOCTOU between `resolve`'s
        // containment check and this read: re-verify the final component
        // isn't a symlink right before opening it. See `contain_within_roost`
        // for why this can't be a full close without per-platform
        // `openat`-style APIs.
        if is_symlink(&p) {
            return Err(format!("Path resolves to a symlink, refusing read: {rel}"));
        }
        let len = fs::metadata(&p)
            .map_err(|e| format!("Failed to inspect {rel}: {e}"))?
            .len();
        if len > MAX_NOTE_BYTES as u64 {
            return Err(format!(
                "Note is too large to read through MCP ({len} bytes, max {MAX_NOTE_BYTES}): {rel}"
            ));
        }
        fs::read_to_string(&p).map_err(|e| format!("Failed to read {rel}: {e}"))
    }

    /// Resolve `rel` for a write: reserved-path guard, size cap, containment,
    /// and the same symlink re-check `read` does. Shared by `write_atomic`
    /// and `write_atomic_checked` so both get identical guards.
    fn check_writable(&self, rel: &str, content: &str) -> Result<PathBuf, String> {
        if content.len() > MAX_NOTE_BYTES {
            return Err(format!(
                "Content too large ({} bytes, max {MAX_NOTE_BYTES}): {rel}",
                content.len()
            ));
        }
        // Never create or overwrite hidden / Corvux-owned entries
        // (.corvux, .corvux-trash, .attachments, .stignore, .git, …). Any
        // path component starting with `.` is off-limits for writes.
        if rel
            .replace('\\', "/")
            .split('/')
            .any(|c| c.starts_with('.') && c != "." && c != "..")
        {
            return Err(format!(
                "Refusing to write to a reserved/hidden path: {rel}"
            ));
        }
        let dest = self.resolve(rel)?;
        if is_symlink(&dest) {
            return Err(format!("Path resolves to a symlink, refusing write: {rel}"));
        }
        Ok(dest)
    }

    /// Write `content` to `rel` atomically: create parent dirs, write a
    /// sibling temp file, then rename over the destination. The running app
    /// only ever sees a whole file. No conflict check — use
    /// `write_atomic_checked` when the caller needs to know the file didn't
    /// change since it was last read.
    pub fn write_atomic(&self, rel: &str, content: &str) -> Result<(), String> {
        let dest = self.check_writable(rel, content)?;
        atomic_write::write_atomic(&dest, content.as_bytes())
    }

    /// Like `write_atomic`, but refuses to commit if the destination's
    /// content doesn't match `expected_before` (`None` = must not already
    /// exist) immediately before the rename. See
    /// `atomic_write::write_atomic_checked` for exactly what this does and
    /// doesn't guarantee.
    pub fn write_atomic_checked(
        &self,
        rel: &str,
        expected_before: Option<&str>,
        content: &str,
    ) -> Result<(), WriteError> {
        let dest = self.check_writable(rel, content).map_err(WriteError::Io)?;
        atomic_write::write_atomic_checked(&dest, expected_before, content.as_bytes())
    }

    /// All markdown files in the roost as `(roost-relative path, full path)`,
    /// skipping dot-directories (`.corvux`, `.corvux-trash`, `.attachments`,
    /// `.git`, …), dotfiles, and — explicitly, not just by the accidental
    /// consequence of `is_file()` below plus walkdir's default
    /// `follow_links: false` — anything reached through a symlink. A
    /// symlinked note or folder inside the roost must never be indexed,
    /// searched, or graphed as if it were roost content.
    pub fn scan_md(&self) -> Vec<(String, PathBuf)> {
        let mut out = Vec::new();
        let walker = walkdir::WalkDir::new(&self.root)
            .into_iter()
            .filter_entry(|e| !is_hidden(e) && !is_symlink_not_root(e));
        for entry in walker.flatten() {
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            if !is_markdown_path(path) {
                continue;
            }
            if let Ok(rel) = path.strip_prefix(&self.root) {
                out.push((rel.to_string_lossy().replace('\\', "/"), path.to_path_buf()));
                if out.len() >= MAX_SCANNED_NOTES {
                    break;
                }
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }
}

/// Skip anything whose name starts with `.` — but never the walk root
/// itself (its name may legitimately be a normal folder we were pointed at).
fn is_hidden(entry: &walkdir::DirEntry) -> bool {
    if entry.depth() == 0 {
        return false;
    }
    entry
        .file_name()
        .to_str()
        .map(|s| s.starts_with('.'))
        .unwrap_or(false)
}

/// Skip a symlinked entry — file or directory — anywhere but the walk root
/// (a symlinked ROOST ROOT is a normal, harmless setup; a symlink somewhere
/// *inside* the roost is not something scan_md should ever follow or index).
/// Also prunes descent into a symlinked directory, independent of whatever
/// `WalkDir::follow_links` is set to.
fn is_symlink_not_root(entry: &walkdir::DirEntry) -> bool {
    entry.depth() != 0 && entry.path_is_symlink()
}

/// Whether `p`'s final path component is itself a symlink (does not follow
/// it). Used to shrink the resolve-to-syscall TOCTOU window right before a
/// read or write.
fn is_symlink(p: &Path) -> bool {
    fs::symlink_metadata(p)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
}

pub(crate) fn is_markdown_path(path: &Path) -> bool {
    path.file_name()
        .map(|n| {
            let lower = n.to_string_lossy().to_ascii_lowercase();
            lower.ends_with(".md") || lower.ends_with(".markdown")
        })
        .unwrap_or(false)
}

pub(crate) fn strip_markdown_suffix(value: &str) -> &str {
    let lower = value.to_ascii_lowercase();
    if lower.ends_with(".markdown") {
        &value[..value.len() - ".markdown".len()]
    } else if lower.ends_with(".md") {
        &value[..value.len() - ".md".len()]
    } else {
        value
    }
}

pub(crate) fn is_user_markdown_rel(rel: &str) -> bool {
    let normalized = rel.replace('\\', "/");
    let path = Path::new(&normalized);
    if !is_markdown_path(path) {
        return false;
    }
    path.components().all(|component| match component {
        Component::Normal(name) => !name.to_string_lossy().starts_with('.'),
        Component::CurDir => true,
        _ => false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn roost_with_corvux() -> (TempDir, Roost) {
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join(".corvux")).unwrap();
        let v = Roost::open(dir.path().to_path_buf()).unwrap();
        (dir, v)
    }

    #[test]
    fn rejects_traversal_and_absolute() {
        let (_d, v) = roost_with_corvux();
        assert!(v.resolve("../escape.md").is_err());
        assert!(v.resolve("a/../../escape.md").is_err());
        assert!(v.resolve("/etc/passwd").is_err());
        assert!(v.resolve("notes/ok.md").is_ok());
    }

    #[test]
    fn atomic_write_then_read_roundtrips() {
        let (_d, v) = roost_with_corvux();
        v.write_atomic("Daily Notes/2026-06-01.md", "hello")
            .unwrap();
        assert_eq!(v.read("Daily Notes/2026-06-01.md").unwrap(), "hello");
        // No temp file left behind.
        let leftovers: Vec<_> = fs::read_dir(v.root.join("Daily Notes"))
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty());
    }

    #[test]
    fn settings_defaults_and_overrides() {
        let (_d, v) = roost_with_corvux();
        assert_eq!(v.settings.daily_note_folder, "Daily Notes");
        assert_eq!(v.settings.inbox_path, "Inbox.md");

        // The app's filename — must be read.
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join(".corvux")).unwrap();
        fs::write(
            dir.path().join(".corvux").join("roost-settings.json"),
            r#"{"version":1,"tasks":{"daily_note_folder":"Journal","inbox_path":"00 Inbox.md"}}"#,
        )
        .unwrap();
        let v2 = Roost::open(dir.path().to_path_buf()).unwrap();
        assert_eq!(v2.settings.daily_note_folder, "Journal");
        assert_eq!(v2.settings.inbox_path, "00 Inbox.md");
    }

    #[test]
    fn settings_current_filename_is_read() {
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join(".corvux")).unwrap();
        fs::write(
            dir.path().join(".corvux").join("settings.json"),
            r#"{"tasks":{"daily_note_folder":"Legacy"}}"#,
        )
        .unwrap();
        let v = Roost::open(dir.path().to_path_buf()).unwrap();
        assert_eq!(v.settings.daily_note_folder, "Legacy");
    }

    #[test]
    fn settings_current_filename_wins_over_rename_candidate() {
        // The file written by the current app is authoritative when a stale
        // rename candidate is also present.
        let dir = TempDir::new().unwrap();
        let corvux = dir.path().join(".corvux");
        fs::create_dir_all(&corvux).unwrap();
        fs::write(
            corvux.join("roost-settings.json"),
            r#"{"tasks":{"daily_note_folder":"FromRenameCandidate"}}"#,
        )
        .unwrap();
        fs::write(
            corvux.join("settings.json"),
            r#"{"tasks":{"daily_note_folder":"FromCurrentApp"}}"#,
        )
        .unwrap();
        let v = Roost::open(dir.path().to_path_buf()).unwrap();
        assert_eq!(v.settings.daily_note_folder, "FromCurrentApp");
    }

    #[test]
    fn settings_pre_rename_vault_filename_still_read() {
        // Staging window: until the app rename ships, the app still writes
        // `vault-settings.json` and existing installs have it on disk. It
        // must be read when `roost-settings.json` is absent.
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join(".corvux")).unwrap();
        fs::write(
            dir.path().join(".corvux").join("vault-settings.json"),
            r#"{"tasks":{"daily_note_folder":"FromVaultFile"}}"#,
        )
        .unwrap();
        let v = Roost::open(dir.path().to_path_buf()).unwrap();
        assert_eq!(v.settings.daily_note_folder, "FromVaultFile");
    }

    #[test]
    fn settings_roost_filename_wins_over_pre_rename_vault() {
        // When both exist, the new `roost-settings.json` is authoritative.
        let dir = TempDir::new().unwrap();
        let corvux = dir.path().join(".corvux");
        fs::create_dir_all(&corvux).unwrap();
        fs::write(
            corvux.join("roost-settings.json"),
            r#"{"tasks":{"daily_note_folder":"FromRoost"}}"#,
        )
        .unwrap();
        fs::write(
            corvux.join("vault-settings.json"),
            r#"{"tasks":{"daily_note_folder":"FromVault"}}"#,
        )
        .unwrap();
        let v = Roost::open(dir.path().to_path_buf()).unwrap();
        assert_eq!(v.settings.daily_note_folder, "FromRoost");
    }

    #[test]
    fn scan_skips_dot_dirs() {
        let (_d, v) = roost_with_corvux();
        v.write_atomic("note.md", "a").unwrap();
        v.write_atomic("sub/deep.md", "b").unwrap();
        // A markdown file inside .corvux must be ignored.
        fs::write(v.root.join(".corvux").join("internal.md"), "x").unwrap();
        let found: Vec<String> = v.scan_md().into_iter().map(|(r, _)| r).collect();
        assert_eq!(
            found,
            vec!["note.md".to_string(), "sub/deep.md".to_string()]
        );
    }

    #[test]
    fn user_reads_reject_reserved_and_non_markdown_paths() {
        let (_d, v) = roost_with_corvux();
        fs::write(v.root.join(".corvux/settings.json"), "secret").unwrap();
        fs::write(v.root.join("plain.json"), "secret").unwrap();
        assert!(v.read(".corvux/settings.json").is_err());
        assert!(v.read("plain.json").is_err());
        assert_eq!(v.read_internal(".corvux/settings.json").unwrap(), "secret");
    }

    #[test]
    fn scan_accepts_all_app_markdown_extensions_case_insensitively() {
        let (_d, v) = roost_with_corvux();
        fs::write(v.root.join("a.MD"), "a").unwrap();
        fs::write(v.root.join("b.markdown"), "b").unwrap();
        fs::write(v.root.join("c.Markdown"), "c").unwrap();
        let found: Vec<_> = v.scan_md().into_iter().map(|(rel, _)| rel).collect();
        assert_eq!(found, vec!["a.MD", "b.markdown", "c.Markdown"]);
    }

    // AUDIT-076 (port): the textual `..`/absolute rejection misses in-roost
    // symlinks that point outside. These exercise the symlink-resolution
    // containment in `contain_within_roost`. `#[cfg(unix)]` because creating
    // symlinks on Windows needs elevated privileges; CI runs on Linux.

    #[cfg(unix)]
    #[test]
    fn read_through_symlink_rejected() {
        use std::os::unix::fs::symlink;
        let (d, v) = roost_with_corvux();
        // A victim file OUTSIDE the roost, with an in-roost symlink to it.
        // Using a test-created sibling (not a host path like /etc) keeps it
        // cross-platform across the unix family.
        let outside = d.path().parent().unwrap().join("mcp-symlink-victim.txt");
        fs::write(&outside, b"victim").unwrap();
        symlink(&outside, d.path().join("leak")).unwrap();
        assert!(
            v.read("leak").is_err(),
            "read through an escaping symlink must be blocked"
        );
        let _ = fs::remove_file(&outside);
    }

    #[cfg(unix)]
    #[test]
    fn read_through_ancestor_symlink_rejected() {
        use std::os::unix::fs::symlink;
        let (d, v) = roost_with_corvux();
        // A directory symlink inside the roost pointing outside — reading a
        // child through it (`linkdir/secret.txt`) has no `..` in the arg.
        let outside = d.path().parent().unwrap().join("mcp-outside-dir");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("secret.txt"), b"victim").unwrap();
        symlink(&outside, d.path().join("linkdir")).unwrap();
        assert!(
            v.read("linkdir/secret.txt").is_err(),
            "read through an ancestor symlink must be blocked"
        );
        let _ = fs::remove_dir_all(&outside);
    }

    #[cfg(unix)]
    #[test]
    fn write_through_symlink_does_not_escape() {
        use std::os::unix::fs::symlink;
        let (d, v) = roost_with_corvux();
        let outside = d.path().parent().unwrap().join("mcp-write-escape-target");
        let _ = fs::remove_file(&outside);
        symlink(&outside, d.path().join("attach.md")).unwrap();
        assert!(
            v.write_atomic("attach.md", "data").is_err(),
            "write through an escaping symlink must be blocked"
        );
        assert!(
            !outside.exists(),
            "write must not have escaped the roost via the symlink"
        );
    }

    // ── Cluster A: checked writes ───────────────────────────────────────

    #[test]
    fn write_atomic_checked_conflicts_when_disk_diverged() {
        let (_d, v) = roost_with_corvux();
        v.write_atomic("note.md", "a").unwrap();
        v.write_atomic("note.md", "x").unwrap(); // simulates an external writer
        let err = v
            .write_atomic_checked("note.md", Some("a"), "b")
            .unwrap_err();
        assert!(matches!(err, WriteError::Conflict));
        assert_eq!(v.read("note.md").unwrap(), "x"); // untouched
    }

    #[test]
    fn write_atomic_checked_create_only_conflicts_if_now_exists() {
        let (_d, v) = roost_with_corvux();
        v.write_atomic("note.md", "already here").unwrap();
        let err = v.write_atomic_checked("note.md", None, "new").unwrap_err();
        assert!(matches!(err, WriteError::Conflict));
    }

    #[test]
    fn write_atomic_checked_proceeds_when_base_matches() {
        let (_d, v) = roost_with_corvux();
        v.write_atomic("note.md", "a").unwrap();
        v.write_atomic_checked("note.md", Some("a"), "b").unwrap();
        assert_eq!(v.read("note.md").unwrap(), "b");
    }

    #[test]
    fn write_atomic_rejects_content_over_max_size() {
        let (_d, v) = roost_with_corvux();
        let huge = "x".repeat(MAX_NOTE_BYTES + 1);
        assert!(v.write_atomic("note.md", &huge).is_err());
        assert!(!v.exists("note.md"));
    }

    #[cfg(unix)]
    #[test]
    fn write_atomic_preserves_permissions_on_overwrite() {
        use std::os::unix::fs::PermissionsExt;
        let (d, v) = roost_with_corvux();
        v.write_atomic("note.md", "a").unwrap();
        let path = d.path().join("note.md");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        v.write_atomic("note.md", "b").unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn concurrent_checked_writes_one_wins_one_conflicts() {
        use std::sync::{Arc, Barrier};
        let (_d, v) = roost_with_corvux();
        v.write_atomic("note.md", "base").unwrap();
        let v = Arc::new(v);
        let barrier = Arc::new(Barrier::new(2));
        let mut handles = Vec::new();
        for content in ["writer-a", "writer-b"] {
            let v = Arc::clone(&v);
            let barrier = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                v.write_atomic_checked("note.md", Some("base"), content)
            }));
        }
        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let oks = results.iter().filter(|r| r.is_ok()).count();
        assert_eq!(
            oks, 1,
            "exactly one concurrent checked write should succeed"
        );
        let final_content = v.read("note.md").unwrap();
        assert!(final_content == "writer-a" || final_content == "writer-b");
    }

    // ── Cluster B: symlinks must never be indexed ───────────────────────

    #[cfg(unix)]
    #[test]
    fn scan_md_excludes_symlinked_files_and_dirs() {
        use std::os::unix::fs::symlink;
        let (d, v) = roost_with_corvux();
        v.write_atomic("real.md", "kept").unwrap();

        let outside_file = d.path().parent().unwrap().join("scan-symlink-file.md");
        fs::write(&outside_file, "leak").unwrap();
        symlink(&outside_file, d.path().join("linked.md")).unwrap();

        let outside_dir = d.path().parent().unwrap().join("scan-symlink-dir");
        fs::create_dir_all(&outside_dir).unwrap();
        fs::write(outside_dir.join("secret.md"), "leak2").unwrap();
        symlink(&outside_dir, d.path().join("linkdir")).unwrap();

        let found: Vec<String> = v.scan_md().into_iter().map(|(r, _)| r).collect();
        assert_eq!(
            found,
            vec!["real.md".to_string()],
            "symlinked file and symlinked-dir contents must both be excluded"
        );

        let _ = fs::remove_file(&outside_file);
        let _ = fs::remove_dir_all(&outside_dir);
    }
}
