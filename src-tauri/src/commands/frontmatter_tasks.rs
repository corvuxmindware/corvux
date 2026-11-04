//! Frontmatter task rewrite helpers (Phase 2.2 polish — close the
//! "frontmatter tasks can't be toggled" gap from Phase 2.1).
//!
//! Mirrors the body-task helpers in `commands::tasks` but operates on
//! the YAML `tasks:` array inside `---` frontmatter. Mutations live
//! here rather than in the parser because the parser is read-only by
//! design (`parse_tasks` is also a public Tauri command — keeping it
//! pure makes preview-parsing safe). This module is the write side.
//!
//! Shape we support — same as `parser::checkboxes::parse_frontmatter_tasks`:
//!
//! ```yaml
//! ---
//! tasks:
//!   - text: "Review architecture doc"
//!     due: 2026-05-08
//!     time: "10:00"
//!     completed: true       # added by toggle path
//!     completed_at: 2026-05-09
//!   - "Friday: review"        # inline string form (text only)
//! ---
//! ```
//!
//! The parser also accepts the inline form. When toggling completion on
//! an inline-form task we promote it to the mapping form so we have a
//! place to put `completed: true`.

/// What we want to do to a frontmatter task. Mirrors the body-task
/// mutation surface (`rewrite_checkbox`, `rewrite_task_text`, etc.) so
/// the agent tools' dispatch is symmetric.
#[derive(Debug, Clone)]
pub enum FrontmatterMutation {
    /// Flip `completed:` between true/false. Adds `completed_at: <today>`
    /// when marking done; strips it when reopening.
    ToggleCompleted(bool),
    /// Replace the entire `text:` value verbatim.
    UpdateText(String),
    /// Update `due:` and/or `time:`. Empty string = leave unchanged;
    /// the literal `"clear"` removes that key.
    Reschedule { new_date: String, new_time: String },
    /// Remove the entire item (its bullet line + indented continuations).
    Delete,
}

/// Find the frontmatter region. Returns `(open_end, close_start)` in
/// byte offsets — the region between is the YAML body proper. None if
/// the file has no frontmatter or it's unterminated.
fn frontmatter_region(content: &str) -> Option<(usize, usize)> {
    let after_open = if content.starts_with("---\r\n") {
        5
    } else if content.starts_with("---\n") {
        4
    } else {
        return None;
    };
    let rest = &content[after_open..];
    let mut line_start = 0;
    for line in rest.split_inclusive('\n') {
        let trimmed_end = line.trim_end_matches(['\r', '\n']);
        if trimmed_end == "---" || trimmed_end == "..." {
            return Some((after_open, after_open + line_start));
        }
        line_start += line.len();
    }
    None
}

fn indent(line: &str) -> usize {
    line.bytes()
        .take_while(|b| *b == b' ' || *b == b'\t')
        .count()
}

/// Anchor match for a frontmatter task: case-sensitive prefix on the
/// task's `text` value (after stripping quotes). Mirrors the body-task
/// `line_body_starts_with` heuristic so the agent's `text_anchor` arg
/// works the same way for both.
fn item_text_starts_with(item_text: &str, anchor: &str) -> bool {
    item_text.trim_matches(['"', '\'']).starts_with(anchor)
}

/// One parsed frontmatter task item with its line range. We record the
/// indices into a `Vec<&str>` so we can splice replacement lines back
/// in without re-scanning the whole file.
#[derive(Debug)]
struct ItemRange {
    /// 0-based line index of the `- ...` bullet line within the
    /// frontmatter body (NOT the file).
    start: usize,
    /// One past the last line that belongs to this item (exclusive).
    end: usize,
    /// The `text:` value, used for anchor matching.
    text: String,
    /// True when the original was the inline string form (`- "foo"`),
    /// so toggling completion has to promote it to mapping form.
    is_inline: bool,
    /// Indent of the bullet line — needed when emitting new keys.
    bullet_indent: usize,
}

