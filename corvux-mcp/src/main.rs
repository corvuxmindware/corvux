//! Corvux local MCP server.
//!
//! A lightweight, standalone Model Context Protocol server that exposes a
//! Corvux roost (a folder of markdown) as tools an MCP client — Claude
//! Desktop, Cursor, Claude Code, etc. — can call to capture notes, add
//! tasks, and read the roost.
//!
//! Transport: newline-delimited JSON-RPC 2.0 on stdin/stdout. The roost
//! path is the first CLI argument, or the `CORVUX_ROOST` environment
//! variable. Diagnostics go to stderr (MCP clients surface stderr in their
//! logs).

mod atomic_write;
mod claude_desktop;
mod conventions;
mod frontmatter;
mod index;
mod roost;
mod server;

use std::io::{self, BufRead, Write};
use std::path::PathBuf;

use roost::Roost;
use server::{AccessMode, Server};

/// Maximum JSON-RPC frame size. The bounded reader below stops retaining bytes
/// at this limit and discards the remainder through the newline, so an
/// oversized client frame cannot force an unbounded allocation.
const MAX_REQUEST_BYTES: usize = 10 * 1024 * 1024;

enum InputFrame {
    Line(String),
    Oversized(usize),
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mode = match requested_mode(&args) {
        Ok(mode) => mode,
        Err(error) => {
            eprintln!("corvux-mcp: {error}");
            std::process::exit(2);
        }
    };
    // Subcommands for the "Connect Claude Desktop" flow. Corvux's button
    // spawns this binary with one of these; everything else is serve mode.
    match args.first().map(String::as_str) {
        Some("--connect") => {
            let roost = args.get(1).cloned().unwrap_or_default();
            if roost.trim().is_empty() {
                eprintln!("usage: corvux-mcp --connect <roost-path>");
                std::process::exit(2);
            }
            return run_admin(claude_desktop::connect(&roost, mode.as_str()));
        }
        Some("--disconnect") => return run_admin(claude_desktop::disconnect()),
        Some("--status") => return run_admin(claude_desktop::status()),
        _ => {}
    }

    let roost_arg = args
        .first()
        .cloned()
        .filter(|s| !s.starts_with('-'))
        .or_else(|| std::env::var("CORVUX_ROOST").ok());

    let Some(roost_path) = roost_arg else {
        eprintln!(
            "corvux-mcp {}\n\nusage: corvux-mcp <roost-path> [--mode read-only|capture-only|full]\n   or: set CORVUX_ROOST=<roost-path>\n\nThe roost path is the folder your Corvux notes live in\n(find it in Corvux: Roost Switcher or Settings -> Manage Roosts).",
            env!("CARGO_PKG_VERSION")
        );
        std::process::exit(2);
    };

    let roost = match Roost::open(PathBuf::from(&roost_path)) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("corvux-mcp: {e}");
            std::process::exit(1);
        }
    };
    eprintln!(
        "corvux-mcp {}: serving roost {}",
        env!("CARGO_PKG_VERSION"),
        roost.root.display()
    );
    if !roost.is_corvux_roost() {
        eprintln!(
            "corvux-mcp: warning — no .corvux/ folder at that path; tools will refuse to write until it's a real Corvux roost."
        );
    }

    let server = Server::with_access_mode(roost, mode);
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut out = stdout.lock();

    let mut input = stdin.lock();
    loop {
        let frame = match read_bounded_line(&mut input, MAX_REQUEST_BYTES) {
            Ok(Some(frame)) => frame,
            Ok(None) => break,
            Err(e) => {
                eprintln!("corvux-mcp: stdin read error: {e}");
                break;
            }
        };
        let line = match frame {
            InputFrame::Line(line) => line,
            InputFrame::Oversized(actual) => {
                let err = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": serde_json::Value::Null,
                    "error": {
                        "code": -32600,
                        "message": format!("Request too large ({actual} bytes, max {MAX_REQUEST_BYTES})")
                    }
                });
                write_message(&mut out, &err);
                continue;
            }
        };
        if line.trim().is_empty() {
            continue;
        }

        let msg: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                // Parse error — reply per JSON-RPC with a null id.
                let resp = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": serde_json::Value::Null,
                    "error": { "code": -32700, "message": format!("Parse error: {e}") }
                });
                write_message(&mut out, &resp);
                continue;
            }
        };

        if let Some(response) = server.handle(&msg) {
            write_message(&mut out, &response);
        }
    }
}

