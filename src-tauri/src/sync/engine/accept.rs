//! Inbound listener: TCP accept loop + per-connection handshake spawn.
//! `start_listening` (in `engine.rs`) installs the listener; the long-lived
//! task is implemented here. Each accepted socket runs `handle_incoming` in
//! its own task so a slow handshake can't starve other clients.

use std::sync::Arc;

use iroh::Endpoint;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{RwLock, Semaphore};
use tokio_rustls::TlsAcceptor;
use tracing::{info, warn};

/// SYN-M4 / SYN-I6: max inbound handshakes in flight at once, per accept loop.
/// A pre-auth connection flood (before the PIN/NodeId gate) would otherwise
/// spawn one handshake task per socket without bound — exhausting memory / FDs
/// on a phone. At capacity the accept loop parks (new connections wait in the
/// OS/iroh backlog) until a permit frees. 32 is far above any legitimate
/// simultaneous-pairing need; a real peer only ever opens one at a time.
const MAX_CONCURRENT_INBOUND_HANDSHAKES: usize = 32;

use crate::sync::crypto::fingerprint::Fingerprint;
use crate::sync::crypto::pairing::Pin;
use crate::sync::error::{SyncError, SyncResult, TransportError};
use crate::sync::transport::iroh::IrohTransport;
use crate::sync::transport::tcp_tls::TcpTlsTransport;

use super::engine::SyncEngine;
use super::peer::{self, Role};
use super::state::{PeerConnectionState, SyncEvent};
use crate::sync::protocol::handshake::ServerHandshake;

pub(super) async fn accept_loop(
    engine: SyncEngine,
    listener: TcpListener,
    acceptor: TlsAcceptor,
    our_fp: Fingerprint,
    incoming_pin: Pin,
) {
    let handshake_slots = Arc::new(Semaphore::new(MAX_CONCURRENT_INBOUND_HANDSHAKES));
    loop {
        // SYN-M4: acquire a handshake slot BEFORE accepting/spawning. At
        // capacity this parks the loop (backpressure — new sockets wait in the
        // OS backlog) so a flood can't spawn unbounded handshake tasks. The
        // permit is moved into the task and released when the handshake ends.
        let permit = match handshake_slots.clone().acquire_owned().await {
            Ok(p) => p,
            Err(_) => break, // semaphore closed — shouldn't happen; bail cleanly
        };
        let (tcp, addr) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, "accept failed");
                continue;
            }
        };
        info!(%addr, "sync: incoming connection");
        let engine = engine.clone();
        let acceptor = acceptor.clone();
        let pin = incoming_pin.clone();
        // SYN-I4/B7: per-connection handshake tracked + cancel-aware. A stalled
        // inbound handshake (bounded by HANDSHAKE_TIMEOUT) must not outlive
        // `stop()` — cancel drops it immediately, and the tracker drains it.
        engine.clone().spawn_tracked(move |token| async move {
            let _permit = permit; // held for the handshake's lifetime (SYN-M4)
            tokio::select! {
                _ = token.cancelled() => {}
                r = handle_incoming(engine, acceptor, tcp, our_fp, pin) => match r {
                    Ok(()) => info!(%addr, "sync: incoming session established"),
                    Err(e) => warn!(error = %e, %addr, "sync: incoming session failed"),
                }
            }
        });
    }
}

