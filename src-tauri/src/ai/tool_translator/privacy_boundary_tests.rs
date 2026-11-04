//! AI-8: privacy-boundary regression tests.
//!
//! The individual path sanitizers (`sanitize_for_llm`, `scrub_roost_paths`)
//! have their own unit tests. What was missing — and what this file adds — is
//! an assertion on the **assembled outbound cloud request payload**: after a
//! tool result carrying an absolute roost path is sanitized the way the agent
//! loop does it (see `ai/tools/agent_loop/mod.rs`), and then folded into the
//! provider wire shape (`build_gemini_contents` for Gemini,
//! `build_openai_messages` for OpenRouter / OpenAI-compat), the serialized
//! body must not contain the user's absolute path — drive letter, OS
//! username, or roost root. Corvux is marketed as privacy-first; a leak here
//! would ship the user's disk layout to a cloud model on every tool call.
//!
//! Strategy: plant a unique **canary** token inside the absolute roost root.
//! If the canary survives sanitize → assemble → serialize, a path leaked.

use crate::ai::provider::ChatMessage;
use crate::ai::tools::note_ref::{sanitize_for_llm, scrub_roost_paths};
use serde_json::json;

/// A roost root whose path contains a token that must never reach a cloud
/// provider. Includes a Windows drive + a fake OS username so the test
/// exercises the exact PII shape we care about.
const CANARY_ROOST: &str = r"W:\Users\canary_user_7f3a9\Roost";
const CANARY_TOKEN: &str = "canary_user_7f3a9";

/// Build the message history the way the agent loop would after running a
/// tool: a user turn, plus a tool response whose JSON body was passed through
/// the loop's sanitizers. Returns messages ready to hand to a provider
/// payload builder.
fn sanitized_tool_history() -> Vec<ChatMessage> {
    // A successful tool result carrying absolute paths under the known path
    // keys (`path`, `file_path`, `note_path`) plus an unrelated content field.
    let success = json!({
        "path": format!(r"{CANARY_ROOST}\Brain-Dump\Secret Plans.md"),
        "results": [
            { "note_path": format!(r"{CANARY_ROOST}\Journal\2026-07-04.md"), "score": 0.9 }
        ],
        "excerpt": "the note body itself is preserved verbatim"
    });
    let sanitized_success = sanitize_for_llm(success, Some(CANARY_ROOST));

    // An error string that interpolated an absolute path via Display — this
    // is the path the loop scrubs textually (JSON key rewriting can't catch
    // it because it's inside free text).
    let error_msg = format!(r"failed to read {CANARY_ROOST}\Brain-Dump\Secret Plans.md: not found");
    let scrubbed_error = scrub_roost_paths(&error_msg, Some(CANARY_ROOST));
    let error_body = json!({ "error": scrubbed_error, "fatal": false });

    vec![
        ChatMessage::user("what's in my secret plans note?"),
        ChatMessage::tool_response(
            "call_1",
            "search_roost",
            serde_json::to_string(&sanitized_success).unwrap(),
        ),
        ChatMessage::tool_response(
            "call_2",
            "read_note",
            serde_json::to_string(&error_body).unwrap(),
        ),
    ]
}

fn assert_no_canary(payload_json: &str, provider: &str) {
    assert!(
        !payload_json.contains(CANARY_TOKEN),
        "{provider} payload leaked the absolute roost path (canary token present):\n{payload_json}"
    );
    // The drive-letter form must be gone too (belt and suspenders — the
    // canary token alone could be missed if the path were partially stripped).
    assert!(
        !payload_json.contains(r"W:\Users")
            && !payload_json.to_ascii_lowercase().contains("w:/users"),
        "{provider} payload leaked a Windows absolute path prefix:\n{payload_json}"
    );
}

#[test]
fn gemini_payload_does_not_leak_absolute_roost_paths() {
    let msgs = sanitized_tool_history();
    let contents = super::gemini::build_gemini_contents(&msgs);
    let payload = serde_json::to_string(&contents).expect("serialize gemini contents");
    assert_no_canary(&payload, "gemini");
    // Sanity: the roost-relative remainder DID survive (we scrubbed the
    // prefix, not the whole path), so we didn't just delete everything.
    assert!(
        payload.contains("Brain-Dump/Secret Plans"),
        "gemini payload should retain the roost-relative path form:\n{payload}"
    );
}

#[test]
fn openai_payload_does_not_leak_absolute_roost_paths() {
    let msgs = sanitized_tool_history();
    let chat = super::openai_compat::build_openai_messages(None, &msgs);
    let payload = serde_json::to_string(&chat).expect("serialize openai messages");
    assert_no_canary(&payload, "openai");
    assert!(
        payload.contains("Brain-Dump/Secret Plans"),
        "openai payload should retain the roost-relative path form:\n{payload}"
    );
}

#[test]
fn anthropic_payload_does_not_leak_absolute_roost_paths() {
    let msgs = sanitized_tool_history();
    let chat = super::anthropic::build_anthropic_messages(&msgs);
    let payload = serde_json::to_string(&chat).expect("serialize anthropic messages");
    assert_no_canary(&payload, "anthropic");
    assert!(
        payload.contains("Brain-Dump/Secret Plans"),
        "anthropic payload should retain the roost-relative path form:\n{payload}"
    );
}

#[test]
fn user_content_is_preserved_verbatim_not_over_scrubbed() {
    // The privacy scrub must be surgical: it strips absolute paths, never
    // the user's actual note content / queries. Guards against a future
    // over-eager sanitizer that redacts too much.
    let msgs = sanitized_tool_history();
    let payload = serde_json::to_string(&super::gemini::build_gemini_contents(&msgs)).unwrap();
    assert!(payload.contains("the note body itself is preserved verbatim"));
    assert!(payload.contains("what's in my secret plans note?"));
}
