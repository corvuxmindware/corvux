//! Checkbox-task parser. Scans markdown for `- [ ]` / `- [x]` lines and
//! extracts the Obsidian-Tasks-compatible emoji metadata documented in
//! `Corvux/ROOST-CONVENTIONS.md`. Pure regex-style parsing — no LLM call.
//!
//! Slice 1 of Phase 2.1. Output rows feed `:task` in CozoDB; the Tasks pane
//! (slice 2) reads them. The parser is also exposed as a `#[tauri::command]`
//! so the frontend can preview-parse without round-tripping through the
//! indexer.

use serde::Serialize;

/// One checkbox task extracted from a markdown source.
///
/// Field semantics:
/// - `source_line`: 1-based line number in the body for inline checkboxes;
///   negative virtual line numbers (-1, -2, ...) for tasks derived from
///   the frontmatter `tasks:` array. Negative values are unique per
///   frontmatter task so the (note_id, source_line) composite key in
///   `:task` doesn't collide. Stable when external edits move the line —
///   see `content_anchor`.
/// - `content_anchor`: first ~30 chars of the task text, used as a
///   fallback identity when a line shifts (agent rewrite, vim insert, etc.)
/// - `due_at` / `completed_at`: empty string means "not set." We use empty
///   strings rather than `Option<String>` because the CozoDB columns are
///   `String` (no nullable column types in the existing schema).
/// - `linked_person` / `linked_project`: derived from inline `[[wikilinks]]`
///   and `#project/X` tags in the task text. Just the *first* match each;
///   the full set lives in tags/links from the existing pass-1 indexer.
#[derive(Debug, Serialize, Clone, PartialEq, Eq, specta::Type)]
pub struct ParsedTask {
    pub source_line: i32,
    pub text: String,
    pub content_anchor: String,
    pub completed: bool,
    pub due_at: String,
    pub completed_at: String,
    pub linked_person: String,
    pub linked_project: String,
    pub tags: Vec<String>,
    pub wikilinks: Vec<String>,
}

/// Extract every checkbox-task line from `content`. Skips fenced code blocks
/// and the YAML frontmatter region. Indentation is allowed (sub-tasks render
/// the same way in MVP — nesting is V2).
///
/// Frontmatter `tasks:` array is parsed separately by [`parse_frontmatter_tasks`]
/// and merged here for the public command.
#[tauri::command]
#[specta::specta]
pub fn parse_tasks(content: String) -> Vec<ParsedTask> {
    // The command is a frontend preview with no note-path context: resolve
    // "today" from the local clock so lone-⏰ checkboxes preview correctly,
    // and let the note's own frontmatter `date:` supply any inherited date.
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    parse_tasks_in_note(&content, None, Some(&today))
}

/// Core task parse (convention 0.3). A body checkbox is a task ONLY when it
/// has an *effective date*; an undated checkbox in an undated note is
/// document structure (a checklist), not a to-do, and yields nothing.
///
/// Effective date precedence, per line:
/// 1. an inline `📅 YYYY-MM-DD`;
/// 2. else the note's own date — `note_date` (a daily note's filename stem)
///    or the note's frontmatter `date:` (frontmatter wins over `note_date`);
/// 3. else, for a lone `⏰` (time, no date anywhere), `today`.
///
/// A `⏰` time attaches to whichever effective date applies, so
/// `- [ ] standup ⏰ 09:00` in a daily note becomes that day at 09:00, and in
/// any other note becomes `today` at 09:00 (a "remind me today" line).
///
/// `today` is threaded in (not read from the clock) so the parse stays pure
/// and the shared golden-corpus contract with `corvux-mcp` is deterministic.
pub fn parse_tasks_in_note(
    content: &str,
    note_date: Option<&str>,
    today: Option<&str>,
) -> Vec<ParsedTask> {
    let mut out = Vec::new();

    // Note-level opt-out: a top-level `tasks: off` (also false/none/no)
    // property means "this note's checkboxes are document structure, not
    // to-dos." Skip the whole note — body checkboxes AND the frontmatter
    // `tasks:` array alike. Checked first so a single chokepoint covers
    // the Tasks pane, the notification scheduler, and the agent tools at
    // once (they all read the `:task` rows the indexer derives from here).
    if tasks_disabled(content) {
        return out;
    }

    // The note's effective date for inheritance: an explicit frontmatter
    // `date:` wins, else the caller-supplied note date (daily-note stem).
    let fm_date = frontmatter_date(content);
    let note_eff_date: Option<&str> = fm_date.as_deref().or(note_date);

    let frontmatter_end = frontmatter_end_offset(content);

    out.extend(parse_frontmatter_tasks(content));

    let mut in_code_block = false;
    let mut line_no: i32 = 0;
    for line in content.lines() {
        line_no += 1;
        let line_start = line.as_ptr() as usize - content.as_ptr() as usize;
        if line_start < frontmatter_end {
            continue;
        }

        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_code_block = !in_code_block;
            continue;
        }
        if in_code_block {
            continue;
        }

        if let Some(task) = parse_checkbox_line(trimmed, line_no, note_eff_date, today) {
            out.push(task);
        }
    }

    out
}

