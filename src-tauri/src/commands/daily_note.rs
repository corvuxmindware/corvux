//! Daily-note command and its date arithmetic. Pulled out of `files.rs`
//! so the civil-days algorithm + weekday mapping don't clutter core
//! file-CRUD reading.

use chrono::{Datelike, Local};

use tauri::{AppHandle, Manager, State};

use super::files::RoostFsState;

/// DN-1: roost-relative path of the user's optional daily-note template.
/// Lives under the hidden `.corvux/` technical folder (Locked Decision #15)
/// but its contents are the user's to edit.
const DAILY_TEMPLATE_REL: &str = ".corvux/templates/daily.md";

/// NEW-DN2: roost-portable settings file the frontend writes (see
/// `src/stores/roostSettings.ts`). The daily-notes folder lives at
/// `tasks.daily_note_folder`. Reading it here makes the setting the source of
/// truth for EVERY caller of `create_daily_note` — AI tools, MCP, mobile
/// shortcuts — not just the frontend that happens to pass the right `folder`.
const ROOST_SETTINGS_REL: &str = ".corvux/settings.json";

/// The shipped default when neither the settings file nor the caller supplies a
/// folder. Must match `defaultTasksSettings()` in `roostSettings.ts`.
const DEFAULT_DAILY_FOLDER: &str = "Daily Notes";

/// NEW-DN2: resolve the effective daily-notes folder. The roost-portable
/// `tasks.daily_note_folder` setting wins; if the file is missing, malformed,
/// or the field is absent/blank, fall back to the caller-supplied `folder`
/// (the frontend's convention); if THAT is blank too, the built-in default.
///
/// Pure over its inputs so it's unit-testable without a Tauri/RoostFs harness.
fn resolve_daily_folder(settings_json: Option<&str>, fallback: &str) -> String {
    if let Some(raw) = settings_json {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(raw) {
            if let Some(f) = v
                .get("tasks")
                .and_then(|t| t.get("daily_note_folder"))
                .and_then(|d| d.as_str())
            {
                let f = f.trim();
                if !f.is_empty() {
                    return f.to_string();
                }
            }
        }
    }
    let fb = fallback.trim();
    if !fb.is_empty() {
        return fb.to_string();
    }
    DEFAULT_DAILY_FOLDER.to_string()
}

/// The built-in daily-note layout, used when the user hasn't supplied their
/// own `.corvux/templates/daily.md`. Pure so it can be unit-tested and reused
/// as the render fallback.
fn default_daily_template(date_str: &str, weekday: &str) -> String {
    format!(
        "---\ndate: {date_str}\ntype: daily\ntags:\n  - daily\n---\n\n# {weekday}, {date_str}\n\n## Tasks\n\n- [ ] \n\n## Notes\n\n\n\n## Journal\n\n\n"
    )
}

/// DN-1: render a user template by substituting the supported placeholders.
/// Deliberately tiny — Obsidian/Tana-style templating (cursor, prompts,
/// includes) is out of scope; we support the handful a daily note needs:
///   `{{date}}`     → `YYYY-MM-DD`
///   `{{weekday}}`  → full weekday name (e.g. `Monday`)
///   `{{title}}`    → `Weekday, YYYY-MM-DD` (the default header line)
/// Unknown `{{...}}` tokens are left untouched so a typo is visible in the
/// note rather than silently eaten.
fn render_daily_template(raw: &str, date_str: &str, weekday: &str) -> String {
    raw.replace("{{date}}", date_str)
        .replace("{{weekday}}", weekday)
        .replace("{{title}}", &format!("{weekday}, {date_str}"))
}

/// DN-1 + NEW-DN1: choose the daily-note body. Renders the user's template
/// when present and non-blank; otherwise (missing, unreadable, or
/// empty/whitespace-only) falls back to the built-in default. Pure so the
/// "empty template must not produce a blank note" rule has a test.
fn pick_daily_template(user_template: Option<&str>, date_str: &str, weekday: &str) -> String {
    match user_template {
        Some(raw) if !raw.trim().is_empty() => render_daily_template(raw, date_str, weekday),
        _ => default_daily_template(date_str, weekday),
    }
}

