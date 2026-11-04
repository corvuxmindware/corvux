// human_label exposed for future UI surface
#![allow(dead_code)]

//! Secret-pattern scanner for tool-call args (agent-hardening § 3.4.3).
//!
//! Runs against the JSON `args` object before the approval gate emits
//! `ai:tool-pending`. Each match becomes a [`SecretWarning`] attached
//! to the payload; the frontend renders these above the diff so the
//! user sees "this approval contains an AWS-key-shaped string" BEFORE
//! deciding whether to approve.
//!
//! Threat model (`agent-hardening-and-self-improvement.md` § 2):
//! - **T6** — LLM, prompted by injection, encodes secrets into a future
//!   tool call (e.g. `roost_write` body) the user might approve thinking
//!   it's benign.
//! - **T7** — User pasted an API key into a note by accident; LLM later
//!   writes it back to a synced location.
//!
//! The scanner does NOT prevent the call — it surfaces risk so the
//! human-in-the-loop can catch it. False positives are tolerable; false
//! negatives are the failure mode we minimize.
//!
//! # Design
//!
//! - **Hand-rolled patterns, no regex dep.** Four shapes (AWS-shape,
//!   JWT-shape, long-base64, URL-with-credentials) all map to simple
//!   state machines. Keeping out of `regex` shaves ~150 KB of binary
//!   and avoids one more crate in the supply chain.
//! - **Field-scoped recursion.** Walks the `serde_json::Value` tree;
//!   reports `field: "args.body[0].text"` style paths so the user
//!   knows where in the call the secret-shaped content sits.
//! - **Bounded preview.** Each warning carries a 40-char preview of
//!   the matched substring (with PII masked) so the user can identify
//!   the suspicious string without it leaking through the audit log.

use serde::Serialize;
use serde_json::Value;

/// Kind of suspicious pattern detected. The frontend uses this to pick
/// an icon / color and the human-readable description.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, serde::Deserialize, specta::Type)]
#[serde(rename_all = "snake_case")]
pub enum SecretKind {
    /// `AKIA[0-9A-Z]{16}` — AWS access key ID shape.
    AwsAccessKeyId,
    /// Three base64-url segments joined by `.` — JWT shape.
    JwtLike,
    /// Continuous base64 run ≥ 200 chars. Could be a key, a token, a
    /// signed payload, or an embedded image. Lower-confidence signal
    /// than the typed shapes above; the user sees the preview and
    /// decides.
    Base64Blob,
    /// `https://user:pass@host/...` — URL with inline credentials.
    UrlCredentials,
    /// Gemini API key shape (`AIza[A-Za-z0-9_-]{35}`). Cheap to detect
    /// and a real-world risk: the user's own Gemini key could be
    /// echoed back via an injection-coerced tool call.
    GeminiApiKey,
    /// Generic high-entropy hex string (≥40 hex chars). Catches blake3
    /// digests, signing secrets, and similar. Low-confidence by
    /// design — frequently a false positive on legitimate hashes.
    LongHex,
}

impl SecretKind {
    pub fn human_label(&self) -> &'static str {
        match self {
            SecretKind::AwsAccessKeyId => "AWS access key ID shape",
            SecretKind::JwtLike => "JSON Web Token shape",
            SecretKind::Base64Blob => "long base64 blob",
            SecretKind::UrlCredentials => "URL with inline credentials",
            SecretKind::GeminiApiKey => "Gemini API key shape",
            SecretKind::LongHex => "long hex blob (possible hash / signing secret)",
        }
    }
}

/// One detection. `field` is a dotted path into the JSON args (e.g.
/// `args.body`, `args.parts[2].text`). `preview` is the partial
/// substring (masked in the middle for keyed shapes) so the user can
/// recognize it without it leaking in plaintext into log files.
#[derive(Debug, Clone, Serialize, serde::Deserialize, specta::Type)]
pub struct SecretWarning {
    pub kind: SecretKind,
    pub field: String,
    pub preview: String,
}

/// Cap on the number of warnings returned. A single payload generating
/// 50 false-positive base64 hits would drown the real signal; we cap
/// at this many and let the user inspect manually if they want to dig
/// deeper. Chosen to comfortably fit in a tool card without scrolling.
const MAX_WARNINGS: usize = 6;

