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
/// Two things make room when it fills: `sync_unpair` removes the unpaired
/// peer's entry ([`forget_confirmed_peer`]), and a full record drops the
/// entries that no longer vouch for anything ([`prune_unpaired`]).
const MAX_CONFIRMED_PEERS: usize = 512;

/// Serializes the confirmed-peers read-modify-write. A native dialog must
/// NEVER be awaited while this is held (it would stall every pairing and
/// unpair for up to the dialog timeout) — `sync_invariants::sy7_*` pins that:
/// once the lane is taken, a function awaits nothing else.
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
///
/// An entry lives exactly as long as the trust it records (SY-4): `sync_unpair`
/// removes the peer's entry ([`forget_confirmed_peer`]), and a full record drops
/// the entries of peers that are no longer paired ([`prune_unpaired`]). Entries
/// written by builds before that fix for peers unpaired earlier are not found by
/// either until the peer is paired and unpaired again, or the record fills up.
pub(super) fn confirmed_peers_file(dir: &std::path::Path) -> std::path::PathBuf {
    dir.join("confirmed_sync_peers.json")
}

/// Read the record, telling "nothing there / garbage" (both: empty — the gate
/// trusts nothing from them) apart from "could not be read" (`Err`). Only
/// [`forget_confirmed_peer`] needs the difference: it must not claim a peer was
/// forgotten from a record it could not read.
fn try_load_confirmed_peers(
    dir: &std::path::Path,
) -> Result<std::collections::BTreeSet<String>, String> {
    let text = match std::fs::read_to_string(confirmed_peers_file(dir)) {
        Ok(text) => text,
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::InvalidData
            ) =>
        {
            return Ok(Default::default())
        }
        Err(e) => return Err(format!("read confirmed peers: {e}")),
    };
    Ok(serde_json::from_str::<Vec<String>>(&text)
        .map(|list| list.into_iter().collect())
        .unwrap_or_default())
}

