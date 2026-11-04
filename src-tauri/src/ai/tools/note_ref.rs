//! Unified note-reference resolver.
//!
//! Tools accept user/agent-supplied "note references" in several shapes:
//! - `"daily"` shorthand → today's daily note (created if missing, when
//!   the caller is a write tool)
//! - Absolute or `\\?\`-prefixed (Windows UNC) path inside the roost
//! - Roost-relative path with or without the `.md` extension
//! - Bare title or `[[wiki-link]]` → looked up in CozoDB via `LinkResolver`
//! - Empty input + `open_note_path` → currently-open note (for tools whose
//!   default target is "the note the user is looking at")
//!
//! Before this module existed, every tool had its own resolver and they
//! disagreed on every dimension above. `tasks_crud` and `roost_edit`
//! supported `"daily"` shorthand; `roost_read` did not. `roost_read`
//! supported title lookup; `roost_edit` did not. Only `tasks_crud` (after
//! a recent fix) stripped the Windows UNC prefix before the
//! `starts_with(roost)` check, so `roost_edit` would fail with "path
//! escapes roost directory" on a path that was clearly inside the roost.
//! Only `roost_read` canonicalized for symlink safety; everyone else
//! relied on string-prefix matching.
//!
//! Production tools use one backend-agnostic entry point: [`resolve_rel`].
//! Its two policy flags control daily-note creation and open-note fallback;
//! title lookup is always attempted when a path does not exist. Returning a
//! roost-relative identity lets `RoostFs` enforce traversal and symlink policy
//! on native roots while supporting Android SAF tree URIs unchanged.

#[cfg(test)]
use std::path::{Path, PathBuf};

use tauri::{AppHandle, Manager};

use super::registry::{ToolContext, ToolError};
use crate::commands::files::{to_rel, RoostFsState};
use crate::commands::graph::GraphDb;
use crate::db::queries::LinkResolver;

/// Options controlling [`resolve`]'s behavior. Use one of the constructor
/// helpers (`for_read`, `for_write`, `for_task_op`) — direct construction
/// is fine but the helpers encode the per-tool policy in one place so
/// switching a tool's resolution mode is a one-line change.
#[derive(Copy, Clone)]
#[cfg(test)]
pub struct ResolveOpts<'a> {
    /// Absolute roost root.
    pub roost: &'a str,
    /// App handle, used for the graph-DB title lookup. Pass `None` only
    /// in tests where the DB isn't installed; production callers always
    /// have an `AppHandle` available.
    pub app: Option<&'a AppHandle>,
    /// Currently-open note path (if any). Only consulted when
    /// `allow_open_fallback = true` and `input` is empty.
    pub open_note_path: Option<&'a str>,
    /// If true, `"daily"` shorthand creates today's daily note (and its
    /// parent folder) when it doesn't exist. False = error if missing.
    pub allow_create_daily: bool,
    /// If true, empty input falls back to `open_note_path` when set.
    pub allow_open_fallback: bool,
}