/// Cap on individual string size we scan. Past this, we're almost
/// certainly looking at file content (note body, diff payload) where
/// the user-visible diff is already the trust surface, not the
/// scanner. Avoids quadratic cost on multi-MB args (which shouldn't
/// happen at all, but defense in depth).
const MAX_STRING_BYTES: usize = 64 * 1024;

/// Scan an arbitrary JSON value for secret-shaped substrings. Returns
/// a deduplicated list capped at [`MAX_WARNINGS`].
pub fn scan_value(args: &Value) -> Vec<SecretWarning> {
    let mut out = Vec::new();
    walk(args, "args", &mut out);
    // Dedup by (kind, preview) — different fields might surface the
    // same string (e.g. duplicated across body + summary). Keep the
    // first occurrence; the dotted path of the first hit is enough.
    let mut seen: std::collections::HashSet<(SecretKind, String)> =
        std::collections::HashSet::new();
    let mut deduped = Vec::with_capacity(out.len());
    for w in out {
        let k = (w.kind, w.preview.clone());
        if seen.insert(k) {
            deduped.push(w);
            if deduped.len() >= MAX_WARNINGS {
                break;
            }
        }
    }
    deduped
}

fn walk(v: &Value, path: &str, out: &mut Vec<SecretWarning>) {
    if out.len() >= MAX_WARNINGS {
        return;
    }
    match v {
        Value::String(s) => {
            if s.len() > MAX_STRING_BYTES {
                return;
            }
            scan_string(s, path, out);
        }
        Value::Array(arr) => {
            for (i, child) in arr.iter().enumerate() {
                let next = format!("{path}[{i}]");
                walk(child, &next, out);
                if out.len() >= MAX_WARNINGS {
                    return;
                }
            }
        }
        Value::Object(obj) => {
            for (k, child) in obj.iter() {
                let next = format!("{path}.{k}");
                walk(child, &next, out);
                if out.len() >= MAX_WARNINGS {
                    return;
                }
            }
        }
        // Numbers / bool / null can't carry secret shapes — skip.
        _ => {}
    }
}

/// Run every detector against `s`. Each detector calls
/// [`record_warning`] for hits. Order matters only insofar as the
/// final dedup keeps the first occurrence.
fn scan_string(s: &str, field: &str, out: &mut Vec<SecretWarning>) {
    detect_aws_key(s, field, out);
    detect_gemini_key(s, field, out);
    detect_jwt(s, field, out);
    detect_url_credentials(s, field, out);
    detect_base64_blob(s, field, out);
    detect_long_hex(s, field, out);
}

fn record(out: &mut Vec<SecretWarning>, kind: SecretKind, field: &str, preview: String) {
    if out.len() >= MAX_WARNINGS {
        return;
    }
    out.push(SecretWarning {
        kind,
        field: field.to_string(),
        preview,
    });
}

/// Build a masked preview of a matched substring. Shows the first 4
/// chars + `…` + last 4 chars when the match is long enough, else just
/// the literal substring truncated to 40 chars.
fn masked_preview(full: &str) -> String {
    let chars: Vec<char> = full.chars().collect();
    if chars.len() <= 8 {
        return chars.iter().collect();
    }
    let head: String = chars.iter().take(4).collect();
    let tail: String = chars
        .iter()
        .rev()
        .take(4)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    format!("{head}…{tail}")
}

// ─── Detectors ────────────────────────────────────────────────────

/// AWS access key: literal `AKIA` followed by 16 uppercase alphanumerics.
fn detect_aws_key(s: &str, field: &str, out: &mut Vec<SecretWarning>) {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i + 20 <= bytes.len() {
        if &bytes[i..i + 4] == b"AKIA" {
            let tail = &bytes[i + 4..i + 20];
            if tail
                .iter()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
            {
                // Must be at a word boundary on both sides — otherwise
                // a substring of a longer base64 blob triggers this.
                let left_ok = i == 0 || !is_token_char(bytes[i - 1]);
                let right_ok = i + 20 == bytes.len() || !is_token_char(bytes[i + 20]);
                if left_ok && right_ok {
                    let m = std::str::from_utf8(&bytes[i..i + 20]).unwrap_or("AKIA…");
                    record(out, SecretKind::AwsAccessKeyId, field, masked_preview(m));
                }
            }
        }
        i += 1;
    }
}

