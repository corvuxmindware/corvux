//! Markdown conventions for a Corvux roost.
//!
//! These are FAITHFUL copies of the formats the Corvux indexer parses
//! (`src-tauri/src/parser/checkboxes.rs`) and writes
//! (`src-tauri/src/commands/daily_note.rs`, `tasks.rs`). They are
//! duplicated here on purpose — the MCP server is a lightweight standalone
//! binary that must NOT link the full app crate (cozo / onnx / iroh /
//! llama). Keep them in lock-step with the app; the tests at the bottom of
//! this file pin the exact strings the indexer expects, so drift fails CI.

use chrono::{Datelike, Local};

use crate::frontmatter;

/// Build a task line in the exact form the indexer recognizes:
/// `- [ ] <text>` with an optional ` 📅 YYYY-MM-DD` due date and an
/// optional ` ⏰ HH:MM` due time. The clock is only emitted alongside a
/// date — the indexer ignores a lone `⏰`, so emitting one would just look
/// broken to the user.
pub fn format_task_line(text: &str, due_date: Option<&str>, due_time: Option<&str>) -> String {
    let mut line = format!("- [ ] {}", text.trim());
    if let Some(d) = due_date {
        let d = d.trim();
        if !d.is_empty() {
            line.push_str(&format!(" 📅 {d}"));
            if let Some(t) = due_time {
                let t = t.trim();
                if !t.is_empty() {
                    line.push_str(&format!(" ⏰ {t}"));
                }
            }
        }
    }
    line
}

/// Today's `(YYYY-MM-DD, Weekday)` in the user's LOCAL timezone.
///
/// Local — not UTC — on purpose: a user in IST opening the daily note at
/// 00:30 would otherwise land on yesterday's file (the app fixed this same
/// bug; see daily_note.rs). Both halves come from one `Local::now()` read
/// so they can't straddle midnight.
pub fn today_local() -> (String, &'static str) {
    let now = Local::now();
    let date = now.format("%Y-%m-%d").to_string();
    let weekday = match now.weekday() {
        chrono::Weekday::Mon => "Monday",
        chrono::Weekday::Tue => "Tuesday",
        chrono::Weekday::Wed => "Wednesday",
        chrono::Weekday::Thu => "Thursday",
        chrono::Weekday::Fri => "Friday",
        chrono::Weekday::Sat => "Saturday",
        chrono::Weekday::Sun => "Sunday",
    };
    (date, weekday)
}

/// The built-in daily-note template, byte-for-byte identical to the app's
/// `default_daily_template` (`daily_note.rs`). Used when the user hasn't
/// supplied their own `.corvux/templates/daily.md` — see `pick_daily_template`.
/// Header is `# <Weekday>, <YYYY-MM-DD>`.
pub fn daily_note_template(date_str: &str, weekday: &str) -> String {
    format!(
        "---\ndate: {date_str}\ntype: daily\ntags:\n  - daily\n---\n\n# {weekday}, {date_str}\n\n## Tasks\n\n- [ ] \n\n## Notes\n\n\n\n## Journal\n\n\n"
    )
}

/// DN-1 (ported from the app's `daily_note.rs::render_daily_template`):
/// substitute the supported placeholders in a user-supplied daily-note
/// template. Deliberately tiny — Obsidian/Tana-style templating (cursor,
/// prompts, includes) is out of scope; just the three tokens a daily note
/// needs:
///   `{{date}}`     → `YYYY-MM-DD`
///   `{{weekday}}`  → full weekday name (e.g. `Monday`)
///   `{{title}}`    → `Weekday, YYYY-MM-DD` (the default header line)
/// Unknown `{{...}}` tokens are left untouched so a typo is visible in the
/// note rather than silently eaten.
pub fn render_daily_template(raw: &str, date_str: &str, weekday: &str) -> String {
    raw.replace("{{date}}", date_str)
        .replace("{{weekday}}", weekday)
        .replace("{{title}}", &format!("{weekday}, {date_str}"))
}

/// DN-1: choose the daily-note body — the user's template when present and
/// non-blank, else the built-in default. Mirrors the app's
/// `pick_daily_template` so a missing, unreadable, or accidentally-blanked
/// custom template never produces a frontmatter-less empty note. The caller
/// (`server.rs`) supplies `user_template` by reading
/// `.corvux/templates/daily.md`, the same path the app reads.
pub fn pick_daily_template(user_template: Option<&str>, date_str: &str, weekday: &str) -> String {
    match user_template {
        Some(raw) if !raw.trim().is_empty() => render_daily_template(raw, date_str, weekday),
        _ => daily_note_template(date_str, weekday),
    }
}