fn load_confirmed_peers(dir: &std::path::Path) -> std::collections::BTreeSet<String> {
    try_load_confirmed_peers(dir).unwrap_or_default()
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

/// Peers confirmed in THIS run whose record could not be written (disk full,
/// read-only app-data, record full). Without it a failed save made every
/// later connect to the same peer ask again. Memory only: after a restart the
/// peer is unconfirmed again unless the paired-peer registry knows it (fail
/// closed, never trusted from a record that was never written).
static SESSION_CONFIRMED_PEERS: std::sync::Mutex<std::collections::BTreeSet<String>> =
    std::sync::Mutex::new(std::collections::BTreeSet::new());

fn session_confirmed_peers() -> std::sync::MutexGuard<'static, std::collections::BTreeSet<String>> {
    // Plain data; a panic elsewhere cannot leave it half-updated.
    SESSION_CONFIRMED_PEERS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The iroh NodeId a confirm key names, in raw bytes, for searching the
/// paired-peer registry. `None` for any other key (TCP keys and malformed keys
/// are treated as unknown — see [`already_paired`]).
fn iroh_node_of_key(key: &str) -> Option<[u8; 32]> {
    let node = key
        .strip_prefix("iroh:")?
        .parse::<iroh::EndpointId>()
        .ok()?;
    Some(*node.as_bytes())
}

/// Does this registry entry describe the iroh node `node`?
fn identity_is_node(id: &crate::sync::store::peer_store::PairedIdentity, node: &[u8; 32]) -> bool {
    id.node_id.as_ref() == Some(node)
        || id
            .addr
            .as_deref()
            .filter(|a| a.starts_with("corvux1:"))
            .and_then(|a| crate::sync::transport::iroh::decode_ticket(a).ok())
            .is_some_and(|peer| peer.id.as_bytes() == node)
}

/// RF-3 (R1 migration): did the user already pair with this iroh peer?
/// Read-only lookup in the backend-owned paired-peer registries
/// (`<app_data>/vaults/<index dir>/sync/peers.bin`, written only after a
/// completed PIN handshake). Pairings made before the R1 dialog existed are
/// therefore not asked about again on a manual connect, and a device the user
/// UNPAIRED (removed from `peers.bin`) is asked about again.
///
/// Trust boundary (SA-1): the webview can write files inside every roost it
/// can open — including the marker file `paths::is_marked_index_dir` looks
/// for, and a fake `sync/peers.bin`. A registry therefore counts ONLY when
/// its directory
///   * is a direct child of `vaults/` named exactly like a directory
///     `roost_db_dir` creates (`id-` + 32 hex, or the legacy 16-hex hash) —
///     a user folder cannot be given such a name through any webview-reachable
///     command (named roosts are created under `roosts/`, with a sanitized
///     slug, and never above an existing folder); AND
///   * carries the marker; AND
///   * is not itself a roost root the user confirmed (an index dir is never a
///     roost; a folder that is both is user content).
///
/// TCP peers are deliberately NOT looked up (SA-3): a TCP "peer" is just
/// `ip:port` text, which proves nothing about who answers on that address
/// today (DHCP reuse). TCP pairing is the legacy path; a pre-upgrade TCP peer
/// is asked about once.
///
/// Scope matches the confirmed record: app-wide, not per roost. So a device
/// that is still paired in ANOTHER roost counts as paired here even after it was
/// unpaired in this one (SY-4 residual: per-roost scoping would add a prompt for
/// every user who pairs one device into a second roost).
/// Any read or decode problem counts as "not paired" (the dialog asks).
fn already_paired(app_data_dir: &std::path::Path, key: &str) -> bool {
    let Some(node) = iroh_node_of_key(key) else {
        return false;
    };
    let Ok(entries) = std::fs::read_dir(app_data_dir.join("vaults")) else {
        return false;
    };
    let roost_roots = super::roost::confirmed_roots_now(app_data_dir);
    entries.flatten().any(|entry| {
        let dir = entry.path();
        let name_ok = dir
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(crate::paths::is_backend_index_dir_name);
        if !name_ok || !crate::paths::is_marked_index_dir(&dir) {
            return false;
        }
        if roost_roots.contains(&crate::paths::normalize_key(&dir.to_string_lossy())) {
            return false;
        }
        match crate::sync::store::peer_store::read_paired_identities(&dir.join("sync")) {
            Ok(ids) => ids.iter().any(|id| identity_is_node(id, &node)),
            Err(e) => {
                tracing::debug!(error = %e, "sync pair: a paired-peer registry could not be read");
                false
            }
        }
    })
}

/// How [`authorize_first_pair`] authorized a pairing.
///
/// The distinction matters (SA-2): only `UserConfirmed` means a human just
/// answered a native dialog for THIS action, which is the only thing that may
/// open the inbound enrollment window. `AlreadyKnown` pairs without a dialog,
/// so a hijacked webview can trigger it at will for any NodeId it can read
/// from `sync_status` — it must not open a window or reset a guess counter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PairAuth {
    /// Confirmed earlier (record / this run) or already paired — no dialog.
    AlreadyKnown,
    /// The user answered "yes" in the native dialog just now.
    UserConfirmed,
}