/// Create or open today's daily note. Returns the display path
/// (display-root-prefixed). Creates `{roost}/{folder}/YYYY-MM-DD.md`
/// with a template if missing.
///
/// AUDIT-037: this command no longer accepts a frontend-supplied roost
/// path as a fallback. A roost MUST be open (registered in
/// [`RoostFsState`]) for it to succeed — same contract as `read_file`
/// and `write_file`. The previous fallback wrote via `std::fs` with
/// zero path validation, which let a compromised renderer create files
/// anywhere on disk by passing an arbitrary `roost_path`.
#[tauri::command]
#[specta::specta]
pub async fn create_daily_note(
    folder: String,
    state: State<'_, RoostFsState>,
    app: AppHandle,
) -> Result<String, String> {
    let backend = state
        .current()
        .await
        .ok_or_else(|| "No roost is open".to_string())?;

    // Use the user's *local* date, not UTC. Previously this took
    // `SystemTime::now() / 86400` which is UTC days since the epoch,
    // so a user in IST (UTC+5:30) opening the daily note at 00:30 on
    // May 23 got a `2026-05-22.md` (because UTC was still 19:00 on the
    // 22nd). Reported by a user in India 2026-05-23.
    let now_local = Local::now();
    let date_str = now_local.format("%Y-%m-%d").to_string();
    let weekday = match now_local.weekday() {
        chrono::Weekday::Mon => "Monday",
        chrono::Weekday::Tue => "Tuesday",
        chrono::Weekday::Wed => "Wednesday",
        chrono::Weekday::Thu => "Thursday",
        chrono::Weekday::Fri => "Friday",
        chrono::Weekday::Sat => "Saturday",
        chrono::Weekday::Sun => "Sunday",
    };

    // NEW-DN2: the roost-portable setting is the source of truth for WHERE the
    // daily note lives — the passed `folder` is only a fallback. Without this,
    // non-frontend callers (AI tools, MCP, mobile shortcuts) that don't read
    // `roostSettings.ts` would bypass the user's configured folder. Any read/
    // decode failure degrades to the passed `folder`, then the built-in default.
    let settings_json = match backend.read(ROOST_SETTINGS_REL).await {
        Ok(bytes) => String::from_utf8(bytes).ok(),
        Err(_) => None,
    };
    let folder_effective = resolve_daily_folder(settings_json.as_deref(), &folder);

    let folder_clean = folder_effective.trim_matches(['/', '\\']);
    let rel = format!("{folder_clean}/{date_str}.md");

    // DN-1: user-customizable template. If the roost has a
    // `.corvux/templates/daily.md`, render it (substituting the date/weekday
    // placeholders) instead of the built-in layout — this is how a user adds
    // habit trackers, standup blocks, weather, etc. Any read/decode failure
    // — OR an empty/whitespace-only template (NEW-DN1) — falls back to the
    // default so a missing, malformed, or accidentally-blanked template never
    // produces a frontmatter-less empty note.
    let user_template = match backend.read(DAILY_TEMPLATE_REL).await {
        Ok(bytes) => String::from_utf8(bytes).ok(),
        Err(_) => None,
    };
    let template = pick_daily_template(user_template.as_deref(), &date_str, weekday);

    // mkdir is idempotent only where AlreadyExists is acceptable — our
    // NativeFs returns AlreadyExists there, so swallow it.
    if let Err(e) = backend.mkdir(folder_clean).await {
        if !matches!(e, crate::roostfs::RoostError::AlreadyExists(_)) {
            return Err(format!("Failed to create daily folder: {e}"));
        }
    }
    if !backend
        .exists(&rel)
        .await
        .map_err(|e| format!("Failed to stat daily note: {e}"))?
    {
        backend
            .create(&rel, template.as_bytes())
            .await
            .map_err(|e| format!("Failed to create daily note: {e}"))?;
        // Notify sync + refresh the tree so a daily note created on Android SAF
        // (no file watcher) leaves the device instead of stalling until its
        // first edit.
        if let Some(sync) = app.try_state::<super::sync::SyncEngineState>() {
            super::files::notify_sync_local_write(sync.inner(), &rel, &template).await;
        }
        super::roost_tree_cache::invalidate(&app, backend.display_root());
    }
    Ok(format!(
        "{}/{rel}",
        backend.display_root().trim_end_matches(['/', '\\'])
    ))
}