fn requested_mode(args: &[String]) -> Result<AccessMode, String> {
    let from_flag = if let Some(index) = args.iter().position(|arg| arg == "--mode") {
        Some(
            args.get(index + 1)
                .filter(|value| !value.starts_with('-'))
                .ok_or_else(|| "--mode requires read-only, capture-only, or full".to_string())?
                .as_str(),
        )
    } else {
        None
    };
    let from_env = std::env::var("CORVUX_MCP_MODE").ok();
    AccessMode::parse(from_flag.or(from_env.as_deref()).unwrap_or("read-only"))
}

/// Print the result of an admin subcommand (`--connect` etc.) and exit:
/// the message to stdout on success, the error to stderr with code 1.
fn run_admin(result: Result<String, String>) {
    match result {
        Ok(msg) => {
            println!("{msg}");
            std::process::exit(0);
        }
        Err(e) => {
            eprintln!("corvux-mcp: {e}");
            std::process::exit(1);
        }
    }
}

/// Write one JSON-RPC message as a single line and flush — clients read
/// newline-delimited frames.
fn write_message(out: &mut impl Write, value: &serde_json::Value) {
    match serde_json::to_string(value) {
        Ok(s) => {
            if writeln!(out, "{s}").is_err() {
                return;
            }
            let _ = out.flush();
        }
        Err(e) => eprintln!("corvux-mcp: failed to serialize response: {e}"),
    }
}

fn read_bounded_line(reader: &mut impl BufRead, max: usize) -> io::Result<Option<InputFrame>> {
    let mut kept = Vec::with_capacity(max.min(64 * 1024));
    let mut total = 0usize;
    let mut saw_any = false;
    loop {
        let buffer = reader.fill_buf()?;
        if buffer.is_empty() {
            if !saw_any {
                return Ok(None);
            }
            break;
        }
        saw_any = true;
        let newline = buffer.iter().position(|b| *b == b'\n');
        let consumed = newline.map_or(buffer.len(), |index| index + 1);
        let payload_len = newline.unwrap_or(buffer.len());
        total = total.saturating_add(payload_len);
        if kept.len() < max {
            let remaining = max - kept.len();
            kept.extend_from_slice(&buffer[..payload_len.min(remaining)]);
        }
        reader.consume(consumed);
        if newline.is_some() {
            break;
        }
    }
    if total > max {
        return Ok(Some(InputFrame::Oversized(total)));
    }
    if kept.last() == Some(&b'\r') {
        kept.pop();
    }
    let line = String::from_utf8(kept)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "stdin frame is not UTF-8"))?;
    Ok(Some(InputFrame::Line(line)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_oversized_line_without_retaining_it() {
        let huge = format!("{}\nnext\n", "x".repeat(1025));
        let mut input = std::io::Cursor::new(huge.into_bytes());
        assert!(matches!(
            read_bounded_line(&mut input, 1024).unwrap(),
            Some(InputFrame::Oversized(1025))
        ));
        assert!(matches!(
            read_bounded_line(&mut input, 1024).unwrap(),
            Some(InputFrame::Line(line)) if line == "next"
        ));
    }

    #[test]
    fn accepts_normal_sized_line() {
        let mut input = std::io::Cursor::new(b"{\"jsonrpc\":\"2.0\"}\n".to_vec());
        assert!(matches!(
            read_bounded_line(&mut input, MAX_REQUEST_BYTES).unwrap(),
            Some(InputFrame::Line(_))
        ));
    }

    #[test]
    fn accepts_line_exactly_at_the_cap() {
        let exact = format!("{}\n", "x".repeat(1024));
        let mut input = std::io::Cursor::new(exact.into_bytes());
        assert!(matches!(
            read_bounded_line(&mut input, 1024).unwrap(),
            Some(InputFrame::Line(line)) if line.len() == 1024
        ));
    }

    #[test]
    fn access_mode_defaults_to_read_only() {
        assert_eq!(requested_mode(&[]).unwrap(), AccessMode::ReadOnly);
    }

    #[test]
    fn access_mode_flag_selects_full_access_explicitly() {
        let args = vec![
            "/roost".to_string(),
            "--mode".to_string(),
            "full".to_string(),
        ];
        assert_eq!(requested_mode(&args).unwrap(), AccessMode::Full);
    }

    #[test]
    fn access_mode_flag_without_value_fails_closed() {
        let args = vec!["/roost".to_string(), "--mode".to_string()];
        assert!(requested_mode(&args).is_err());
    }
}