/// Backend-agnostic resolver used by production tools. It returns a
/// roost-relative identity and delegates existence/path safety to `RoostFs`,
/// which keeps the same tool surface working for native and Android SAF roots.
pub async fn resolve_rel<R: tauri::Runtime + 'static>(
    input: &str,
    ctx: &ToolContext<R>,
    allow_create_daily: bool,
    allow_open_fallback: bool,
) -> Result<String, ToolError> {
    let roost = ctx.require_roost("note resolver")?;
    let state = ctx
        .app
        .try_state::<RoostFsState>()
        .ok_or_else(|| ToolError::fatal("roost filesystem unavailable"))?;
    let backend = state
        .current()
        .await
        .ok_or_else(|| ToolError::fatal("no roost open"))?;

    let mut value = strip_wiki_markers(input.trim()).to_string();
    if value.is_empty() && allow_open_fallback {
        value = ctx
            .open_note_path
            .as_deref()
            .map(strip_wiki_markers)
            .unwrap_or("")
            .to_string();
    }
    if value.is_empty() {
        return Err(ToolError::retryable(
            "missing note path — pass a roost-relative path, title, or 'daily'",
        ));
    }
    if value.eq_ignore_ascii_case("daily") {
        let folder = read_daily_note_folder_backend(backend.as_ref())
            .await
            .unwrap_or_else(|| "Daily Notes".to_string());
        let rel = format!(
            "{}/{}.md",
            folder.trim_matches('/'),
            chrono::Local::now().format("%Y-%m-%d")
        );
        if !allow_create_daily && !backend.exists(&rel).await.unwrap_or(false) {
            return Err(ToolError::retryable("today's daily note does not exist"));
        }
        return Ok(rel);
    }
    if value.contains("..") {
        return Err(ToolError::fatal("note path cannot contain '..'"));
    }

    let mut rel = to_rel(&value, roost).unwrap_or_else(|| {
        value
            .trim_start_matches(r"\\?\")
            .replace('\\', "/")
            .trim_start_matches('/')
            .to_string()
    });
    let looks_absolute = std::path::Path::new(&value).is_absolute()
        || value.starts_with("content://")
        || value.as_bytes().get(1) == Some(&b':');
    if looks_absolute && to_rel(&value, roost).is_none() {
        return Err(ToolError::fatal("path escapes roost directory"));
    }
    if rel.is_empty() {
        return Err(ToolError::retryable("note path resolves to the roost root"));
    }

    let has_extension = rel
        .rsplit('/')
        .next()
        .is_some_and(|name| name.contains('.'));
    let with_md = if has_extension {
        rel.clone()
    } else {
        format!("{rel}.md")
    };
    if backend.exists(&with_md).await.unwrap_or(false) {
        return Ok(with_md);
    }
    if backend.exists(&rel).await.unwrap_or(false) {
        return Ok(rel);
    }
    if let Some(title_rel) = try_title_lookup_rel(&value, &ctx.app) {
        if backend.exists(&title_rel).await.unwrap_or(false) {
            return Ok(title_rel);
        }
    }
    rel = with_md;
    Ok(rel)
}

pub async fn read_rel<R: tauri::Runtime + 'static>(
    ctx: &ToolContext<R>,
    rel: &str,
) -> Result<String, ToolError> {
    ctx.require_roost("note read")?;
    let state = ctx
        .app
        .try_state::<RoostFsState>()
        .ok_or_else(|| ToolError::fatal("roost filesystem unavailable"))?;
    let backend = state
        .current()
        .await
        .ok_or_else(|| ToolError::fatal("no roost open"))?;
    let bytes = backend
        .read(rel)
        .await
        .map_err(|e| ToolError::retryable(format!("read {rel}: {e}")))?;
    String::from_utf8(bytes).map_err(|e| ToolError::retryable(format!("{rel} is not UTF-8: {e}")))
}

pub fn display_join(roost: &str, rel: &str) -> String {
    format!(
        "{}/{}",
        roost.trim_end_matches('/'),
        rel.trim_start_matches('/')
    )
}

fn try_title_lookup_rel<R: tauri::Runtime>(title: &str, app: &AppHandle<R>) -> Option<String> {
    let graph = app.try_state::<GraphDb>()?;
    let guard = graph.db.lock().ok()?;
    let db = guard.as_ref()?;
    let resolver = LinkResolver::build(db).ok()?;
    let note_id = resolver.resolve(title, "")?;
    Some(format!("{note_id}.md"))
}

async fn read_daily_note_folder_backend(backend: &dyn crate::roostfs::RoostFs) -> Option<String> {
    for name in [
        ".corvux/roost-settings.json",
        ".corvux/vault-settings.json",
        ".corvux/settings.json",
    ] {
        let Ok(bytes) = backend.read(name).await else {
            continue;
        };
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
            continue;
        };
        if let Some(folder) = value
            .get("tasks")
            .and_then(|v| v.get("daily_note_folder"))
            .and_then(|v| v.as_str())
        {
            return Some(folder.to_string());
        }
    }
    None
}