/// The R1 gate: `Ok(_)` = this peer may be paired — [`PairAuth::AlreadyKnown`]
/// (confirmed earlier, remembered this run, or an existing pairing in the
/// paired-peer registry) or [`PairAuth::UserConfirmed`] (the user just
/// answered yes in the native dialog). Decline / dismiss / failure to show the
/// dialog ⇒ `Err(SYNC_PAIR_DECLINED)`.
pub(super) async fn authorize_first_pair(
    app_data_dir: &std::path::Path,
    key: &str,
    shown_id: &str,
    confirm: &(impl super::native_confirm::NativeConfirm + ?Sized),
) -> Result<PairAuth, String> {
    if load_confirmed_peers(app_data_dir).contains(key)
        || session_confirmed_peers().contains(key)
        || already_paired(app_data_dir, key)
    {
        return Ok(PairAuth::AlreadyKnown);
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
    record_user_confirmation(app_data_dir, key, MAX_CONFIRMED_PEERS).await;
    Ok(PairAuth::UserConfirmed)
}

/// The user just answered yes for `key`: remember it, durably if possible.
/// The pair proceeds whatever happens here — the user said yes — but an answer
/// that cannot be written is remembered for THIS run only (across a restart an
/// unwritable record is not trusted: fail closed).
///
/// Holds the record lane for its whole body and awaits nothing after taking it.
async fn record_user_confirmation(app_data_dir: &std::path::Path, key: &str, cap: usize) {
    let _lane = CONFIRMED_PEERS_WRITE.lock().await;
    let mut peers = load_confirmed_peers(app_data_dir);
    if peers.contains(key) {
        // Recorded concurrently — but the user answered yes to THIS request.
        return;
    }
    if peers.len() >= cap {
        // SY-4: make room by dropping entries that no longer vouch for anything
        // (a peer that was unpaired, or never finished pairing). Peers that are
        // still paired keep their entry, so nobody who is paired is asked again.
        let dropped = prune_unpaired(app_data_dir, &mut peers);
        tracing::info!(
            dropped,
            limit = cap,
            "sync pair: confirmed-peer record was full — dropped stale entries"
        );
    }
    if peers.len() >= cap {
        // Every entry is a peer that is still paired: nothing to drop. The pair
        // proceeds; this run remembers it; a restart asks again unless the
        // pairing completed (then the paired-peer registry knows the peer).
        tracing::warn!(
            limit = cap,
            "sync pair: confirmed-peer record is full — remembered for this run only"
        );
        session_confirmed_peers().insert(key.to_string());
        return;
    }
    peers.insert(key.to_string());
    if let Err(e) = save_confirmed_peers(app_data_dir, &peers) {
        tracing::warn!(error = %e, "sync pair: persisting confirmed peer failed — remembered for this run only");
        session_confirmed_peers().insert(key.to_string());
    }
}

/// Keep only the entries that still vouch for something: an iroh peer that is
/// currently paired in a trusted registry ([`already_paired`]). Everything else
/// — an unpaired or never-paired iroh peer, and every `tcp:` entry (TCP never
/// counts as paired, SA-3) — only means "asked once", and is asked again.
/// Returns how many were dropped.
fn prune_unpaired(
    app_data_dir: &std::path::Path,
    peers: &mut std::collections::BTreeSet<String>,
) -> usize {
    let before = peers.len();
    peers.retain(|key| iroh_node_of_key(key).is_some() && already_paired(app_data_dir, key));
    before - peers.len()
}

/// SY-4: forget the first-pair confirmation of a peer the user just unpaired —
/// the persisted record AND this run's session memory — so that connecting to it
/// again asks in the native dialog. Without it a hijacked webview could re-pair
/// an unpaired identity (a lost phone) silently.
///
/// Fail closed: a record that cannot be read or rewritten is an `Err`, and the
/// caller (`sync_unpair`) then refuses the unpair instead of claiming it while
/// the old trust remains. An empty `keys` is a no-op.
///
/// Holds the record lane and awaits nothing after taking it.
pub(super) async fn forget_confirmed_peer(
    app_data_dir: &std::path::Path,
    keys: &[String],
) -> Result<(), String> {
    if keys.is_empty() {
        return Ok(());
    }
    let _lane = CONFIRMED_PEERS_WRITE.lock().await;
    session_confirmed_peers().retain(|k| !keys.contains(k));
    let mut peers = try_load_confirmed_peers(app_data_dir)?;
    let before = peers.len();
    peers.retain(|k| !keys.contains(k));
    if peers.len() != before {
        save_confirmed_peers(app_data_dir, &peers)?;
    }
    Ok(())
}

/// Every confirm key (see [`peer_confirm_key`]) under which the user could have
/// confirmed this peer: its iroh NodeId (the recorded id, and the one inside a
/// stored `corvux1:` ticket) and, for a legacy pairing, its `tcp:` address.
/// Built from the peer's own record, so it matches what the pairing commands
/// keyed on at pair time.
pub(super) fn confirm_keys_for_peer(
    record: &crate::sync::store::peer_store::PeerRecord,
) -> Vec<String> {
    let mut keys: Vec<String> = Vec::new();
    let mut push = |key: String| {
        if !keys.contains(&key) {
            keys.push(key);
        }
    };
    if let Some(node) = record
        .peer_node_id
        .and_then(|bytes| iroh::EndpointId::from_bytes(&bytes).ok())
    {
        push(peer_confirm_key("iroh", &node.to_string()));
    }
    match record.last_known_addr.as_deref() {
        Some(addr) if addr.starts_with("corvux1:") => {
            if let Ok(peer) = crate::sync::transport::iroh::decode_ticket(addr) {
                push(peer_confirm_key("iroh", &peer.id.to_string()));
            }
        }
        Some(addr) => {
            if let Ok(sock) = addr.parse::<std::net::SocketAddr>() {
                push(peer_confirm_key("tcp", &sock.to_string()));
            }
        }
        None => {}
    }
    keys
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

    // ── RF-3: peers paired before the R1 dialog existed ─────────────────

    /// Keychain stand-in: the registry under test holds no secrets.
    struct NoSecrets;
    impl crate::sync::store::peer_store::SecretBackend for NoSecrets {
        fn get(&self, _key: &str) -> Result<Option<crate::sensitive::SecretString>, String> {
            Ok(None)
        }
        fn set(&self, _key: &str, _value: &str) -> Result<(), String> {
            Ok(())
        }
        fn delete(&self, _key: &str) -> Result<(), String> {
            Ok(())
        }
    }

    /// Write a paired-peer registry the way the engine does, under
    /// `<app_data>/vaults/<index_dir>/sync`, optionally with the marker
    /// `roost_db_dir` stamps on every real index dir.
    fn pair_in_registry(
        app_data: &std::path::Path,
        index_dir: &str,
        marked: bool,
        node_id: Option<[u8; 32]>,
        addr: Option<&str>,
    ) {
        use crate::sync::store::peer_store::{PeerRecord, PeerStore};
        let dir = app_data.join("vaults").join(index_dir);
        std::fs::create_dir_all(dir.join("sync")).unwrap();
        if marked {
            std::fs::write(dir.join(".corvux-derived-index"), b"x").unwrap();
        }
        let mut store =
            PeerStore::load_with_secrets(&dir.join("sync"), std::sync::Arc::new(NoSecrets))
                .unwrap();
        let mut record = PeerRecord::new(
            crate::sync::identity::DeviceId([node_id.map_or(1, |n| n[0]); 16]),
            [0u8; 32],
            "phone".into(),
        );
        record.peer_node_id = node_id;
        record.last_known_addr = addr.map(str::to_string);
        store.upsert(record).unwrap();
    }

    fn node(seed: u8) -> iroh::EndpointId {
        iroh::SecretKey::from_bytes(&[seed; 32]).public()
    }

    #[tokio::test]
    async fn a_peer_paired_before_the_upgrade_is_not_asked_about() {
        let app_data = TempDir::new().unwrap();
        let known = node(21);
        pair_in_registry(
            app_data.path(),
            "id-00000000000000000000000000000021",
            true,
            Some(*known.as_bytes()),
            Some("10.0.0.21:4242"),
        );
        let dialog = Scripted::deny(); // would refuse if asked — it must not be

        let iroh_key = peer_confirm_key("iroh", &known.to_string());
        let auth = authorize_first_pair(app_data.path(), &iroh_key, "iroh node", &dialog)
            .await
            .unwrap();
        assert_eq!(auth, PairAuth::AlreadyKnown);
        assert_eq!(dialog.times_asked(), 0);
    }

    /// SA-3: a TCP "peer" is `ip:port` text. Seeing that text in a registry
    /// proves nothing about who answers there today (DHCP reuse), so it must
    /// not silence the dialog.
    #[tokio::test]
    async fn a_tcp_address_in_the_registry_does_not_vouch_for_a_tcp_peer() {
        let app_data = TempDir::new().unwrap();
        pair_in_registry(
            app_data.path(),
            "id-00000000000000000000000000000021",
            true,
            Some(*node(21).as_bytes()),
            Some("10.0.0.21:4242"),
        );
        let dialog = Scripted::deny();
        let tcp_key = peer_confirm_key("tcp", "10.0.0.21:4242");
        assert_eq!(
            authorize_first_pair(app_data.path(), &tcp_key, "10.0.0.21:4242", &dialog)
                .await
                .unwrap_err(),
            SYNC_PAIR_DECLINED
        );
        assert_eq!(dialog.times_asked(), 1);
    }

    /// SA-1: the marker is a plain file the webview can plant in any folder
    /// it can write. A marked folder whose NAME is not one `roost_db_dir`
    /// would create must vouch for nothing, even with a perfectly formed
    /// registry for the attacker's NodeId inside it.
    #[tokio::test]
    async fn a_marked_folder_with_a_non_index_name_vouches_for_nothing() {
        let app_data = TempDir::new().unwrap();
        let attacker = node(31);
        for (i, name) in [
            "my-notes",
            "id-1234",
            "id-0000000000000000000000000000003Z",
            "legacy-roost",
        ]
        .into_iter()
        .enumerate()
        {
            pair_in_registry(
                app_data.path(),
                name,
                true, // the planted marker
                Some(*attacker.as_bytes()),
                None,
            );
            let dialog = Scripted::deny();
            let key = peer_confirm_key("iroh", &attacker.to_string());
            assert_eq!(
                authorize_first_pair(app_data.path(), &key, "iroh node", &dialog)
                    .await
                    .unwrap_err(),
                SYNC_PAIR_DECLINED,
                "folder #{i} ({name}) must not vouch for a peer"
            );
            assert_eq!(dialog.times_asked(), 1, "folder #{i} ({name})");
        }
    }

    /// SA-1, second line: even a correctly named, marked folder does not vouch
    /// when it is itself a roost the user opened — an index dir is never a
    /// roost, so a folder that is both is user content.
    #[tokio::test]
    async fn an_index_shaped_folder_that_is_a_confirmed_roost_vouches_for_nothing() {
        let app_data = TempDir::new().unwrap();
        let attacker = node(32);
        let name = "id-00000000000000000000000000000032";
        pair_in_registry(
            app_data.path(),
            name,
            true,
            Some(*attacker.as_bytes()),
            None,
        );
        let dir = app_data.path().join("vaults").join(name);
        let roots = vec![crate::paths::normalize_key(&dir.to_string_lossy())];
        std::fs::write(
            app_data.path().join("confirmed_roost_roots.json"),
            serde_json::to_string(&roots).unwrap(),
        )
        .unwrap();
        let dialog = Scripted::deny();
        let key = peer_confirm_key("iroh", &attacker.to_string());
        assert_eq!(
            authorize_first_pair(app_data.path(), &key, "iroh node", &dialog)
                .await
                .unwrap_err(),
            SYNC_PAIR_DECLINED
        );
        assert_eq!(dialog.times_asked(), 1);
    }

    /// SA-2: callers must be able to tell "a human just said yes" from
    /// "known already" — only the former may open the enrollment window.
    #[tokio::test]
    async fn the_gate_reports_whether_the_user_just_confirmed() {
        let app_data = TempDir::new().unwrap();
        let fresh = peer_confirm_key("iroh", &node(33).to_string());

        let allow = Scripted::allow();
        assert_eq!(
            authorize_first_pair(app_data.path(), &fresh, "iroh node", &allow)
                .await
                .unwrap(),
            PairAuth::UserConfirmed
        );
        assert_eq!(allow.times_asked(), 1);

        // Same peer again: remembered, so no dialog and NOT a fresh confirm.
        let deny = Scripted::deny();
        assert_eq!(
            authorize_first_pair(app_data.path(), &fresh, "iroh node", &deny)
                .await
                .unwrap(),
            PairAuth::AlreadyKnown
        );
        assert_eq!(deny.times_asked(), 0);

        // A peer known only from the paired-peer registry: also not a confirm.
        pair_in_registry(
            app_data.path(),
            "id-00000000000000000000000000000034",
            true,
            Some(*node(34).as_bytes()),
            None,
        );
        let reg_key = peer_confirm_key("iroh", &node(34).to_string());
        assert_eq!(
            authorize_first_pair(app_data.path(), &reg_key, "iroh node", &deny)
                .await
                .unwrap(),
            PairAuth::AlreadyKnown
        );
        assert_eq!(deny.times_asked(), 0);
    }

    #[tokio::test]
    async fn a_peer_known_only_by_its_reconnect_ticket_is_recognised() {
        let app_data = TempDir::new().unwrap();
        let known = node(22);
        let ticket =
            crate::sync::transport::iroh::encode_ticket(&iroh::EndpointAddr::new(known), None)
                .unwrap();
        pair_in_registry(
            app_data.path(),
            "id-00000000000000000000000000000022",
            true,
            None,
            Some(&*ticket),
        );
        let dialog = Scripted::deny();
        let key = peer_confirm_key("iroh", &known.to_string());
        authorize_first_pair(app_data.path(), &key, "iroh node", &dialog)
            .await
            .unwrap();
        assert_eq!(dialog.times_asked(), 0);
    }

    #[tokio::test]
    async fn an_unpaired_peer_is_still_asked_about() {
        let app_data = TempDir::new().unwrap();
        pair_in_registry(
            app_data.path(),
            "id-00000000000000000000000000000023",
            true,
            Some(*node(23).as_bytes()),
            Some("10.0.0.23:4242"),
        );
        let dialog = Scripted::deny();
        let stranger = peer_confirm_key("iroh", &node(24).to_string());
        assert_eq!(
            authorize_first_pair(app_data.path(), &stranger, "iroh node", &dialog)
                .await
                .unwrap_err(),
            SYNC_PAIR_DECLINED
        );
        let other_addr = peer_confirm_key("tcp", "10.0.0.24:4242");
        assert_eq!(
            authorize_first_pair(app_data.path(), &other_addr, "10.0.0.24:4242", &dialog)
                .await
                .unwrap_err(),
            SYNC_PAIR_DECLINED
        );
        assert_eq!(dialog.times_asked(), 2);
    }

    #[tokio::test]
    async fn a_registry_outside_a_marked_index_dir_is_ignored() {
        // A user folder under the legacy `vaults/` namespace is not an index
        // dir and must never vouch for a peer.
        let app_data = TempDir::new().unwrap();
        let planted = node(25);
        pair_in_registry(
            app_data.path(),
            "my-notes",
            false,
            Some(*planted.as_bytes()),
            None,
        );
        let dialog = Scripted::deny();
        let key = peer_confirm_key("iroh", &planted.to_string());
        assert!(
            authorize_first_pair(app_data.path(), &key, "iroh node", &dialog)
                .await
                .is_err()
        );
        assert_eq!(dialog.times_asked(), 1);
    }

    #[tokio::test]
    async fn a_failed_save_does_not_reprompt_in_this_run() {
        let parent = TempDir::new().unwrap();
        let unwritable = parent.path().join("missing"); // every save fails
        let key = peer_confirm_key("tcp", "10.0.0.26:4242");

        let first = Scripted::allow();
        authorize_first_pair(&unwritable, &key, "10.0.0.26:4242", &first)
            .await
            .unwrap();
        assert_eq!(first.times_asked(), 1);

        let again = Scripted::deny();
        authorize_first_pair(&unwritable, &key, "10.0.0.26:4242", &again)
            .await
            .unwrap();
        assert_eq!(again.times_asked(), 0, "remembered for this run");
        assert!(
            !load_confirmed_peers(&unwritable).contains(&key),
            "nothing persisted, so a restart asks again"
        );
    }

    // ── SY-4: unpair forgets the confirmation ───────────────────────────

    /// Confirm `key` through the dialog (the record then trusts it), with no
    /// paired-peer registry anywhere — the "confirmed, then unpaired" state.
    async fn confirm_once(dir: &std::path::Path, key: &str) {
        let allow = Scripted::allow();
        assert_eq!(
            authorize_first_pair(dir, key, "peer", &allow)
                .await
                .unwrap(),
            PairAuth::UserConfirmed
        );
        assert_eq!(allow.times_asked(), 1);
    }

    async fn is_asked_again(dir: &std::path::Path, key: &str) -> bool {
        let deny = Scripted::deny();
        let refused = authorize_first_pair(dir, key, "peer", &deny)
            .await
            .map(|_| false)
            .unwrap_or_else(|e| e == SYNC_PAIR_DECLINED);
        refused && deny.times_asked() == 1
    }

    /// The finding's scenario, end to end: confirm → pair → unpair a lost phone
    /// → a hijacked webview names the same NodeId again. Before the fix the
    /// record still trusted it and the pair went through with no dialog.
    #[tokio::test]
    async fn unpairing_a_peer_makes_the_dialog_come_back() {
        let dir = TempDir::new().unwrap();
        let key = peer_confirm_key("iroh", &node(51).to_string());
        confirm_once(dir.path(), &key).await;

        // Trusted until unpaired.
        let silent = Scripted::deny();
        authorize_first_pair(dir.path(), &key, "peer", &silent)
            .await
            .unwrap();
        assert_eq!(silent.times_asked(), 0);

        forget_confirmed_peer(dir.path(), std::slice::from_ref(&key))
            .await
            .unwrap();
        assert!(!load_confirmed_peers(dir.path()).contains(&key));
        assert!(
            is_asked_again(dir.path(), &key).await,
            "an unpaired peer must be asked about again"
        );
    }

    /// A confirmation that could not be written lives in this run's memory; the
    /// unpair must clear that too or the peer stays trusted until restart.
    #[tokio::test]
    async fn unpairing_also_clears_the_session_only_confirmation() {
        let parent = TempDir::new().unwrap();
        let unwritable = parent.path().join("missing"); // every save fails
        let key = peer_confirm_key("iroh", &node(52).to_string());
        confirm_once(&unwritable, &key).await;
        assert!(session_confirmed_peers().contains(&key));

        forget_confirmed_peer(&unwritable, std::slice::from_ref(&key))
            .await
            .unwrap();
        assert!(!session_confirmed_peers().contains(&key));
        assert!(is_asked_again(&unwritable, &key).await);
    }

    #[tokio::test]
    async fn forgetting_one_peer_leaves_the_others_confirmed() {
        let dir = TempDir::new().unwrap();
        let gone = peer_confirm_key("iroh", &node(53).to_string());
        let kept = peer_confirm_key("iroh", &node(54).to_string());
        confirm_once(dir.path(), &gone).await;
        confirm_once(dir.path(), &kept).await;

        forget_confirmed_peer(dir.path(), std::slice::from_ref(&gone))
            .await
            .unwrap();
        assert!(is_asked_again(dir.path(), &gone).await);
        let silent = Scripted::deny();
        authorize_first_pair(dir.path(), &kept, "peer", &silent)
            .await
            .unwrap();
        assert_eq!(silent.times_asked(), 0, "an unrelated peer stays confirmed");
    }

    #[tokio::test]
    async fn forgetting_nothing_or_an_unknown_peer_changes_nothing() {
        let dir = TempDir::new().unwrap();
        forget_confirmed_peer(dir.path(), &[]).await.unwrap();
        assert!(
            !confirmed_peers_file(dir.path()).exists(),
            "no keys: not even a record is created"
        );
        let key = peer_confirm_key("iroh", &node(55).to_string());
        confirm_once(dir.path(), &key).await;
        let before = std::fs::read_to_string(confirmed_peers_file(dir.path())).unwrap();
        forget_confirmed_peer(dir.path(), &["iroh:never-seen".to_string()])
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(confirmed_peers_file(dir.path())).unwrap(),
            before
        );
    }

    /// Fail closed: if the record cannot be rewritten the unpair is refused
    /// (an Err), and the old confirmation is NOT silently left behind as if it
    /// had been removed.
    #[tokio::test]
    async fn forgetting_fails_closed_when_the_record_cannot_be_rewritten() {
        let dir = TempDir::new().unwrap();
        let key = peer_confirm_key("iroh", &node(56).to_string());
        confirm_once(dir.path(), &key).await;
        // A directory squatting on the temp name makes the rewrite fail.
        std::fs::create_dir(dir.path().join("confirmed_sync_peers.json.tmp")).unwrap();
        let err = forget_confirmed_peer(dir.path(), std::slice::from_ref(&key))
            .await
            .unwrap_err();
        assert!(err.contains("write confirmed peers"), "{err}");
        assert!(
            load_confirmed_peers(dir.path()).contains(&key),
            "the record is unchanged, so the caller must treat the unpair as failed"
        );
    }

    #[tokio::test]
    async fn forgetting_fails_closed_when_the_record_cannot_be_read() {
        let dir = TempDir::new().unwrap();
        // A directory where the record should be: unreadable, not absent.
        std::fs::create_dir(confirmed_peers_file(dir.path())).unwrap();
        let key = peer_confirm_key("iroh", &node(57).to_string());
        assert!(forget_confirmed_peer(dir.path(), &[key]).await.is_err());
    }

    fn record_for(
        node_id: Option<[u8; 32]>,
        addr: Option<&str>,
    ) -> crate::sync::store::peer_store::PeerRecord {
        let mut r = crate::sync::store::peer_store::PeerRecord::new(
            crate::sync::identity::DeviceId([7u8; 16]),
            [0u8; 32],
            "phone".into(),
        );
        r.peer_node_id = node_id;
        r.last_known_addr = addr.map(str::to_string);
        r
    }

    /// The keys must be exactly what the pairing commands keyed on, or the
    /// unpair removes nothing: `iroh:` + the NodeId's Display, `tcp:` + the
    /// socket address.
    #[test]
    fn the_keys_for_a_peer_match_the_ones_the_pair_commands_use() {
        let by_id = node(61);
        let by_ticket = node(62);
        let ticket =
            crate::sync::transport::iroh::encode_ticket(&iroh::EndpointAddr::new(by_ticket), None)
                .unwrap();
        let keys = confirm_keys_for_peer(&record_for(Some(*by_id.as_bytes()), Some(&*ticket)));
        assert_eq!(
            keys,
            vec![
                peer_confirm_key("iroh", &by_id.to_string()),
                peer_confirm_key("iroh", &by_ticket.to_string()),
            ]
        );

        // The same node recorded twice is one key.
        let same = confirm_keys_for_peer(&record_for(Some(*by_ticket.as_bytes()), Some(&*ticket)));
        assert_eq!(same, vec![peer_confirm_key("iroh", &by_ticket.to_string())]);

        let tcp = confirm_keys_for_peer(&record_for(None, Some("10.0.0.5:4242")));
        assert_eq!(tcp, vec![peer_confirm_key("tcp", "10.0.0.5:4242")]);

        assert!(confirm_keys_for_peer(&record_for(None, None)).is_empty());
        assert!(confirm_keys_for_peer(&record_for(None, Some("not an address"))).is_empty());
        assert!(confirm_keys_for_peer(&record_for(None, Some("corvux1:garbage"))).is_empty());
    }

    // ── SY-4: the record's cap has a real removal path ──────────────────

    #[tokio::test]
    async fn a_full_record_drops_stale_entries_and_keeps_paired_peers() {
        let app_data = TempDir::new().unwrap();
        let paired = node(71);
        pair_in_registry(
            app_data.path(),
            "id-00000000000000000000000000000071",
            true,
            Some(*paired.as_bytes()),
            None,
        );
        let paired_key = peer_confirm_key("iroh", &paired.to_string());
        let stale_iroh = peer_confirm_key("iroh", &node(72).to_string());
        let stale_tcp = peer_confirm_key("tcp", "10.0.0.72:4242");
        let mut full = BTreeSet::new();
        full.insert(paired_key.clone());
        full.insert(stale_iroh.clone());
        full.insert(stale_tcp.clone());
        full.insert(peer_confirm_key("iroh", &node(73).to_string()));
        save_confirmed_peers(app_data.path(), &full).unwrap();

        let fresh = peer_confirm_key("iroh", &node(74).to_string());
        record_user_confirmation(app_data.path(), &fresh, 4).await;

        let after = load_confirmed_peers(app_data.path());
        assert!(after.contains(&fresh), "the new confirmation is recorded");
        assert!(after.contains(&paired_key), "a still-paired peer is kept");
        assert!(!after.contains(&stale_iroh));
        assert!(!after.contains(&stale_tcp));
        assert_eq!(after.len(), 2);
    }

    /// When every entry is a peer that is still paired there is nothing to
    /// drop: the pair still proceeds (the user said yes), this run remembers it,
    /// and nobody who is paired loses their entry.
    #[tokio::test]
    async fn a_record_full_of_paired_peers_falls_back_to_this_run_only() {
        let app_data = TempDir::new().unwrap();
        let (a, b) = (node(75), node(76));
        pair_in_registry(
            app_data.path(),
            "id-00000000000000000000000000000075",
            true,
            Some(*a.as_bytes()),
            None,
        );
        pair_in_registry(
            app_data.path(),
            "id-00000000000000000000000000000076",
            true,
            Some(*b.as_bytes()),
            None,
        );
        let mut full = BTreeSet::new();
        full.insert(peer_confirm_key("iroh", &a.to_string()));
        full.insert(peer_confirm_key("iroh", &b.to_string()));
        save_confirmed_peers(app_data.path(), &full).unwrap();

        let fresh = peer_confirm_key("iroh", &node(77).to_string());
        record_user_confirmation(app_data.path(), &fresh, 2).await;

        assert_eq!(
            load_confirmed_peers(app_data.path()),
            full,
            "nothing evicted"
        );
        assert!(session_confirmed_peers().contains(&fresh));
    }

    #[test]
    fn pruning_keeps_only_peers_that_are_still_paired() {
        let app_data = TempDir::new().unwrap();
        let paired = node(78);
        pair_in_registry(
            app_data.path(),
            "id-00000000000000000000000000000078",
            true,
            Some(*paired.as_bytes()),
            None,
        );
        let mut peers = BTreeSet::new();
        peers.insert(peer_confirm_key("iroh", &paired.to_string()));
        peers.insert(peer_confirm_key("iroh", &node(79).to_string()));
        peers.insert(peer_confirm_key("tcp", "10.0.0.79:4242"));
        peers.insert("garbage".to_string());
        assert_eq!(prune_unpaired(app_data.path(), &mut peers), 3);
        assert_eq!(peers.len(), 1);
    }

    #[test]
    fn short_id_truncates_on_char_boundaries() {
        assert_eq!(short_id(&"ab".repeat(40), 16).chars().count(), 15);
        assert_eq!(short_id("short", 16), "short");
    }
}
