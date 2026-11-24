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
//! - **Hand-rolled patterns, no regex dep.** The shapes (AWS-shape,
//!   Gemini, provider API keys / tokens, PEM private-key headers,
//!   JWT-shape, long-base64, long-hex, URL-with-credentials) all map to
//!   simple state machines. Keeping out of `regex` shaves ~150 KB of
//!   binary and avoids one more crate in the supply chain.
//! - **Long strings are scanned in overlapping windows** (AT-8), never
//!   skipped whole: a secret pasted after 100 KB of note text is still
//!   found. Distinct hits are deduplicated BEFORE the display cap, and
//!   hits past the cap are reported as one "N more" row, not dropped.
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
    /// Provider API key / token with a fixed, documented prefix and
    /// length: OpenRouter (`sk-or-v1-` + 64 hex), OpenAI (`sk-proj-…`,
    /// `sk-svcacct-…`, `sk-admin-…`, legacy `sk-` + 48 alphanumerics,
    /// or a `sk-` key carrying the `T3BlbkFJ` marker), Anthropic
    /// (`sk-ant-…`), GitHub (`ghp_`/`gho_`/`ghu_`/`ghs_`/`ghr_` + 36,
    /// `github_pat_…`).
    ProviderApiKey,
    /// PEM private-key header (`-----BEGIN [RSA |EC |OPENSSH |…]PRIVATE
    /// KEY-----`, PGP `PRIVATE KEY BLOCK`). The preview is the header
    /// line itself, never key material.
    PrivateKeyBlock,
    /// Not a finding: more distinct findings exist than fit the card
    /// (or part of a huge string was not scanned). The preview says how
    /// many (`12 more`, `200+ more`).
    MoreNotShown,
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
            SecretKind::ProviderApiKey => "API key / token shape",
            SecretKind::PrivateKeyBlock => "private key block",
            SecretKind::MoreNotShown => "more findings not shown",
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

/// Number of DISTINCT findings shown. A single payload generating 50
/// false-positive base64 hits would drown the real signal, so only this many
/// rows are listed; the rest are summarised in one `MoreNotShown` row ("N
/// more") instead of silently dropped (AT-8). The cap applies AFTER
/// deduplication — the same key repeated six times is one finding, not six.
const MAX_WARNINGS: usize = 8;

/// Distinct findings counted before the scan stops looking for more. Bounds
/// the work on a payload that is nothing but secret-shaped strings; the "N
/// more" row then reads `N+`.
const MAX_TRACKED: usize = 200;

/// Strings longer than this are scanned in windows of this size. Each window
/// is a plain `&str` slice (char-boundary aligned), so every detector keeps its
/// whole-string semantics inside it.
const WINDOW_BYTES: usize = 64 * 1024;

/// Consecutive windows overlap by this much, so any single finding up to this
/// long (tokens, PEM headers, JWTs, URLs) lies wholly inside at least one
/// window instead of straddling a cut.
const WINDOW_OVERLAP: usize = 4 * 1024;

/// Most bytes of ONE string that are scanned. Past this the string is almost
/// certainly file content whose user-visible diff is the trust surface; the
/// unscanned remainder is reported, not hidden.
const MAX_STRING_SCAN_BYTES: usize = 4 * 1024 * 1024;

/// Most bytes scanned across one whole value (many large strings).
const MAX_TOTAL_SCAN_BYTES: usize = 16 * 1024 * 1024;

/// Collects findings: deduplicates by `(kind, preview)` BEFORE applying the
/// display cap, counts what did not fit, and tracks input that was not scanned.
struct Sink {
    shown: Vec<SecretWarning>,
    seen: std::collections::HashSet<(SecretKind, String)>,
    /// Distinct findings past [`MAX_WARNINGS`].
    extra: usize,
    /// Bytes handed to the detectors so far (all windows' strings, once).
    scanned: usize,
    /// Bytes skipped because a string/value budget ran out.
    skipped: usize,
}

impl Sink {
    fn new() -> Self {
        Self {
            shown: Vec::new(),
            seen: std::collections::HashSet::new(),
            extra: 0,
            scanned: 0,
            skipped: 0,
        }
    }

    /// True once enough distinct findings are known that looking for more
    /// would only burn time.
    fn full(&self) -> bool {
        self.shown.len() + self.extra >= MAX_TRACKED
    }