/// Append `text` to the end of the `## <heading>` section of `content`.
///
/// Mirrors the app's `append_task_to_section` (generalized to any heading):
/// find the (case-insensitive) `## <heading>` line, insert just before the
/// next `## ` header (or end of file), and keep exactly one blank line
/// before that next header. If the section doesn't exist, one is appended
/// at the end of the file. The original trailing-newline state is kept.
pub fn append_under_heading(content: &str, heading: &str, text: &str) -> String {
    let lines: Vec<&str> = content.lines().collect();
    let want = format!("## {}", heading.trim());
    let header_idx = lines
        .iter()
        .position(|l| l.trim().eq_ignore_ascii_case(&want));

    let result = match header_idx {
        Some(h) => {
            // First `## ` header after this one bounds the section.
            let mut insert_at = lines.len();
            for (i, l) in lines.iter().enumerate().skip(h + 1) {
                if l.trim_start().starts_with("## ") {
                    insert_at = i;
                    break;
                }
            }
            let mut head: Vec<String> = lines[..insert_at].iter().map(|s| s.to_string()).collect();
            // Drop trailing blank lines inside the section so the new line
            // sits right under the existing content.
            while head.len() > h + 1 && head.last().map(|s| s.trim().is_empty()).unwrap_or(false) {
                head.pop();
            }
            // If the section was empty (we're back at the header line), keep
            // one blank line between the heading and the new content.
            if head.len() == h + 1 {
                head.push(String::new());
            }
            head.push(text.to_string());

            let tail: Vec<String> = lines[insert_at..].iter().map(|s| s.to_string()).collect();
            if !tail.is_empty() {
                head.push(String::new()); // one blank line before the next header
                head.extend(tail);
            }
            head.join("\n")
        }
        None => {
            let mut s = content.trim_end().to_string();
            if !s.is_empty() {
                s.push_str("\n\n");
            }
            s.push_str(&format!("## {}\n\n{}", heading.trim(), text));
            s
        }
    };

    // Preserve the original trailing newline (markdown files conventionally
    // end with one; the app's files do).
    let mut result = result;
    if content.ends_with('\n') && !result.ends_with('\n') {
        result.push('\n');
    }
    result
}

/// Convenience: append a task line under the `## Tasks` section.
pub fn append_under_tasks(content: &str, task_line: &str) -> String {
    append_under_heading(content, "Tasks", task_line)
}

/// Append free text to the end of a note, separated by a blank line.
/// Used by `append_to_note` when no target heading is given.
pub fn append_to_end(content: &str, text: &str) -> String {
    let mut s = content.trim_end().to_string();
    if !s.is_empty() {
        s.push_str("\n\n");
    }
    s.push_str(text.trim_end());
    s.push('\n');
    s
}

/// Complete the single open task whose text contains `needle`: flip
/// `[ ]` → `[x]` and append ` ✅ <today>` (the completion stamp the app
/// adds). Errors if zero or more than one open task matches, so we never
/// complete the wrong one. Returns `(new_content, the_completed_line)`.
pub fn complete_task_in(
    content: &str,
    needle: &str,
    today: &str,
) -> Result<(String, String), String> {
    let want = needle.to_lowercase();
    let lines: Vec<&str> = content.lines().collect();
    let matches: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter_map(|(i, line)| {
            strip_open_checkbox(line.trim_start())
                .filter(|rest| rest.to_lowercase().contains(&want))
                .map(|_| i)
        })
        .collect();

    match matches.len() {
        0 => Err(format!("No open task matching \"{needle}\".")),
        1 => {
            let idx = matches[0];
            let completed = format!(
                "{} ✅ {today}",
                lines[idx].replacen("[ ]", "[x]", 1).trim_end()
            );
            let mut new_lines: Vec<String> = lines.iter().map(|s| s.to_string()).collect();
            new_lines[idx] = completed.clone();
            let mut out = new_lines.join("\n");
            if content.ends_with('\n') && !out.ends_with('\n') {
                out.push('\n');
            }
            Ok((out, completed))
        }
        n => Err(format!(
            "{n} open tasks match \"{needle}\" — be more specific."
        )),
    }
}

/// A pending (`- [ ]`) task line discovered while scanning a note.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenTask {
    /// 1-based line number within the note.
    pub line_no: usize,
    /// Task text as written (metadata tokens like `📅` kept inline).
    pub text: String,
    /// Parsed `📅 YYYY-MM-DD` due date, if present.
    pub due_date: Option<String>,
}

