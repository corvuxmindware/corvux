//! Queue endpoint URL validation. Pulled out of `commands::sync` per
//! Q-03 so the trust boundary between user-supplied endpoints and the
//! network layer is in one auditable file. The validator is the only
//! gate between an attacker-controlled URL and `QueueClient::new` —
//! every userinfo / scheme / allowlist case lives here.
//!
//! S-02: see
//! `docs/audits/engineering-code-security-audit-2026-05-24.md` for the
//! bypass that motivated the rewrite (manual `://` + `:` splitting
//! read `meetcorvux.com` as the host of
//! `https://meetcorvux.com:443@evil.example/q`, while reqwest dialled
//! `evil.example`). Now routes through `url::Url`.

/// Hosts always accepted for queue endpoints, even without a user pin.
pub(super) const DEFAULT_QUEUE_HOST_ALLOWLIST: &[&str] = &["meetcorvux.com"];

/// Validate a queue endpoint URL. Rejects `http://`, malformed URLs,
/// URLs that carry userinfo (the `userinfo@host` form RFC 3986 allows),
/// and any host outside the allowlist or the user-pinned host. Returns
/// the trimmed URL on success (no other rewriting — preserves the
/// user's path/query exactly).
pub(super) fn validate_queue_endpoint(url: &str, pinned_host: &str) -> Result<String, String> {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        return Ok(String::new()); // clearing the endpoint
    }

    let parsed =
        url::Url::parse(trimmed).map_err(|e| format!("queue endpoint is not a valid URL: {e}"))?;

    // Plaintext leaks the social graph, message sizes, and Ed25519
    // auth-challenge signatures even with encrypted payloads.
    match parsed.scheme() {
        "https" => {}
        "http" => return Err("queue endpoint must use https://".into()),
        s => return Err(format!("queue endpoint must use https:// (got '{s}')")),
    }

    // RFC 3986 userinfo (`user[:pass]@host`) is the bypass vector — a
    // friendly-looking allowlisted host can be moved into the userinfo
    // while the actual `host_str()` reqwest dials is something else.
    // Forbid userinfo outright; there's no legitimate reason for a
    // queue endpoint to carry credentials in the URL.
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err("queue endpoint must not contain user:password@ — strip credentials".into());
    }

    let host = parsed
        .host_str()
        .ok_or_else(|| "queue endpoint missing host".to_string())?
        .to_ascii_lowercase();
    if host.is_empty() {
        return Err("queue endpoint missing host".into());
    }

    // Allowlist comparison. Default allowlist accepts `meetcorvux.com`
    // and any subdomain; pinned hosts match exactly. If the pinned host
    // contains a `:port`, the URL's port must match. If the pinned host
    // has no explicit port, any port (or none) is fine.
    let in_default = DEFAULT_QUEUE_HOST_ALLOWLIST
        .iter()
        .any(|allowed| host == *allowed || host.ends_with(&format!(".{allowed}")));

    let in_pinned = if pinned_host.is_empty() {
        false
    } else {
        let pinned = pinned_host.to_ascii_lowercase();
        match pinned.split_once(':') {
            Some((pin_host, pin_port_str)) => {
                let pin_port: u16 = pin_port_str
                    .parse()
                    .map_err(|_| format!("pinned host has invalid port: '{pinned_host}'"))?;
                host == pin_host && parsed.port() == Some(pin_port)
            }
            None => host == pinned,
        }
    };

    if !in_default && !in_pinned {
        return Err(format!(
            "queue host '{host}' is not in the allowlist; pin it first via Settings"
        ));
    }
    Ok(trimmed.to_string())
}

/// Normalize a pin request (`host` + the "type it twice" confirmation) into the
/// stored form. An empty host means "clear the pin".
///
/// The double entry is a TYPO guard for a human, nothing more: both strings
/// arrive in the same IPC message, so a script in the webview satisfies it
/// trivially. The real control is [`authorize_pin_change`].
pub(super) fn normalize_pin_host(host: &str, host_confirm: &str) -> Result<String, String> {
    let host = host.trim().to_ascii_lowercase();
    let confirm = host_confirm.trim().to_ascii_lowercase();
    if host != confirm {
        return Err("host and confirmation must match exactly".into());
    }
    if !host.is_empty() {
        // Sanity: host should not include a scheme or path. We accept hostnames
        // only (`example.com`, `queue.example.com:8443` allowed).
        if host.contains("://") || host.contains('/') {
            return Err("pinned host must be a bare hostname, no scheme or path".into());
        }
        if host.chars().any(|c| c.is_whitespace()) {
            return Err("pinned host must not contain whitespace".into());
        }
    }
    Ok(host)
}