    fn record(&mut self, kind: SecretKind, field: &str, preview: String) {
        if self.full() {
            return;
        }
        // Different fields may surface the same string (body + summary, or
        // the overlap of two windows): keep the first occurrence only.
        if !self.seen.insert((kind, preview.clone())) {
            return;
        }
        if self.shown.len() < MAX_WARNINGS {
            self.shown.push(SecretWarning {
                kind,
                field: field.to_string(),
                preview,
            });
        } else {
            self.extra += 1;
        }
    }

    fn finish(mut self) -> Vec<SecretWarning> {
        if self.extra > 0 || self.skipped > 0 {
            let mut parts = Vec::new();
            if self.extra > 0 {
                let plus = if self.full() { "+" } else { "" };
                parts.push(format!("{}{plus} more", self.extra));
            }
            if self.skipped > 0 {
                parts.push(format!("{} KiB not scanned", self.skipped.div_ceil(1024)));
            }
            self.shown.push(SecretWarning {
                kind: SecretKind::MoreNotShown,
                field: "args".to_string(),
                preview: parts.join(", "),
            });
        }
        self.shown
    }
}

/// Scan an arbitrary JSON value for secret-shaped substrings. Returns the
/// distinct findings, at most [`MAX_WARNINGS`] of them plus one
/// [`SecretKind::MoreNotShown`] summary row when more exist.
pub fn scan_value(args: &Value) -> Vec<SecretWarning> {
    let mut sink = Sink::new();
    walk(args, "args", &mut sink);
    sink.finish()
}

fn walk(v: &Value, path: &str, sink: &mut Sink) {
    if sink.full() {
        return;
    }
    match v {
        Value::String(s) => scan_string(s, path, sink),
        Value::Array(arr) => {
            for (i, child) in arr.iter().enumerate() {
                let next = format!("{path}[{i}]");
                walk(child, &next, sink);
                if sink.full() {
                    return;
                }
            }
        }
        Value::Object(obj) => {
            for (k, child) in obj.iter() {
                let next = format!("{path}.{k}");
                walk(child, &next, sink);
                if sink.full() {
                    return;
                }
            }
        }
        // Numbers / bool / null can't carry secret shapes — skip.
        _ => {}
    }
}

/// Scan one string: whole when it is short, otherwise in overlapping
/// [`WINDOW_BYTES`] windows up to the scan budgets. A string is never skipped
/// outright — whatever is not scanned is counted into the summary row.
fn scan_string(s: &str, field: &str, sink: &mut Sink) {
    // Every cut below goes through the shared UTF-8-safe `utf8_prefix` (F9:
    // no hand-rolled `is_char_boundary` walks). It rounds DOWN to a boundary,
    // which is always what is wanted here: a smaller `limit`/`end` just scans
    // a few bytes less, and a smaller window start just overlaps a few bytes
    // more.
    let budget = MAX_TOTAL_SCAN_BYTES.saturating_sub(sink.scanned);
    let limit = super::utf8_prefix(s, s.len().min(MAX_STRING_SCAN_BYTES).min(budget)).len();
    sink.skipped += s.len() - limit;
    sink.scanned += limit;

    let bytes = s.as_bytes();
    let mut start = 0usize;
    loop {
        let end = super::utf8_prefix(s, (start + WINDOW_BYTES).min(limit)).len();
        let prev = (start > 0).then(|| bytes[start - 1]);
        scan_window(&s[start..end], prev, field, sink);
        if end >= limit || sink.full() {
            break;
        }
        // Next window starts WINDOW_OVERLAP before this one ended (rounded
        // down to a char boundary). Always past `start`: the overlap and the
        // rounding (< 4 bytes) are tiny next to the window.
        start = super::utf8_prefix(s, end - WINDOW_OVERLAP).len();
    }
}

/// Run every detector against one window. `prev` is the byte just before the
/// window in the full string (`None` at the true start), so a token cut by the
/// window edge is not mistaken for one that begins there. Order matters only
/// insofar as the dedup keeps the first occurrence.
fn scan_window(s: &str, prev: Option<u8>, field: &str, sink: &mut Sink) {
    detect_aws_key(s, prev, field, sink);
    detect_gemini_key(s, prev, field, sink);
    detect_provider_keys(s, prev, field, sink);
    detect_private_key_header(s, field, sink);
    detect_jwt(s, prev, field, sink);
    detect_url_credentials(s, field, sink);
    detect_base64_blob(s, prev, field, sink);
    detect_long_hex(s, prev, field, sink);
}