/// Try to interpret a single (already-trim_start'd) line as a checkbox task.
/// Returns None if the line isn't a `- [ ]` / `- [x]` checkbox, OR if it is a
/// checkbox but has no effective date (convention 0.3 — see
/// [`parse_tasks_in_note`] for the date-precedence rule).
fn parse_checkbox_line(
    line: &str,
    line_no: i32,
    note_eff_date: Option<&str>,
    today: Option<&str>,
) -> Option<ParsedTask> {
    // Match `- [ ]` / `- [x]` / `- [X]` / `* [ ]` / `+ [ ]` after optional
    // bullet markers. Obsidian-Tasks accepts `*` and `+` too.
    let after_bullet = line
        .strip_prefix("- ")
        .or_else(|| line.strip_prefix("* "))
        .or_else(|| line.strip_prefix("+ "))?;

    let (state, rest) = if let Some(r) = after_bullet.strip_prefix("[ ] ") {
        (false, r)
    } else if let Some(r) = after_bullet.strip_prefix("[x] ") {
        (true, r)
    } else if let Some(r) = after_bullet.strip_prefix("[X] ") {
        (true, r)
    } else if after_bullet == "[ ]" || after_bullet == "[x]" || after_bullet == "[X]" {
        // Empty task — checkbox with no text. Skip; nothing useful to track.
        return None;
    } else {
        return None;
    };

    let (text, due_date, due_time, completed_at) = strip_emoji_metadata(rest);
    if text.is_empty() {
        return None;
    }

    // A checkbox is a task when it has a date CONTEXT: an inline 📅, a time ⏰,
    // or simply living in a dated note (daily-note filename / frontmatter
    // `date:`). Otherwise it's a plain checklist item, not a task.
    let is_task = !due_date.is_empty() || !due_time.is_empty() || note_eff_date.is_some();
    if !is_task {
        return None;
    }

    // `due_at` carries a DUE date only from explicit per-line signals: an
    // inline 📅 (+ optional ⏰), or a lone ⏰ which resolves to today. A
    // checkbox that is a task *only* because the note carries a `date:` gets
    // NO due date — it's visible but never "overdue" and never fires a
    // notification. The note's `date:` is the note's date, not a deadline.
    let due_at = if !due_date.is_empty() {
        if due_time.is_empty() {
            due_date
        } else {
            format!("{due_date} {due_time}")
        }
    } else if !due_time.is_empty() {
        match today {
            Some(t) => format!("{t} {due_time}"),
            None => String::new(),
        }
    } else {
        String::new()
    };

    let tags = extract_inline_tags(&text);
    let wikilinks = extract_wikilinks(&text);
    let linked_project = tags
        .iter()
        .find(|t| t.starts_with("project/"))
        .cloned()
        .unwrap_or_default();
    let linked_person = wikilinks.first().cloned().unwrap_or_default();

    Some(ParsedTask {
        source_line: line_no,
        content_anchor: anchor(&text),
        text,
        completed: state,
        due_at,
        completed_at,
        linked_person,
        linked_project,
        tags,
        wikilinks,
    })
}

/// Given the body of a task (everything after `- [ ] `), strip the emoji
/// metadata (📅 due date, ⏰ due time, ✅ completion date) and return the
/// cleaned text plus the date, time, and completion date as SEPARATE strings
/// (empty = not present). The caller combines date + time into `due_at`,
/// because under convention 0.3 the effective date can be inherited from the
/// note (or, for a lone ⏰, be "today") rather than always coming from an
/// inline 📅.
fn strip_emoji_metadata(body: &str) -> (String, String, String, String) {
    let mut text = body.to_string();
    let mut due_date = String::new();
    let mut due_time = String::new();
    let mut completed_at = String::new();

    if let Some((before, date)) = take_after_marker(&text, "📅") {
        due_date = date;
        text = before;
    }
    if let Some((before, time)) = take_after_marker(&text, "⏰") {
        due_time = time;
        text = before;
    }
    if let Some((before, done)) = take_after_marker(&text, "✅") {
        completed_at = done;
        text = before;
    }

    (
        text.trim_end().to_string(),
        due_date,
        due_time,
        completed_at,
    )
}

/// Find `marker` in `text` and pull off the whitespace-separated token that
/// follows it. Returns `(text_with_marker_and_token_removed, captured_token)`.
fn take_after_marker(text: &str, marker: &str) -> Option<(String, String)> {
    let pos = text.find(marker)?;
    let after = &text[pos + marker.len()..];
    let after = after.trim_start();
    let token: String = after.chars().take_while(|c| !c.is_whitespace()).collect();
    if token.is_empty() {
        return None;
    }
    let consumed = pos
        + marker.len()
        + (after.as_ptr() as usize - text[pos + marker.len()..].as_ptr() as usize)
        + token.len();
    let mut before = text[..pos].trim_end().to_string();
    if consumed < text.len() {
        let tail = text[consumed..].trim_start();
        if !tail.is_empty() {
            before.push(' ');
            before.push_str(tail);
        }
    }
    Some((before, token))
}