/// F3 (sharp-edges 2026-11-11): pinning a host is what lets a queue endpoint on
/// that host pass [`validate_queue_endpoint`], so it is the real trust decision.
/// Setting a NEW pin needs the user's answer in a native OS dialog the webview
/// cannot click. Clearing a pin, or re-saving the pin already in place, widens
/// nothing and does not prompt.
pub(super) async fn authorize_pin_change(
    current_pin: &str,
    new_pin: &str,
    confirm: &(impl super::native_confirm::NativeConfirm + ?Sized),
) -> Result<(), String> {
    if new_pin.is_empty() || new_pin.eq_ignore_ascii_case(current_pin) {
        return Ok(());
    }
    let body = format!(
        "Corvux is about to trust \"{}\" as a sync queue server — a relay that holds your \
         encrypted sync data while your other devices are offline.\n\n\
         The server cannot read your notes, but it will see when and how much you sync. \
         Only continue for a server you run or trust.\n\n\
         If you did not just do this in Settings \u{2192} Sync, choose Cancel.",
        super::native_confirm::dialog_safe(new_pin, 100)
    );
    if confirm
        .confirm("Trust this sync server?", &body, "Trust server")
        .await
    {
        Ok(())
    } else {
        Err("Not trusted: the confirmation was cancelled, so the host was not pinned.".into())
    }
}

// ── R1: native confirm for pairing with an unknown sync peer ──────────

/// Stable error token when the user declined the first-pair dialog (R1).
/// The pairing UI matches this to show "cancelled" instead of a raw error.
/// Keep in sync with `isSyncPairDeclined` in the TS layer.
pub(super) const SYNC_PAIR_DECLINED: &str = "SYNC_PAIR_DECLINED";

/// Cap on the confirmed-peer record (each entry required a human click).
const MAX_CONFIRMED_PEERS: usize = 512;

/// Serializes the confirmed-peers read-modify-write.
static CONFIRMED_PEERS_WRITE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Backend-owned record of sync peers the user confirmed in a native
/// dialog (`app_data_dir/confirmed_sync_peers.json`) — sharp-edges R1.
///
/// `sync_connect` / `sync_connect_iroh` take the peer address/ticket AND
/// the PIN from the webview, and an attacker running their own node
/// chooses both — so the app-layer proof alone cannot tell a chosen peer
/// from a user-chosen one. The first pair with a peer identity the backend
/// has never confirmed needs the user's answer in a dialog the webview
/// cannot click. Reconnects to already-confirmed peers stay silent (the
/// frontend auto-dial loop is inert since SYN-M10; the engine's own
/// reconnect supervisor does not cross this IPC boundary). Fail closed:
/// unreadable or garbage ⇒ empty ⇒ every pair re-confirms.
pub(super) fn confirmed_peers_file(dir: &std::path::Path) -> std::path::PathBuf {
    dir.join("confirmed_sync_peers.json")
}

fn load_confirmed_peers(dir: &std::path::Path) -> std::collections::BTreeSet<String> {
    let Ok(text) = std::fs::read_to_string(confirmed_peers_file(dir)) else {
        return Default::default();
    };
    match serde_json::from_str::<Vec<String>>(&text) {
        Ok(list) => list.into_iter().collect(),
        Err(_) => Default::default(),
    }
}

fn save_confirmed_peers(
    dir: &std::path::Path,
    peers: &std::collections::BTreeSet<String>,
) -> Result<(), String> {
    let json = serde_json::to_string_pretty(peers).map_err(|e| e.to_string())?;
    let tmp = dir.join("confirmed_sync_peers.json.tmp");
    std::fs::write(&tmp, json.as_bytes()).map_err(|e| format!("write confirmed peers: {e}"))?;
    std::fs::rename(&tmp, confirmed_peers_file(dir))
        .map_err(|e| format!("rename confirmed peers: {e}"))
}

/// Identity key for the confirm record. `kind` distinguishes transports:
/// `iroh:` + NodeId hex, or `tcp:` + socket address. The decision happens
/// BEFORE the dial, so the key is the peer identity the webview itself
/// supplied — which is exactly the value a hijacked renderer chooses.
pub(super) fn peer_confirm_key(kind: &str, id: &str) -> String {
    format!("{kind}:{id}")
}