/// Gemini API key: `AIza` + 35 chars of `[A-Za-z0-9_-]`.
fn detect_gemini_key(s: &str, field: &str, out: &mut Vec<SecretWarning>) {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i + 39 <= bytes.len() {
        if &bytes[i..i + 4] == b"AIza" {
            let tail = &bytes[i + 4..i + 39];
            if tail
                .iter()
                .all(|c| c.is_ascii_alphanumeric() || *c == b'_' || *c == b'-')
            {
                let left_ok = i == 0 || !is_token_char(bytes[i - 1]);
                let right_ok = i + 39 == bytes.len() || !is_token_char(bytes[i + 39]);
                if left_ok && right_ok {
                    let m = std::str::from_utf8(&bytes[i..i + 39]).unwrap_or("AIza…");
                    record(out, SecretKind::GeminiApiKey, field, masked_preview(m));
                }
            }
        }
        i += 1;
    }
}

/// JWT shape: three base64-url segments joined by `.`. Each segment
/// must be ≥ 10 chars to avoid catching arbitrary three-dotted strings
/// (e.g. semver versions).
fn detect_jwt(s: &str, field: &str, out: &mut Vec<SecretWarning>) {
    // Walk through the string token-by-token. A token is a max run of
    // chars matching base64-url (`A-Za-z0-9_-`) without `.`. We need
    // three consecutive `token.token.token` separated by single dots
    // and bordered by non-token chars on the outside.
    let bytes = s.as_bytes();
    let n = bytes.len();
    let mut i = 0;
    while i < n {
        // Move i to a word boundary
        let left_ok = i == 0 || !is_jwt_char(bytes[i - 1]);
        if !left_ok {
            i += 1;
            continue;
        }
        let seg1 = jwt_segment_len(bytes, i);
        if seg1 < 10 || i + seg1 >= n || bytes[i + seg1] != b'.' {
            i += 1;
            continue;
        }
        let start2 = i + seg1 + 1;
        let seg2 = jwt_segment_len(bytes, start2);
        if seg2 < 10 || start2 + seg2 >= n || bytes[start2 + seg2] != b'.' {
            i += 1;
            continue;
        }
        let start3 = start2 + seg2 + 1;
        let seg3 = jwt_segment_len(bytes, start3);
        if seg3 < 10 {
            i += 1;
            continue;
        }
        let end = start3 + seg3;
        let right_ok = end == n || !is_jwt_char(bytes[end]);
        if !right_ok {
            i += 1;
            continue;
        }
        if let Ok(m) = std::str::from_utf8(&bytes[i..end]) {
            record(out, SecretKind::JwtLike, field, masked_preview(m));
        }
        i = end;
    }
}

fn is_jwt_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'-'
}

fn jwt_segment_len(bytes: &[u8], start: usize) -> usize {
    let mut n = 0;
    while start + n < bytes.len() && is_jwt_char(bytes[start + n]) {
        n += 1;
    }
    n
}

/// `https?://[^/]+:[^/]+@host/...` — URL with credentials embedded.
fn detect_url_credentials(s: &str, field: &str, out: &mut Vec<SecretWarning>) {
    let lower = s.to_lowercase();
    for needle in ["http://", "https://"] {
        let mut search_from = 0;
        while let Some(rel) = lower[search_from..].find(needle) {
            let url_start = search_from + rel;
            // Find the end of the URL — first whitespace or end-of-string.
            let after = url_start + needle.len();
            let end = s[after..]
                .find(|c: char| c.is_whitespace())
                .map(|n| after + n)
                .unwrap_or(s.len());
            let body = &s[after..end];
            // Look for `:` ... `@` before the first `/`. Both segments
            // must be non-empty.
            if let Some(at) = body.find('@') {
                let creds = &body[..at];
                if let Some(colon) = creds.find(':') {
                    let user = &creds[..colon];
                    let pass = &creds[colon + 1..];
                    if !user.is_empty()
                        && !pass.is_empty()
                        && !user.contains('/')
                        && !pass.contains('/')
                    {
                        let m = &s[url_start..end.min(url_start + 60)];
                        record(out, SecretKind::UrlCredentials, field, masked_preview(m));
                    }
                }
            }
            search_from = end;
        }
    }
}