/// Resolve a note reference to an absolute file path inside the roost.
///
/// On success, returns a path that is guaranteed to be inside the roost.
/// The file at that path may or may not exist — that's intentional, since
/// write tools call this BEFORE creating a new file.
#[cfg(test)]
pub fn resolve(input: &str, opts: ResolveOpts<'_>) -> Result<PathBuf, ToolError> {
    // Strip `[[wiki-link]]` markers and surrounding whitespace.
    let mut trimmed = strip_wiki_markers(input.trim()).to_string();

    // Empty input → open-note fallback (if allowed and set).
    if trimmed.is_empty() {
        if opts.allow_open_fallback {
            if let Some(open) = opts.open_note_path {
                let stripped = strip_wiki_markers(open.trim()).to_string();
                if !stripped.is_empty() {
                    trimmed = stripped;
                }
            }
        }
        if trimmed.is_empty() {
            return Err(ToolError::retryable(
                "missing 'note_path' — pass a roost-relative path, 'daily' shorthand, or a wiki-link title"
            ));
        }
    }

    // `"daily"` shorthand — case-insensitive on purpose so the LLM can
    // write either casing.
    if trimmed.eq_ignore_ascii_case("daily") {
        return resolve_daily(opts.roost, opts.allow_create_daily);
    }

    // Reject `..` traversal. Don't bother checking once vs always — any
    // occurrence is suspicious in a roost-relative path.
    if trimmed.contains("..") {
        return Err(ToolError::fatal("note_path cannot contain '..'"));
    }

    let candidate = Path::new(&trimmed);
    let abs = if candidate.is_absolute() {
        strip_unc_prefix(candidate.to_path_buf())
    } else {
        let mut p = PathBuf::from(opts.roost);
        p.push(&trimmed);
        p
    };
    let with_md = if abs.extension().is_none() {
        let mut p = abs.clone();
        p.set_extension("md");
        p
    } else {
        abs.clone()
    };

    // R-01 (2026-05-24 re-audit): pick the variant that actually exists
    // on disk. Each branch must distinguish three cases — Inside (ok),
    // Outside (HARD ERROR — symlink/junction escape), and NotFound
    // (fall through to the next candidate). The previous shape returned
    // `Option<PathBuf>` from a single helper, conflating Outside and
    // NotFound; a junction pointing outside the roost would canonicalize
    // outside, the helper returned None, and the function fell through
    // to a textual `starts_with` prefix check that approved the in-roost-
    // looking path. The caller then read the path with `std::fs` and the
    // OS followed the junction off-roost.
    match canonicalize_against_roost(&with_md, opts.roost) {
        CanonResult::Inside(p) => return Ok(p),
        CanonResult::Outside => {
            return Err(ToolError::fatal("path escapes roost directory"));
        }
        CanonResult::NotFound => {}
    }
    match canonicalize_against_roost(&abs, opts.roost) {
        CanonResult::Inside(p) => return Ok(p),
        CanonResult::Outside => {
            return Err(ToolError::fatal("path escapes roost directory"));
        }
        CanonResult::NotFound => {}
    }

    // Path didn't resolve on disk. Try DB title lookup so bare titles
    // (e.g. "2026-05-11" → "Daily Notes/2026-05-11.md") work the same
    // way they do in `roost_read`. Outside-roost title hits are
    // refused — a malicious DB row shouldn't escalate to a read.
    if let Some(app) = opts.app {
        if let Some(p) = try_title_lookup(&trimmed, opts.roost, app) {
            match canonicalize_against_roost(&p, opts.roost) {
                CanonResult::Inside(canon) => return Ok(canon),
                CanonResult::Outside => {
                    return Err(ToolError::fatal("path escapes roost directory"));
                }
                CanonResult::NotFound => {}
            }
        }
    }

    // Creation path — the file doesn't exist anywhere. Walk up to the
    // deepest existing ancestor, canonicalize that, reassemble the tail,
    // and verify the final assembled path is still inside the roost.
    // Mirrors `NativeFs::safe_resolve` so an in-roost junction whose
    // target is outside (e.g. `<roost>/linkdir/newfile.md`) cannot host
    // a "create new file" call that escapes the roost.
    safe_resolve_for_creation(&with_md, opts.roost)
}