/// Short, human-checkable form of a peer id for the dialog (64-hex NodeIds
/// do not fit; the first 16 chars are enough to compare with the host card).
pub(super) fn short_id(id: &str, chars: usize) -> String {
    super::native_confirm::dialog_safe(
        &id.chars().take(chars.saturating_sub(1)).collect::<String>(),
        chars,
    )
}

/// The R1 gate: `Ok(())` = this peer may be paired (already confirmed, or
/// the user just answered yes in the native dialog). Decline / dismiss /
/// failure to show the dialog ⇒ `Err(SYNC_PAIR_DECLINED)`.
pub(super) async fn authorize_first_pair(
    app_data_dir: &std::path::Path,
    key: &str,
    shown_id: &str,
    confirm: &(impl super::native_confirm::NativeConfirm + ?Sized),
) -> Result<(), String> {
    if load_confirmed_peers(app_data_dir).contains(key) {
        return Ok(());
    }
    let body = format!(
        "Corvux is about to pair with this device and sync every note in this \
         roost with it, in both directions:\n\n{}\n\n\
         A paired device can read and change all notes in the roost. Only \
         continue if you recognize this device.\n\n\
         If you did not just scan a code or enter an address in Corvux, \
         choose Cancel.",
        super::native_confirm::dialog_safe(shown_id, 120)
    );
    if !confirm
        .confirm("Pair with this device?", &body, "Pair")
        .await
    {
        return Err(SYNC_PAIR_DECLINED.to_string());
    }
    let _lane = CONFIRMED_PEERS_WRITE.lock().await;
    let mut peers = load_confirmed_peers(app_data_dir);
    if peers.len() >= MAX_CONFIRMED_PEERS && !peers.contains(key) {
        return Err(format!(
            "Too many confirmed sync peers (limit {MAX_CONFIRMED_PEERS}); unpair one first."
        ));
    }
    peers.insert(key.to_string());
    if let Err(e) = save_confirmed_peers(app_data_dir, &peers) {
        // Fail safe: the pair proceeds, but the next pair with this peer
        // will confirm again rather than trusting an unwritable record.
        tracing::warn!(error = %e, "sync pair: persisting confirmed peer failed");
    }
    Ok(())
}

/// R1-in (sharp-edges residual): showing the pairing code / starting the
/// listener is what admits an INBOUND first pair — `sync_listen_iroh` mints
/// the ticket + opens the enrollment window, and both it and the ticket+PIN
/// come from (or go to) the webview. A hijacked renderer can reveal a fresh
/// code to an attacker's device with no user gesture, so the reveal itself
/// needs the user's answer in a native OS dialog. Decline/dismiss/failure ⇒
/// `Err` and the caller must not mint a ticket nor open the window. Reconnect
/// autostart (`sync_try_autostart`) deliberately does NOT open the enrollment
/// window, so it is unaffected.
pub(super) async fn authorize_pairing_reveal(
    confirm: &(impl super::native_confirm::NativeConfirm + ?Sized),
) -> Result<(), String> {
    let body = "Corvux is about to show your sync pairing code (and accept a \
                new device while pairing is active).\n\n\
                Any device that gets this code can pair and then read and \
                change every note in this roost.\n\n\
                If you did not just choose \u{201c}Show code\u{201d} in Corvux, \
                choose Cancel.";
    if confirm
        .confirm("Show your pairing code?", body, "Show code")
        .await
    {
        Ok(())
    } else {
        Err(
            "Pairing cancelled: the confirmation was cancelled, so no pairing \
             code was shown and no device can pair."
                .into(),
        )
    }
}

#[cfg(test)]
mod pin_tests {
    use super::super::native_confirm::test_support::Scripted;
    use super::*;

    #[test]
    fn normalizes_and_requires_matching_entries() {
        assert_eq!(
            normalize_pin_host(" Q.Example.NET ", "q.example.net").unwrap(),
            "q.example.net"
        );
        assert!(normalize_pin_host("a.example", "b.example").is_err());
        assert_eq!(normalize_pin_host("", "  ").unwrap(), "");
    }

    #[test]
    fn rejects_scheme_path_and_whitespace() {
        assert!(normalize_pin_host("https://q.example", "https://q.example").is_err());
        assert!(normalize_pin_host("q.example/x", "q.example/x").is_err());
        assert!(normalize_pin_host("q example", "q example").is_err());
    }