/// Iroh accept loop. Mirrors `accept_loop` for the iroh transport.
///
/// Iroh's `Endpoint::accept()` returns an `Incoming` future per peer that
/// initiates a connection. `Incoming.await` resolves to a `Connection`
/// once the QUIC handshake (and therefore NodeId authentication) is
/// complete. We then open the bi-stream we use as our message channel
/// and hand the resulting `IrohTransport` to `peer::spawn` exactly the
/// same way `accept_loop` hands the `TcpTlsTransport` over — the engine
/// is transport-agnostic past this point.
pub(super) async fn iroh_accept_loop(
    engine: SyncEngine,
    endpoint: Endpoint,
    incoming_pin: Arc<RwLock<Pin>>,
) {
    let handshake_slots = Arc::new(Semaphore::new(MAX_CONCURRENT_INBOUND_HANDSHAKES));
    while let Some(incoming) = endpoint.accept().await {
        // SYN-M4: bound concurrent handshakes (twin of the TCP loop). At
        // capacity this parks before pulling the next `incoming`, so a QUIC
        // connection flood can't spawn unbounded handshake tasks.
        let permit = match handshake_slots.clone().acquire_owned().await {
            Ok(p) => p,
            Err(_) => break,
        };
        let pin = incoming_pin.read().await.clone();
        let engine = engine.clone();
        // SYN-I4/B7: per-connection handshake tracked + cancel-aware (twin of
        // the TCP accept loop above).
        engine.clone().spawn_tracked(move |token| async move {
            let _permit = permit; // held for the handshake's lifetime (SYN-M4)
            tokio::select! {
                _ = token.cancelled() => {}
                r = handle_iroh_incoming(engine, incoming, pin) => match r {
                    Ok(()) => info!("iroh: incoming session established"),
                    Err(e) => warn!(error = %e, "iroh: incoming session failed"),
                }
            }
        });
    }
    // The accept loop only returns when the endpoint is closed (e.g. on
    // engine shutdown). Logging at info is fine here — the absence of
    // this line in the journal would confirm the loop is still running.
    info!("iroh: accept loop ended (endpoint closed)");
}