/// Convert days since Unix epoch to (year, month, day) using Howard
/// Hinnant's civil-days algorithm. Valid for the full proleptic Gregorian
/// range; no external chrono dependency needed.
#[allow(dead_code)] // used by callers in tests + sibling modules; trips dead_code under --lib alone
pub(super) fn days_to_ymd(days: i64) -> (i64, u32, u32) {
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

/// Get weekday name from days since Unix epoch (epoch = Thursday).
#[allow(dead_code)] // used in daily-note templates that gate on the day name
fn weekday_name(days: i64) -> &'static str {
    match days.rem_euclid(7) {
        0 => "Thursday",
        1 => "Friday",
        2 => "Saturday",
        3 => "Sunday",
        4 => "Monday",
        5 => "Tuesday",
        6 => "Wednesday",
        _ => unreachable!(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── DN-1: daily-note template ────────────────────────────────────────

    #[test]
    fn default_template_carries_frontmatter_and_sections() {
        let t = default_daily_template("2026-07-04", "Saturday");
        assert!(t.contains("date: 2026-07-04"));
        assert!(t.contains("type: daily"));
        assert!(t.contains("# Saturday, 2026-07-04"));
        assert!(t.contains("## Tasks") && t.contains("## Notes") && t.contains("## Journal"));
    }

    #[test]
    fn render_template_substitutes_supported_placeholders() {
        let raw = "# {{title}}\ndate:: {{date}} ({{weekday}})\n- [ ] standup";
        let out = render_daily_template(raw, "2026-07-04", "Saturday");
        assert_eq!(
            out,
            "# Saturday, 2026-07-04\ndate:: 2026-07-04 (Saturday)\n- [ ] standup"
        );
    }

    // ── NEW-DN2: daily-folder resolution from roost settings ─────────────

    #[test]
    fn resolve_daily_folder_prefers_settings_over_fallback() {
        let json = r#"{"version":1,"tasks":{"daily_note_folder":"Journal/Daily","inbox_path":"Inbox.md"}}"#;
        assert_eq!(
            resolve_daily_folder(Some(json), "Daily Notes"),
            "Journal/Daily"
        );
    }

    #[test]
    fn resolve_daily_folder_falls_back_when_no_settings_file() {
        // Missing file (None) → caller's folder wins.
        assert_eq!(resolve_daily_folder(None, "My Dailies"), "My Dailies");
    }

    #[test]
    fn resolve_daily_folder_falls_back_on_malformed_json() {
        assert_eq!(
            resolve_daily_folder(Some("{not json"), "My Dailies"),
            "My Dailies"
        );
    }

    #[test]
    fn resolve_daily_folder_falls_back_when_field_absent_or_blank() {
        // tasks present but no daily_note_folder → fallback.
        assert_eq!(
            resolve_daily_folder(Some(r#"{"tasks":{"inbox_path":"Inbox.md"}}"#), "FB"),
            "FB"
        );
        // Blank/whitespace value in settings → fallback (not an empty folder).
        assert_eq!(
            resolve_daily_folder(Some(r#"{"tasks":{"daily_note_folder":"   "}}"#), "FB"),
            "FB"
        );
        // No tasks object at all → fallback.
        assert_eq!(resolve_daily_folder(Some(r#"{"version":1}"#), "FB"), "FB");
    }

    #[test]
    fn resolve_daily_folder_uses_default_when_settings_and_fallback_blank() {
        // Neither source usable → the shipped default.
        assert_eq!(resolve_daily_folder(None, "  "), DEFAULT_DAILY_FOLDER);
        assert_eq!(
            resolve_daily_folder(Some(r#"{"tasks":{"daily_note_folder":""}}"#), ""),
            DEFAULT_DAILY_FOLDER
        );
    }

    #[test]
    fn render_template_leaves_unknown_tokens_untouched() {
        // A typo'd/unsupported placeholder stays visible rather than being
        // silently dropped.
        let out = render_daily_template("{{weahter}} {{date}}", "2026-07-04", "Saturday");
        assert_eq!(out, "{{weahter}} 2026-07-04");
    }

    #[test]
    fn render_template_repeats_placeholder_everywhere() {
        let out = render_daily_template("{{date}} … {{date}}", "2026-07-04", "Saturday");
        assert_eq!(out, "2026-07-04 … 2026-07-04");
    }

    #[test]
    fn pick_template_falls_back_when_missing_or_blank() {
        // NEW-DN1: None, empty, and whitespace-only all fall back to the
        // default — never a frontmatter-less blank note.
        let def = default_daily_template("2026-07-04", "Saturday");
        assert_eq!(pick_daily_template(None, "2026-07-04", "Saturday"), def);
        assert_eq!(pick_daily_template(Some(""), "2026-07-04", "Saturday"), def);
        assert_eq!(
            pick_daily_template(Some("   \n\t "), "2026-07-04", "Saturday"),
            def
        );
    }

    #[test]
    fn pick_template_uses_user_template_when_present() {
        let out = pick_daily_template(Some("# {{title}}"), "2026-07-04", "Saturday");
        assert_eq!(out, "# Saturday, 2026-07-04");
    }

    /// Pin the civil-days algorithm against known calendar dates. If
    /// anyone retypes Howard Hinnant's formula and gets a sign wrong,
    /// the daily-note filename starts producing the wrong YYYY-MM-DD.
    #[test]
    fn days_to_ymd_known_dates() {
        // 1970-01-01 = epoch day 0
        assert_eq!(days_to_ymd(0), (1970, 1, 1));
        // 2000-01-01: known leap-year boundary case
        assert_eq!(days_to_ymd(10957), (2000, 1, 1));
        // 2026-05-22: today, per session date
        assert_eq!(days_to_ymd(20595), (2026, 5, 22));
        // Pre-epoch date — formula must handle negative days
        assert_eq!(days_to_ymd(-1), (1969, 12, 31));
    }

    /// Weekday-from-days must agree with the calendar at day 0
    /// (1970-01-01 = Thursday). Off-by-one here puts every daily-note
    /// header on the wrong day.
    #[test]
    fn weekday_name_aligns_with_calendar() {
        assert_eq!(weekday_name(0), "Thursday");
        assert_eq!(weekday_name(1), "Friday");
        assert_eq!(weekday_name(6), "Wednesday");
        // rem_euclid handles negative inputs correctly
        assert_eq!(weekday_name(-1), "Wednesday");
        // 2026-05-22 was a Friday
        assert_eq!(weekday_name(20595), "Friday");
    }
}