/// Contiguous base64 run ≥ 200 chars. Cheap signal: arbitrary-length
/// blobs encode arbitrary-shape secrets, and a 200-char floor avoids
/// catching short hashes / IDs.
fn detect_base64_blob(s: &str, field: &str, out: &mut Vec<SecretWarning>) {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if !is_base64_char(bytes[i]) {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && is_base64_char(bytes[i]) {
            i += 1;
        }
        let run = i - start;
        if run >= 200 {
            if let Ok(m) = std::str::from_utf8(&bytes[start..(start + run.min(80))]) {
                record(out, SecretKind::Base64Blob, field, masked_preview(m));
            }
        }
    }
}

fn is_base64_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'+' || c == b'/' || c == b'='
}

/// Contiguous hex run ≥ 40 chars (160 bits — SHA-1 length and up).
/// Low-confidence: many legitimate strings hit this (blake3 digests,
/// commit hashes, etc.). Still worth surfacing so the user can spot
/// "wait, why is the agent passing my private key around."
fn detect_long_hex(s: &str, field: &str, out: &mut Vec<SecretWarning>) {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if !bytes[i].is_ascii_hexdigit() {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && bytes[i].is_ascii_hexdigit() {
            i += 1;
        }
        let run = i - start;
        if run >= 40 {
            // Must be bordered by non-hex (avoids matching the middle
            // of a longer alphanumeric run — those are caught by
            // base64 detector).
            let left_ok = start == 0 || !is_token_char(bytes[start - 1]);
            let right_ok = i == bytes.len() || !is_token_char(bytes[i]);
            if left_ok && right_ok {
                if let Ok(m) = std::str::from_utf8(&bytes[start..(start + run.min(60))]) {
                    record(out, SecretKind::LongHex, field, masked_preview(m));
                }
            }
        }
    }
}