/// True when the byte before position `i` (or `prev` at a window start) is
/// NOT a `word` byte — i.e. a token starting at `i` really starts there.
fn boundary_before(bytes: &[u8], i: usize, prev: Option<u8>, word: fn(u8) -> bool) -> bool {
    let before = if i == 0 { prev } else { Some(bytes[i - 1]) };
    before.is_none_or(|b| !word(b))
}

fn record(sink: &mut Sink, kind: SecretKind, field: &str, preview: String) {
    sink.record(kind, field, preview);
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
fn detect_aws_key(s: &str, prev: Option<u8>, field: &str, sink: &mut Sink) {
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
                let left_ok = boundary_before(bytes, i, prev, is_token_char);
                let right_ok = i + 20 == bytes.len() || !is_token_char(bytes[i + 20]);
                if left_ok && right_ok {
                    let m = std::str::from_utf8(&bytes[i..i + 20]).unwrap_or("AKIA…");
                    record(sink, SecretKind::AwsAccessKeyId, field, masked_preview(m));
                }
            }
        }
        i += 1;
    }
}

/// Gemini API key: `AIza` + 35 chars of `[A-Za-z0-9_-]`.
fn detect_gemini_key(s: &str, prev: Option<u8>, field: &str, sink: &mut Sink) {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i + 39 <= bytes.len() {
        if &bytes[i..i + 4] == b"AIza" {
            let tail = &bytes[i + 4..i + 39];
            if tail
                .iter()
                .all(|c| c.is_ascii_alphanumeric() || *c == b'_' || *c == b'-')
            {
                let left_ok = boundary_before(bytes, i, prev, is_token_char);
                let right_ok = i + 39 == bytes.len() || !is_token_char(bytes[i + 39]);
                if left_ok && right_ok {
                    let m = std::str::from_utf8(&bytes[i..i + 39]).unwrap_or("AIza…");
                    record(sink, SecretKind::GeminiApiKey, field, masked_preview(m));
                }
            }
        }
        i += 1;
    }
}

/// Provider API keys / tokens with a fixed prefix AND a documented length, so
/// ordinary words and identifiers do not trip it (AT-8). Each shape must start
/// at a word boundary:
///
/// - OpenRouter: `sk-or-v1-` + exactly 64 hex digits.
/// - Anthropic: `sk-ant-` + a run of ≥ 40 `[A-Za-z0-9_-]`.
/// - OpenAI: `sk-proj-` / `sk-svcacct-` / `sk-admin-` + a run of ≥ 40; or a
///   `sk-` run of ≥ 40 carrying OpenAI's `T3BlbkFJ` marker; or the legacy
///   `sk-` + exactly 48 alphanumerics.
/// - GitHub: `ghp_` / `gho_` / `ghu_` / `ghs_` / `ghr_` + ≥ 36 alphanumerics;
///   `github_pat_` + a run of ≥ 70 `[A-Za-z0-9_]`.
///
/// `sk-learn`, `task-force`, `desk-lamp-…`, `sk-` + 47 characters and
/// `ghp_` + a short word are all NOT flagged.
fn detect_provider_keys(s: &str, prev: Option<u8>, field: &str, sink: &mut Sink) {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Only token starts can begin a key; skip the rest cheaply.
        if !matches!(bytes[i], b's' | b'g') || !boundary_before(bytes, i, prev, is_token_char) {
            i += 1;
            continue;
        }
        match provider_key_len(&bytes[i..]) {
            Some(len) => {
                // Previews only need head + tail; cap the copy for a huge run.
                let m = std::str::from_utf8(&bytes[i..i + len.min(200)]).unwrap_or("sk-…");
                record(sink, SecretKind::ProviderApiKey, field, masked_preview(m));
                i += len;
            }
            None => i += 1,
        }
    }
}

