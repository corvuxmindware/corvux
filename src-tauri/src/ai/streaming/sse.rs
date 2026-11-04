//! Server-Sent Events frame splitting.
//!
//! SSE delivers events as text records separated by blank lines (RFC: two
//! `\n` characters, optionally `\r\n\r\n`). HTTP streams arrive byte-by-byte
//! with no respect for record boundaries, so callers accumulate into a
//! `String` buffer and ask this helper "where does the next complete event
//! end?"

/// Find the byte offset *just past* the end of the first complete SSE event
/// in `buffer`. Returns `None` when the buffer holds only a partial event
/// (caller should keep appending more bytes).
///
/// The return is the offset of the byte after the trailing blank line, so
/// `buffer[..pos]` is the complete event (including the delimiter) and
/// `buffer[pos..]` is whatever remains.
///
/// Handles both `\n\n` (Unix-style, what every cloud LLM provider ships) and
/// `\r\n\r\n` (strict-RFC servers — kept for safety, never observed in
/// practice from Gemini / OpenAI-compat / Ollama). Prefers `\n\n` when both
/// are present because a `\n\n` match means the event is already framed; a
/// later `\r\n\r\n` would be a *different* event further down the buffer.
pub fn find_sse_boundary(buffer: &str) -> Option<usize> {
    if let Some(pos) = buffer.find("\n\n") {
        return Some(pos + 2);
    }
    if let Some(pos) = buffer.find("\r\n\r\n") {
        return Some(pos + 4);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_buffer_returns_none() {
        assert_eq!(find_sse_boundary(""), None);
    }

    #[test]
    fn partial_event_returns_none() {
        // A single data line with no trailing blank-line delimiter is not
        // yet a complete event.
        assert_eq!(find_sse_boundary("data: hello\n"), None);
    }

    #[test]
    fn unix_style_delimiter_returns_offset_past_blank_line() {
        let buf = "data: hello\n\n";
        let pos = find_sse_boundary(buf).expect("must find delimiter");
        assert_eq!(&buf[..pos], "data: hello\n\n");
        assert_eq!(&buf[pos..], "");
    }

    #[test]
    fn crlf_delimiter_returns_offset_past_blank_line() {
        let buf = "data: hello\r\n\r\n";
        let pos = find_sse_boundary(buf).expect("must find delimiter");
        assert_eq!(&buf[..pos], "data: hello\r\n\r\n");
    }

    #[test]
    fn split_returns_only_first_event() {
        // Two events back-to-back — find_sse_boundary returns the end of
        // the first one so the caller can consume it and keep parsing.
        let buf = "data: a\n\ndata: b\n\n";
        let pos = find_sse_boundary(buf).expect("must find first delimiter");
        assert_eq!(&buf[..pos], "data: a\n\n");
        assert_eq!(&buf[pos..], "data: b\n\n");
    }

    #[test]
    fn multiline_event_handled_correctly() {
        // SSE events can contain multiple `data:` lines that form one
        // logical event — the delimiter is the blank line, not the first
        // `\n`.
        let buf = "event: chunk\ndata: line1\ndata: line2\n\n";
        let pos = find_sse_boundary(buf).expect("must find delimiter");
        assert_eq!(&buf[..pos], "event: chunk\ndata: line1\ndata: line2\n\n");
    }

    #[test]
    fn prefers_first_delimiter_when_both_present() {
        // Pathological: a CRLF delimiter later in the buffer must not
        // win over a Unix-style delimiter earlier — `\n\n` framed the
        // first event already, so that's the boundary we report.
        let buf = "data: a\n\ndata: b\r\n\r\n";
        let pos = find_sse_boundary(buf).expect("must find delimiter");
        assert_eq!(&buf[..pos], "data: a\n\n");
    }
}