fn is_token_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'-'
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn empty_args_produces_no_warnings() {
        let v = json!({});
        assert!(scan_value(&v).is_empty());
    }

    #[test]
    fn benign_strings_produce_no_warnings() {
        let v = json!({
            "path": "projects/foo.md",
            "body": "Update timeline to July 1.\n\nNo secrets here.",
        });
        assert!(scan_value(&v).is_empty());
    }

    #[test]
    fn aws_key_id_detected() {
        let v = json!({ "body": "DB cred: AKIA1234567890ABCDEF" });
        let w = scan_value(&v);
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].kind, SecretKind::AwsAccessKeyId);
        assert!(w[0].field.contains("body"));
    }

    #[test]
    fn aws_key_in_array_reports_indexed_path() {
        let v = json!({ "tokens": ["foo", "AKIA1234567890ABCDEF", "bar"] });
        let w = scan_value(&v);
        assert_eq!(w.len(), 1);
        assert!(w[0].field.contains("tokens[1]"), "got: {}", w[0].field);
    }

    #[test]
    fn aws_key_substring_in_word_does_not_match() {
        // `AKIA` followed by 16 valid chars but bordered by letter
        // either side — likely just a noisy base64 run, not an actual
        // key. Detector requires word boundaries.
        let v = json!({ "body": "xAKIA1234567890ABCDEFx" });
        let w = scan_value(&v);
        assert!(
            !w.iter().any(|w| w.kind == SecretKind::AwsAccessKeyId),
            "should NOT fire on embedded shape"
        );
    }

    #[test]
    fn gemini_key_detected() {
        // Real-shape key (not an actual key — random alphanumerics).
        // Format: literal "AIza" + exactly 35 chars of [A-Za-z0-9_-].
        let v = json!({ "key": "AIzaSyA0123456789ABCDEFGHIJklmnopqrstuv" });
        let w = scan_value(&v);
        assert!(
            w.iter().any(|w| w.kind == SecretKind::GeminiApiKey),
            "got: {:?}",
            w
        );
    }

    #[test]
    fn jwt_three_segment_detected() {
        // 30+ chars each, three segments. Realistic-shaped JWT body.
        let jwt = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c";
        let v = json!({ "auth": format!("Bearer {jwt}") });
        let w = scan_value(&v);
        assert!(w.iter().any(|w| w.kind == SecretKind::JwtLike));
    }

    #[test]
    fn jwt_short_segments_not_detected() {
        // Three dotted segments but each too short — looks like a
        // version string or path, not a JWT.
        let v = json!({ "x": "v1.2.3" });
        let w = scan_value(&v);
        assert!(!w.iter().any(|w| w.kind == SecretKind::JwtLike));
    }

    #[test]
    fn url_with_credentials_detected() {
        let v = json!({ "url": "Try https://alice:hunter2@db.internal/path" });
        let w = scan_value(&v);
        assert!(w.iter().any(|w| w.kind == SecretKind::UrlCredentials));
    }

    #[test]
    fn url_without_credentials_not_detected() {
        let v = json!({ "url": "https://github.com/anthropics/claude-agent-sdk" });
        let w = scan_value(&v);
        assert!(!w.iter().any(|w| w.kind == SecretKind::UrlCredentials));
    }

    #[test]
    fn long_base64_blob_detected() {
        let blob: String = "A".repeat(250);
        let v = json!({ "body": format!("Token: {blob}") });
        let w = scan_value(&v);
        assert!(w.iter().any(|w| w.kind == SecretKind::Base64Blob));
    }

    #[test]
    fn short_base64_not_detected() {
        let v = json!({ "body": "Token: aGVsbG8=" });
        let w = scan_value(&v);
        assert!(!w.iter().any(|w| w.kind == SecretKind::Base64Blob));
    }

    #[test]
    fn long_hex_detected() {
        // 40 hex chars = SHA-1 length. Often a real signal.
        let v = json!({ "secret": "a3f1c2b4e5d6789012345678901234567890abcd" });
        let w = scan_value(&v);
        assert!(w.iter().any(|w| w.kind == SecretKind::LongHex));
    }

    #[test]
    fn short_hex_not_detected() {
        let v = json!({ "short": "deadbeef" });
        let w = scan_value(&v);
        assert!(!w.iter().any(|w| w.kind == SecretKind::LongHex));
    }

    #[test]
    fn warnings_capped_at_max() {
        // Manufacture 20 hits; ensure cap fires.
        let mut obj = serde_json::Map::new();
        for i in 0..20 {
            obj.insert(
                format!("k{i}"),
                json!(format!(
                    "AKIA1234567890ABCDE{}",
                    (b'A' + (i as u8 % 26)) as char
                )),
            );
        }
        let v = Value::Object(obj);
        let w = scan_value(&v);
        assert!(w.len() <= MAX_WARNINGS);
    }

    #[test]
    fn duplicate_match_deduplicated() {
        // Same key shape in two fields — collapsed to one warning.
        let v = json!({
            "primary": "AKIA1234567890ABCDEF",
            "backup":  "AKIA1234567890ABCDEF",
        });
        let w = scan_value(&v);
        let aws: Vec<_> = w
            .iter()
            .filter(|w| w.kind == SecretKind::AwsAccessKeyId)
            .collect();
        assert_eq!(aws.len(), 1, "duplicate previews should collapse");
    }

    #[test]
    fn deep_nesting_walked() {
        let v = json!({
            "outer": { "middle": { "inner": "AKIA1234567890ABCDEF" } }
        });
        let w = scan_value(&v);
        assert!(w.iter().any(|w| w.field.contains("outer.middle.inner")));
    }

    #[test]
    fn masked_preview_hides_middle() {
        let m = masked_preview("AKIA1234567890ABCDEF");
        // 4 + ellipsis + 4
        assert!(m.starts_with("AKIA"));
        assert!(m.contains('…'));
        assert!(m.ends_with("CDEF"));
        assert!(m.len() < 20);
    }

    #[test]
    fn oversize_string_skipped() {
        // 200 KB of fake-key-shaped content. Scanner should NOT spin
        // for seconds — it should skip the field outright.
        let big = "AKIA1234567890ABCDEF ".repeat(15_000);
        let v = json!({ "body": big });
        let w = scan_value(&v);
        // No warnings because the field exceeded MAX_STRING_BYTES.
        // (Trade-off documented in the module rustdoc.)
        assert!(
            w.is_empty(),
            "scanner must skip oversized strings, not iterate them"
        );
    }
}