/// Length of the provider key that starts at `rest[0]`, if one does.
fn provider_key_len(rest: &[u8]) -> Option<usize> {
    let run = |from: usize, ok: fn(u8) -> bool| {
        rest.get(from..)
            .map_or(0, |t| t.iter().take_while(|&&b| ok(b)).count())
    };
    if rest.starts_with(b"sk-or-v1-") {
        let hex = run(9, |b| b.is_ascii_hexdigit());
        let end = 9 + hex;
        let bounded = rest.get(end).is_none_or(|&b| !is_token_char(b));
        return (hex == 64 && bounded).then_some(end);
    }
    if rest.starts_with(b"sk-ant-") {
        let n = run(7, is_token_char);
        return (n >= 40).then_some(7 + n);
    }
    for prefix in [&b"sk-proj-"[..], b"sk-svcacct-", b"sk-admin-"] {
        if rest.starts_with(prefix) {
            let n = run(prefix.len(), is_token_char);
            return (n >= 40).then_some(prefix.len() + n);
        }
    }
    if rest.starts_with(b"sk-") {
        let n = run(3, is_token_char);
        let body = &rest[3..3 + n];
        let has_marker = body.windows(8).any(|w| w == b"T3BlbkFJ");
        let legacy = n == 48 && body.iter().all(u8::is_ascii_alphanumeric);
        return ((n >= 40 && has_marker) || legacy).then_some(3 + n);
    }
    if rest.len() >= 4
        && rest.starts_with(b"gh")
        && matches!(rest[2], b'p' | b'o' | b'u' | b's' | b'r')
        && rest[3] == b'_'
    {
        let n = run(4, |b| b.is_ascii_alphanumeric());
        return (n >= 36).then_some(4 + n);
    }
    if rest.starts_with(b"github_pat_") {
        let n = run(11, |b| b.is_ascii_alphanumeric() || b == b'_');
        return (n >= 70).then_some(11 + n);
    }
    None
}

/// PEM private-key header: `-----BEGIN ` + an upper-case label ending in
/// `PRIVATE KEY` (or `PRIVATE KEY BLOCK` for PGP) + `-----`. `PUBLIC KEY`,
/// `CERTIFICATE`, and a header with no closing dashes are not flagged. The
/// preview is the header line — it is not key material.
fn detect_private_key_header(s: &str, field: &str, sink: &mut Sink) {
    const OPEN: &str = "-----BEGIN ";
    const CLOSE: &[u8] = b"-----";
    const MAX_LABEL: usize = 48;
    let bytes = s.as_bytes();
    let mut from = 0;
    while let Some(rel) = s[from..].find(OPEN) {
        let after = from + rel + OPEN.len();
        let window_end = (after + MAX_LABEL).min(bytes.len());
        if let Some(close) = bytes[after..window_end]
            .windows(CLOSE.len())
            .position(|w| w == CLOSE)
        {
            // Both ends sit next to ASCII bytes, so the slice is on boundaries.
            let label = &s[after..after + close];
            let upper_ascii = label.bytes().all(|b| b.is_ascii_uppercase() || b == b' ');
            if upper_ascii
                && (label.ends_with("PRIVATE KEY") || label.ends_with("PRIVATE KEY BLOCK"))
            {
                record(
                    sink,
                    SecretKind::PrivateKeyBlock,
                    field,
                    format!("{OPEN}{label}-----"),
                );
            }
        }
        from = after;
    }
}