    /// The audit's attack, verbatim: both fields supplied by the same caller.
    /// The double entry passes — the native dialog is what stops it.
    #[tokio::test]
    async fn a_script_that_types_the_host_twice_is_still_stopped_by_the_dialog() {
        let host = normalize_pin_host("evil.example", "evil.example").unwrap();
        let dialog = Scripted::deny();
        let err = authorize_pin_change("", &host, &dialog).await.unwrap_err();
        assert!(err.contains("not pinned"), "{err}");
        assert_eq!(dialog.times_asked(), 1);
    }

    #[tokio::test]
    async fn an_approved_new_pin_goes_through() {
        let dialog = Scripted::allow();
        authorize_pin_change("old.example", "q.example", &dialog)
            .await
            .unwrap();
        assert_eq!(dialog.times_asked(), 1);
    }

    #[tokio::test]
    async fn clearing_or_resaving_the_same_pin_never_prompts() {
        let dialog = Scripted::deny();
        authorize_pin_change("q.example", "", &dialog)
            .await
            .unwrap();
        authorize_pin_change("q.example", "q.example", &dialog)
            .await
            .unwrap();
        authorize_pin_change("q.example", "Q.EXAMPLE", &dialog)
            .await
            .unwrap();
        assert_eq!(dialog.times_asked(), 0);
    }

    #[tokio::test]
    async fn control_characters_in_the_host_cannot_forge_dialog_text() {
        // `normalize_pin_host` rejects whitespace, but `\r`/`\n` are whitespace
        // too — so a forged line never reaches the dialog. Belt and braces:
        // dialog_safe flattens anything that slips through.
        assert!(normalize_pin_host("a.example\nFAKE", "a.example\nFAKE").is_err());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_http() {
        assert!(validate_queue_endpoint("http://meetcorvux.com/q", "").is_err());
    }

    #[test]
    fn rejects_unknown_host() {
        let err = validate_queue_endpoint("https://evil.example.com/q", "").unwrap_err();
        assert!(
            err.contains("allowlist"),
            "expected allowlist error, got: {err}"
        );
    }

    #[test]
    fn accepts_default_host_and_subdomains() {
        assert!(validate_queue_endpoint("https://meetcorvux.com/q", "").is_ok());
        assert!(validate_queue_endpoint("https://queue.meetcorvux.com/q", "").is_ok());
    }

    #[test]
    fn accepts_pinned_host() {
        assert!(validate_queue_endpoint("https://q.example.net/q", "q.example.net").is_ok());
    }

    #[test]
    fn pinned_host_case_insensitive() {
        assert!(validate_queue_endpoint("https://Q.Example.Net/q", "q.example.net").is_ok());
    }

    #[test]
    fn empty_clears() {
        assert_eq!(validate_queue_endpoint("", "").unwrap(), "");
        assert_eq!(validate_queue_endpoint("   ", "").unwrap(), "");
    }

    #[test]
    fn rejects_garbage() {
        assert!(validate_queue_endpoint("not-a-url", "").is_err());
        assert!(validate_queue_endpoint("https://", "").is_err());
    }

    /// S-02: the userinfo bypass that started this fix. Manual host
    /// parsing read `meetcorvux.com` as the host (the part before the
    /// first `:`); the real `url::Url` parser correctly sees
    /// `meetcorvux.com:443` as userinfo and `evil.example` as the host.
    #[test]
    fn rejects_userinfo_port_bypass() {
        let err =
            validate_queue_endpoint("https://meetcorvux.com:443@evil.example/q", "").unwrap_err();
        // Must reject — either via userinfo strip or via allowlist
        // (the real host is `evil.example`). Both are correct outcomes.
        assert!(
            err.contains("user:password") || err.contains("allowlist"),
            "expected userinfo / allowlist rejection, got: {err}"
        );
    }

    #[test]
    fn rejects_userinfo_at_only() {
        assert!(validate_queue_endpoint("https://meetcorvux.com@evil.example/q", "").is_err());
    }

    #[test]
    fn rejects_password_userinfo() {
        assert!(validate_queue_endpoint("https://user:pass@meetcorvux.com/q", "").is_err());
    }

    /// Lookalike subdomain shape — the trailing-dot check would let a
    /// host that merely *contains* the allowed name slip through if
    /// the comparison were string-contains rather than the strict
    /// `ends_with(".{allowed}")` we use. Lock it down.
    #[test]
    fn rejects_lookalike_subdomain() {
        assert!(
            validate_queue_endpoint("https://meetcorvux.com.evil.example/q", "").is_err(),
            "lookalike subdomain must not satisfy the allowlist"
        );
    }

    /// Hosts must compare case-insensitively — both `url::Url`'s
    /// internal normalization and our explicit `to_ascii_lowercase`
    /// guarantee this.
    #[test]
    fn accepts_uppercase_host() {
        assert!(validate_queue_endpoint("https://MeetCorvux.com/q", "").is_ok());
    }

    /// Pinned host with an explicit port — endpoint URL must use that
    /// same port. The previous validator stripped the port off both
    /// sides and compared just the hostname, which silently approved
    /// `q.example.net:8080` against a pin of `q.example.net:8443`.
    #[test]
    fn accepts_pinned_host_with_port() {
        assert!(
            validate_queue_endpoint("https://q.example.net:8443/q", "q.example.net:8443").is_ok()
        );
    }

    #[test]
    fn rejects_pinned_host_wrong_port() {
        let err = validate_queue_endpoint("https://q.example.net:8080/q", "q.example.net:8443")
            .unwrap_err();
        assert!(
            err.contains("allowlist"),
            "expected allowlist error, got: {err}"
        );
    }
}

/// R1 (sharp-edges residual): pairing copies the whole roost to a peer the
/// webview named, so the first pair needs a native confirm; a confirmed
/// peer never prompts again.
#[cfg(test)]
mod first_pair_gate_tests {
    use super::super::native_confirm::test_support::Scripted;
    use super::*;
    use std::collections::BTreeSet;
    use tempfile::TempDir;