fn anchor(text: &str) -> String {
    let s: String = text.chars().take(30).collect();
    s
}

/// The task-body text of a checkbox line as [`parse_checkbox_line`] records it
/// — the text after `- [ ]` / `- [x]` with the 📅/⏰/✅ emoji metadata stripped —
/// or `None` if the line isn't a checkbox. Kept in lockstep with the parser so
/// a stored `content_anchor` (the first chars of that text) can be matched back
/// to the same line even when the raw line carries leading/trailing metadata.
/// Used by the task-completion rewriter to identify a task robustly.
pub(crate) fn checkbox_task_text(line: &str) -> Option<String> {
    let trimmed = line.trim_start();
    let after_bullet = trimmed
        .strip_prefix("- ")
        .or_else(|| trimmed.strip_prefix("* "))
        .or_else(|| trimmed.strip_prefix("+ "))?;
    let rest = after_bullet
        .strip_prefix("[ ] ")
        .or_else(|| after_bullet.strip_prefix("[x] "))
        .or_else(|| after_bullet.strip_prefix("[X] "))?;
    Some(strip_emoji_metadata(rest).0)
}

fn extract_inline_tags(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'#' {
            let preceded_ok = i == 0 || bytes[i - 1].is_ascii_whitespace();
            if !preceded_ok {
                i += 1;
                continue;
            }
            i += 1;
            let mut tag = String::new();
            while i < bytes.len() {
                let c = bytes[i];
                if c.is_ascii_alphanumeric() || c == b'-' || c == b'_' || c == b'/' {
                    tag.push(c as char);
                    i += 1;
                } else {
                    break;
                }
            }
            // Same Obsidian rule as parser/tags.rs: at least one non-digit.
            if !tag.is_empty() && tag.chars().any(|c| !c.is_ascii_digit()) {
                out.push(tag);
            }
        } else {
            i += 1;
        }
    }
    out
}

fn extract_wikilinks(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars = text.char_indices().peekable();
    while let Some((idx, c)) = chars.next() {
        if c == '[' {
            if let Some(&(_, next)) = chars.peek() {
                if next == '[' {
                    chars.next();
                    let rest = &text[idx + 2..];
                    if let Some(end_rel) = rest.find("]]") {
                        let inner = &rest[..end_rel];
                        // Display alias: `[[Target|Display]]` — keep target only.
                        let target = inner.split('|').next().unwrap_or(inner).trim();
                        if !target.is_empty() {
                            out.push(target.to_string());
                        }
                    }
                }
            }
        }
    }
    out
}

/// Frontmatter `tasks:` block list. Recognized form:
///
/// ```yaml
/// tasks:
///   - text: "Review architecture doc"
///     due: 2026-05-02
///     time: "10:00"
///   - text: "Reply to Ravi"
///     due: 2026-05-03
/// ```
///
/// Or terse string form: `- "Friday: review architecture"` (no due/time fields).
///
/// We don't pull in a YAML parser — the pattern is narrow. Indented block-list
/// items only; flow-style `tasks: [...]` is not supported in MVP because
/// embedded date/time becomes ambiguous to scan.
///
/// Frontmatter `tasks:` items are *explicit* task declarations, so they remain
/// tasks even without a date (unlike body checkboxes under convention 0.3).
/// Their due date comes only from their own `due`/`time` fields — a dateless
/// item has no due date (the note's own `date:` is not a deadline).
fn parse_frontmatter_tasks(content: &str) -> Vec<ParsedTask> {
    if !content.starts_with("---\n") && !content.starts_with("---\r\n") {
        return Vec::new();
    }
    let after_open = if content.starts_with("---\r\n") { 5 } else { 4 };
    let rest = &content[after_open..];
    let close = find_frontmatter_close(rest);
    let Some(close_line_start) = close else {
        return Vec::new();
    };
    let body = &rest[..close_line_start];

    let mut out = Vec::new();
    let lines: Vec<&str> = body.lines().collect();
    let mut i = 0;
    let mut virtual_line: i32 = 0;
    while i < lines.len() {
        // AUDIT-019: top-level `tasks:` only, exact match. Block-form list
        // only — flow-form (`tasks: [a, b]`) has `[` in the line and is
        // handled separately when MVP support lands.
        if is_top_level_tasks_key(lines[i]) {
            i += 1;
            // Walk indented `- ...` items.
            while i < lines.len() {
                let raw = lines[i];
                let t = raw.trim_start();
                if t.starts_with("- ") || t.starts_with("-\t") {
                    let item = t.trim_start_matches("- ").trim();
                    if let Some(mut task) = parse_frontmatter_task_item(item, &lines, &mut i) {
                        virtual_line -= 1;
                        task.source_line = virtual_line;
                        out.push(task);
                    } else {
                        i += 1;
                    }
                } else if t.is_empty() {
                    i += 1;
                } else {
                    break;
                }
            }
        } else {
            i += 1;
        }
    }
    out
}

