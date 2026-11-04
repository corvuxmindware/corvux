//! "Connect to Claude Desktop" — runs the bundled `corvux-mcp` sidecar to
//! register (or unregister) itself in Claude Desktop's MCP config.
//!
//! The sidecar ships next to the app executable (Tauri `externalBin`). We
//! locate it via `current_exe()`'s directory. In dev builds the sidecar
//! isn't copied next to the dev binary, so we fall back to its build output
//! under `corvux-mcp/target/release` (debug-only path, never shipped).

use std::path::PathBuf;
use tauri::State;

use super::files::RoostFsState;

/// Absolute path to the bundled `corvux-mcp` helper.
fn sidecar_path() -> Result<PathBuf, String> {
    let name = if cfg!(windows) {
        "corvux-mcp.exe"
    } else {
        "corvux-mcp"
    };
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let beside = dir.join(name);
            if beside.exists() {
                return Ok(beside);
            }
        }
    }
    #[cfg(debug_assertions)]
    {
        let dev = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("corvux-mcp")
            .join("target")
            .join("release")
            .join(name);
        if dev.exists() {
            return Ok(dev);
        }
    }
    Err("Corvux helper (corvux-mcp) not found next to the app. Reinstall Corvux.".to_string())
}

/// Build the standard `mcpServers` config JSON an MCP client (Cursor, Claude
/// Code, Windsurf, Cline, Claude Desktop…) needs, pointing at the bundled
/// helper `bin` and serving `roost`. Pretty-printed; `serde_json` handles
/// escaping so Windows backslashes and spaces in paths come out valid.
///
/// Pure so it's unit-testable without resolving the real sidecar path.
fn normalized_mode(mode: Option<&str>) -> Result<&str, String> {
    match mode.unwrap_or("read-only") {
        "full" | "read-only" | "capture-only" => Ok(mode.unwrap_or("read-only")),
        other => Err(format!("Unknown MCP access mode: {other}")),
    }
}

fn build_config_json(bin: &str, roost: &str, mode: &str) -> String {
    let cfg = serde_json::json!({
        "mcpServers": {
            "corvux": {
                "command": bin,
                "args": [roost, "--mode", mode],
            }
        }
    });
    // Pretty-print can't fail for this fixed, finite structure.
    serde_json::to_string_pretty(&cfg).unwrap_or_default()
}

async fn active_native_roost(requested: &str, state: &RoostFsState) -> Result<String, String> {
    let backend = state
        .current()
        .await
        .ok_or_else(|| "Open a roost first.".to_string())?;
    let active = backend.display_root();
    if active.starts_with("content://") {
        return Err(
            "MCP requires a desktop filesystem roost and is unavailable for Android SAF roosts."
                .to_string(),
        );
    }
    if !crate::commands::roost::same_roost_root(active, std::path::Path::new(requested)) {
        return Err("MCP request does not match the active roost".to_string());
    }
    Ok(active.to_string())
}

/// Return the copy-paste MCP config JSON for the bundled helper serving
/// `roost`. This is the universal path for MCP clients that don't have a
/// one-click button (everything except Claude Desktop): the user pastes it
/// into that tool's MCP config. The `command` is the resolved absolute path
/// to the bundled `corvux-mcp` helper.
#[tauri::command]
#[specta::specta]
pub async fn mcp_config_json(
    roost: String,
    mode: Option<String>,
    state: State<'_, RoostFsState>,
) -> Result<String, String> {
    let roost = roost.trim();
    if roost.is_empty() {
        return Err("Open a roost first.".to_string());
    }
    let roost = active_native_roost(roost, state.inner()).await?;
    let bin = sidecar_path()?;
    let mode = normalized_mode(mode.as_deref())?;
    Ok(build_config_json(&bin.to_string_lossy(), &roost, mode))
}

/// Map a frontend `action` (+ optional `roost`) to the sidecar CLI args.
///
/// Pure so it can be unit-tested without spawning the helper. Pinned
/// behavior:
///   - `connect` requires a non-blank roost path (errors otherwise) and
///     passes it through verbatim;
///   - `disconnect` / `status` ignore `roost`;
///   - any other action is rejected with the offending string echoed.
fn build_mcp_args(
    action: &str,
    roost: Option<String>,
    mode: Option<String>,
) -> Result<Vec<String>, String> {
    match action {
        "connect" => {
            let roost = roost
                .filter(|v| !v.trim().is_empty())
                .ok_or_else(|| "Open a roost before connecting.".to_string())?;
            let mode = normalized_mode(mode.as_deref())?;
            Ok(vec![
                "--connect".to_string(),
                roost,
                "--mode".to_string(),
                mode.to_string(),
            ])
        }
        "disconnect" => Ok(vec!["--disconnect".to_string()]),
        "status" => Ok(vec!["--status".to_string()]),
        other => Err(format!("Unknown action: {other}")),
    }
}

