//! Unit tests for the small string/JSON helpers in
//! `agent_loop/mod.rs` (`approval_summary`, `short_summary`,
//! `clean_prior_text`). Split out per AUDIT-035 so the parent module
//! stops growing — these helpers don't need an `AppHandle` and are
//! safe to run on every platform.
//!
//! True end-to-end loop tests still need `tauri::test::MockRuntime`,
//! which is currently broken on Windows; see the comment block above
//! `loop_feedback_tests` in `mod.rs` for the workaround story.

use super::{approval_summary, clean_prior_text, short_summary};
use crate::ai::tools::registry::SafetyTier;
use serde_json::json;

// clean_prior_text --------------------------------------------------

#[test]
fn clean_prior_text_strips_tool_call_block_and_trims() {
    let raw = "  Here is my plan.\n<tool_call>{\"name\":\"x\",\"args\":{}}</tool_call>\nThanks. ";
    let out = clean_prior_text(raw);
    assert!(!out.contains("<tool_call>"), "tag survived: {out}");
    assert!(!out.contains("</tool_call>"), "close tag survived: {out}");
    assert!(out.starts_with("Here is my plan."));
    assert!(out.ends_with("Thanks."));
}

#[test]
fn clean_prior_text_on_plain_text_is_idempotent() {
    // No markup → nothing to strip; trim still applies.
    assert_eq!(clean_prior_text("  hello world\n"), "hello world");
}

// approval_summary --------------------------------------------------

#[test]
fn approval_summary_prefers_path_field_with_tier_verb() {
    let args = json!({ "path": "Journal/today.md", "extra": "ignored" });
    let s = approval_summary("roost_write", SafetyTier::Write, &args);
    assert_eq!(s, "Write: Journal/today.md");
}

#[test]
fn approval_summary_destructive_tier_uses_delete_verb() {
    let args = json!({ "note_path": "drafts/old.md" });
    let s = approval_summary("delete_note", SafetyTier::Destructive, &args);
    assert_eq!(s, "Delete: drafts/old.md");
}

#[test]
fn approval_summary_falls_back_to_query_then_tool_name() {
    // No path → query wins.
    let q = approval_summary(
        "roost_search",
        SafetyTier::Read,
        &json!({ "query": "embeddings" }),
    );
    assert_eq!(q, "roost_search: embeddings");

    // No path, no query, no text → bare tool name.
    let bare = approval_summary("noop", SafetyTier::Read, &json!({ "other": 1 }));
    assert_eq!(bare, "noop");
}

#[test]
fn approval_summary_truncates_long_text_to_60_chars() {
    // 70-char text input — must be cut to 60 chars to keep the
    // approval card single-line. Counts characters, not bytes, so
    // non-ASCII content stays well-formed.
    let long = "a".repeat(70);
    let s = approval_summary("memorize", SafetyTier::Write, &json!({ "text": long }));
    // Format is `memorize: <body>` — strip the prefix.
    let body = s.strip_prefix("memorize: ").expect("expected prefix");
    assert_eq!(body.chars().count(), 60);
}

// short_summary -----------------------------------------------------

#[test]
fn short_summary_prefers_summary_then_message() {
    assert_eq!(
        short_summary(&json!({"summary": "wrote file", "message": "ignored"})),
        "wrote file"
    );
    assert_eq!(
        short_summary(&json!({"message": "1 row deleted"})),
        "1 row deleted"
    );
}

#[test]
fn short_summary_counts_results_array_when_no_summary() {
    let v = json!({ "results": [1, 2, 3, 4] });
    assert_eq!(short_summary(&v), "4 result(s)");
}

#[test]
fn short_summary_counts_bare_array_and_defaults_to_ok() {
    assert_eq!(short_summary(&json!([1, 2])), "2 item(s)");
    // Unknown shape → "ok". Locks the fallback so future refactors
    // don't accidentally bubble raw JSON into the tool card.
    assert_eq!(short_summary(&json!({"weird": true})), "ok");
}