/// Parse one `- ...` item from a `tasks:` block. Advances `i` past the item.
/// Two shapes:
///
/// 1. Inline string: `- "Friday: review architecture"` — text only.
/// 2. Mapping: subsequent indented `key: value` lines. Recognized keys:
///    `text`, `due`, `time`, `completed`, `completed_at`.
fn parse_frontmatter_task_item(
    first_item: &str,
    lines: &[&str],
    i: &mut usize,
) -> Option<ParsedTask> {
    // Inline string-only form: `- "..."` or `- text without a colon`.
    let first_clean = first_item.trim().trim_matches(['"', '\'']);
    let first_has_colon =
        first_item.contains(':') && !first_item.starts_with('"') && !first_item.starts_with('\'');

    let mut text = String::new();
    let mut due_date = String::new();
    let mut due_time = String::new();
    let mut completed = false;
    let mut completed_at = String::new();

    if first_has_colon {
        // Mapping form. The first item line is itself a `key: value`.
        let (k, v) = split_kv(first_item);
        absorb_kv(
            &k,
            &v,
            &mut text,
            &mut due_date,
            &mut due_time,
            &mut completed,
            &mut completed_at,
        );
        *i += 1;
        // Subsequent same-indent lines (continuation lines for this item)
        // are also `key: value`. They start without `- ` and have indent > the
        // bullet line.
        let bullet_indent = lines.get(*i - 1).map(|s| indent(s)).unwrap_or(0);
        while *i < lines.len() {
            let line = lines[*i];
            let ind = indent(line);
            let ts = line.trim_start();
            if ts.is_empty() {
                *i += 1;
                continue;
            }
            if ts.starts_with("- ") {
                break;
            }
            if ind > bullet_indent {
                let (k, v) = split_kv(ts);
                absorb_kv(
                    &k,
                    &v,
                    &mut text,
                    &mut due_date,
                    &mut due_time,
                    &mut completed,
                    &mut completed_at,
                );
                *i += 1;
            } else {
                break;
            }
        }
    } else if !first_clean.is_empty() {
        text = first_clean.to_string();
        *i += 1;
    } else {
        return None;
    }

    if text.is_empty() {
        return None;
    }

    // Due date comes only from the item's own `due`/`time` — the note's own
    // `date:` is not a deadline (convention 0.3).
    let due_at = match (due_date.as_str(), due_time.as_str()) {
        ("", _) => String::new(),
        (d, "") => d.to_string(),
        (d, t) => format!("{d} {t}"),
    };

    let tags = extract_inline_tags(&text);
    let wikilinks = extract_wikilinks(&text);
    let linked_project = tags
        .iter()
        .find(|t| t.starts_with("project/"))
        .cloned()
        .unwrap_or_default();
    let linked_person = wikilinks.first().cloned().unwrap_or_default();

    Some(ParsedTask {
        source_line: 0, // frontmatter tasks have no line in the body
        content_anchor: anchor(&text),
        text,
        completed,
        due_at,
        completed_at,
        linked_person,
        linked_project,
        tags,
        wikilinks,
    })
}

fn split_kv(line: &str) -> (String, String) {
    if let Some(idx) = line.find(':') {
        let k = line[..idx].trim().to_string();
        let v = line[idx + 1..].trim().trim_matches(['"', '\'']).to_string();
        (k, v)
    } else {
        (line.to_string(), String::new())
    }
}

#[allow(clippy::too_many_arguments)]
fn absorb_kv(
    k: &str,
    v: &str,
    text: &mut String,
    due: &mut String,
    time: &mut String,
    completed: &mut bool,
    completed_at: &mut String,
) {
    match k {
        "text" => *text = v.to_string(),
        "due" => *due = v.to_string(),
        "time" => *time = v.to_string(),
        "completed" => *completed = matches!(v, "true" | "yes" | "True" | "TRUE"),
        "completed_at" => *completed_at = v.to_string(),
        _ => {}
    }
}

fn indent(line: &str) -> usize {
    line.bytes()
        .take_while(|b| *b == b' ' || *b == b'\t')
        .count()
}

fn frontmatter_end_offset(content: &str) -> usize {
    if !content.starts_with("---\n") && !content.starts_with("---\r\n") {
        return 0;
    }
    let after_open = if content.starts_with("---\r\n") { 5 } else { 4 };
    let rest = &content[after_open..];
    match find_frontmatter_close(rest) {
        Some(close_line_start) => {
            // Include the closing `---\n` line in the skipped region.
            let after_close = &rest[close_line_start..];
            let close_line_len = after_close
                .find('\n')
                .map(|n| n + 1)
                .unwrap_or(after_close.len());
            after_open + close_line_start + close_line_len
        }
        None => 0,
    }
}

/// AUDIT-019: a line is the top-level `tasks:` key only if it has no
/// leading whitespace, equals `tasks:` exactly, and is not the inline-flow
/// form (`tasks: [a, b]`). Catches the prior bugs where suffix-matching
/// `ends_with("tasks:")` would have happily accepted any `*_tasks:` and
/// any indented `tasks:` under a parent key.
fn is_top_level_tasks_key(line: &str) -> bool {
    let trimmed = line.trim_end_matches(['\r', '\n']);
    trimmed == "tasks:"
}