/// Three-way classification of a candidate path:
/// - `Inside(canon)` — file exists and canonicalizes inside the roost
/// - `Outside` — file exists but canonicalizes outside the roost
///   (symlink / junction / reparse point pointing off-roost)
/// - `NotFound` — file does not exist; caller falls through
///
/// The `Outside` case must be propagated as an error by callers — it
/// is the symlink-escape signal the prior `Option`-returning shape
/// erased.
#[cfg(test)]
enum CanonResult {
    Inside(PathBuf),
    Outside,
    NotFound,
}

#[cfg(test)]
fn canonicalize_against_roost(path: &Path, roost: &str) -> CanonResult {
    let Ok(roost_canon) = std::fs::canonicalize(roost) else {
        // Roost root can't be canonicalized — pathological state, treat
        // as not-found rather than crashing so the caller's later checks
        // can fail in a controlled way.
        return CanonResult::NotFound;
    };
    let roost_canon = strip_unc_prefix(roost_canon);
    match std::fs::canonicalize(path) {
        Ok(c) => {
            let canon = strip_unc_prefix(c);
            if canon.starts_with(&roost_canon) {
                CanonResult::Inside(canon)
            } else {
                CanonResult::Outside
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => CanonResult::NotFound,
        // Other I/O errors (permission denied, etc.) — treat as not-found
        // so the creation-path fallback gets a chance to verify the
        // deepest existing ancestor.
        Err(_) => CanonResult::NotFound,
    }
}

/// R-01: creation-path safety. For a not-yet-existing file inside the
/// roost, walk up to the deepest existing ancestor, canonicalize that,
/// then reassemble the missing tail. The reassembled path must still
/// start with the canonical roost root — otherwise an in-roost
/// junction makes a create-here path land outside the roost.
#[cfg(test)]
fn safe_resolve_for_creation(target: &Path, roost: &str) -> Result<PathBuf, ToolError> {
    let Ok(roost_canon) = std::fs::canonicalize(roost) else {
        return Err(ToolError::fatal("roost root does not exist"));
    };
    let roost_canon = strip_unc_prefix(roost_canon);

    let mut probe = target.to_path_buf();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let canonical_existing = loop {
        match std::fs::canonicalize(&probe) {
            Ok(c) => break strip_unc_prefix(c),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let name = probe.file_name().map(|n| n.to_os_string());
                let parent = probe.parent().map(|p| p.to_path_buf());
                match (name, parent) {
                    (Some(n), Some(p)) if !p.as_os_str().is_empty() => {
                        tail.push(n);
                        probe = p;
                    }
                    _ => {
                        return Err(ToolError::fatal("path has no resolvable ancestor"));
                    }
                }
            }
            Err(_) => {
                return Err(ToolError::fatal("path escapes roost directory"));
            }
        }
    };

    let mut final_path = canonical_existing;
    for name in tail.iter().rev() {
        final_path.push(name);
    }

    if !final_path.starts_with(&roost_canon) {
        return Err(ToolError::fatal("path escapes roost directory"));
    }
    Ok(final_path)
}

fn strip_wiki_markers(s: &str) -> &str {
    s.trim_start_matches("[[").trim_end_matches("]]").trim()
}

/// Convert a path (absolute or roost-relative, with or without `.md`) into
/// the user-facing display form: roost-relative, forward slashes, no `.md`
/// extension. Used by [`sanitize_for_llm`] to rewrite tool results before
/// they re-enter the LLM context so the agent never quotes
/// `W:\Xidean-test-roost\Brain-Dump\Foo.md` back to the user — it sees
/// `Brain-Dump/Foo` instead and naturally refers to it that way.
///
/// Case-insensitive on the roost prefix so Windows drive-letter casing
/// (`W:` vs `w:`) doesn't defeat the strip.
pub fn display_path(input: &str, roost: &str) -> String {
    fn norm(s: &str) -> String {
        s.trim()
            .trim_start_matches(r"\\?\")
            .replace('\\', "/")
            .trim_end_matches('/')
            .to_string()
    }

    let path = norm(input);
    let v = norm(roost);

    let rel: &str = if path.len() > v.len()
        && path.as_bytes()[v.len()] == b'/'
        && path[..v.len()].eq_ignore_ascii_case(&v)
    {
        &path[v.len() + 1..]
    } else {
        path.as_str()
    };

    rel.trim_end_matches(".md")
        .trim_start_matches('/')
        .to_string()
}

/// Strip the absolute roost-root prefix from anywhere inside a free-text
/// string (e.g. a tool ERROR message that interpolated `path.display()` or an
/// `io::Error` carrying an absolute path), leaving the roost-relative
/// remainder. `sanitize_for_llm` only rewrites known path *keys* in a JSON
/// tree; error strings re-enter the LLM as a bare `error` value, so they need
/// this textual scrub to honor the "no absolute paths to the LLM" rule (which
/// otherwise leaks the user's drive layout + OS username).
///
/// ASCII-case-insensitive on the prefix (Windows drive casing) and matches
/// both `\` and `/` forms. Non-path content is preserved (aside from
/// backslashes becoming forward slashes, which is the friendlier form anyway).
pub fn scrub_roost_paths(message: &str, roost: Option<&str>) -> String {
    let Some(roost) = roost else {
        return message.to_string();
    };
    let root = roost.trim_start_matches(r"\\?\").replace('\\', "/");
    if root.is_empty() {
        return message.to_string();
    }
    let normalized = message.replace('\\', "/");
    let root_bytes = root.as_bytes();
    let nb = normalized.as_bytes();
    let mut out = String::with_capacity(normalized.len());
    let mut i = 0;
    while i < nb.len() {
        if i + root_bytes.len() <= nb.len()
            && nb[i..i + root_bytes.len()].eq_ignore_ascii_case(root_bytes)
        {
            i += root_bytes.len();
            if i < nb.len() && nb[i] == b'/' {
                i += 1; // drop the separator after the stripped root
            }
            continue;
        }
        // Copy one whole UTF-8 char so a multibyte title can't be split.
        let ch = normalized[i..].chars().next().unwrap();
        let len = ch.len_utf8();
        out.push_str(&normalized[i..i + len]);
        i += len;
    }
    out
}

/// Walk a tool-result JSON tree and rewrite string values under known
/// path keys (`path`, `file_path`, `note_path`) into [`display_path`]
/// form. Leaves every other key untouched — so user content, queries,
/// titles, etc. are preserved verbatim. Called from the agent loop after
/// the frontend ToolResult has captured the original absolute `file_path`
/// (frontend needs that for auto-open; LLM does not).
pub fn sanitize_for_llm(value: serde_json::Value, roost: Option<&str>) -> serde_json::Value {
    let Some(v) = roost else {
        return value;
    };
    walk(value, v)
}

fn walk(value: serde_json::Value, roost: &str) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let mut out = serde_json::Map::with_capacity(map.len());
            for (k, val) in map {
                let is_path_key = matches!(k.as_str(), "path" | "file_path" | "note_path");
                let new_val = if is_path_key {
                    if let Some(s) = val.as_str() {
                        serde_json::Value::String(display_path(s, roost))
                    } else {
                        walk(val, roost)
                    }
                } else {
                    walk(val, roost)
                };
                out.insert(k, new_val);
            }
            serde_json::Value::Object(out)
        }
        serde_json::Value::Array(arr) => {
            serde_json::Value::Array(arr.into_iter().map(|v| walk(v, roost)).collect())
        }
        other => other,
    }
}

