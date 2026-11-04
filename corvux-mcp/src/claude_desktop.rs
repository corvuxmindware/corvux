//! Wire this binary into Claude Desktop's config — the engine behind
//! Corvux's one-click "Connect Claude Desktop" button.
//!
//! Design: the bundled binary registers ITSELF. When run as
//! `corvux-mcp --connect <roost>`, it writes its own absolute path
//! (`current_exe()`) plus the roost into Claude Desktop's
//! `claude_desktop_config.json`, merging safely so any other servers and
//! preferences are preserved. Corvux's button just spawns the sidecar with
//! that flag — no path-guessing in the app.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

/// Absolute path to Claude Desktop's config file for this OS. Claude
/// Desktop is Windows/macOS only; the Linux branch exists for dev/CI.
pub fn config_path() -> Result<PathBuf, String> {
    #[cfg(windows)]
    {
        let appdata = std::env::var("APPDATA").map_err(|_| "APPDATA is not set".to_string())?;
        Ok(PathBuf::from(appdata)
            .join("Claude")
            .join("claude_desktop_config.json"))
    }
    #[cfg(target_os = "macos")]
    {
        let home = std::env::var("HOME").map_err(|_| "HOME is not set".to_string())?;
        Ok(PathBuf::from(home)
            .join("Library")
            .join("Application Support")
            .join("Claude")
            .join("claude_desktop_config.json"))
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let home = std::env::var("HOME").map_err(|_| "HOME is not set".to_string())?;
        Ok(PathBuf::from(home)
            .join(".config")
            .join("Claude")
            .join("claude_desktop_config.json"))
    }
}

/// Merge a `corvux` MCP server entry into an existing config (or a fresh
/// one), pointing at `command` with the roost as its argument. Everything
/// else in the config is preserved. Pure — returns the new JSON text.
#[cfg(test)]
pub fn merge_config(existing: Option<&str>, command: &str, roost: &str) -> Result<String, String> {
    merge_config_mode(existing, command, roost, "full")
}

pub fn merge_config_mode(
    existing: Option<&str>,
    command: &str,
    roost: &str,
    mode: &str,
) -> Result<String, String> {
    let mut root: Value = match existing {
        Some(s) if !s.trim().is_empty() => serde_json::from_str(s).map_err(|e| {
            format!("Claude Desktop config is not valid JSON ({e}); fix or remove it first.")
        })?,
        _ => json!({}),
    };
    let obj = root
        .as_object_mut()
        .ok_or_else(|| "Claude Desktop config root is not a JSON object".to_string())?;

    let servers = obj.entry("mcpServers").or_insert_with(|| json!({}));
    let servers = servers
        .as_object_mut()
        .ok_or_else(|| "`mcpServers` in the config is not a JSON object".to_string())?;

    servers.insert(
        "corvux".to_string(),
        json!({
            "command": command,
            "args": [roost, "--mode", mode],
            "env": { "CORVUX_MCP_MANAGED": "1" }
        }),
    );

    serde_json::to_string_pretty(&root).map_err(|e| e.to_string())
}

/// Remove the `corvux` entry from a config, leaving the rest intact.
pub fn remove_config(existing: &str) -> Result<String, String> {
    let mut root: Value = serde_json::from_str(existing)
        .map_err(|e| format!("Claude Desktop config is not valid JSON ({e})."))?;
    if let Some(servers) = root.get_mut("mcpServers").and_then(Value::as_object_mut) {
        servers.remove("corvux");
    }
    serde_json::to_string_pretty(&root).map_err(|e| e.to_string())
}

/// The roost currently wired to `corvux` in a config, if any.
pub fn connected_roost(existing: &str) -> Option<String> {
    let root: Value = serde_json::from_str(existing).ok()?;
    root.get("mcpServers")?
        .get("corvux")?
        .get("args")?
        .get(0)?
        .as_str()
        .map(str::to_string)
}

/// The `command` field of the existing `corvux` entry, if any.
fn entry_command(existing: &str) -> Option<String> {
    let root: Value = serde_json::from_str(existing).ok()?;
    root.get("mcpServers")?
        .get("corvux")?
        .get("command")?
        .as_str()
        .map(str::to_string)
}

/// This process's own executable path, normalized for comparison (symlinks
/// resolved on non-Windows, matching `connect`'s own resolution; separators
/// normalized so a config written with `\\` compares equal to a `/`-using
/// `current_exe()` reading, and vice versa).
fn current_exe_str() -> Result<String, String> {
    let exe = std::env::current_exe().map_err(|e| format!("current_exe failed: {e}"))?;
    #[cfg(not(windows))]
    let exe = exe.canonicalize().unwrap_or(exe);
    Ok(exe.to_string_lossy().replace('\\', "/"))
}