/// Note-level task opt-out. Returns true when the frontmatter carries a
/// top-level `tasks:` key whose value is a scalar in {off, false, none,
/// no} (case-insensitive, surrounding quotes ignored). Such a note's
/// checkboxes are document structure (planning docs, templates, imported
/// READMEs) — not personal to-dos — so [`parse_tasks`] yields nothing for
/// it and the indexer writes zero `:task` rows.
///
/// The block-list form (`tasks:` followed by indented `- ...` items) has
/// an EMPTY value on the `tasks:` line, so it never trips this — only the
/// scalar opt-out does. The scalar/array distinction is load-bearing and
/// pinned by tests below.
///
/// Mirrors `corvux-mcp/src/conventions.rs::note_tasks_disabled` so the
/// app indexer and the standalone MCP parser agree on which notes opt out
/// (the shared golden-corpus fixture cross-checks both).
fn tasks_disabled(content: &str) -> bool {
    if !content.starts_with("---\n") && !content.starts_with("---\r\n") {
        return false;
    }
    let after_open = if content.starts_with("---\r\n") { 5 } else { 4 };
    let rest = &content[after_open..];
    let Some(close_line_start) = find_frontmatter_close(rest) else {
        return false;
    };
    let body = &rest[..close_line_start];
    for line in body.lines() {
        // Top-level only: no leading whitespace. An indented `tasks:` under
        // a parent key (`config:\n  tasks: off`) is a nested map, not the
        // note-level property — `strip_prefix` fails on the leading spaces.
        let Some(value) = line.trim_end_matches(['\r', '\n']).strip_prefix("tasks:") else {
            continue;
        };
        let value = value.trim();
        // Empty value → the block-list opener (`tasks:` with `- items`
        // below). That's the task ARRAY form, never an opt-out.
        if value.is_empty() {
            return false;
        }
        let value = value.trim_matches(['"', '\'']).to_lowercase();
        return matches!(value.as_str(), "off" | "false" | "none" | "no");
    }
    false
}

fn find_frontmatter_close(body: &str) -> Option<usize> {
    let mut line_start = 0;
    for line in body.split_inclusive('\n') {
        let trimmed_end = line.trim_end_matches(['\r', '\n']);
        if trimmed_end == "---" || trimmed_end == "..." {
            return Some(line_start);
        }
        line_start += line.len();
    }
    None
}

/// Extract a top-level `date: YYYY-MM-DD` value from the note's frontmatter,
/// if present and shaped like an ISO date. This is the note's own date — it
/// makes every checkbox in a dated note (daily notes, dated journals) a task
/// under convention 0.3. Surrounding quotes are tolerated.
///
/// Mirrors `corvux-mcp/src/conventions.rs::frontmatter_date` so the app
/// indexer and the standalone MCP parser agree on which notes are "dated".
pub(crate) fn frontmatter_date(content: &str) -> Option<String> {
    if !content.starts_with("---\n") && !content.starts_with("---\r\n") {
        return None;
    }
    let after_open = if content.starts_with("---\r\n") { 5 } else { 4 };
    let rest = &content[after_open..];
    let close = find_frontmatter_close(rest)?;
    let body = &rest[..close];
    for line in body.lines() {
        // Top-level only — no leading whitespace (an indented `date:` is a
        // nested map value, not the note's own date property).
        let Some(value) = line.trim_end_matches(['\r', '\n']).strip_prefix("date:") else {
            continue;
        };
        let value = value.trim().trim_matches(['"', '\'']);
        return is_iso_date(value).then(|| value.to_string());
    }
    None
}