/// Strip Windows extended-length-path prefix (`\\?\`). The roost root is
/// stored without it, but agent-supplied paths sometimes carry it; without
/// stripping, a `starts_with` check would silently fail on a path that's
/// clearly inside the roost.
#[cfg(test)]
fn strip_unc_prefix(p: PathBuf) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(stripped) = s.strip_prefix(r"\\?\") {
        PathBuf::from(stripped)
    } else {
        p
    }
}

// Old `canonical_inside_roost` removed in favour of
// `canonicalize_against_roost` (above) — the prior shape returned
// `Option<PathBuf>` and conflated "outside roost" with "doesn't
// exist," which was the root cause of R-01.

#[cfg(test)]
fn resolve_daily(roost: &str, allow_create: bool) -> Result<PathBuf, ToolError> {
    let folder = read_daily_note_folder(roost).unwrap_or_else(|| "Daily Notes".to_string());
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let mut path = PathBuf::from(roost);
    path.push(&folder);
    path.push(format!("{today}.md"));

    if path.exists() {
        return Ok(path);
    }
    if !allow_create {
        return Err(ToolError::retryable(format!(
            "today's daily note ({today}.md) doesn't exist yet in {folder}/ — create it first or pass a roost-relative path"
        )));
    }
    // AUDIT-033: file creation moved to async [`ensure_daily_note`] so the
    // bootstrap stub goes through RoostFs (sync notifications + atomic
    // write semantics). When `allow_create=true` we just return the
    // speculative path here; the caller is expected to invoke
    // `ensure_daily_note` first, or proceed straight to its own
    // RoostFs-backed write_atomic (which creates the file).
    Ok(path)
}