/// Walk the frontmatter body and collect every item under the first
/// `tasks:` block. We don't support multiple `tasks:` blocks in MVP
/// (the parser doesn't either).
fn collect_items(fm_lines: &[&str]) -> Option<Vec<ItemRange>> {
    let mut tasks_idx: Option<usize> = None;
    for (i, line) in fm_lines.iter().enumerate() {
        // AUDIT-019: top-level `tasks:` only, exact match. Rejects
        // `sub_tasks:`, `customer_tasks:`, any indented `tasks:` nested
        // under a parent key, and the inline-flow form `tasks: [a, b]`.
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed == "tasks:" {
            tasks_idx = Some(i);
            break;
        }
    }
    let start_after_tasks = tasks_idx? + 1;

    let mut items: Vec<ItemRange> = Vec::new();
    let mut i = start_after_tasks;
    while i < fm_lines.len() {
        let raw = fm_lines[i];
        let t = raw.trim_start();
        if t.is_empty() {
            i += 1;
            continue;
        }
        let bullet_indent = indent(raw);
        if !(t.starts_with("- ") || t.starts_with("-\t")) {
            // Hit a sibling key in the frontmatter — `tasks:` block over.
            break;
        }

        let item_body = t.trim_start_matches("- ").trim();
        let has_colon =
            item_body.contains(':') && !item_body.starts_with('"') && !item_body.starts_with('\'');

        let item_start = i;
        let mut text = String::new();
        let is_inline;
        if has_colon {
            is_inline = false;
            // First line is a `key: value` part of the mapping.
            let (k, v) = split_kv(item_body);
            if k == "text" {
                text = strip_quotes(&v);
            }
            i += 1;
            // Continuation lines: indented past the bullet.
            while i < fm_lines.len() {
                let line = fm_lines[i];
                let ts = line.trim_start();
                if ts.is_empty() {
                    i += 1;
                    continue;
                }
                if ts.starts_with("- ") || ts.starts_with("-\t") {
                    break;
                }
                if indent(line) > bullet_indent {
                    let (k, v) = split_kv(ts);
                    if k == "text" {
                        text = strip_quotes(&v);
                    }
                    i += 1;
                } else {
                    break;
                }
            }
        } else {
            is_inline = true;
            text = strip_quotes(item_body);
            i += 1;
        }

        // Walk past trailing blank lines that belong to this item.
        let mut end = i;
        while end > item_start + 1 && fm_lines[end - 1].trim().is_empty() {
            end -= 1;
        }

        items.push(ItemRange {
            start: item_start,
            end,
            text,
            is_inline,
            bullet_indent,
        });
    }
    Some(items)
}

fn split_kv(line: &str) -> (String, String) {
    if let Some(idx) = line.find(':') {
        let k = line[..idx].trim().to_string();
        let v = line[idx + 1..].trim().to_string();
        (k, v)
    } else {
        (line.to_string(), String::new())
    }
}

fn strip_quotes(v: &str) -> String {
    v.trim().trim_matches(['"', '\'']).to_string()
}

/// Render a YAML scalar, quoting only when needed (contains `:`, `#`,
/// leading/trailing whitespace, or starts with a YAML special char).
/// Keeps round-tripped output from drifting into a mess of unnecessary
/// quotes on every save.
fn yaml_scalar(v: &str) -> String {
    let needs_quote = v.is_empty()
        || v.contains(':')
        || v.contains('#')
        || v.contains('"')
        || v.contains('\'')
        || v.starts_with(['[', '{', '!', '&', '*', '|', '>', '%', '@', '`'])
        || v.starts_with(' ')
        || v.ends_with(' ');
    if needs_quote {
        let escaped = v.replace('\\', "\\\\").replace('"', "\\\"");
        format!("\"{escaped}\"")
    } else {
        v.to_string()
    }
}