/// Cheap `YYYY-MM-DD` shape check (matches `corvux-mcp`'s `is_iso_date`). Not
/// a calendar validation — just rejects obvious non-dates.
fn is_iso_date(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && b[..4].iter().all(u8::is_ascii_digit)
        && b[5..7].iter().all(u8::is_ascii_digit)
        && b[8..10].iter().all(u8::is_ascii_digit)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fixed "today" so lone-⏰ resolution is deterministic in tests.
    const TODAY: &str = "2026-06-11";

    /// Parse as a *regular* (undated) note: no note date, today pinned. Bare
    /// checkboxes here are NOT tasks (convention 0.3) — only dated ones are.
    fn t(content: &str) -> Vec<ParsedTask> {
        parse_tasks_in_note(content, None, Some(TODAY))
    }

    /// Parse as a *dated* note (daily note / dated journal): every checkbox
    /// inherits `note_date` and so becomes a task.
    fn t_dated(content: &str, note_date: &str) -> Vec<ParsedTask> {
        parse_tasks_in_note(content, Some(note_date), Some(TODAY))
    }

    // ── core effective-date rule (convention 0.3) ─────────────────────

    #[test]
    fn bare_checkbox_in_undated_note_is_not_a_task() {
        // The headline change: an undated checkbox in a regular note is a
        // plain checklist item, not a to-do.
        assert!(t("- [ ] Review architecture doc").is_empty());
        assert!(t("- [x] Done already").is_empty());
        assert!(t("- [ ] Decision: CozoDB vs SQLite").is_empty());
    }

    #[test]
    fn inline_date_makes_a_task() {
        let tasks = t("- [ ] Review architecture doc 📅 2026-05-02");
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].text, "Review architecture doc");
        assert_eq!(tasks[0].due_at, "2026-05-02");
        assert!(!tasks[0].completed);
        assert_eq!(tasks[0].source_line, 1);
    }

    #[test]
    fn inline_date_and_time_makes_a_reminder() {
        let tasks = t("- [ ] Stand-up call 📅 2026-05-02 ⏰ 10:00");
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].text, "Stand-up call");
        assert_eq!(tasks[0].due_at, "2026-05-02 10:00");
    }

    #[test]
    fn lone_time_resolves_to_today() {
        // ⏰ with no date anywhere = "remind me today at that time".
        let tasks = t("- [ ] Floating standup ⏰ 10:00");
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].due_at, "2026-06-11 10:00");
    }

    #[test]
    fn lone_time_in_dated_note_still_resolves_to_today() {
        // A ⏰ is an explicit per-line reminder signal → today, even in a
        // dated note (the note's own date is not a deadline).
        let tasks = t_dated("- [ ] Standup ⏰ 09:00", "2026-04-05");
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].due_at, "2026-06-11 09:00");
    }

    #[test]
    fn dated_note_checkbox_is_a_task_with_no_due_date() {
        // Every checkbox in a dated note is a VISIBLE task, but the note's
        // date is not a due date — due_at stays empty (never overdue).
        let tasks = t_dated("- [ ] Buy milk\n- [ ] Pay rent", "2026-06-11");
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].due_at, "");
        assert_eq!(tasks[1].due_at, "");
    }

    #[test]
    fn frontmatter_date_makes_checkboxes_visible_tasks_without_due() {
        // A top-level `date:` makes the note "dated", so its checkboxes are
        // tasks — but with NO due date (the `date:` is the note's date, not a
        // deadline). An inline 📅 on a line still sets that line's due date.
        let content = "---\ndate: 2026-04-05\nuser: technoo\n---\n\n- [ ] Protein hit (140g+)\n- [ ] Steps hit (5k+) 📅 2026-04-05\n";
        let tasks = parse_tasks_in_note(content, None, Some(TODAY));
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].text, "Protein hit (140g+)");
        assert_eq!(tasks[0].due_at, ""); // inherited from note → no due date
        assert_eq!(tasks[1].due_at, "2026-04-05"); // inline 📅 → real due date
    }

    #[test]
    fn inline_date_sets_due_in_a_dated_note() {
        // A line's own 📅 sets the due date even when the note is dated.
        let tasks = t_dated("- [ ] File taxes 📅 2026-07-15", "2026-06-11");
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].due_at, "2026-07-15");
    }

    #[test]
    fn capital_x_completed_with_date() {
        let tasks = t("- [X] Capital X also done 📅 2026-05-01");
        assert_eq!(tasks.len(), 1);
        assert!(tasks[0].completed);
    }

    #[test]
    fn completed_with_completion_date() {
        let tasks = t("- [x] Shipped slice 1 📅 2026-05-01 ✅ 2026-05-01");
        assert_eq!(tasks.len(), 1);
        assert!(tasks[0].completed);
        assert_eq!(tasks[0].due_at, "2026-05-01");
        assert_eq!(tasks[0].completed_at, "2026-05-01");
    }

    #[test]
    fn tags_and_wikilinks_extracted() {
        let tasks = t("- [ ] Reply to [[Ravi]] about API spec #project/atlas 📅 2026-05-02");
        assert_eq!(tasks.len(), 1);
        assert!(tasks[0].tags.contains(&"project/atlas".to_string()));
        assert_eq!(tasks[0].wikilinks, vec!["Ravi".to_string()]);
        assert_eq!(tasks[0].linked_person, "Ravi");
        assert_eq!(tasks[0].linked_project, "project/atlas");
    }

    #[test]
    fn wikilink_with_alias() {
        let tasks = t_dated("- [ ] Talk to [[Ravi|the API guy]] today", "2026-06-11");
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].wikilinks, vec!["Ravi".to_string()]);
    }

    #[test]
    fn skip_fenced_code_block() {
        // The checkbox inside the fence is never a task; the real one needs a
        // date to qualify.
        let content = "```\n- [ ] not a task\n```\n- [ ] real task 📅 2026-06-12";
        let tasks = t(content);
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].text, "real task");
        assert_eq!(tasks[0].source_line, 4);
    }

    #[test]
    fn skip_frontmatter_body_region() {
        // The note's `date:` makes its body checkboxes tasks (with no due
        // date); the `---` fences and `title:`/`date:` lines are not tasks.
        let content =
            "---\ntitle: Note\ndate: 2026-06-01\n---\n\n- [ ] task one\n- [x] task two ✅ 2026-05-01\n";
        let tasks = t(content);
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].text, "task one");
        assert_eq!(tasks[0].due_at, ""); // dated note → visible task, no due date
        assert_eq!(tasks[0].source_line, 6);
        assert!(tasks[1].completed);
    }

    #[test]
    fn frontmatter_tasks_block() {
        // Explicit frontmatter `tasks:` items stay tasks even without a date.
        let content = "---\ntitle: Sprint\ntasks:\n  - text: \"Review architecture\"\n    due: 2026-05-02\n  - text: \"Reply to Ravi\"\n---\n\nbody";
        let tasks = t(content);
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].text, "Review architecture");
        assert_eq!(tasks[0].due_at, "2026-05-02");
        assert_eq!(tasks[1].text, "Reply to Ravi");
        assert_eq!(tasks[1].due_at, "");
    }

    #[test]
    fn frontmatter_tasks_do_not_inherit_note_date() {
        // A dateless frontmatter task has no due date even in a dated note —
        // the note's `date:` is not a deadline (convention 0.3).
        let content = "---\ndate: 2026-06-01\ntasks:\n  - text: \"Reply to Ravi\"\n---\nbody";
        let tasks = t(content);
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].due_at, "");
    }

    #[test]
    fn frontmatter_tasks_with_time() {
        let content =
            "---\ntasks:\n  - text: \"Standup\"\n    due: 2026-05-02\n    time: \"10:00\"\n---\nbody";
        let tasks = t(content);
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].due_at, "2026-05-02 10:00");
    }

    #[test]
    fn empty_checkbox_is_skipped() {
        assert!(t_dated("- [ ]", "2026-06-11").is_empty());
    }

    #[test]
    fn multiple_tasks_get_sequential_lines() {
        let content = "first line\n- [ ] one\nfiller\n- [x] two";
        let tasks = t_dated(content, "2026-06-11");
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].source_line, 2);
        assert_eq!(tasks[1].source_line, 4);
    }

    #[test]
    fn indented_subtask_still_parses() {
        // Indent allowed; nesting (sub-tasks) is V2 but the line is still a task.
        let content = "- [ ] parent\n  - [ ] child";
        let tasks = t_dated(content, "2026-06-11");
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[1].text, "child");
    }

    #[test]
    fn star_and_plus_bullets() {
        let tasks = t_dated("* [ ] starred\n+ [x] plused", "2026-06-11");
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].text, "starred");
        assert!(tasks[1].completed);
    }

    #[test]
    fn content_anchor_truncates_at_30_chars() {
        let tasks = t_dated(
            "- [ ] a very long task text that goes well beyond thirty characters for sure",
            "2026-06-11",
        );
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].content_anchor.chars().count(), 30);
    }

    #[test]
    fn no_tasks_in_plain_prose() {
        let tasks = t_dated(
            "Just some prose with #a-tag and [[A Link]] but no checkboxes.",
            "2026-06-11",
        );
        assert_eq!(tasks.len(), 0);
    }

    // ── AUDIT-019: `tasks:` matcher must be top-level + exact ─────────

    #[test]
    fn frontmatter_sub_tasks_key_not_treated_as_tasks() {
        // `sub_tasks:` is a different YAML key — items under it must NOT
        // show up in the parsed task list.
        let content = "---\nsub_tasks:\n  - text: \"do not show\"\n    due: 2026-05-25\n---\nbody";
        let tasks = t(content);
        assert!(
            tasks.is_empty(),
            "sub_tasks: should not be parsed as the tasks: block, got: {tasks:?}"
        );
    }

    #[test]
    fn frontmatter_customer_tasks_key_not_treated_as_tasks() {
        let content =
            "---\ncustomer_tasks:\n  - text: \"do not show\"\n    due: 2026-05-25\n---\nbody";
        let tasks = t(content);
        assert!(tasks.is_empty(), "customer_tasks: must not match");
    }

    #[test]
    fn frontmatter_inline_flow_tasks_not_parsed() {
        // Flow-form `tasks: [a, b]` is not the block-list shape this
        // parser supports; it must not be misidentified as a block opener.
        let content = "---\ntasks: [a, b, c]\n---\nbody";
        let tasks = t(content);
        assert!(
            tasks.is_empty(),
            "inline-flow tasks: form must not be parsed as block"
        );
    }

    #[test]
    fn frontmatter_nested_indented_tasks_not_parsed() {
        // An indented `tasks:` under a parent key is a nested map, not the
        // top-level task list we recognize.
        let content = "---\nconfig:\n  tasks:\n    - text: \"nope\"\n---\nbody";
        let tasks = t(content);
        assert!(tasks.is_empty(), "indented tasks: must not match top-level");
    }

    #[test]
    fn frontmatter_top_level_tasks_still_parsed() {
        // Regression guard: the tightening must not break the basic case.
        let content = "---\ntasks:\n  - text: \"do this\"\n---\nbody";
        let tasks = t(content);
        assert_eq!(tasks.len(), 1, "top-level tasks: must still parse");
        assert_eq!(tasks[0].text, "do this");
    }

    // ── tasks: off note-level opt-out ─────────────────────────────────

    #[test]
    fn tasks_off_disables_body_checkboxes() {
        // Opt-out wins even when a `date:` would otherwise make them tasks.
        let content =
            "---\ndate: 2026-06-11\ntasks: off\n---\n\n- [ ] should not index\n- [x] nor this\n";
        assert!(
            t(content).is_empty(),
            "a note with `tasks: off` must yield zero tasks"
        );
    }

    #[test]
    fn tasks_off_accepts_false_none_no_and_is_case_insensitive() {
        for v in ["off", "false", "none", "no", "OFF", "False", "None", "NO"] {
            let content = format!("---\ndate: 2026-06-11\ntasks: {v}\n---\n\n- [ ] hidden\n");
            assert!(
                t(&content).is_empty(),
                "`tasks: {v}` should disable indexing for the note"
            );
        }
    }

    #[test]
    fn tasks_off_accepts_quoted_scalar() {
        let content = "---\ndate: 2026-06-11\ntasks: \"off\"\n---\n\n- [ ] hidden\n";
        assert!(t(content).is_empty(), "quoted scalar opt-out must work");
    }

    #[test]
    fn tasks_unknown_scalar_does_not_disable() {
        // Only the documented opt-out tokens disable. A stray scalar like
        // `tasks: later` is not an opt-out — a dated checkbox indexes normally.
        let content = "---\ntasks: later\n---\n\n- [ ] still a task 📅 2026-06-12\n";
        let tasks = t(content);
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].text, "still a task");
    }

    #[test]
    fn tasks_off_disables_frontmatter_task_array_too() {
        // The opt-out wins even over an (unusual) note that has both a
        // scalar `tasks: off` line — there is no array here, just the
        // scalar — and body checkboxes.
        let content = "---\ntype: template\ntasks: none\n---\n\n- [ ] skeleton 📅 2026-06-12\n";
        assert!(t(content).is_empty());
    }

    #[test]
    fn block_list_tasks_is_not_an_opt_out() {
        // The `tasks:` block-list opener has an empty inline value; it must
        // keep parsing as the task ARRAY, never be mistaken for an opt-out.
        let content = "---\ntasks:\n  - text: \"keep me\"\n    due: 2026-06-10\n---\nbody";
        let tasks = t(content);
        assert_eq!(tasks.len(), 1, "block-list tasks: must still parse");
        assert_eq!(tasks[0].text, "keep me");
        assert_eq!(tasks[0].due_at, "2026-06-10");
    }

    #[test]
    fn indented_tasks_off_under_parent_key_is_ignored() {
        // A nested `tasks: off` under another key is a child map value, not
        // the note-level property — must NOT disable indexing.
        let content = "---\nconfig:\n  tasks: off\n---\n\n- [ ] real task 📅 2026-06-12\n";
        let tasks = t(content);
        assert_eq!(tasks.len(), 1, "indented tasks: off must not opt out");
        assert_eq!(tasks[0].text, "real task");
    }

    #[test]
    fn tasks_off_only_applies_with_frontmatter() {
        // A bare body line that looks like `tasks: off` (not in frontmatter)
        // is just prose — a dated checkbox still indexes.
        let content = "tasks: off\n\n- [ ] body task 📅 2026-06-12\n";
        let tasks = t(content);
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].text, "body task");
    }

    /// Shared golden corpus — cross-checks the app parser against the SAME
    /// fixtures the standalone `corvux-mcp` server asserts against (its
    /// `conventions.rs::golden_corpus_matches_shared_fixtures`). The two
    /// crates duplicate the markdown dialect on purpose (the MCP binary must
    /// not link the app's heavy stack); this fixture set is the contract that
    /// keeps them from drifting. Both parse with the same fixed `today` and
    /// no caller note-date, so each fixture's date context lives in its own
    /// content (inline 📅, frontmatter `date:`, or a lone ⏰ = today).
    /// Projection: OPEN tasks only, identified by a distinctive text fragment
    /// + the resolved date (or "" when undated).
    #[test]
    fn golden_corpus_matches_shared_fixtures() {
        use std::path::Path;
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("corvux-mcp")
            .join("tests")
            .join("fixtures");
        let raw = std::fs::read_to_string(dir.join("expected.json")).expect("read expected.json");
        let expected: serde_json::Value = serde_json::from_str(&raw).expect("parse expected.json");

        for (file, items) in expected.as_object().unwrap() {
            if file.starts_with('_') {
                continue; // metadata keys, not fixtures
            }
            let content = std::fs::read_to_string(dir.join(file)).expect(file);
            let open: Vec<ParsedTask> = parse_tasks_in_note(&content, None, Some(TODAY))
                .into_iter()
                .filter(|t| !t.completed)
                .collect();
            let exp = items.as_array().unwrap();
            assert_eq!(
                open.len(),
                exp.len(),
                "{file}: open-task count mismatch; got {open:?}"
            );
            for e in exp {
                let contains = e["contains"].as_str().unwrap();
                let due = e["due"].as_str().unwrap();
                // The app stores `due_at` as `YYYY-MM-DD[ HH:MM]`; the shared
                // contract compares only the date part.
                let n = open
                    .iter()
                    .filter(|task| {
                        task.text.contains(contains)
                            && task.due_at.split(' ').next().unwrap_or("") == due
                    })
                    .count();
                assert_eq!(
                    n, 1,
                    "{file}: expected exactly one open task containing {contains:?} due {due:?}, got {n}"
                );
            }
        }
    }
}