/// JWT shape: three base64-url segments joined by `.`. Each segment
/// must be ≥ 10 chars to avoid catching arbitrary three-dotted strings
/// (e.g. semver versions).
fn detect_jwt(s: &str, prev: Option<u8>, field: &str, sink: &mut Sink) {
    // Walk through the string token-by-token. A token is a max run of
    // chars matching base64-url (`A-Za-z0-9_-`) without `.`. We need
    // three consecutive `token.token.token` separated by single dots
    // and bordered by non-token chars on the outside.
    let bytes = s.as_bytes();
    let n = bytes.len();
    let mut i = 0;
    while i < n {
        // Move i to a word boundary
        let left_ok = boundary_before(bytes, i, prev, is_jwt_char);
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
            record(sink, SecretKind::JwtLike, field, masked_preview(m));
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
fn detect_url_credentials(s: &str, field: &str, sink: &mut Sink) {
    // Schemes are ASCII; a Unicode fold can change byte lengths before a URL.
    let lower = s.to_ascii_lowercase();
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
                        let m = super::utf8_prefix(&s[url_start..end], 60);
                        record(sink, SecretKind::UrlCredentials, field, masked_preview(m));
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
fn detect_base64_blob(s: &str, prev: Option<u8>, field: &str, sink: &mut Sink) {
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
        // A run that begins at a window edge but continues the previous
        // window's text is the TAIL of a run already judged from where it
        // started; reporting it again would list one huge blob once per window.
        if start == 0 && prev.is_some_and(is_base64_char) {
            continue;
        }
        let run = i - start;
        if run >= 200 {
            if let Ok(m) = std::str::from_utf8(&bytes[start..(start + run.min(80))]) {
                record(sink, SecretKind::Base64Blob, field, masked_preview(m));
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
fn detect_long_hex(s: &str, prev: Option<u8>, field: &str, sink: &mut Sink) {
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
            // base64 detector). A hex run glued to a provider prefix
            // (`sk-or-v1-<64 hex>`) is reported by `detect_provider_keys`.
            let left_ok = boundary_before(bytes, start, prev, is_token_char);
            let right_ok = i == bytes.len() || !is_token_char(bytes[i]);
            if left_ok && right_ok {
                if let Ok(m) = std::str::from_utf8(&bytes[start..(start + run.min(60))]) {
                    record(sink, SecretKind::LongHex, field, masked_preview(m));
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

    #[test]
    fn url_credentials_keep_unicode_offsets_and_preview_boundaries() {
        let mut sink = Sink::new();
        detect_url_credentials("\u{130}https://\u{fc}", "body", &mut sink);
        assert!(sink.shown.is_empty());
        let text = format!(
            "\u{212a}\u{130}HTTPS://user:{}@host/path",
            "\u{754c}".repeat(40)
        );
        detect_url_credentials(&text, "body", &mut sink);
        let warnings = sink.finish();
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].kind, SecretKind::UrlCredentials);
    }

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
    fn warnings_capped_at_max_and_the_rest_are_counted_not_dropped() {
        // 20 DISTINCT hits: MAX_WARNINGS listed + one "N more" row.
        let mut obj = serde_json::Map::new();
        for i in 0..20 {
            obj.insert(
                format!("k{i:02}"),
                json!(format!(
                    "AKIA1234567890ABCDE{}",
                    (b'A' + (i as u8 % 26)) as char
                )),
            );
        }
        let v = Value::Object(obj);
        let w = scan_value(&v);
        assert_eq!(w.len(), MAX_WARNINGS + 1, "{w:?}");
        let more = w.last().unwrap();
        assert_eq!(more.kind, SecretKind::MoreNotShown);
        assert_eq!(more.preview, format!("{} more", 20 - MAX_WARNINGS));
        assert!(w[..MAX_WARNINGS]
            .iter()
            .all(|x| x.kind == SecretKind::AwsAccessKeyId));
    }

    /// The cap used to count RAW hits before dedup: the same key repeated more
    /// than the cap filled it, and every different secret after it was never
    /// scanned.
    #[test]
    fn duplicates_do_not_consume_the_cap() {
        let dup = "AKIA1234567890ABCDEF";
        let mut arr: Vec<Value> = (0..MAX_WARNINGS * 3).map(|_| json!(dup)).collect();
        arr.push(json!("ghp_0123456789abcdefghijklmnopqrstuvwxyz"));
        arr.push(json!("-----BEGIN RSA PRIVATE KEY-----"));
        let w = scan_value(&json!({ "parts": arr }));
        let kinds: Vec<SecretKind> = w.iter().map(|x| x.kind).collect();
        assert_eq!(
            kinds,
            vec![
                SecretKind::AwsAccessKeyId,
                SecretKind::ProviderApiKey,
                SecretKind::PrivateKeyBlock
            ],
            "{w:?}"
        );
    }

    #[test]
    fn a_payload_of_nothing_but_secrets_stops_early_with_a_plus() {
        let mut obj = serde_json::Map::new();
        for i in 0..(MAX_TRACKED + 50) {
            obj.insert(format!("k{i:04}"), json!(format!("AKIA{i:016}")));
        }
        let w = scan_value(&Value::Object(obj));
        let more = w.last().unwrap();
        assert_eq!(more.kind, SecretKind::MoreNotShown);
        assert_eq!(
            more.preview,
            format!("{}+ more", MAX_TRACKED - MAX_WARNINGS)
        );
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

    // ── AT-8: long strings are scanned in windows, not skipped ───────

    #[test]
    fn oversize_string_is_scanned_not_skipped_and_stays_fast() {
        // ~300 KB of the same key: one finding after dedup, and it must not
        // take seconds.
        let big = "AKIA1234567890ABCDEF ".repeat(15_000);
        let started = std::time::Instant::now();
        let w = scan_value(&json!({ "body": big }));
        assert_eq!(w.len(), 1, "{w:?}");
        assert_eq!(w[0].kind, SecretKind::AwsAccessKeyId);
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
    }

    #[test]
    fn secret_after_a_hundred_kib_of_text_is_found() {
        let body = format!(
            "{}\nkey: AKIA1234567890ABCDEF\n",
            "lorem ipsum ".repeat(9_000)
        );
        assert!(body.len() > 100 * 1024);
        let w = scan_value(&json!({ "body": body }));
        assert!(
            w.iter().any(|x| x.kind == SecretKind::AwsAccessKeyId),
            "{w:?}"
        );
    }

    /// A secret that straddles the first window's end is found via the overlap,
    /// at every offset around the cut and with multi-byte text in front.
    #[test]
    fn secret_straddling_a_window_boundary_is_found() {
        let key = "AKIA1234567890ABCDEF";
        for filler in ["a ", "界"] {
            for back in 0..=key.len() + 2 {
                let mut text = filler.repeat(WINDOW_BYTES); // longer than a window
                let cut = text.len().min(WINDOW_BYTES);
                let at = super::super::utf8_prefix(&text, cut - back.min(cut)).len();
                text.insert_str(at, &format!(" {key} "));
                let w = scan_value(&json!({ "body": text }));
                assert!(
                    w.iter().any(|x| x.kind == SecretKind::AwsAccessKeyId),
                    "key {back} bytes before the window end was missed (filler {filler:?})"
                );
            }
        }
    }

    /// One 1 MiB base64 run spans many windows; it is still ONE finding.
    #[test]
    fn a_huge_base64_run_is_reported_once_not_once_per_window() {
        let blob = "QUJD".repeat(256 * 1024); // 1 MiB of base64
        let w = scan_value(&json!({ "body": blob }));
        let n = w
            .iter()
            .filter(|x| x.kind == SecretKind::Base64Blob)
            .count();
        assert_eq!(n, 1, "{w:?}");
        // Same for a huge hex run.
        let hex = "ab".repeat(512 * 1024);
        let w = scan_value(&json!({ "body": hex }));
        let n = w.iter().filter(|x| x.kind == SecretKind::LongHex).count();
        assert!(n <= 1, "{w:?}");
    }

    #[test]
    fn input_past_the_scan_budget_is_reported_not_hidden() {
        let big = "x".repeat(MAX_STRING_SCAN_BYTES + 10 * 1024);
        let w = scan_value(&json!({ "body": big }));
        let more = w.last().expect("a summary row");
        assert_eq!(more.kind, SecretKind::MoreNotShown);
        assert_eq!(more.preview, "10 KiB not scanned");
    }

    // ── AT-8: provider key / token / PEM detectors ───────────────────

    fn kinds_of(text: &str) -> Vec<SecretKind> {
        scan_value(&json!({ "body": text }))
            .into_iter()
            .map(|w| w.kind)
            .collect()
    }

    fn flags_provider_key(text: &str) -> bool {
        kinds_of(text).contains(&SecretKind::ProviderApiKey)
    }

    const HEX64: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn openrouter_key_is_detected_and_is_not_swallowed_by_the_hex_rule() {
        // The AT-8 miss: the 64 hex run after `sk-or-v1-` has a `-` on its
        // left, so the long-hex detector's boundary rule skipped it.
        let key = format!("sk-or-v1-{HEX64}");
        assert_eq!(
            kinds_of(&format!("OPENROUTER_API_KEY={key}")),
            vec![SecretKind::ProviderApiKey]
        );
        // 63 or 65 hex digits is a different shape.
        assert!(!flags_provider_key(&format!("sk-or-v1-{}", &HEX64[..63])));
        assert!(!flags_provider_key(&format!("sk-or-v1-{HEX64}f")));
        // A word glued in front is not a token start.
        assert!(!flags_provider_key(&format!("xsk-or-v1-{HEX64}")));
    }

    #[test]
    fn openai_keys_are_detected() {
        let proj = format!("sk-proj-{}", "aB3_-".repeat(20)); // 100 chars
        assert!(flags_provider_key(&format!("key={proj}")));
        assert!(flags_provider_key(&format!(
            "sk-svcacct-{}",
            "a1".repeat(30)
        )));
        assert!(flags_provider_key(&format!("sk-admin-{}", "a1".repeat(30))));
        // Legacy: sk- + exactly 48 alphanumerics.
        assert!(flags_provider_key(&format!("sk-{}", "aB1".repeat(16))));
        // Newer unprefixed keys carry the T3BlbkFJ marker.
        assert!(flags_provider_key(&format!(
            "sk-{}T3BlbkFJ{}",
            "a".repeat(20),
            "b".repeat(20)
        )));
    }

    #[test]
    fn anthropic_key_is_detected() {
        let key = format!("sk-ant-api03-{}AA", "xY9_-".repeat(18));
        assert!(flags_provider_key(&format!("ANTHROPIC_API_KEY={key}")));
        assert!(!flags_provider_key("sk-ant-api03-short"));
    }

    #[test]
    fn github_tokens_are_detected() {
        for p in ["ghp_", "gho_", "ghu_", "ghs_", "ghr_"] {
            assert!(
                flags_provider_key(&format!("{p}{}", "aB3".repeat(12))),
                "{p}"
            );
        }
        assert!(flags_provider_key(&format!(
            "github_pat_{}_{}",
            "A1b2C3".repeat(4),
            "z9".repeat(30)
        )));
        // Too short / wrong prefix letter / not at a word start.
        assert!(!flags_provider_key("ghp_tooshort"));
        assert!(!flags_provider_key(&format!("ghx_{}", "a".repeat(36))));
        assert!(!flags_provider_key(&format!("xghp_{}", "a".repeat(36))));
        assert!(!flags_provider_key("github_pat_tooshort"));
    }

    #[test]
    fn ordinary_text_with_key_like_prefixes_is_not_flagged() {
        // sk- + 47 alphanumerics (one short of the legacy 48) and + 49 (one over).
        let short47 = format!("sk-{}", "a".repeat(47));
        let long49 = format!("sk-{}", "a".repeat(49));
        for text in [
            "sk-learn is a python library",
            "install scikit-learn and sk-learn-intelex today",
            "desk-lamp-with-a-very-long-descriptive-name-1234567890abcdefgh",
            "task-force-delta-sk-1234567890abcdefghijklmnopqrstuvwxyzABCDEFGH",
            short47.as_str(),
            long49.as_str(),
            "ghp is short for GitHub Personal token",
            "github_pat is how fine-grained tokens start",
            "-----BEGIN PUBLIC KEY-----",
            "-----BEGIN CERTIFICATE-----",
            "-----BEGIN PRIVATE KEY",
            "BEGIN RSA PRIVATE KEY",
        ] {
            let k = kinds_of(text);
            assert!(
                !k.contains(&SecretKind::ProviderApiKey)
                    && !k.contains(&SecretKind::PrivateKeyBlock),
                "false positive on {text:?}: {k:?}"
            );
        }
    }

    #[test]
    fn pem_private_key_headers_are_detected_and_never_echo_key_bytes() {
        for header in [
            "-----BEGIN PRIVATE KEY-----",
            "-----BEGIN RSA PRIVATE KEY-----",
            "-----BEGIN EC PRIVATE KEY-----",
            "-----BEGIN OPENSSH PRIVATE KEY-----",
            "-----BEGIN ENCRYPTED PRIVATE KEY-----",
            "-----BEGIN PGP PRIVATE KEY BLOCK-----",
        ] {
            let text =
                format!("config:\n{header}\nMIIEvQIBADANBgkqhkiG9w0BAQEFAASC\n-----END X-----");
            let w = scan_value(&json!({ "body": text }));
            let pem: Vec<_> = w
                .iter()
                .filter(|x| x.kind == SecretKind::PrivateKeyBlock)
                .collect();
            assert_eq!(pem.len(), 1, "{header}: {w:?}");
            assert_eq!(pem[0].preview, header);
            assert!(!pem[0].preview.contains("MIIE"));
        }
    }

    #[test]
    fn provider_key_straddling_a_window_boundary_is_found() {
        let key = format!("sk-or-v1-{HEX64}");
        for back in [0usize, 1, 20, 40, key.len()] {
            let mut text = " ".repeat(WINDOW_BYTES + 1000);
            text.replace_range(WINDOW_BYTES - back..WINDOW_BYTES - back, &key);
            let w = scan_value(&json!({ "body": text }));
            assert!(
                w.iter().any(|x| x.kind == SecretKind::ProviderApiKey),
                "missed at back={back}"
            );
        }
    }
}