/// Apply a mutation to the frontmatter task whose `text:` starts with
/// `anchor`. Returns the new file content.
///
/// Errors when:
/// - The file has no frontmatter, or the frontmatter is unterminated.
/// - No `tasks:` block exists.
/// - No item matches the anchor, or more than one does.
pub fn rewrite_frontmatter_task(
    content: &str,
    anchor: &str,
    mutation: FrontmatterMutation,
) -> Result<String, String> {
    let (open_end, close_start) = frontmatter_region(content)
        .ok_or_else(|| "Note has no frontmatter — frontmatter task ops only apply to files with `---` frontmatter".to_string())?;
    let fm_body = &content[open_end..close_start];
    // Don't drop the trailing newline of the last line — split_inclusive
    // keeps it so we can rebuild byte-exact.
    let fm_lines: Vec<&str> = fm_body.split_inclusive('\n').collect();
    // For matching/searching we use trim-end-matches versions; keep the
    // raw lines as-is for splicing.
    let raw_lines: Vec<&str> = fm_lines.clone();
    // Trimmed view (what `collect_items` walks).
    let trimmed_lines: Vec<&str> = raw_lines
        .iter()
        .map(|l| l.trim_end_matches(['\r', '\n']))
        .collect();

    let items = collect_items(&trimmed_lines)
        .ok_or_else(|| "No `tasks:` block in frontmatter".to_string())?;

    let matches: Vec<&ItemRange> = items
        .iter()
        .filter(|it| item_text_starts_with(&it.text, anchor))
        .collect();
    let target = match matches.len() {
        0 => {
            return Err(format!("No frontmatter task matches anchor \"{anchor}\""));
        }
        1 => matches[0],
        n => {
            return Err(format!(
                "{n} frontmatter tasks match anchor \"{anchor}\" — use a longer anchor to disambiguate"
            ));
        }
    };

    // Build replacement lines for the target item.
    let new_item_lines = match &mutation {
        FrontmatterMutation::Delete => Vec::<String>::new(),
        FrontmatterMutation::ToggleCompleted(completed) => {
            apply_toggle(target, &raw_lines, *completed)
        }
        FrontmatterMutation::UpdateText(new_text) => {
            apply_update_text(target, &raw_lines, new_text)
        }
        FrontmatterMutation::Reschedule { new_date, new_time } => {
            apply_reschedule(target, &raw_lines, new_date, new_time)
        }
    };

    // Splice new_item_lines into raw_lines at [target.start..target.end).
    let mut out_fm = String::with_capacity(fm_body.len() + 64);
    for line in &raw_lines[..target.start] {
        out_fm.push_str(line);
    }
    for line in &new_item_lines {
        out_fm.push_str(line);
        if !line.ends_with('\n') {
            out_fm.push('\n');
        }
    }
    for line in &raw_lines[target.end..] {
        out_fm.push_str(line);
    }

    let mut full = String::with_capacity(content.len() + 64);
    full.push_str(&content[..open_end]);
    full.push_str(&out_fm);
    full.push_str(&content[close_start..]);
    Ok(full)
}

/// Toggle `completed:` on the target. If the original was inline-form
/// (`- "Friday: review"`) we promote it to mapping form so there's a
/// place to put the new key.
fn apply_toggle(target: &ItemRange, raw_lines: &[&str], completed: bool) -> Vec<String> {
    // Pull the existing keys + values.
    let bullet_indent = " ".repeat(target.bullet_indent);
    let key_indent = " ".repeat(target.bullet_indent + 2);
    let today = today_iso_local();

    let mut keys = read_existing_keys(target, raw_lines);
    if target.is_inline {
        // Promote: only `text` is set today.
        keys.insert(
            "text".to_string(),
            keys.get("text")
                .cloned()
                .unwrap_or_else(|| target.text.clone()),
        );
    }

    if completed {
        keys.insert("completed".to_string(), "true".to_string());
        keys.entry("completed_at".to_string()).or_insert(today);
    } else {
        keys.remove("completed");
        keys.remove("completed_at");
    }

    render_mapping_item(&bullet_indent, &key_indent, &keys)
}

fn apply_update_text(target: &ItemRange, raw_lines: &[&str], new_text: &str) -> Vec<String> {
    let bullet_indent = " ".repeat(target.bullet_indent);
    let key_indent = " ".repeat(target.bullet_indent + 2);
    let mut keys = read_existing_keys(target, raw_lines);
    keys.insert("text".to_string(), new_text.to_string());
    render_mapping_item(&bullet_indent, &key_indent, &keys)
}