async fn handle_iroh_incoming(
    engine: SyncEngine,
    incoming: iroh::endpoint::Incoming,
    pin: Pin,
) -> SyncResult<()> {
    // The QUIC handshake completes here. After this `await` returns, the
    // peer has proven possession of their Ed25519 private key — the
    // remote NodeId on the resulting connection is authenticated.
    let connection = incoming
        .await
        .map_err(|e| SyncError::Transport(TransportError::Iroh(format!("incoming: {e}"))))?;
    // Pull the peer's NodeId out of the connection before we move it into
    // the transport. iroh has already verified this — it's the
    // store-and-forward queue's authoritative recipient address.
    // iroh 1.x: `remote_id()` returns `EndpointId` directly (no Result),
    // post-handshake. Wrap in `Some` to preserve the Optional that
    // downstream code already handles for the legacy TCP-TLS path.
    // The NodeId QUIC just authenticated is the peer's cryptographic
    // identity; captured here (before `connection` moves into the transport)
    // for the store-and-forward queue address and the AUDIT-081 weld below.
    let peer_node_id = connection.remote_id();
    let peer_node_id_bytes: Option<[u8; 32]> = Some(*peer_node_id.as_bytes());
    let transport = IrohTransport::accept(connection).await?;
    let device_id = engine.identity().device_id();
    let display = engine.device_name().to_string();
    let roost_name = engine.roost_name().to_string();
    let vault_id = engine.vault_id();
    // SEC-2: pre-load the resumption tokens we hold so a reconnect from a known
    // peer authenticates by proving its token instead of re-proving the PIN.
    // M-3 / AUDIT-015: also pre-load the roost id we recorded for each known
    // device so the handshake can reject a roost-id swap BEFORE deriving a
    // session (the post-handshake compare below is a backstop for what this
    // misses). Built under the same store lock as the resumption tokens.
    let mut resumption_tokens: std::collections::HashMap<
        crate::sync::identity::DeviceId,
        [u8; 32],
    > = std::collections::HashMap::new();
    let mut expected_peer_vault_ids: std::collections::HashMap<
        crate::sync::identity::DeviceId,
        [u8; 16],
    > = std::collections::HashMap::new();
    // B4: derive the peer device from the QUIC-authenticated NodeId and, if we
    // already hold it in the peer store, mark it exempt from the enrollment
    // window. This lets a device paired BEFORE resumption tokens shipped
    // (`resumption_token == None`, so it's seen as first-pair) reconnect and
    // acquire a token without the user re-opening pairing. Keyed on the
    // authenticated NodeId (not the spoofable Hello device_id): a stranger with
    // a fresh NodeId is never in the store, so is never exempt — the enrollment
    // gate against first-pair spraying is unchanged. Computed under the same
    // store lock as the token/roost-id preload.
    let mut enrollment_exempt_device: Option<crate::sync::identity::DeviceId> = None;
    {
        let store = engine.peer_store_lock().await;
        for r in store.list() {
            if let Some(t) = r.resumption_token {
                resumption_tokens.insert(r.device(), t);
            }
            if let Some(v) = r.peer_vault_id {
                expected_peer_vault_ids.insert(r.device(), v);
            }
        }
        if let Ok(authed) = crate::sync::transport::iroh::device_id_from_node_id(peer_node_id) {
            if store.get(authed).is_some() {
                enrollment_exempt_device = Some(authed);
            }
        }
    }
    // AUDIT-077: inject the engine-shared pairing rate limiter so the
    // brute-force counter spans every accepted iroh connection.
    // M-11: rate-limit per source. The authenticated NodeId is the stablest
    // per-peer key iroh gives us pre-session (a relayed connection's IP is the
    // relay's, which would lump all relayed peers together); keying on it means
    // one abusive/flappy peer can't drive the global counter to the lockout cap
    // and deny pairing to everyone else.
    let rate_limit_key = format!("iroh:{}", hex::encode(peer_node_id.as_bytes()));
    let mut handshake = ServerHandshake::new(device_id, display, roost_name, vault_id, pin)
        .with_shared_rate_limiter(engine.pairing_rate_limiter())
        .with_enrollment_window(engine.enrollment_window())
        .with_resumption_tokens(resumption_tokens.clone())
        .with_expected_peer_vault_ids(expected_peer_vault_ids)
        .with_enrollment_exempt_device(enrollment_exempt_device)
        .with_rate_limit_key(rate_limit_key);
    // SRX layer 3: bind this session's one-time secret (embedded in the ticket
    // we handed out) into the first-pair proof, so a peer that only has the
    // public NodeId — never the ticket — can't complete a first pair. A
    // reconnect ignores it (it authenticates by resumption token).
    if let Some(secret) = engine.pairing_secret_handle().read().await.as_ref() {
        handshake = handshake.with_pairing_secret(secret.clone());
    }
    let (handle, task) = peer::spawn(
        transport,
        Role::Server { handshake },
        engine.roost_path_buf(),
        engine.roost_fs_clone(),
        engine.doc_store_clone(),
        engine.note_locks_clone(),
        engine.pending_trash_clone(),
        engine.events_sender(),
        engine.tracker_clone(),
        engine.child_cancel_token(),
    )
    .await?;
    let peer_device = handle.device_id;
    let peer_name = handle.display_name.clone();
    let queue_pair_key = handle.queue_pair_key;
    let peer_vault_id = handle.peer_vault_id;
    // Symmetric pairing: the dialer may have offered its own redial
    // credentials in the handshake so WE (the host) can dial it back after a
    // restart, instead of reconnect being one-directional.
    let peer_reconnect = handle.peer_reconnect.clone();
    // SEC-2: on a FIRST pair (we held no token for this device) persist the
    // freshly-derived resumption token; a reconnect already had one and must
    // not overwrite it.
    let is_first_pair = !resumption_tokens.contains_key(&peer_device);
    let resumption_token = handle.resumption_token;
    // AUDIT-081: weld the peer's self-asserted handshake device_id to the
    // iroh NodeId QUIC authenticated (rationale + shared check in
    // transport::iroh::verify_node_device_binding). Reject a mismatch before
    // persisting the peer's NodeId / queue key under a spoofed id.
    if let Err(e) =
        crate::sync::transport::iroh::verify_node_device_binding(peer_device, peer_node_id)
    {
        handle.shutdown().await;
        tracing::warn!(
            error = %e,
            claimed = %hex::encode(peer_device.0),
            "AUDIT-081: rejecting iroh peer — handshake device_id does not match NodeId-derived identity"
        );
        return Err(e.into());
    }
    // AUDIT-015: if we've paired with this device before under a different
    // roost id, refuse to register the connection. Same device, different
    // roost means the user pointed Corvux at a new folder (or someone
    // swapped the bytes) — merging their CRDT history into ours would
    // weld unrelated trees together.
    {
        let store = engine.peer_store_lock().await;
        if let Some(existing) = store.get(peer_device) {
            if let Some(expected) = existing.peer_vault_id {
                if expected != peer_vault_id {
                    drop(store);
                    handle.shutdown().await;
                    tracing::warn!(
                        device = %hex::encode(peer_device.0),
                        expected = %hex::encode(expected),
                        got = %hex::encode(peer_vault_id),
                        "AUDIT-015: rejecting reconnect — peer's roost id changed since first pair"
                    );
                    return Err(crate::sync::error::ProtocolError::Aborted(
                        "roost id mismatch: this device was previously paired under a different roost".into(),
                    )
                    .into());
                }
            }
        }
    }
    engine.register_peer(handle, task).await;
    // Iroh authenticates the peer at the QUIC layer via NodeId — there's
    // no equivalent of the TLS-fingerprint exchange to record here. The
    // peer record's `fingerprint` field stays zeroed until the v3 schema
    // migration adds an explicit `node_id_pubkey` column.
    {
        let mut store = engine.peer_store_lock().await;
        // Credential-preserving (NOT `upsert(PeerRecord::new(..))`): an
        // inbound reconnect that carries no fresh offer must not blank the
        // stored redial ticket/PIN — that wipe left this side unable to dial
        // the peer back after a restart ("re-pair needed"). It also erased
        // the SEC-2 resumption token on every non-first-pair reconnect.
        store.ensure_peer(peer_device, [0u8; 32], peer_name.clone())?;
        store.remember_queue_pair(peer_device, peer_node_id_bytes, Some(queue_pair_key))?;
        store.remember_peer_vault_id(peer_device, peer_vault_id)?;
        // Symmetric redial: persist the dialer's offered ticket + PIN so our
        // reconnect supervisor can dial it back (makes `can_auto_reconnect`
        // true for an inbound-paired peer — previously only the dialer could
        // reconnect, so a host restart left the link down until the dialer
        // happened to redial).
        if let Some((ticket, pin)) = peer_reconnect {
            if let Err(e) = store.remember_connection(peer_device, Some(ticket), Some(pin)) {
                tracing::warn!(error = %e, "accept: failed to persist peer redial credentials");
            }
        }
        if is_first_pair {
            if let Err(e) = store.remember_resumption_token(peer_device, Some(resumption_token)) {
                tracing::warn!(error = %e, "accept: failed to persist resumption token");
            }
        }
    }
    engine
        .set_peer_state(peer_device, PeerConnectionState::Connected)
        .await;
    engine.send_event(SyncEvent::PeerConnected {
        device_id: peer_device,
        display_name: peer_name,
    });
    Ok(())
}