/// Whether an existing `corvux` entry's `command` points at THIS binary —
/// so `status`/`disconnect` only touch an entry they can confirm they own,
/// rather than assuming any `corvux`-named entry was created by an earlier
/// run of this exact binary (it could be a foreign/unrelated server that
/// merely chose the same key name).
fn entry_is_ours(command: Option<&str>) -> bool {
    let Ok(exe) = current_exe_str() else {
        return false;
    };
    command
        .map(|c| c.replace('\\', "/") == exe)
        .unwrap_or(false)
}

fn entry_is_managed(existing: &str) -> bool {
    let Ok(root) = serde_json::from_str::<Value>(existing) else {
        return false;
    };
    let Some(entry) = root.get("mcpServers").and_then(|v| v.get("corvux")) else {
        return false;
    };
    if entry
        .get("env")
        .and_then(|v| v.get("CORVUX_MCP_MANAGED"))
        .and_then(Value::as_str)
        == Some("1")
    {
        return true;
    }
    // Backward compatibility for configs written before the ownership marker.
    entry
        .get("command")
        .and_then(Value::as_str)
        .and_then(|command| Path::new(command).file_stem())
        .map(|stem| stem.to_string_lossy().eq_ignore_ascii_case("corvux-mcp"))
        .unwrap_or(false)
}