/// AUDIT-033: ensure today's daily note exists, creating a minimal
/// bootstrap stub through `RoostFs` when it doesn't. Mirrors what the
/// previous sync `resolve_daily` did via `std::fs::write` but now flows
/// through the same atomic-write + sync-notification chain as the
/// user-facing file commands. Callers should invoke this before
/// dispatching `resolve("daily", ...)` when they need the file to exist.
pub async fn ensure_daily_note<R: tauri::Runtime + 'static>(
    ctx: &super::registry::ToolContext<R>,
    _roost: &str,
) -> Result<String, ToolError> {
    ctx.require_roost("daily note")?;
    let state = ctx
        .app
        .try_state::<RoostFsState>()
        .ok_or_else(|| ToolError::fatal("roost filesystem unavailable"))?;
    let backend = state
        .current()
        .await
        .ok_or_else(|| ToolError::fatal("no roost open"))?;
    let folder = read_daily_note_folder_backend(backend.as_ref())
        .await
        .unwrap_or_else(|| "Daily Notes".to_string());
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let rel = format!("{}/{today}.md", folder.trim_end_matches('/'));
    if backend.exists(&rel).await.unwrap_or(false) {
        return Ok(rel);
    }
    let initial = format!("# {today}\n\n## Tasks\n\n");
    crate::commands::files::create_roost_note_synced(&ctx.app, &rel, &initial)
        .await
        .map_err(|e| ToolError::retryable(format!("create daily note: {e}")))?;
    Ok(rel)
}

#[cfg(test)]
fn read_daily_note_folder(roost: &str) -> Option<String> {
    // Filename is migrating with the vault->roost rename. Try the new name
    // first, then the name this reader historically used, then the bare
    // `settings.json` the frontend actually writes — so existing installs
    // keep resolving a custom daily-note folder during the staged rollout.
    let dir = Path::new(roost).join(".corvux");
    let text = [
        "roost-settings.json",
        "vault-settings.json",
        "settings.json",
    ]
    .into_iter()
    .find_map(|name| std::fs::read_to_string(dir.join(name)).ok())?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    v.get("tasks")?
        .get("daily_note_folder")?
        .as_str()
        .map(str::to_string)
}