/// Connect / disconnect / check the Corvux MCP server in Claude Desktop's
/// config. `action` is `"connect" | "disconnect" | "status"`; `roost` is
/// the absolute roost path (required for `connect`). Returns the helper's
/// status line for display.
#[tauri::command]
#[specta::specta]
pub async fn claude_desktop_mcp(
    action: String,
    roost: Option<String>,
    mode: Option<String>,
    state: State<'_, RoostFsState>,
) -> Result<String, String> {
    let bin = sidecar_path()?;
    let roost = if action == "connect" {
        let requested = roost
            .as_deref()
            .ok_or_else(|| "Open a roost before connecting.".to_string())?;
        Some(active_native_roost(requested, state.inner()).await?)
    } else {
        None
    };
    let args: Vec<String> = build_mcp_args(&action, roost, mode)?;

    // Spawn the short-lived helper off the async runtime; it does file I/O.
    tokio::task::spawn_blocking(move || {
        let mut cmd = std::process::Command::new(&bin);
        cmd.args(&args);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // CREATE_NO_WINDOW — don't flash a console window from the GUI app.
            cmd.creation_flags(0x0800_0000);
        }
        let output = cmd
            .output()
            .map_err(|e| format!("Failed to run the Corvux helper: {e}"))?;
        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
        } else {
            let err = String::from_utf8_lossy(&output.stderr).trim().to_string();
            Err(if err.is_empty() {
                "The Corvux helper reported an error.".to_string()
            } else {
                err
            })
        }
    })
    .await
    .map_err(|e| format!("helper task failed: {e}"))?
}

#[cfg(test)]
mod tests {
    //! `sidecar_path` (locates the helper via `current_exe`/env) and the
    //! process spawn aren't deterministically testable here. The pure
    //! action→args mapping IS, and it's the part that gates whether a
    //! `connect` without a roost silently passes a bad arg to the helper.
    use super::*;

    #[test]
    fn connect_passes_roost_through() {
        let args = build_mcp_args(
            "connect",
            Some("/home/u/roost".to_string()),
            Some("read-only".to_string()),
        )
        .unwrap();
        assert_eq!(
            args,
            vec!["--connect", "/home/u/roost", "--mode", "read-only"]
        );
    }

    #[test]
    fn connect_without_roost_errors() {
        let err = build_mcp_args("connect", None, None).unwrap_err();
        assert!(err.contains("Open a roost"));
    }

    #[test]
    fn connect_with_blank_roost_errors() {
        // Whitespace-only is treated as absent, not a valid path.
        let err = build_mcp_args("connect", Some("   ".to_string()), None).unwrap_err();
        assert!(err.contains("Open a roost"));
    }

    #[test]
    fn disconnect_and_status_ignore_roost() {
        assert_eq!(
            build_mcp_args("disconnect", Some("/ignored".to_string()), None).unwrap(),
            vec!["--disconnect"]
        );
        assert_eq!(
            build_mcp_args("status", None, None).unwrap(),
            vec!["--status"]
        );
    }

    #[test]
    fn unknown_action_echoes_offender() {
        let err = build_mcp_args("frobnicate", None, None).unwrap_err();
        assert!(err.contains("Unknown action: frobnicate"));
    }

    #[test]
    fn config_json_has_expected_shape() {
        let json = build_config_json("/usr/local/bin/corvux-mcp", "/home/u/My Roost", "read-only");
        let v: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        assert_eq!(
            v["mcpServers"]["corvux"]["command"],
            "/usr/local/bin/corvux-mcp"
        );
        assert_eq!(v["mcpServers"]["corvux"]["args"][0], "/home/u/My Roost");
        assert_eq!(v["mcpServers"]["corvux"]["args"][2], "read-only");
    }

    #[test]
    fn config_json_escapes_windows_path() {
        // Backslashes must be JSON-escaped so the pasted config is valid.
        let bin = r"C:\Users\u\AppData\Local\Corvux\corvux-mcp.exe";
        let roost = r"C:\Users\u\Roost";
        let json = build_config_json(bin, roost, "full");
        // Raw output contains escaped backslashes…
        assert!(json.contains(r"corvux-mcp.exe"));
        assert!(
            json.contains(r"\\Users\\u\\"),
            "backslashes must be escaped:\n{json}"
        );
        // …and round-trips back to the exact unescaped path.
        let v: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        assert_eq!(v["mcpServers"]["corvux"]["command"], bin);
        assert_eq!(v["mcpServers"]["corvux"]["args"][0], roost);
    }

    #[test]
    fn access_mode_defaults_to_read_only_and_rejects_unknown_values() {
        assert_eq!(normalized_mode(None).unwrap(), "read-only");
        assert_eq!(
            normalized_mode(Some("capture-only")).unwrap(),
            "capture-only"
        );
        assert!(normalized_mode(Some("write-only")).is_err());
    }
}