async fn handle_incoming(
    engine: SyncEngine,
    acceptor: TlsAcceptor,
    tcp: TcpStream,
    our_fp: Fingerprint,
    pin: Pin,
) -> SyncResult<()> {
    let (transport, addr) = TcpTlsTransport::accept(&acceptor, tcp, our_fp).await?;
    let device_id = engine.identity().device_id();
    let display = engine.device_name().to_string();
    let roost_name = engine.roost_name().to_string();
    let vault_id = engine.vault_id();
    // AUDIT-077: shared rate limiter (same instance as the iroh accept path).
    // M-11: key the backoff by source IP so one host can't lock out pairing
    // for everyone. The TCP path exposes the real peer socket address.
    let rate_limit_key = tcp_rate_limit_key(addr.ip());
    let handshake = ServerHandshake::new(device_id, display, roost_name, vault_id, pin)
        .with_shared_rate_limiter(engine.pairing_rate_limiter())
        .with_enrollment_window(engine.enrollment_window())
        .with_rate_limit_key(rate_limit_key);
    let (handle, task) = peer::spawn(
        transport,
        Role::Server { handshake },
        engine.roost_path_buf(),
        engine.roost_fs_clone(),
        engine.doc_store_clone(),
        engine.note_locks_clone(),
        engine.pending_trash_clone(),
        engine.events_sender(),
        engine.tracker_clone(),
        engine.child_cancel_token(),
    )
    .await?;
    let peer_device = handle.device_id;
    let peer_name = handle.display_name.clone();
    let queue_pair_key = handle.queue_pair_key;
    let peer_vault_id = handle.peer_vault_id;
    {
        let store = engine.peer_store_lock().await;
        if let Some(existing) = store.get(peer_device) {
            if let Some(expected) = existing.peer_vault_id {
                if expected != peer_vault_id {
                    drop(store);
                    handle.shutdown().await;
                    tracing::warn!(
                        device = %hex::encode(peer_device.0),
                        expected = %hex::encode(expected),
                        got = %hex::encode(peer_vault_id),
                        "AUDIT-015: rejecting reconnect — peer's roost id changed since first pair"
                    );
                    return Err(crate::sync::error::ProtocolError::Aborted(
                        "roost id mismatch: this device was previously paired under a different roost".into(),
                    )
                    .into());
                }
            }
        }
    }
    engine.register_peer(handle, task).await;
    // Incoming connections don't know the peer's fingerprint from the TLS
    // layer (server-side TLS without mTLS), so we can't pin it here.
    // Phase 2: add mTLS or a signed fingerprint-exchange in the handshake.
    {
        let mut store = engine.peer_store_lock().await;
        // Credential-preserving — same rationale as the iroh accept block:
        // a reconnect must not blank stored redial credentials.
        store.ensure_peer(peer_device, [0u8; 32], peer_name.clone())?;
        // TLS-only pairings have no iroh NodeId — the queue can still
        // hold the pair key for symmetry, but a TLS-only peer has no
        // queue address to receive at. Persist the key anyway; the
        // engine's push code skips peers with `peer_node_id == None`.
        store.remember_queue_pair(peer_device, None, Some(queue_pair_key))?;
        store.remember_peer_vault_id(peer_device, peer_vault_id)?;
    }
    engine
        .set_peer_state(peer_device, PeerConnectionState::Connected)
        .await;
    engine.send_event(SyncEvent::PeerConnected {
        device_id: peer_device,
        display_name: peer_name,
    });
    Ok(())
}