fn apply_reschedule(
    target: &ItemRange,
    raw_lines: &[&str],
    new_date: &str,
    new_time: &str,
) -> Vec<String> {
    let bullet_indent = " ".repeat(target.bullet_indent);
    let key_indent = " ".repeat(target.bullet_indent + 2);
    let mut keys = read_existing_keys(target, raw_lines);

    if !new_date.is_empty() {
        if new_date == "clear" {
            keys.remove("due");
        } else {
            keys.insert("due".to_string(), new_date.to_string());
        }
    }
    if !new_time.is_empty() {
        if new_time == "clear" {
            keys.remove("time");
        } else {
            keys.insert("time".to_string(), new_time.to_string());
        }
    }

    render_mapping_item(&bullet_indent, &key_indent, &keys)
}

/// Read the existing key/value pairs out of an item's lines. Inline-form
/// items get `text` synthesized from `target.text` so subsequent
/// rendering can promote them to mapping form.
fn read_existing_keys(
    target: &ItemRange,
    raw_lines: &[&str],
) -> std::collections::BTreeMap<String, String> {
    use std::collections::BTreeMap;
    let mut keys: BTreeMap<String, String> = BTreeMap::new();
    if target.is_inline {
        keys.insert("text".to_string(), target.text.clone());
        return keys;
    }
    for line in raw_lines.iter().take(target.end).skip(target.start) {
        let line = *line;
        let trimmed_end = line.trim_end_matches(['\r', '\n']);
        let ts = trimmed_end.trim_start();
        let ts = ts.trim_start_matches("- ").trim_start_matches("-\t");
        if let Some(idx) = ts.find(':') {
            let k = ts[..idx].trim().to_string();
            let v = ts[idx + 1..].trim().to_string();
            let v = strip_quotes(&v);
            keys.insert(k, v);
        }
    }
    keys
}

/// Emit a mapping-form item: `- text: ...` then continuation lines
/// `  due: ...`, `  time: ...`, `  completed: true`, `  completed_at: ...`.
/// Render order is deterministic to keep diffs minimal across rewrites:
/// text → due → time → completed → completed_at → other keys (sorted).
fn render_mapping_item(
    bullet_indent: &str,
    key_indent: &str,
    keys: &std::collections::BTreeMap<String, String>,
) -> Vec<String> {
    const ORDER: &[&str] = &["text", "due", "time", "completed", "completed_at"];
    let mut lines: Vec<String> = Vec::new();

    let mut emit = |k: &str, v: &str, first: &mut bool| {
        let scalar = if k == "completed" {
            // Bool literal — never quote.
            v.to_string()
        } else {
            yaml_scalar(v)
        };
        if *first {
            lines.push(format!("{bullet_indent}- {k}: {scalar}\n"));
            *first = false;
        } else {
            lines.push(format!("{key_indent}{k}: {scalar}\n"));
        }
    };

    let mut first = true;
    for k in ORDER {
        if let Some(v) = keys.get(*k) {
            emit(k, v, &mut first);
        }
    }
    for (k, v) in keys.iter() {
        if ORDER.contains(&k.as_str()) {
            continue;
        }
        emit(k, v, &mut first);
    }
    lines
}