/// Title → path lookup via the graph DB's `LinkResolver`. Reconstructs an
/// absolute path from the matched note id (which is the roost-relative,
/// lowercase, extension-less id like `daily notes/2026-05-11`). Returns
/// `None` when:
/// - the graph DB isn't installed (early startup, tests)
/// - the lock can't be acquired
/// - the title doesn't match any note
/// - the reconstructed path doesn't exist on disk (DB out of sync with FS)
#[cfg(test)]
fn try_title_lookup(title: &str, roost: &str, app: &AppHandle) -> Option<PathBuf> {
    let graph = app.try_state::<GraphDb>()?;
    let guard = graph.db.lock().ok()?;
    let db = guard.as_ref()?;
    let resolver = LinkResolver::build(db).ok()?;
    let note_id = resolver.resolve(title, "")?;
    let mut p = PathBuf::from(roost);
    for seg in note_id.split('/') {
        p.push(seg);
    }
    p.set_extension("md");
    if p.exists() {
        Some(p)
    } else {
        None
    }
}

#[cfg(test)]
mod path_scrub_tests {
    use super::scrub_roost_paths;

    #[test]
    fn strips_absolute_roost_prefix_from_error_text() {
        let roost = r"W:\Project_Technoo\Roost";
        let msg =
            r"note not found: 'W:\Project_Technoo\Roost\Brain-Dump\Foo.md'. Use roost_search.";
        let out = scrub_roost_paths(msg, Some(roost));
        assert!(
            !out.to_lowercase().contains("project_technoo/roost"),
            "absolute prefix must be gone: {out}"
        );
        assert!(
            out.contains("Brain-Dump/Foo.md"),
            "relative remainder kept: {out}"
        );
    }

    #[test]
    fn case_insensitive_drive_letter() {
        // Roost stored with `W:`, message produced with `w:` — must still strip.
        let out = scrub_roost_paths(r"at w:\roost\a\b.md now", Some(r"W:\Roost"));
        assert!(!out.to_lowercase().contains("roost/a/b") || out.contains("a/b.md"));
        assert!(out.contains("a/b.md"), "got: {out}");
    }

    #[test]
    fn noop_without_roost_or_match() {
        assert_eq!(
            scrub_roost_paths("plain message", Some(r"W:\Roost")),
            "plain message"
        );
        assert_eq!(scrub_roost_paths("anything", None), "anything");
    }
}

#[cfg(test)]
mod symlink_escape_tests {
    //! R-01 (2026-05-24 re-audit): the previous `resolve` returned an
    //! in-roost-looking path when an existing candidate canonicalized
    //! outside the roost, because the `Option`-returning helper
    //! conflated "outside roost" with "doesn't exist." The caller
    //! (`roost_read`, `note_outline`, `roost_edit`) then read the path
    //! with `std::fs`, and the OS followed the symlink/junction off
    //! the roost. These tests prove the new three-way classification
    //! rejects every shape of escape.
    //!
    //! Creating symlinks on Windows requires admin or developer mode,
    //! so the tests are unix-gated. CI runs on Linux (per
    //! `docs/dev/testing.md` Q-02). The defense itself runs on all
    //! platforms because it routes through `std::fs::canonicalize`,
    //! which follows reparse points on Windows just as it follows
    //! symlinks on Unix.
    use super::{resolve, ResolveOpts};