/// M-11: rate-limit key for a TCP peer's source IP. IPv4 keys on the full
/// address, but an IPv6 host controls its entire /64 (2^64 addresses) — keying
/// on the full v6 address lets it rotate freely to sidestep the per-source
/// pairing limit, so collapse v6 to its /64 network prefix. A single abusive
/// network is then one key, not 2^64.
fn tcp_rate_limit_key(ip: std::net::IpAddr) -> String {
    match ip {
        std::net::IpAddr::V4(v4) => format!("tcp4:{v4}"),
        std::net::IpAddr::V6(v6) => {
            let s = v6.segments();
            // First four 16-bit segments = the /64 network prefix.
            format!("tcp6:{:x}:{:x}:{:x}:{:x}::/64", s[0], s[1], s[2], s[3])
        }
    }
}

#[cfg(test)]
mod rate_limit_key_tests {
    use super::tcp_rate_limit_key;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    #[test]
    fn ipv6_addresses_in_same_64_share_a_key() {
        // Two different hosts in the same /64 must map to ONE key so an
        // attacker can't rotate the low 64 bits to escape the limit.
        let a = tcp_rate_limit_key(IpAddr::V6(Ipv6Addr::new(
            0x2001, 0xdb8, 0xabcd, 0x1, 0, 0, 0, 0x1,
        )));
        let b = tcp_rate_limit_key(IpAddr::V6(Ipv6Addr::new(
            0x2001, 0xdb8, 0xabcd, 0x1, 0xffff, 0xffff, 0xffff, 0xffff,
        )));
        assert_eq!(a, b, "same /64 → same rate-limit key");

        // A different /64 is a distinct key.
        let c = tcp_rate_limit_key(IpAddr::V6(Ipv6Addr::new(
            0x2001, 0xdb8, 0xabcd, 0x2, 0, 0, 0, 0x1,
        )));
        assert_ne!(a, c, "different /64 → different key");
    }

    #[test]
    fn ipv4_addresses_are_keyed_individually() {
        let a = tcp_rate_limit_key(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7)));
        let b = tcp_rate_limit_key(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 8)));
        assert_ne!(a, b, "distinct IPv4 hosts are distinct keys");
    }
}