fn today_iso_local() -> String {
    use chrono::Local;
    Local::now().format("%Y-%m-%d").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> String {
        "---\n\
         title: Test\n\
         tasks:\n\
         \x20\x20- text: \"Review architecture\"\n\
         \x20\x20\x20\x20due: 2026-05-08\n\
         \x20\x20- text: \"Reply to Ravi\"\n\
         \x20\x20\x20\x20due: 2026-05-09\n\
         \x20\x20\x20\x20time: \"10:00\"\n\
         ---\n\
         \n\
         body content\n"
            .to_string()
    }

    #[test]
    fn toggle_complete_adds_completed_and_stamp() {
        let out = rewrite_frontmatter_task(
            &sample(),
            "Review architecture",
            FrontmatterMutation::ToggleCompleted(true),
        )
        .unwrap();
        assert!(out.contains("completed: true"));
        assert!(out.contains("completed_at:"));
        // Other tasks untouched.
        assert!(out.contains("Reply to Ravi"));
    }

    #[test]
    fn toggle_uncomplete_strips_keys() {
        let with_done = "---\n\
            tasks:\n\
            \x20\x20- text: foo\n\
            \x20\x20\x20\x20completed: true\n\
            \x20\x20\x20\x20completed_at: 2026-05-01\n\
            ---\n\
            body\n";
        let out = rewrite_frontmatter_task(
            with_done,
            "foo",
            FrontmatterMutation::ToggleCompleted(false),
        )
        .unwrap();
        assert!(!out.contains("completed: true"));
        assert!(!out.contains("completed_at:"));
        assert!(out.contains("text: foo"));
    }

    #[test]
    fn update_text_replaces_value() {
        let out = rewrite_frontmatter_task(
            &sample(),
            "Review",
            FrontmatterMutation::UpdateText("Review architecture v2".to_string()),
        )
        .unwrap();
        assert!(out.contains("Review architecture v2"));
        assert!(!out.contains("text: \"Review architecture\"\n"));
    }

    #[test]
    fn reschedule_updates_due_and_time() {
        let out = rewrite_frontmatter_task(
            &sample(),
            "Reply",
            FrontmatterMutation::Reschedule {
                new_date: "2026-05-15".to_string(),
                new_time: "14:30".to_string(),
            },
        )
        .unwrap();
        assert!(out.contains("due: 2026-05-15"));
        assert!(out.contains("time: \"14:30\""));
        // Other task left alone.
        assert!(out.contains("due: 2026-05-08"));
    }

    #[test]
    fn reschedule_clear_removes_keys() {
        let out = rewrite_frontmatter_task(
            &sample(),
            "Reply",
            FrontmatterMutation::Reschedule {
                new_date: "clear".to_string(),
                new_time: "clear".to_string(),
            },
        )
        .unwrap();
        // Reply's keys gone, Review's untouched.
        assert!(!out.contains("due: 2026-05-09"));
        assert!(!out.contains("time: \"10:00\""));
        assert!(out.contains("due: 2026-05-08"));
    }

    #[test]
    fn delete_removes_the_item_only() {
        let out =
            rewrite_frontmatter_task(&sample(), "Reply", FrontmatterMutation::Delete).unwrap();
        assert!(!out.contains("Reply to Ravi"));
        assert!(out.contains("Review architecture"));
    }

    #[test]
    fn inline_form_promoted_on_toggle() {
        let inline = "---\n\
            tasks:\n\
            \x20\x20- \"Friday: review\"\n\
            ---\n\
            body\n";
        let out =
            rewrite_frontmatter_task(inline, "Friday", FrontmatterMutation::ToggleCompleted(true))
                .unwrap();
        // Promoted to mapping form with text + completed.
        assert!(out.contains("text:"));
        assert!(out.contains("completed: true"));
    }

    #[test]
    fn errors_when_no_frontmatter() {
        let out = rewrite_frontmatter_task(
            "no frontmatter here\n",
            "anything",
            FrontmatterMutation::ToggleCompleted(true),
        );
        assert!(out.is_err());
    }

    #[test]
    fn errors_on_ambiguous_anchor() {
        let amb = "---\n\
            tasks:\n\
            \x20\x20- text: foo bar\n\
            \x20\x20- text: foo baz\n\
            ---\n\
            body\n";
        let err = rewrite_frontmatter_task(amb, "foo", FrontmatterMutation::ToggleCompleted(true))
            .unwrap_err();
        assert!(err.contains("disambiguate"));
    }

    #[test]
    fn errors_when_anchor_not_found() {
        let err = rewrite_frontmatter_task(
            &sample(),
            "ghost",
            FrontmatterMutation::ToggleCompleted(true),
        )
        .unwrap_err();
        assert!(err.contains("ghost"));
    }
}