    #[test]
    fn confirmed_peers_round_trip_and_fail_closed() {
        let dir = TempDir::new().unwrap();
        assert!(load_confirmed_peers(dir.path()).is_empty());
        let mut peers = BTreeSet::new();
        peers.insert(peer_confirm_key("iroh", &"a".repeat(64)));
        peers.insert(peer_confirm_key("tcp", "192.168.1.9:4242"));
        save_confirmed_peers(dir.path(), &peers).unwrap();
        assert_eq!(load_confirmed_peers(dir.path()), peers);
        std::fs::write(confirmed_peers_file(dir.path()), b"not json").unwrap();
        assert!(load_confirmed_peers(dir.path()).is_empty());
    }

    #[tokio::test]
    async fn a_confirmed_peer_pairs_without_a_dialog() {
        let dir = TempDir::new().unwrap();
        let key = peer_confirm_key("tcp", "10.0.0.5:4242");
        let mut peers = BTreeSet::new();
        peers.insert(key.clone());
        save_confirmed_peers(dir.path(), &peers).unwrap();

        let dialog = Scripted::deny(); // would refuse if asked — it must not be
        authorize_first_pair(dir.path(), &key, "10.0.0.5:4242", &dialog)
            .await
            .unwrap();
        assert_eq!(dialog.times_asked(), 0);
    }

    #[tokio::test]
    async fn a_new_peer_needs_the_dialog_and_a_decline_refuses() {
        let dir = TempDir::new().unwrap();
        let key = peer_confirm_key("iroh", &"b".repeat(64));
        let dialog = Scripted::deny();
        let err = authorize_first_pair(dir.path(), &key, "bbbb…64hex", &dialog)
            .await
            .unwrap_err();
        assert_eq!(err, SYNC_PAIR_DECLINED);
        assert_eq!(dialog.times_asked(), 1);
        assert!(
            load_confirmed_peers(dir.path()).is_empty(),
            "a declined peer must not be recorded as confirmed"
        );
    }

    #[tokio::test]
    async fn an_allowed_new_peer_is_persisted_and_never_prompts_again() {
        let dir = TempDir::new().unwrap();
        let key = peer_confirm_key("iroh", &"c".repeat(64));
        let dialog = Scripted::allow();
        authorize_first_pair(dir.path(), &key, "cccc…64hex", &dialog)
            .await
            .unwrap();
        assert_eq!(dialog.times_asked(), 1);

        let re_dialog = Scripted::deny();
        authorize_first_pair(dir.path(), &key, "cccc…64hex", &re_dialog)
            .await
            .unwrap();
        assert_eq!(re_dialog.times_asked(), 0, "confirmed peers pair silently");
    }

    #[test]
    fn short_id_truncates_on_char_boundaries() {
        assert_eq!(short_id(&"ab".repeat(40), 16).chars().count(), 15);
        assert_eq!(short_id("short", 16), "short");
    }
}