/// Parse the pending tasks out of a note's markdown. Recognizes the bullet
/// variants the indexer accepts (`-`, `*`, `+`) with an unchecked `[ ]`
/// box, at any indentation. Completed (`[x]`) tasks are skipped.
///
/// Mirrors the app's `parser/checkboxes.rs` so `list_open_tasks` agrees with
/// the Tasks pane:
///   - the YAML frontmatter region is NOT scanned for body checkboxes (its
///     `---` fences and metadata are not tasks);
///   - fenced code blocks (```` ``` ````/`~~~`) are skipped — a `- [ ]` in a
///     code sample is documentation, not a task;
///   - the frontmatter `tasks:` block-list (open items only) IS parsed, with
///     a `line_no` of 0 since those tasks have no body line.
pub fn parse_open_tasks(
    content: &str,
    note_date: Option<&str>,
    today: Option<&str>,
) -> Vec<OpenTask> {
    // Note-level opt-out: a top-level `tasks: off` (also false/none/no)
    // property means "this note's checkboxes are document structure, not
    // to-dos" — skip the whole note.
    if note_tasks_disabled(content) {
        return Vec::new();
    }

    // The note's effective date for inheritance (convention 0.3): an explicit
    // frontmatter `date:` wins, else the caller-supplied note date (a daily
    // note's filename stem). Body checkboxes inherit it; without any effective
    // date a body checkbox is a plain checklist item, not a task.
    let fm_date = frontmatter_date(content);
    let note_eff_date: Option<&str> = fm_date.as_deref().or(note_date);

    // Frontmatter `tasks:` array first (open items only). These have no body
    // line, so they carry `line_no: 0`; the list renderer omits the `:line`.
    let mut out = parse_frontmatter_open_tasks(content);

    let fm_end = frontmatter_end_line(content);
    let mut in_code_block = false;
    for (i, line) in content.lines().enumerate() {
        // Skip the frontmatter region (lines up to and including the close).
        if matches!(fm_end, Some(end) if i <= end) {
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
        if let Some(rest) = strip_open_checkbox(trimmed) {
            let text = rest.trim().to_string();
            if text.is_empty() {
                continue; // skip the empty placeholder line in daily notes
            }
            // A checkbox is a task with a date CONTEXT: an inline 📅, a ⏰, or
            // living in a dated note. The DUE date comes only from an inline
            // 📅 or a lone ⏰ (→ today); a note-dated checkbox is a visible
            // task with NO due date (the note's `date:` is not a deadline).
            let inline = extract_due_date(rest);
            let has_time = rest.contains('⏰');
            if inline.is_none() && !has_time && note_eff_date.is_none() {
                continue;
            }
            let due_date: Option<String> = if inline.is_some() {
                inline
            } else if has_time {
                today.map(|t| t.to_string())
            } else {
                None
            };
            out.push(OpenTask {
                line_no: i + 1,
                due_date,
                text,
            });
        }
    }
    out
}

/// The note's date inferred from a daily-note filename stem (`Daily Notes/
/// 2026-06-10.md` → `2026-06-10`). `None` unless the stem is shaped like an
/// ISO date. In [`parse_open_tasks`] the frontmatter `date:` takes precedence
/// over this; it's the fallback for a daily note that lacks one.
pub fn note_date_from_path(path: &str) -> Option<String> {
    let base = path.rsplit(['/', '\\']).next().unwrap_or(path);
    let stem = base.strip_suffix(".md").unwrap_or(base);
    is_iso_date(stem).then(|| stem.to_string())
}

/// 0-based line index of the frontmatter's CLOSING fence, if the note opens
/// with a well-formed `---` block. Lines `0..=idx` are frontmatter and must
/// be skipped by the body scanner. Returns `None` (no skip) when the note
/// doesn't open with `---` or the block is never closed — matching the app's
/// `frontmatter_end_offset` "not frontmatter" fallthrough.
fn frontmatter_end_line(content: &str) -> Option<usize> {
    if content.lines().next().map(str::trim) != Some("---") {
        return None;
    }
    content
        .lines()
        .enumerate()
        .skip(1)
        .find(|(_, l)| matches!(l.trim(), "---" | "..."))
        .map(|(i, _)| i)
}

/// Parse the OPEN tasks from a top-level frontmatter `tasks:` block list.
/// Faithful to the app's `parse_frontmatter_tasks` (block-list form only;
/// flow-form `tasks: [a, b]` and nested/indented `tasks:` are intentionally
/// ignored), filtered to incomplete items. Completed items (`completed:
/// true`) are dropped since this server only surfaces open tasks.
fn parse_frontmatter_open_tasks(content: &str) -> Vec<OpenTask> {
    let Some(fm_inner) = frontmatter::split(content).0 else {
        return Vec::new();
    };
    let lines: Vec<&str> = fm_inner.lines().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        // Top-level `tasks:` only — exact match, no leading indent (matches
        // the app's `is_top_level_tasks_key`).
        if lines[i].trim_end_matches(['\r', '\n']) == "tasks:" {
            i += 1;
            while i < lines.len() {
                let t = lines[i].trim_start();
                if t.starts_with("- ") || t.starts_with("-\t") {
                    match parse_frontmatter_task_item(&lines, &mut i) {
                        Some((task, completed)) => {
                            if !completed {
                                out.push(task);
                            }
                        }
                        None => i += 1,
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

/// Parse one `- ...` item from a `tasks:` block, advancing `i` past it.
/// Two shapes (same as the app): an inline string (`- "do this"`) or a
/// mapping (`- text: "..."` plus indented `due:` / `completed:` lines).
/// Returns `(open_task, completed_flag)`.
fn parse_frontmatter_task_item(lines: &[&str], i: &mut usize) -> Option<(OpenTask, bool)> {
    let first_item = lines[*i].trim_start().trim_start_matches("- ").trim();
    let first_clean = first_item.trim_matches(['"', '\'']);
    let first_has_colon =
        first_item.contains(':') && !first_item.starts_with('"') && !first_item.starts_with('\'');

    let mut text = String::new();
    let mut due = String::new();
    let mut completed = false;

    if first_has_colon {
        let (k, v) = split_kv(first_item);
        absorb_task_kv(&k, &v, &mut text, &mut due, &mut completed);
        *i += 1;
        let bullet_indent = lines.get(*i - 1).map(|s| indent(s)).unwrap_or(0);
        while *i < lines.len() {
            let line = lines[*i];
            let ts = line.trim_start();
            if ts.is_empty() {
                *i += 1;
                continue;
            }
            if ts.starts_with("- ") {
                break;
            }
            if indent(line) > bullet_indent {
                let (k, v) = split_kv(ts);
                absorb_task_kv(&k, &v, &mut text, &mut due, &mut completed);
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
    // Explicit frontmatter tasks stay tasks even when dateless; the due date
    // comes only from the item's own `due` (the note's `date:` is not a
    // deadline — convention 0.3).
    let due_date = if is_iso_date(&due) { Some(due) } else { None };
    Some((
        OpenTask {
            line_no: 0,
            text,
            due_date,
        },
        completed,
    ))
}

fn split_kv(line: &str) -> (String, String) {
    match line.find(':') {
        Some(idx) => (
            line[..idx].trim().to_string(),
            line[idx + 1..].trim().trim_matches(['"', '\'']).to_string(),
        ),
        None => (line.to_string(), String::new()),
    }
}

/// Absorb one `key: value` line from a frontmatter task mapping. Only the
/// keys this server surfaces are read; `time` / `completed_at` are ignored.
fn absorb_task_kv(k: &str, v: &str, text: &mut String, due: &mut String, completed: &mut bool) {
    match k {
        "text" => *text = v.to_string(),
        "due" => *due = v.to_string(),
        "completed" => *completed = matches!(v, "true" | "yes" | "True" | "TRUE"),
        _ => {}
    }
}

fn indent(line: &str) -> usize {
    line.bytes()
        .take_while(|b| *b == b' ' || *b == b'\t')
        .count()
}

/// If `s` begins with an unchecked checkbox bullet, return the text after
/// it. Matches `- [ ] `, `* [ ] `, `+ [ ] ` (one ASCII space inside the
/// box, the form the app writes).
fn strip_open_checkbox(s: &str) -> Option<&str> {
    for bullet in ['-', '*', '+'] {
        let prefix = format!("{bullet} [ ] ");
        if let Some(rest) = s.strip_prefix(&prefix) {
            return Some(rest);
        }
    }
    None
}

/// Pull the `📅 YYYY-MM-DD` token out of a task body, if present.
fn extract_due_date(s: &str) -> Option<String> {
    let pos = s.find('📅')?;
    let after = s[pos + '📅'.len_utf8()..].trim_start();
    let date: String = after
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '-')
        .collect();
    if is_iso_date(&date) {
        Some(date)
    } else {
        None
    }
}

/// The note's own `date:` (top-level frontmatter scalar), if shaped like an
/// ISO date. This makes every checkbox in a dated note (daily notes, dated
/// journals) a task under convention 0.3. Mirrors the app's
/// `checkboxes.rs::frontmatter_date`.
fn frontmatter_date(content: &str) -> Option<String> {
    let fm = frontmatter::split(content).0?;
    frontmatter::parse(&fm).into_iter().find_map(|(k, v)| {
        if !k.eq_ignore_ascii_case("date") {
            return None;
        }
        match v {
            frontmatter::PropValue::Scalar(s) if is_iso_date(s.trim()) => {
                Some(s.trim().to_string())
            }
            _ => None,
        }
    })
}

/// `true` when the note carries a top-level `tasks: off` (also `false` /
/// `none` / `no`) frontmatter property. NOTE: the block-list form
/// (`tasks:` followed by `- ...` items) parses as a `List`, not a `Scalar`,
/// so it never trips this — only the scalar opt-out does.
fn note_tasks_disabled(content: &str) -> bool {
    let Some(fm) = frontmatter::split(content).0 else {
        return false;
    };
    frontmatter::parse(&fm).iter().any(|(k, v)| {
        k.eq_ignore_ascii_case("tasks")
            && matches!(
                v,
                frontmatter::PropValue::Scalar(s)
                    if matches!(s.trim().to_lowercase().as_str(), "off" | "false" | "none" | "no")
            )
    })
}

/// Reschedule the single OPEN task whose body contains `needle`: upsert its
/// `📅` date and/or `⏰` time tokens. Each of `new_date` / `new_time` is one
/// of: `""` (leave the token untouched), the literal `"clear"` (remove it),
/// or a value (replace, or append if absent). Errors if zero or more than
/// one open task matches, so we never reschedule the wrong one. Returns
/// `(new_content, the_rewritten_line)`. Mirrors the app's
/// `tasks::rewrite_task_metadata` / `upsert_emoji_token`.
pub fn reschedule_task_in(
    content: &str,
    needle: &str,
    new_date: &str,
    new_time: &str,
) -> Result<(String, String), String> {
    let want = needle.to_lowercase();
    let lines: Vec<&str> = content.lines().collect();
    let matches: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter_map(|(i, line)| {
            strip_open_checkbox(line.trim_start())
                .filter(|rest| rest.to_lowercase().contains(&want))
                .map(|_| i)
        })
        .collect();

    let idx = match matches.len() {
        0 => return Err(format!("No open task matching \"{needle}\".")),
        1 => matches[0],
        n => {
            return Err(format!(
                "{n} open tasks match \"{needle}\" — be more specific."
            ))
        }
    };

    let (prefix, body) = split_open_checkbox_line(lines[idx])
        .ok_or_else(|| "Matched line is not a recognizable open checkbox".to_string())?;
    let mut new_body = body;
    if !new_date.is_empty() {
        let v = if new_date == "clear" { "" } else { new_date };
        new_body = upsert_emoji_token(&new_body, '📅', v);
    }
    if !new_time.is_empty() {
        let v = if new_time == "clear" { "" } else { new_time };
        new_body = upsert_emoji_token(&new_body, '⏰', v);
    }
    let new_line = format!("{prefix}{}", new_body.trim_end());

    let mut new_lines: Vec<String> = lines.iter().map(|s| s.to_string()).collect();
    new_lines[idx] = new_line.clone();
    let mut out = new_lines.join("\n");
    if content.ends_with('\n') && !out.ends_with('\n') {
        out.push('\n');
    }
    Ok((out, new_line))
}

/// Split an open-checkbox line into `(prefix, body)` where `prefix` is
/// everything up to and including `- [ ] ` (indentation + bullet preserved)
/// and `body` is the task text after it. `None` if the line isn't an open
/// checkbox.
fn split_open_checkbox_line(line: &str) -> Option<(String, String)> {
    let indent_len = line
        .bytes()
        .take_while(|b| *b == b' ' || *b == b'\t')
        .count();
    let (indent, rest) = line.split_at(indent_len);
    for bullet in ['-', '*', '+'] {
        let p = format!("{bullet} [ ] ");
        if let Some(body) = rest.strip_prefix(&p) {
            return Some((format!("{indent}{p}"), body.to_string()));
        }
    }
    None
}

/// Insert / replace / delete an emoji-prefixed metadata token in a task
/// body. `value = ""` removes the token (and its emoji); otherwise the
/// token is replaced if present, appended if missing. Other metadata is
/// preserved. Faithful to the app's `tasks::upsert_emoji_token`.
fn upsert_emoji_token(body: &str, emoji: char, value: &str) -> String {
    let emoji_str = emoji.to_string();
    if let Some(p) = body.find(&emoji_str) {
        let before = body[..p].trim_end();
        let after = &body[p + emoji_str.len()..];
        let after_trimmed = after.trim_start();
        let token_end = after_trimmed
            .find(|c: char| c.is_whitespace())
            .unwrap_or(after_trimmed.len());
        let tail = after_trimmed[token_end..].trim_start();
        let mut out = String::with_capacity(body.len() + 16);
        out.push_str(before);
        if !value.is_empty() {
            if !out.is_empty() {
                out.push(' ');
            }
            out.push(emoji);
            out.push(' ');
            out.push_str(value);
        }
        if !tail.is_empty() {
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str(tail);
        }
        out
    } else if value.is_empty() {
        body.to_string()
    } else {
        let mut out = body.trim_end().to_string();
        if !out.is_empty() {
            out.push(' ');
        }
        out.push(emoji);
        out.push(' ');
        out.push_str(value);
        out
    }
}

/// `true` if `s` is a syntactically valid `YYYY-MM-DD` string. Cheap shape
/// check — the indexer is the final arbiter, this just rejects obvious junk.
pub fn is_iso_date(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && b[..4].iter().all(u8::is_ascii_digit)
        && b[5..7].iter().all(u8::is_ascii_digit)
        && b[8..10].iter().all(u8::is_ascii_digit)
}

/// `true` if `s` is a valid 24-hour `HH:MM` string.
pub fn is_hhmm(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() != 5 || b[2] != b':' {
        return false;
    }
    if !(b[0].is_ascii_digit()
        && b[1].is_ascii_digit()
        && b[3].is_ascii_digit()
        && b[4].is_ascii_digit())
    {
        return false;
    }
    let h = (b[0] - b'0') * 10 + (b[1] - b'0');
    let m = (b[3] - b'0') * 10 + (b[4] - b'0');
    h < 24 && m < 60
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fixed "today" so lone-⏰ resolution is deterministic in tests.
    const TODAY: &str = "2026-06-11";

    /// Parse as a *regular* (undated) note: bare checkboxes are NOT tasks
    /// (convention 0.3) — only dated ones are.
    fn open(md: &str) -> Vec<OpenTask> {
        parse_open_tasks(md, None, Some(TODAY))
    }

    /// Parse as a *dated* note (daily note / dated journal): every checkbox
    /// inherits `note_date`.
    fn open_dated(md: &str, note_date: &str) -> Vec<OpenTask> {
        parse_open_tasks(md, Some(note_date), Some(TODAY))
    }

    #[test]
    fn task_line_plain() {
        assert_eq!(format_task_line("Call Ravi", None, None), "- [ ] Call Ravi");
    }

    #[test]
    fn task_line_with_date() {
        assert_eq!(
            format_task_line("Call Ravi", Some("2026-06-05"), None),
            "- [ ] Call Ravi 📅 2026-06-05"
        );
    }

    #[test]
    fn task_line_with_date_and_time() {
        assert_eq!(
            format_task_line("Standup", Some("2026-06-05"), Some("10:00")),
            "- [ ] Standup 📅 2026-06-05 ⏰ 10:00"
        );
    }

    #[test]
    fn task_line_drops_lone_time() {
        // A time without a date is meaningless to the indexer — never emit it.
        assert_eq!(
            format_task_line("Standup", None, Some("10:00")),
            "- [ ] Standup"
        );
    }

    #[test]
    fn daily_template_matches_app() {
        // Pin the exact bytes the app's daily_note.rs:52 produces.
        let t = daily_note_template("2026-06-01", "Monday");
        assert_eq!(
            t,
            "---\ndate: 2026-06-01\ntype: daily\ntags:\n  - daily\n---\n\n# Monday, 2026-06-01\n\n## Tasks\n\n- [ ] \n\n## Notes\n\n\n\n## Journal\n\n\n"
        );
    }

    #[test]
    fn render_daily_template_substitutes_all_placeholders() {
        let out =
            render_daily_template("{{date}} / {{weekday}} / {{title}}", "2026-06-01", "Monday");
        assert_eq!(out, "2026-06-01 / Monday / Monday, 2026-06-01");
    }

    #[test]
    fn render_daily_template_leaves_unknown_tokens_untouched() {
        let out = render_daily_template("{{date}} {{bogus}}", "2026-06-01", "Monday");
        assert_eq!(out, "2026-06-01 {{bogus}}");
    }

    #[test]
    fn pick_daily_template_uses_custom_when_present() {
        let out = pick_daily_template(
            Some("# {{title}}\n\nHabit: [ ] water\n"),
            "2026-06-01",
            "Monday",
        );
        assert_eq!(out, "# Monday, 2026-06-01\n\nHabit: [ ] water\n");
    }

    #[test]
    fn pick_daily_template_falls_back_when_absent_or_blank() {
        let default = daily_note_template("2026-06-01", "Monday");
        assert_eq!(pick_daily_template(None, "2026-06-01", "Monday"), default);
        assert_eq!(
            pick_daily_template(Some("   \n\n  "), "2026-06-01", "Monday"),
            default
        );
    }

    #[test]
    fn append_into_existing_daily_note() {
        let content = daily_note_template("2026-06-01", "Monday");
        let out = append_under_tasks(&content, "- [ ] New task 📅 2026-06-02");
        // The new task lands inside the Tasks section, before ## Notes,
        // and the placeholder empty task is preserved (app behaviour).
        assert!(out.contains("- [ ] \n- [ ] New task 📅 2026-06-02\n\n## Notes"));
        // Did not leak into Notes/Journal.
        assert_eq!(out.matches("## Notes").count(), 1);
    }

    #[test]
    fn append_creates_tasks_section_when_missing() {
        let out = append_under_tasks("# Inbox\n\nsome text\n", "- [ ] Buy milk");
        assert!(out.ends_with("## Tasks\n\n- [ ] Buy milk\n"));
    }

    #[test]
    fn append_into_empty_string() {
        let out = append_under_tasks("", "- [ ] First");
        assert_eq!(out, "## Tasks\n\n- [ ] First");
    }

    #[test]
    fn parse_open_tasks_finds_unchecked_only() {
        // In a dated note every open checkbox is a task; the completed one is
        // still excluded. An inline 📅 sets a due date; checkboxes that are
        // tasks only via the note's date carry NO due date.
        let md = "## Tasks\n\n- [ ] open one 📅 2026-06-05\n- [x] done\n- [ ] open two\n* [ ] bullet star\n";
        let tasks = open_dated(md, "2026-06-11");
        assert_eq!(tasks.len(), 3);
        assert_eq!(tasks[0].text, "open one 📅 2026-06-05");
        assert_eq!(tasks[0].due_date.as_deref(), Some("2026-06-05"));
        assert_eq!(tasks[1].text, "open two");
        assert_eq!(tasks[1].due_date, None); // dated note → task, no due date
        assert_eq!(tasks[2].text, "bullet star");
        assert_eq!(tasks[2].due_date, None);
        assert_eq!(tasks[0].line_no, 3);
    }

    #[test]
    fn undated_checkbox_in_regular_note_is_not_a_task() {
        // Convention 0.3: no effective date ⇒ plain checklist item.
        assert!(open("- [ ] open two\n* [ ] bullet star\n").is_empty());
    }

    #[test]
    fn lone_time_resolves_to_today() {
        let tasks = open("- [ ] Standup ⏰ 09:00\n- [ ] undated\n");
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].text, "Standup ⏰ 09:00");
        assert_eq!(tasks[0].due_date.as_deref(), Some("2026-06-11"));
    }

    #[test]
    fn parse_skips_empty_placeholder() {
        // The daily-note template's bare `- [ ] ` must not count as a task.
        let tasks = open(&daily_note_template("2026-06-01", "Monday"));
        assert!(tasks.is_empty());
    }

    #[test]
    fn parse_skips_fenced_code_block() {
        // A checkbox inside a ``` fence is a code sample, not a task — the
        // app skips it, so we must too. The real task needs a date to qualify.
        let md = "```\n- [ ] not a task\n```\n- [ ] real task\n";
        let tasks = open_dated(md, "2026-06-11");
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].text, "real task");
    }

    #[test]
    fn parse_skips_tilde_fenced_block() {
        let md = "~~~\n- [ ] fenced out\n~~~\n- [ ] counts\n";
        let tasks = open_dated(md, "2026-06-11");
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].text, "counts");
    }

    #[test]
    fn parse_skips_frontmatter_region() {
        // A `- something` line inside YAML frontmatter (e.g. a tags list) is
        // not a body checkbox, and a completed body task is excluded. The note
        // date makes the open body checkbox a task.
        let md = "---\ntitle: Note\ntags:\n  - daily\n---\n\n- [ ] task one\n- [x] task two ✅ 2026-05-01\n";
        let tasks = open_dated(md, "2026-06-11");
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].text, "task one");
        // Body line number is counted over the WHOLE file (frontmatter incl.).
        assert_eq!(tasks[0].line_no, 7);
    }

    #[test]
    fn frontmatter_date_makes_body_checkboxes_visible_tasks_without_due() {
        // A top-level `date:` makes the checkboxes tasks (no caller note-date
        // needed), but with NO due date — the note's date isn't a deadline.
        let md = "---\ndate: 2026-04-05\n---\n\n- [ ] Protein hit\n- [ ] Steps hit 📅 2026-04-05\n";
        let tasks = open(md);
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].due_date, None); // inherited → no due date
        assert_eq!(tasks[1].due_date.as_deref(), Some("2026-04-05")); // inline 📅
    }

    #[test]
    fn parse_frontmatter_tasks_block() {
        // Top-level `tasks:` mapping items become open tasks (line_no 0). They
        // stay tasks even without a date; a dateless one with no note date is
        // due-less.
        let md = "---\ntitle: Sprint\ntasks:\n  - text: \"Review architecture\"\n    due: 2026-05-02\n  - text: \"Reply to Ravi\"\n---\n\nbody\n";
        let tasks = open(md);
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].text, "Review architecture");
        assert_eq!(tasks[0].due_date.as_deref(), Some("2026-05-02"));
        assert_eq!(tasks[0].line_no, 0);
        assert_eq!(tasks[1].text, "Reply to Ravi");
        assert_eq!(tasks[1].due_date, None);
    }

    #[test]
    fn parse_frontmatter_tasks_skips_completed() {
        let md = "---\ntasks:\n  - text: \"done one\"\n    completed: true\n  - text: \"still open\"\n---\nbody\n";
        let tasks = open(md);
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].text, "still open");
    }

    #[test]
    fn parse_ignores_nested_and_flow_tasks_key() {
        // Indented `tasks:` under a parent key, and flow-form `tasks: [..]`,
        // are not the top-level block list (matches the app's AUDIT-019).
        let nested = "---\nconfig:\n  tasks:\n    - text: \"nope\"\n---\nbody\n";
        assert!(open(nested).is_empty());
        let flow = "---\ntasks: [a, b, c]\n---\nbody\n";
        assert!(open(flow).is_empty());
    }

    #[test]
    fn tasks_off_disables_note() {
        let md = "---\ndate: 2026-06-11\ntasks: off\n---\n\n- [ ] should be hidden\n";
        assert!(open(md).is_empty());
        // The block-list `tasks:` form is the task array, NOT the opt-out.
        let block = "---\ntasks:\n  - text: \"keep me\"\n---\nbody\n";
        assert_eq!(open(block).len(), 1);
    }

    #[test]
    fn reschedule_upserts_date_and_time() {
        let md = "## Tasks\n\n- [ ] standup\n";
        let (out, line) = reschedule_task_in(md, "standup", "2026-06-05", "10:00").unwrap();
        assert_eq!(line, "- [ ] standup 📅 2026-06-05 ⏰ 10:00");
        assert!(out.contains("- [ ] standup 📅 2026-06-05 ⏰ 10:00"));
    }

    #[test]
    fn reschedule_replaces_existing_and_clears() {
        let md = "- [ ] call 📅 2026-06-05 ⏰ 10:00\n";
        let (out1, _) = reschedule_task_in(md, "call", "2026-06-09", "").unwrap();
        assert!(out1.contains("- [ ] call 📅 2026-06-09 ⏰ 10:00"));
        // "clear" drops the 📅 token but keeps ⏰.
        let (out2, _) = reschedule_task_in(md, "call", "clear", "").unwrap();
        assert!(!out2.contains("📅"), "date token should be gone: {out2}");
        assert!(out2.contains("⏰ 10:00"));
    }

    #[test]
    fn reschedule_errors_on_zero_or_many() {
        let md = "- [ ] one\n- [ ] one again\n";
        assert!(reschedule_task_in(md, "nope", "2026-06-05", "").is_err());
        assert!(reschedule_task_in(md, "one", "2026-06-05", "").is_err());
    }

    #[test]
    fn tasks_off_does_not_break_golden_block_form() {
        // Guard the interaction: a note with BOTH a scalar opt-out is empty,
        // but a normal note is unaffected.
        assert!(open("---\ntasks: none\n---\n- [ ] x\n").is_empty());
        assert_eq!(open("- [ ] plain 📅 2026-06-12\n").len(), 1);
    }

    #[test]
    fn iso_date_validation() {
        assert!(is_iso_date("2026-06-01"));
        assert!(!is_iso_date("2026-6-1"));
        assert!(!is_iso_date("06/01/2026"));
        assert!(!is_iso_date("2026-06-01T00:00"));
    }

    #[test]
    fn hhmm_validation() {
        assert!(is_hhmm("09:30"));
        assert!(is_hhmm("23:59"));
        assert!(!is_hhmm("24:00"));
        assert!(!is_hhmm("9:30"));
        assert!(!is_hhmm("09:60"));
    }

    #[test]
    fn append_under_arbitrary_heading() {
        let md = "# Note\n\n## Notes\n\nexisting\n\n## Journal\n";
        let out = append_under_heading(md, "Notes", "> a reminder line");
        assert!(out.contains("existing\n> a reminder line\n\n## Journal"));
        assert_eq!(out.matches("## Journal").count(), 1);
    }

    #[test]
    fn append_to_end_separates_with_blank_line() {
        assert_eq!(
            append_to_end("# Note\n\nbody", "more"),
            "# Note\n\nbody\n\nmore\n"
        );
        assert_eq!(append_to_end("", "first"), "first\n");
    }

    #[test]
    fn complete_task_flips_single_match() {
        let md = "## Tasks\n\n- [ ] call Ravi 📅 2026-06-05\n- [ ] buy milk\n";
        let (out, line) = complete_task_in(md, "ravi", "2026-06-01").unwrap();
        assert_eq!(line, "- [x] call Ravi 📅 2026-06-05 ✅ 2026-06-01");
        assert!(out.contains("- [x] call Ravi 📅 2026-06-05 ✅ 2026-06-01"));
        assert!(out.contains("- [ ] buy milk")); // untouched
    }

    #[test]
    fn complete_task_errors_on_zero_or_many() {
        let md = "- [ ] task one\n- [ ] task two\n";
        assert!(complete_task_in(md, "nope", "2026-06-01").is_err());
        assert!(complete_task_in(md, "task", "2026-06-01").is_err()); // 2 matches
    }

    /// Shared golden corpus — the SAME fixtures + expectations are asserted
    /// against the app's `parse_tasks` by a sibling test in
    /// `src-tauri/src/parser/checkboxes.rs`. If MCP's open-task parsing ever
    /// drifts from the app's, one of the two fails. This is what makes the
    /// README's "lock-step with the app" claim actually true.
    #[test]
    fn golden_corpus_matches_shared_fixtures() {
        use std::path::Path;
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures");
        let raw = std::fs::read_to_string(dir.join("expected.json")).expect("read expected.json");
        let expected: serde_json::Value = serde_json::from_str(&raw).expect("parse expected.json");

        for (file, items) in expected.as_object().unwrap() {
            if file.starts_with('_') {
                continue; // metadata keys, not fixtures
            }
            let content = std::fs::read_to_string(dir.join(file)).expect(file);
            // Content-only contract: no caller note-date, fixed today. Each
            // fixture carries its own date context (inline 📅 / frontmatter
            // `date:` / lone ⏰).
            let open = parse_open_tasks(&content, None, Some(TODAY));
            let exp = items.as_array().unwrap();
            assert_eq!(
                open.len(),
                exp.len(),
                "{file}: open-task count mismatch; got {open:?}"
            );
            for e in exp {
                let contains = e["contains"].as_str().unwrap();
                let due = e["due"].as_str().unwrap();
                let n = open
                    .iter()
                    .filter(|t| {
                        t.text.contains(contains) && t.due_date.as_deref().unwrap_or("") == due
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