/// The next unused `<config>.corvux-bak`, `<config>.corvux-bak.1`, `.2`, …
/// path, so repeated reconnects each keep their own prior-state snapshot
/// instead of a second reconnect silently clobbering the first backup.
fn next_backup_path(config: &Path) -> PathBuf {
    let base = with_suffix(config, ".corvux-bak");
    if !base.exists() {
        return base;
    }
    for n in 1..1000 {
        let candidate = with_suffix(config, &format!(".corvux-bak.{n}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    base // astronomically unlikely to be reached; better than refusing to connect
}

/// Connect at an explicit config path (used by `connect` and by tests).
/// Backs up any existing config to a fresh, never-clobbered `*.corvux-bak*`
/// path before writing.
#[cfg(test)]
pub fn connect_at(config: &Path, command: &str, roost: &str) -> Result<(), String> {
    connect_at_mode(config, command, roost, "full")
}

pub fn connect_at_mode(
    config: &Path,
    command: &str,
    roost: &str,
    mode: &str,
) -> Result<(), String> {
    let parent = config
        .parent()
        .ok_or_else(|| "config path has no parent".to_string())?;
    if !parent.exists() {
        return Err(format!(
            "Claude Desktop config folder not found at {}. Is Claude Desktop installed?",
            parent.display()
        ));
    }
    let existing = fs::read_to_string(config).ok();
    if let Some(prior) = &existing {
        if entry_command(prior).is_some() && !entry_is_managed(prior) {
            return Err(
                "Claude Desktop already has a foreign `corvux` MCP entry; refusing to overwrite it. Rename or remove that entry first."
                    .to_string(),
            );
        }
        let bak = next_backup_path(config);
        // Best-effort: a failed backup shouldn't block the connect. Still
        // routed through the hardened primitive (not a raw fs::write) — the
        // backup holds the same config content (possibly other MCP
        // servers' secrets) so it gets the same symlink-safe write.
        let _ = crate::atomic_write::write_atomic(&bak, prior.as_bytes());
    }
    let merged = merge_config_mode(existing.as_deref(), command, roost, mode)?;
    write_atomic(config, &merged)
}

/// Connect this running binary to Claude Desktop for `roost`.
pub fn connect(roost: &str, mode: &str) -> Result<String, String> {
    let exe = std::env::current_exe().map_err(|e| format!("current_exe failed: {e}"))?;
    // Resolve symlinks on macOS (the app may be launched through one). NOT
    // on Windows: canonicalize() there adds a `\\?\` verbatim prefix that
    // child-process launchers (Claude Desktop / Node) can't spawn — and
    // current_exe() is already a clean absolute path on Windows.
    #[cfg(not(windows))]
    let exe = exe.canonicalize().unwrap_or(exe);
    let config = config_path()?;
    connect_at_mode(&config, &exe.to_string_lossy(), roost, mode)?;
    Ok(format!(
        "Connected Corvux to Claude Desktop (roost: {roost}, access: {mode}).\nRestart Claude Desktop to finish."
    ))
}

/// Remove Corvux from Claude Desktop's config. Only touches the entry if its
/// `command` matches this exact binary — a `corvux`-named entry pointing at
/// a different executable is left untouched rather than assumed to be ours.
pub fn disconnect() -> Result<String, String> {
    disconnect_at(&config_path()?)
}

/// `disconnect`'s logic against an explicit config path (used by `disconnect`
/// and by tests, mirroring `connect`/`connect_at`).
fn disconnect_at(config: &Path) -> Result<String, String> {
    let Ok(existing) = fs::read_to_string(config) else {
        return Ok("Corvux was not connected to Claude Desktop.".to_string());
    };
    if connected_roost(&existing).is_none() {
        return Ok("Corvux was not connected to Claude Desktop.".to_string());
    }
    let command = entry_command(&existing);
    if !entry_is_ours(command.as_deref()) && !entry_is_managed(&existing) {
        return Ok(format!(
            "A `corvux` entry exists but points at a different binary ({}) — not removing it. \
             If this is stale, edit the config by hand.",
            command.as_deref().unwrap_or("?")
        ));
    }
    let updated = remove_config(&existing)?;
    write_atomic(config, &updated)?;
    Ok("Disconnected Corvux from Claude Desktop. Restart it to finish.".to_string())
}

/// Report whether Corvux is connected and to which roost. Distinguishes a
/// connection made by THIS binary from a `corvux`-named entry pointing at a
/// different executable (e.g. a stale path after a reinstall, or an
/// unrelated server that happened to choose the same key).
pub fn status() -> Result<String, String> {
    status_at(&config_path()?)
}

/// `status`'s logic against an explicit config path (used by `status` and by
/// tests, mirroring `connect`/`connect_at`).
fn status_at(config: &Path) -> Result<String, String> {
    match fs::read_to_string(config) {
        Ok(existing) => match connected_roost(&existing) {
            Some(roost) => {
                let command = entry_command(&existing);
                if entry_is_ours(command.as_deref()) || entry_is_managed(&existing) {
                    Ok(format!("Connected to roost: {roost}"))
                } else {
                    Ok(format!(
                        "A different corvux-mcp binary is connected (command: {}, roost: {roost}). \
                         Reconnect to take it over.",
                        command.as_deref().unwrap_or("?")
                    ))
                }
            }
            None => Ok("Not connected.".to_string()),
        },
        Err(_) => Ok("Not connected (no Claude Desktop config found).".to_string()),
    }
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut p = path.to_path_buf();
    let name = format!(
        "{}{suffix}",
        path.file_name().unwrap_or_default().to_string_lossy()
    );
    p.set_file_name(name);
    p
}

/// Thin wrapper over the shared hardened primitive (see `atomic_write`) so
/// this module's call sites don't need `.as_bytes()` at every site.
fn write_atomic(path: &Path, content: &str) -> Result<(), String> {
    crate::atomic_write::write_atomic(path, content.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn merge_into_empty_config() {
        let out = merge_config(None, "/path/corvux-mcp", "/my/roost").unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["mcpServers"]["corvux"]["command"], "/path/corvux-mcp");
        assert_eq!(v["mcpServers"]["corvux"]["args"][0], "/my/roost");
    }

    #[test]
    fn merge_preserves_existing_keys_and_servers() {
        let existing = r#"{
            "mcpServers": { "other": { "command": "x" } },
            "preferences": { "sidebarMode": "chat" }
        }"#;
        let out = merge_config(Some(existing), "C:\\corvux-mcp.exe", "C:\\roost").unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        // existing server + preferences survive
        assert_eq!(v["mcpServers"]["other"]["command"], "x");
        assert_eq!(v["preferences"]["sidebarMode"], "chat");
        // corvux added
        assert_eq!(v["mcpServers"]["corvux"]["command"], "C:\\corvux-mcp.exe");
    }

    #[test]
    fn merge_rejects_malformed_json() {
        assert!(merge_config(Some("{not json"), "x", "y").is_err());
    }

    #[test]
    fn remove_and_status_roundtrip() {
        let merged = merge_config(None, "/bin/corvux-mcp", "/roost").unwrap();
        assert_eq!(connected_roost(&merged).as_deref(), Some("/roost"));
        let removed = remove_config(&merged).unwrap();
        assert_eq!(connected_roost(&removed), None);
    }

    #[test]
    fn connect_at_writes_and_backs_up() {
        let dir = TempDir::new().unwrap();
        let cfg = dir.path().join("claude_desktop_config.json");
        fs::write(&cfg, r#"{"preferences":{"a":1}}"#).unwrap();

        connect_at(&cfg, "/bin/corvux-mcp", "/roost").unwrap();

        let written = fs::read_to_string(&cfg).unwrap();
        let v: Value = serde_json::from_str(&written).unwrap();
        assert_eq!(v["mcpServers"]["corvux"]["args"][0], "/roost");
        assert_eq!(v["preferences"]["a"], 1); // preserved
                                              // backup of the original was made
        assert!(dir
            .path()
            .join("claude_desktop_config.json.corvux-bak")
            .exists());
    }

    #[test]
    fn connect_at_errors_when_folder_missing() {
        let dir = TempDir::new().unwrap();
        let cfg = dir.path().join("nope").join("claude_desktop_config.json");
        assert!(connect_at(&cfg, "/bin/x", "/roost").is_err());
    }

    #[test]
    fn connect_at_refuses_to_replace_foreign_entry() {
        let dir = TempDir::new().unwrap();
        let cfg = dir.path().join("claude_desktop_config.json");
        fs::write(
            &cfg,
            r#"{"mcpServers":{"corvux":{"command":"/foreign/server","args":[]}}}"#,
        )
        .unwrap();
        assert!(connect_at(&cfg, "/bin/corvux-mcp", "/roost").is_err());
        assert!(fs::read_to_string(&cfg)
            .unwrap()
            .contains("/foreign/server"));
    }

    #[cfg(unix)]
    #[test]
    fn config_backup_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new().unwrap();
        let cfg = dir.path().join("claude_desktop_config.json");
        fs::write(&cfg, r#"{"mcpServers":{}}"#).unwrap();
        connect_at(&cfg, "/bin/corvux-mcp", "/roost").unwrap();
        let backup = dir.path().join("claude_desktop_config.json.corvux-bak");
        assert_eq!(
            fs::metadata(backup).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    // ── Cluster E: config ownership ─────────────────────────────────────

    #[test]
    fn connect_at_versioned_backup_does_not_clobber_prior_backup() {
        let dir = TempDir::new().unwrap();
        let cfg = dir.path().join("claude_desktop_config.json");
        fs::write(&cfg, r#"{"preferences":{"a":1}}"#).unwrap();

        connect_at(&cfg, "/bin/corvux-mcp", "/roost-one").unwrap();
        connect_at(&cfg, "/bin/corvux-mcp", "/roost-two").unwrap();

        let first_bak = dir.path().join("claude_desktop_config.json.corvux-bak");
        let second_bak = dir.path().join("claude_desktop_config.json.corvux-bak.1");
        assert!(first_bak.exists(), "first backup must survive");
        assert!(
            second_bak.exists(),
            "second reconnect must get its own backup"
        );
        // The first backup is the ORIGINAL config, not overwritten by the
        // second reconnect's prior state.
        let first_bak_content = fs::read_to_string(&first_bak).unwrap();
        assert!(first_bak_content.contains("\"a\":1") || first_bak_content.contains("\"a\": 1"));
    }

    #[test]
    fn status_at_reports_foreign_entry_when_command_differs() {
        let dir = TempDir::new().unwrap();
        let cfg = dir.path().join("claude_desktop_config.json");
        // A command that can never equal this test binary's own exe path.
        let foreign = r#"{"mcpServers":{"corvux":{"command":"/definitely/not/this/test/binary","args":["/roost"]}}}"#;
        fs::write(&cfg, foreign).unwrap();

        let msg = status_at(&cfg).unwrap();
        assert!(
            msg.contains("different corvux-mcp binary"),
            "expected foreign-entry message, got: {msg}"
        );
    }

    #[test]
    fn status_at_reports_not_connected_when_no_entry() {
        let dir = TempDir::new().unwrap();
        let cfg = dir.path().join("claude_desktop_config.json");
        fs::write(&cfg, r#"{"mcpServers":{}}"#).unwrap();
        assert_eq!(status_at(&cfg).unwrap(), "Not connected.");
    }

    #[test]
    fn disconnect_at_leaves_foreign_corvux_entry_untouched_when_command_differs() {
        let dir = TempDir::new().unwrap();
        let cfg = dir.path().join("claude_desktop_config.json");
        let foreign = r#"{"mcpServers":{"corvux":{"command":"/definitely/not/this/test/binary","args":["/roost"]}}}"#;
        fs::write(&cfg, foreign).unwrap();

        let msg = disconnect_at(&cfg).unwrap();
        assert!(
            msg.contains("different binary"),
            "expected a foreign-entry explanation, got: {msg}"
        );
        // The entry must still be there — disconnect must not have touched it.
        let after = fs::read_to_string(&cfg).unwrap();
        assert_eq!(connected_roost(&after).as_deref(), Some("/roost"));
    }

    #[test]
    fn disconnect_at_removes_entry_when_command_is_ours() {
        let dir = TempDir::new().unwrap();
        let cfg = dir.path().join("claude_desktop_config.json");
        let exe = current_exe_str().unwrap();
        let ours = merge_config(None, &exe, "/roost").unwrap();
        fs::write(&cfg, &ours).unwrap();

        let msg = disconnect_at(&cfg).unwrap();
        assert!(msg.contains("Disconnected"), "got: {msg}");
        let after = fs::read_to_string(&cfg).unwrap();
        assert_eq!(connected_roost(&after), None);
    }
}