    fn read_opts<'a>(roost: &'a str) -> ResolveOpts<'a> {
        ResolveOpts {
            roost,
            app: None,
            open_note_path: None,
            allow_create_daily: false,
            allow_open_fallback: false,
        }
    }

    #[cfg(unix)]
    #[test]
    fn rejects_read_through_in_roost_symlink_to_outside_file() {
        use std::os::unix::fs::symlink;
        let roost = tempfile::tempdir().unwrap();
        // Plant a victim file outside the roost.
        let outside = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(outside.path(), "OUTSIDE_ROOST_SENTINEL").unwrap();
        // Place an in-roost symlink that targets it.
        let link = roost.path().join("leak.md");
        symlink(outside.path(), &link).expect("create symlink");

        let res = resolve("leak.md", read_opts(roost.path().to_str().unwrap()));
        assert!(
            res.is_err(),
            "in-roost symlink-to-outside must be refused, got Ok({res:?})"
        );
        let msg = format!("{:?}", res.err().unwrap());
        assert!(
            msg.contains("escapes roost"),
            "expected escape-roost error, got: {msg}"
        );
    }

    /// Ancestor-symlink shape: `<roost>/linkdir` is a directory symlink
    /// to a folder outside the roost. The previous `canonical_inside_roost`
    /// returned None (canonical form lived outside) and the function fell
    /// through to a textual `starts_with(roost)` check that approved the
    /// path. `std::fs::read_to_string(linkdir/secret.md)` then followed
    /// the symlink off-roost.
    #[cfg(unix)]
    #[test]
    fn rejects_read_through_in_roost_directory_symlink_to_outside_file() {
        use std::os::unix::fs::symlink;
        let roost = tempfile::tempdir().unwrap();
        let outside_dir = tempfile::tempdir().unwrap();
        std::fs::write(outside_dir.path().join("secret.md"), "OUTSIDE_DIR_SENTINEL").unwrap();
        symlink(outside_dir.path(), roost.path().join("linkdir")).expect("create dir symlink");

        let res = resolve(
            "linkdir/secret.md",
            read_opts(roost.path().to_str().unwrap()),
        );
        assert!(
            res.is_err(),
            "linkdir traversal must be refused, got Ok({res:?})"
        );
    }

    /// Creation-path safety: the file doesn't exist (yet), but a
    /// directory symlink in its ancestry points outside the roost. A
    /// write tool calling `resolve` on `linkdir/newfile.md` would have
    /// returned a path that, when written, creates the file in the
    /// off-roost target. The new `safe_resolve_for_creation` path
    /// canonicalizes the deepest existing ancestor and refuses.
    #[cfg(unix)]
    #[test]
    fn rejects_creation_path_through_in_roost_directory_symlink() {
        use std::os::unix::fs::symlink;
        let roost = tempfile::tempdir().unwrap();
        let outside_dir = tempfile::tempdir().unwrap();
        symlink(outside_dir.path(), roost.path().join("linkdir")).expect("create dir symlink");

        let res = resolve(
            "linkdir/newfile.md",
            read_opts(roost.path().to_str().unwrap()),
        );
        assert!(
            res.is_err(),
            "creating under a symlinked-out directory must be refused, got Ok({res:?})"
        );
    }

    /// Sanity: a normal in-roost relative path that doesn't exist yet
    /// should still resolve successfully (write tools rely on this to
    /// create new notes).
    #[test]
    fn accepts_in_roost_creation_path() {
        let roost = tempfile::tempdir().unwrap();
        let res = resolve(
            "subdir/newnote.md",
            read_opts(roost.path().to_str().unwrap()),
        )
        .expect("create-path inside roost must resolve");
        let res_str = res.to_string_lossy().to_string();
        assert!(
            res_str.contains("subdir") && res_str.ends_with("newnote.md"),
            "expected resolved creation path, got: {res_str}"
        );
    }

    /// Sanity: an existing in-roost file resolves to its canonical
    /// absolute path.
    #[test]
    fn accepts_existing_in_roost_file() {
        let roost = tempfile::tempdir().unwrap();
        std::fs::write(roost.path().join("a.md"), "hi").unwrap();
        let res = resolve("a.md", read_opts(roost.path().to_str().unwrap())).expect("must resolve");
        assert!(res.ends_with("a.md"));
    }
}
