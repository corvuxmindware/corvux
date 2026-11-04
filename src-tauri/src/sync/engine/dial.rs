//! Dial paths + listener binding on `SyncEngine`.
//!
//! Owns the outbound side of the connection lifecycle (TLS + iroh) plus
//! both listener bind methods and the autoresume helper
//! (`reconnect_all_known_peers`). Split off `engine.rs` to keep the
//! orchestrator focused on shared lifecycle (new/stop/status/watcher).
//!
//! The dial methods touch `self.register_peer` and `self.set_peer_state`
//! which live in `engine.rs`. Inherent impls span files, so the calls
//! resolve normally at the crate boundary.

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use iroh::{EndpointAddr as NodeAddr, EndpointId as NodeId, RelayMode};
use tokio::net::TcpListener;
use tokio::sync::RwLock;
use tokio_rustls::TlsAcceptor;
use tracing::{debug, info, warn};

use crate::sync::crypto::fingerprint::Fingerprint;
use crate::sync::crypto::pairing::Pin;
use crate::sync::error::{EngineError, SyncError, SyncResult, TransportError};
use crate::sync::identity::DeviceId;
use crate::sync::protocol::handshake::ClientHandshake;
use crate::sync::store::peer_store::PeerRecord;
use crate::sync::transport::iroh::{build_endpoint, encode_ticket, IrohTransport};
use crate::sync::transport::tcp_tls::{build_server_config, TcpTlsTransport};

use super::accept::{accept_loop, iroh_accept_loop};
use super::engine::{IrohListenState, ListenState, SyncEngine};
use super::peer::{self, Role};
use super::state::{PeerConnectionState, SyncEvent};
use super::util::now_millis;

/// How often the engine-owned reconnect supervisor sweeps for due peers.
/// Per-peer exponential backoff (`reconnect_backoff_ms`) gates actual
/// dials; this is just the polling granularity.
const RECONNECT_SUPERVISOR_TICK: Duration = Duration::from_secs(15);

/// Wall-clock bound on a single outbound dial (QUIC connect + open stream).
/// Without it a dial to an unreachable peer — very common when the relay is
/// blocked and direct addrs are stale — hangs indefinitely, stranding the
/// peer in `Dialing` (which the supervisor skips) and, on the IPC dial path,
/// holding a lock the whole sync surface waits on. On expiry we fail so
/// backoff engages and the next tick retries.
const DIAL_TIMEOUT: Duration = Duration::from_secs(20);

impl SyncEngine {
    /// Begin accepting inbound connections on `port`. The server uses
    /// `incoming_pin` to verify the pairing proof from the first client
    /// that connects; re-pairing with multiple peers requires calling
    /// this again with a fresh PIN.
    ///
    /// In MVP, only one peer at a time is accepted. Phase 2 may run a
    /// multiplexed PIN pool indexed by display name.
    pub async fn start_listening(&self, port: u16, incoming_pin: Pin) -> SyncResult<u16> {
        // SYN-B6: serialize against stop(). Held across the whole bring-up so a
        // stop() racing us either runs fully first (we then see `stopped` and
        // bail) or blocks until we've installed, then cancels+drains our task.
        let _life = self.inner.lifecycle.lock().await;
        if self.is_stopped() {
            return Err(crate::sync::error::ProtocolError::Aborted(
                "start_listening on a stopped engine".into(),
            )
            .into());
        }
        let mut guard = self.inner.listen_state.lock().await;
        // Idempotent: if we're already listening on this exact port,
        // don't try to re-bind (Windows returns WSAEADDRINUSE = os
        // error 10048 and surfaces an alarming "only one usage per
        // address" message to the user). A request for the SAME port
        // we already bound is almost always the user clicking "Start
        // hosting" on a session that autostart already spun up —
        // return success with the existing port.
        if let Some(existing) = guard.as_ref() {
            if existing.port == port || port == 0 {
                info!(
                    port = existing.port,
                    "start_listening: already listening on this port, no-op"
                );
                return Ok(existing.port);
            }
            // Different port requested while one is already bound —
            // that's a real "you asked for something I can't do while
            // something else is running" situation. Caller should stop
            // first.
            return Err(EngineError::AlreadyRunning.into());
        }
        let addr: SocketAddr = format!("0.0.0.0:{port}").parse().map_err(|_| {
            SyncError::Engine(EngineError::UnsupportedOnMobile("invalid bind address"))
        })?;
        let listener = TcpListener::bind(addr).await.map_err(SyncError::Io)?;
        let bound_port = listener.local_addr().map_err(SyncError::Io)?.port();
        info!(port = bound_port, "sync listener bound");
        let server_cfg = build_server_config(&self.inner.tls_identity)?;
        let acceptor = TlsAcceptor::from(server_cfg);

        let engine = self.clone();
        let our_fp = self.inner.tls_identity.fingerprint;
        // SYN-I4: tracked + cancel-aware. The TCP accept loop blocks on
        // `listener.accept()` forever; cancel drops it (and the listener) so
        // `stop()` drains it. Stored handle kept for belt-and-suspenders abort.
        let task = self.spawn_tracked(move |token| async move {
            tokio::select! {
                _ = token.cancelled() => {}
                _ = accept_loop(engine, listener, acceptor, our_fp, incoming_pin) => {}
            }
        });
        *guard = Some(ListenState {
            port: bound_port,
            task,
        });
        let _ = self
            .inner
            .events
            .send(SyncEvent::ListenerBound { port: bound_port });
        Ok(bound_port)
    }

    /// Bind the iroh endpoint and start its accept loop. Returns this
    /// device's `NodeId` — the only thing the joining device needs to
    /// dial in (typically delivered via QR code).
    ///
    /// Iroh handles LAN discovery, NAT hole-punching, and relay fallback
    /// internally — there's no per-listener "port" to expose. The endpoint
    /// multiplexes everything onto one UDP socket whose ephemeral local
    /// port is irrelevant for cross-network reachability.
    ///
    /// Idempotent: a second hosting call while already listening updates the
    /// inbound pairing PIN and returns the cached `NodeId` without re-binding.
    pub async fn start_listening_iroh(&self, incoming_pin: Pin) -> SyncResult<NodeId> {
        // SYN-B6: serialize against stop(). Reachable from tracked dial tasks
        // (`ensure_iroh_endpoint`) — safe because stop() does NOT hold
        // `lifecycle` across its drain, so a call here during `tracker.wait()`
        // acquires the lock, sees `stopped`, and bails instead of deadlocking.
        let _life = self.inner.lifecycle.lock().await;
        if self.is_stopped() {
            return Err(crate::sync::error::ProtocolError::Aborted(
                "start_listening_iroh on a stopped engine".into(),
            )
            .into());
        }
        let mut guard = self.inner.iroh_listen_state.lock().await;
        if let Some(existing) = guard.as_ref() {
            // Only reuse the cached listener if its accept loop is STILL
            // RUNNING. If the loop exited (endpoint closed, panic) the cached
            // NodeId is a lie — we'd hand out a ticket nobody can dial. Detect
            // the dead task and fall through to rebind a fresh endpoint.
            if !existing.task.is_finished() {
                let node_id = existing.node_id;
                let pin = Arc::clone(&existing.incoming_pin);
                drop(guard);
                *pin.write().await = incoming_pin;
                info!(node_id = %node_id, "iroh listener PIN updated");
                return Ok(node_id);
            }
            warn!(
                node_id = %existing.node_id,
                "iroh accept loop had died — rebinding listener"
            );
            // Dead state is replaced by the `*guard = Some(..)` rebind below.
        }

        // Beta uses Number0's public relays via `RelayMode::Default`. When
        // we stand up our own EC2 relays in production this becomes
        // `RelayMode::Custom(...)` with the corvux-operated relay map; the
        // engine API doesn't change.
        // Strict-P2P mode binds with the relay disabled so no Corvux/Number0
        // relay ever carries our traffic (direct/LAN + hole-punch only).
        let relay_mode = if self
            .inner
            .relay_disabled
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            RelayMode::Disabled
        } else {
            RelayMode::Default
        };
        let endpoint = build_endpoint(
            &self.inner.identity,
            relay_mode,
            self.inner.lan_lookup.clone(),
        )
        .await?;
        let node_id = endpoint.id();
        info!(node_id = %node_id, "iroh listener bound");

        let incoming_pin = Arc::new(RwLock::new(incoming_pin));
        let engine = self.clone();
        let endpoint_for_task = endpoint.clone();
        let incoming_pin_for_task = Arc::clone(&incoming_pin);
        // SYN-I4: tracked + cancel-aware (twin of the TCP accept loop). The
        // iroh loop also ends when `stop()` closes the endpoint; cancel is the
        // uniform signal and the stored handle backs it up.
        let task = self.spawn_tracked(move |token| async move {
            tokio::select! {
                _ = token.cancelled() => {}
                _ = iroh_accept_loop(engine, endpoint_for_task, incoming_pin_for_task) => {}
            }
        });

        *guard = Some(IrohListenState {
            endpoint,
            node_id,
            incoming_pin,
            task,
        });
        let _ = self
            .inner
            .events
            .send(SyncEvent::IrohListenerBound { node_id });
        Ok(node_id)
    }

    /// Rebind the iroh listener so a runtime change to `relay_disabled` (the
    /// strict-P2P toggle) takes effect on the LIVE endpoint instead of waiting
    /// for an app restart. Tears down the current endpoint + accept loop and
    /// binds a fresh one, which re-reads `relay_disabled` in
    /// `start_listening_iroh`.
    ///
    /// The NodeId is derived from `self.inner.identity`, so it is PRESERVED
    /// across the rebind — existing pairing tickets stay valid. No-op on a
    /// dial-only device that never bound a listener (nothing live to change),
    /// and on a stopped engine. Live peers drop when the old endpoint closes;
    /// we force-reconnect them over the new endpoint so the toggle is seamless
    /// rather than waiting on the 15s reconnect-supervisor tick.
    pub async fn rebind_iroh_listener(&self) -> SyncResult<()> {
        if self.is_stopped() {
            return Ok(());
        }
        let incoming_pin = {
            let mut guard = self.inner.iroh_listen_state.lock().await;
            let Some(state) = guard.take() else {
                // No listener bound (dial-only): the next bind will read the
                // updated relay setting anyway, so there's nothing live to redo.
                return Ok(());
            };
            let pin = state.incoming_pin.read().await.clone();
            state.task.abort();
            // Close the old QUIC endpoint so peers see a graceful close (mirrors
            // `stop()`); doesn't wait on a slow relay.
            state.endpoint.close().await;
            pin
        };
        // Bind a fresh endpoint with the current `relay_disabled` setting,
        // preserving the incoming PIN. `iroh_listen_state` is now `None`, so
        // `start_listening_iroh` binds rather than reusing.
        self.start_listening_iroh(incoming_pin).await?;
        // The old endpoint's connections were just dropped — re-dial known peers
        // over the new endpoint now instead of waiting for the supervisor.
        self.force_reconnect_all().await;
        Ok(())
    }

    /// Ensure the shared iroh endpoint exists for outbound dialing without
    /// changing the hosting PIN if the user is already showing a code.
    async fn ensure_iroh_endpoint(&self, fallback_pin: Pin) -> SyncResult<NodeId> {
        {
            let guard = self.inner.iroh_listen_state.lock().await;
            if let Some(existing) = guard.as_ref() {
                return Ok(existing.node_id);
            }
        }
        self.start_listening_iroh(fallback_pin).await
    }

    /// Produce a pairing ticket for the currently-bound iroh endpoint.
    ///
    /// The ticket bundles `node_id` + the device's home relay URL +
    /// any direct addresses we've discovered for ourselves. Joining
    /// devices use it to dial without round-tripping through Iroh's
    /// discovery service first — gives us sub-second pair latency on
    /// LAN and avoids the "no addressing information for NodeId"
    /// failure mode that hits when discovery hasn't published yet.
    ///
    /// Errors if the iroh listener hasn't been started — caller should
    /// invoke `start_listening_iroh` first.
    pub async fn iroh_pairing_ticket(&self) -> SyncResult<String> {
        let endpoint = {
            let guard = self.inner.iroh_listen_state.lock().await;
            guard
                .as_ref()
                .map(|s| s.endpoint.clone())
                .ok_or(EngineError::NotConnected)?
        };
        // iroh 1.x: `addr()` is sync and returns the current EndpointAddr
        // immediately (no longer Result-returning, no await-for-relay).
        // If discovery hasn't landed a home-relay yet, the ticket will
        // be relay-less; joining devices fall back to direct addrs. The
        // previous "wait for home_relay" semantics are no longer
        // exposed on `Endpoint`. Acceptable trade-off — iroh now
        // exposes home-relay readiness as a separate awaitable if we
        // need it back later.
        let addr = endpoint.addr();
        // SRX layer 3: embed the current session's one-time pairing secret (if
        // the user is actively QR-pairing) so the joiner can fold it into the
        // first-pair proof. Read from the engine — the same value the accept path
        // uses — so ticket and handshake never disagree.
        let secret = self.inner.pairing_secret.read().await;
        encode_ticket(&addr, secret.as_ref()).map_err(SyncError::Transport)
    }

    /// Current iroh identity + direct UDP addresses, for handing to mDNS so
    /// a LAN peer's `Discovery::announce` TXT record carries everything
    /// `SyncEngine::feed_lan_endpoint` needs on the far end. `None` when the
    /// iroh listener has not bound yet (e.g. `sync_init` just ran and the
    /// user has not started hosting/dialing this session yet) -- callers
    /// should announce with empty iroh fields rather than block waiting for
    /// a bind that may never happen on a dial-only device.
    pub async fn iroh_endpoint_addr(&self) -> Option<(NodeId, Vec<SocketAddr>)> {
        let guard = self.inner.iroh_listen_state.lock().await;
        let state = guard.as_ref()?;
        let addr = state.endpoint.addr();
        Some((state.node_id, addr.ip_addrs().copied().collect()))
    }

    /// Build our own symmetric-redial offer — `(our dialable ticket, our
    /// current hosting PIN)` — for inclusion in a handshake so the peer can
    /// dial US back later. Returns `None` when we aren't currently hosting an
    /// iroh listener (a dial-only device), in which case reconnect stays
    /// one-directional, exactly as before. Reads the live listening PIN from
    /// `iroh_listen_state` and re-derives the ticket from the current endpoint,
    /// so the offer always reflects our *present* address — this is what lets a
    /// peer's stored ticket for us stay fresh across our address changes.
    pub(super) async fn build_own_reconnect_offer(&self) -> Option<(String, String)> {
        // Snapshot the current hosting PIN and confirm the accept loop is live.
        // Scope the guard so it's dropped before `iroh_pairing_ticket` (which
        // re-locks `iroh_listen_state`) — holding it across that call would
        // deadlock.
        let pin_str = {
            let guard = self.inner.iroh_listen_state.lock().await;
            let state = guard.as_ref()?;
            if state.task.is_finished() {
                return None;
            }
            let pin = state.incoming_pin.read().await;
            pin.as_str().to_string()
        };
        match self.iroh_pairing_ticket().await {
            Ok(ticket) => Some((ticket, pin_str)),
            Err(e) => {
                debug!(error = %e, "build_own_reconnect_offer: no ticket yet — redial stays one-directional");
                None
            }
        }
    }

    /// Initiate outbound connection to a peer. AUDIT-003b: `pinned` is
    /// **mandatory**. The TLS handshake verifies the server cert against
    /// this fingerprint at the rustls layer; a mismatch surfaces as
    /// `TransportError::TlsHandshake`. First-time pairing flows go
    /// through iroh (`sync_connect_iroh`) where the NodeId is the
    /// authentication primitive — there's no first-pair-via-tcp_tls
    /// path in the UI, and the type signature here makes it
    /// unrepresentable. Pin resolution (look up by addr in
    /// `peer_store`) happens at the IPC layer in
    /// `commands::sync::sync_connect`.
    ///
    /// On success, the peer's address + PIN are persisted to
    /// `peer_store` so the next app launch can redial without any UI
    /// interaction — that's what makes autoresume work on Android
    /// where mDNS multicast is blocked.
    pub async fn connect_peer(
        &self,
        addr: SocketAddr,
        pin: Pin,
        pinned: Fingerprint,
    ) -> SyncResult<DeviceId> {
        let pin_str = pin.as_str().to_string();

        // The dial is always against a known fingerprint now (AUDIT-003b),
        // so the corresponding device_id is in peer_store. Attach the
        // Dialing state up-front so the UI sees the spinner immediately.
        let expected_device = self.expected_device_for_fingerprint(pinned).await;
        if let Some(dev) = expected_device {
            self.set_peer_state(dev, PeerConnectionState::Dialing).await;
        }

        let transport = match TcpTlsTransport::connect(addr, pinned).await {
            Ok(t) => t,
            Err(e) => {
                if let Some(dev) = expected_device {
                    self.set_peer_state(
                        dev,
                        PeerConnectionState::Failed {
                            reason: format!("dial: {e}"),
                            at_millis: now_millis(),
                        },
                    )
                    .await;
                }
                return Err(SyncError::Transport(e));
            }
        };
        if let Some(dev) = expected_device {
            self.set_peer_state(dev, PeerConnectionState::Handshaking)
                .await;
        }
        let observed = transport.peer_fingerprint();
        let device_id = self.identity().device_id();
        let display = self.inner.device_name.clone();
        let roost_name = self.inner.roost_name.clone();
        let vault_id = self.inner.vault_id;
        let mut handshake = ClientHandshake::new(device_id, display, roost_name, vault_id, pin);
        // M-3: if this is a redial of a known device, pin its recorded vault_id
        // so a roost swap is rejected pre-session (backstopped post-handshake).
        if let Some(dev) = expected_device {
            if let Some(v) = self
                .inner
                .peer_store
                .lock()
                .await
                .get(dev)
                .and_then(|r| r.peer_vault_id)
            {
                handshake = handshake.with_expected_peer_vault_id(v);
            }
        }
        let (handle, task) = match peer::spawn(
            transport,
            Role::Client { handshake },
            self.inner.roost.clone(),
            self.inner.roost_fs.clone(),
            self.inner.doc_store.clone(),
            self.inner.note_locks.clone(),
            self.pending_trash_clone(),
            self.inner.events.clone(),
            self.tracker_clone(),
            self.child_cancel_token(),
        )
        .await
        {
            Ok(v) => v,
            Err(e) => {
                if let Some(dev) = expected_device {
                    self.set_peer_state(
                        dev,
                        PeerConnectionState::Failed {
                            reason: format!("handshake: {e}"),
                            at_millis: now_millis(),
                        },
                    )
                    .await;
                }
                return Err(e);
            }
        };
        let peer_device = handle.device_id;
        let peer_name = handle.display_name.clone();
        let queue_pair_key = handle.queue_pair_key;
        let peer_vault_id = handle.peer_vault_id;
        // AUDIT-015: client-side mirror of the accept-side check. Same peer
        // device, different roost id since first pair → drop.
        {
            let store = self.inner.peer_store.lock().await;
            if let Some(existing) = store.get(peer_device) {
                if let Some(expected) = existing.peer_vault_id {
                    if expected != peer_vault_id {
                        drop(store);
                        handle.shutdown().await;
                        warn!(
                            device = %hex::encode(peer_device.0),
                            "AUDIT-015: rejecting reconnect — peer's roost id changed since first pair"
                        );
                        if let Some(dev) = expected_device {
                            self.set_peer_state(
                                dev,
                                PeerConnectionState::Failed {
                                    reason: "roost id mismatch".into(),
                                    at_millis: now_millis(),
                                },
                            )
                            .await;
                        }
                        return Err(crate::sync::error::ProtocolError::Aborted(
                            "roost id mismatch".into(),
                        )
                        .into());
                    }
                }
            }
        }
        self.register_peer(handle, task).await;
        self.persist_new_peer(peer_device, peer_name.clone(), observed)
            .await?;
        // Remember how to reach this peer for next launch + the per-pair
        // queue key for offline pushes. Failures here are non-fatal —
        // the user can still manually reconnect later.
        {
            let mut store = self.inner.peer_store.lock().await;
            if let Err(e) =
                store.remember_connection(peer_device, Some(addr.to_string()), Some(pin_str))
            {
                warn!(error = %e, "connect_peer: failed to remember addr/pin for autoresume");
            }
            // TLS-only peers can't receive queue pushes (no NodeId
            // address), but persist the key so a subsequent iroh
            // re-pair completes the picture.
            if let Err(e) = store.remember_queue_pair(peer_device, None, Some(queue_pair_key)) {
                warn!(error = %e, "connect_peer: failed to persist queue pair key");
            }
            if let Err(e) = store.remember_peer_vault_id(peer_device, peer_vault_id) {
                warn!(error = %e, "connect_peer: failed to persist peer roost id");
            }
        }
        self.set_peer_state(peer_device, PeerConnectionState::Connected)
            .await;
        let _ = self.inner.events.send(SyncEvent::PeerConnected {
            device_id: peer_device,
            display_name: peer_name,
        });
        Ok(peer_device)
    }

    /// Initiate outbound connection to a peer over the iroh transport.
    ///
    /// Requires `start_listening_iroh` to have run — we share the same
    /// endpoint for inbound and outbound so we don't burn two UDP sockets
    /// per process. If the endpoint is missing, this returns
    /// `EngineError::NotConnected`; callers should auto-start it.
    ///
    /// `peer` carries the NodeId plus optional direct addresses / relay
    /// hints. When the joining device scans a QR code with only a NodeId,
    /// Iroh's discovery service resolves the rest.
    pub async fn connect_peer_via_iroh(
        &self,
        peer: NodeAddr,
        pin: Pin,
        reconnect_offer: Option<(String, String)>,
        resumption_token: Option<[u8; 32]>,
        pairing_secret: Option<crate::sync::crypto::pairing::PairingSecret>,
    ) -> SyncResult<DeviceId> {
        // Bail if the engine was stopped (roost switch / shutdown): a redial
        // that lazily re-binds the endpoint here would resurrect a torn-down
        // engine and sync into the wrong roost.
        if self.is_stopped() {
            return Err(EngineError::NotConnected.into());
        }
        // In-flight guard at the single dial chokepoint. The supervisor filters
        // Dialing/Handshaking peers, but the other callers (manual reconnect,
        // force_reconnect_all, mDNS auto-dial, the frontend's own triggers, and
        // now the focus/RunEvent::Resumed hooks) do not — so alt-tabbing or a
        // burst of triggers could stack concurrent dials to the SAME peer,
        // producing duplicate connections and connection flap. Dedup here so
        // every path is covered: skip if already connected or mid-dial.
        let expected_device = crate::sync::transport::iroh::device_id_from_node_id(peer.id)?;
        if self.inner.peers.read().await.contains_key(&expected_device) {
            debug!(device = %expected_device, "connect_peer_via_iroh: already connected — skipping redundant dial");
            return Ok(expected_device);
        }
        if matches!(
            self.inner.peer_states.read().await.get(&expected_device),
            Some(PeerConnectionState::Dialing | PeerConnectionState::Handshaking)
        ) {
            debug!(device = %expected_device, "connect_peer_via_iroh: dial already in flight — skipping");
            return Ok(expected_device);
        }
        // Ensure the iroh listener is up — joiner-only devices (paired
        // out, never hosted) don't get their listener restored by
        // `sync_try_autostart` because they have no `iroh_hosting_pin`
        // saved. Without this lazy bind, `reconnect_all_known_peers`
        // hits `EngineError::NotConnected` on the first redial after a
        // restart and silently fails — the user sees a "transport: io:
        // connection lost" toast and is forced to QR-pair again.
        //
        // Keep outbound dialing separate from explicit hosting: if the user is
        // already showing a pairing code, this must not replace that code's
        // inbound PIN with the peer's PIN.
        if let Err(e) = self.ensure_iroh_endpoint(pin.clone()).await {
            warn!(error = %e, "connect_peer_via_iroh: lazy listener bind failed — outbound dial will use existing endpoint or fail");
        }
        let endpoint = {
            let guard = self.inner.iroh_listen_state.lock().await;
            guard.as_ref().map(|s| s.endpoint.clone())
        };
        let endpoint = match endpoint {
            Some(e) => e,
            None => {
                // Endpoint never bound (network down at startup, bind failure).
                // Record Failed so backoff engages and the UI shows a reason —
                // otherwise the supervisor treats the peer as "never tried,
                // due now" and silently respins every tick with no surfaced
                // error.
                self.set_peer_state(
                    expected_device,
                    PeerConnectionState::Failed {
                        reason: "iroh endpoint unavailable".into(),
                        at_millis: now_millis(),
                    },
                )
                .await;
                return Err(EngineError::NotConnected.into());
            }
        };

        let peer_node_id = peer.id;
        // `expected_device` was derived above (for the in-flight guard). Flip
        // the dialing-state dot immediately, before the handshake, so the UI
        // reflects the attempt and any failure attaches to a real peer id.
        self.set_peer_state(expected_device, PeerConnectionState::Dialing)
            .await;

        let transport =
            match tokio::time::timeout(DIAL_TIMEOUT, IrohTransport::connect(&endpoint, peer)).await
            {
                Ok(Ok(t)) => t,
                Ok(Err(e)) => {
                    self.set_peer_state(
                        expected_device,
                        PeerConnectionState::Failed {
                            reason: format!("iroh dial: {e}"),
                            at_millis: now_millis(),
                        },
                    )
                    .await;
                    return Err(SyncError::Transport(e));
                }
                Err(_elapsed) => {
                    self.set_peer_state(
                        expected_device,
                        PeerConnectionState::Failed {
                            reason: "iroh dial timed out".into(),
                            at_millis: now_millis(),
                        },
                    )
                    .await;
                    return Err(SyncError::Transport(TransportError::Iroh(
                        "dial timed out".into(),
                    )));
                }
            };
        self.set_peer_state(expected_device, PeerConnectionState::Handshaking)
            .await;

        let device_id = self.identity().device_id();
        let display = self.inner.device_name.clone();
        let roost_name = self.inner.roost_name.clone();
        let vault_id = self.inner.vault_id;
        let mut handshake = ClientHandshake::new(device_id, display, roost_name, vault_id, pin);
        // Symmetric pairing: on the initial pair, offer OUR redial credentials
        // so the host can dial us back later. Redials pass `None` (the host
        // already stored them at pair time).
        if let Some((ticket, offer_pin)) = reconnect_offer {
            handshake = handshake.with_reconnect_offer(ticket, offer_pin);
        }
        // SEC-2: if we hold a resumption token for this peer, authenticate with
        // it (reconnect) rather than re-proving the human PIN.
        if let Some(tok) = resumption_token {
            handshake = handshake.with_resumption_token(tok);
        }
        // SRX layer 3: on a first pair, bind the one-time secret we extracted
        // from the scanned/pasted ticket into the proof, so the host (which
        // minted it) accepts us and a peer that never had the ticket can't. Only
        // present on the initial QR/ticket pair; reconnects pass `None`.
        if let Some(secret) = pairing_secret {
            handshake = handshake.with_pairing_secret(secret);
        }
        // M-3: pin the vault_id we recorded for this peer so the handshake
        // rejects a roost swap BEFORE deriving a session (the post-handshake
        // compare below stays as a backstop). Only present on a reconnect.
        if let Some(v) = self
            .inner
            .peer_store
            .lock()
            .await
            .get(expected_device)
            .and_then(|r| r.peer_vault_id)
        {
            handshake = handshake.with_expected_peer_vault_id(v);
        }
        let (handle, task) = match peer::spawn(
            transport,
            Role::Client { handshake },
            self.inner.roost.clone(),
            self.inner.roost_fs.clone(),
            self.inner.doc_store.clone(),
            self.inner.note_locks.clone(),
            self.pending_trash_clone(),
            self.inner.events.clone(),
            self.tracker_clone(),
            self.child_cancel_token(),
        )
        .await
        {
            Ok(v) => v,
            Err(e) => {
                self.set_peer_state(
                    expected_device,
                    PeerConnectionState::Failed {
                        reason: format!("iroh handshake: {e}"),
                        at_millis: now_millis(),
                    },
                )
                .await;
                return Err(e);
            }
        };
        let peer_device = handle.device_id;
        let peer_name = handle.display_name.clone();
        let queue_pair_key = handle.queue_pair_key;
        let peer_vault_id = handle.peer_vault_id;
        // SEC-2: on a FIRST pair (we authenticated via PIN — no token was
        // supplied) persist the derived resumption token for future reconnects.
        // A token-authenticated reconnect keeps the existing one.
        let store_resumption_token = resumption_token.is_none();
        let derived_resumption_token = handle.resumption_token;
        // AUDIT-081: weld the peer's self-asserted handshake device_id to
        // the iroh NodeId that QUIC authenticated (rationale + shared check
        // in transport::iroh::verify_node_device_binding). A mismatch means
        // the peer is lying about who it is; reject before we persist its
        // NodeId + per-pair queue key under the spoofed id.
        if let Err(e) =
            crate::sync::transport::iroh::verify_node_device_binding(peer_device, peer_node_id)
        {
            handle.shutdown().await;
            warn!(
                claimed = %hex::encode(peer_device.0),
                authenticated = %hex::encode(expected_device.0),
                "AUDIT-081: rejecting iroh peer — handshake device_id does not match NodeId-derived identity"
            );
            self.set_peer_state(
                expected_device,
                PeerConnectionState::Failed {
                    reason: "device id does not match node id".into(),
                    at_millis: now_millis(),
                },
            )
            .await;
            return Err(SyncError::Transport(e));
        }
        // AUDIT-015: iroh-side mirror of the accept-side check.
        {
            let store = self.inner.peer_store.lock().await;
            if let Some(existing) = store.get(peer_device) {
                if let Some(expected) = existing.peer_vault_id {
                    if expected != peer_vault_id {
                        drop(store);
                        handle.shutdown().await;
                        warn!(
                            device = %hex::encode(peer_device.0),
                            "AUDIT-015: rejecting iroh reconnect — peer's roost id changed since first pair"
                        );
                        self.set_peer_state(
                            expected_device,
                            PeerConnectionState::Failed {
                                reason: "roost id mismatch".into(),
                                at_millis: now_millis(),
                            },
                        )
                        .await;
                        return Err(crate::sync::error::ProtocolError::Aborted(
                            "roost id mismatch".into(),
                        )
                        .into());
                    }
                }
            }
        }
        self.register_peer(handle, task).await;
        // Persist with a zeroed fingerprint — iroh authenticates via NodeId
        // at the QUIC layer, so the legacy fingerprint slot is unused on
        // this path. Phase 2 schema migration replaces it with a
        // `node_id_pubkey` column for autoresume.
        self.persist_new_peer(peer_device, peer_name.clone(), Fingerprint([0u8; 32]))
            .await?;
        // Persist the iroh NodeId + per-pair queue key so the engine can
        // push to this peer through the store-and-forward queue when the
        // live transport is down.
        {
            let mut store = self.inner.peer_store.lock().await;
            if let Err(e) = store.remember_queue_pair(
                peer_device,
                Some(*peer_node_id.as_bytes()),
                Some(queue_pair_key),
            ) {
                warn!(error = %e, "connect_peer_via_iroh: failed to persist queue pair key");
            }
            if let Err(e) = store.remember_peer_vault_id(peer_device, peer_vault_id) {
                warn!(error = %e, "connect_peer_via_iroh: failed to persist peer roost id");
            }
            if store_resumption_token {
                if let Err(e) =
                    store.remember_resumption_token(peer_device, Some(derived_resumption_token))
                {
                    warn!(error = %e, "connect_peer_via_iroh: failed to persist resumption token");
                }
            }
        }
        self.set_peer_state(peer_device, PeerConnectionState::Connected)
            .await;
        let _ = self.inner.events.send(SyncEvent::PeerConnected {
            device_id: peer_device,
            display_name: peer_name,
        });
        Ok(peer_device)
    }

    /// Fire-and-forget: for every known peer that has a stored address
    /// and PIN, spawn a dial task that updates `PeerConnectionState`
    /// through the lifecycle. Skips peers already connected. Returns
    /// immediately — dial progress flows through events.
    ///
    /// This is what `sync_try_autostart` calls to resume automatically
    /// after app restart. Without it, the engine would initialize but
    /// sit idle until the user poked the frontend retry loop.
    pub async fn reconnect_all_known_peers(&self) {
        let peer_records = self.inner.peer_store.lock().await.list();
        let connected_ids: HashSet<DeviceId> =
            self.inner.peers.read().await.keys().copied().collect();
        for record in peer_records {
            if connected_ids.contains(&DeviceId(record.device_id)) {
                continue;
            }
            self.dial_peer_record(record).await;
        }
    }

    /// Foreground / resume reconnect: zero every peer's backoff so they are
    /// all due immediately, then dial. This is the entry point for the app
    /// lifecycle bridge (window focus / `RunEvent::Resumed`) and the manual
    /// "Reconnect" button.
    ///
    /// Why the backoff reset matters: while a mobile app is suspended, the
    /// QUIC link idles out and the supervisor's timer is frozen. By the time
    /// the user reopens the app, `set_peer_state` may have pushed backoff to
    /// its 5-minute cap, so a plain `reconnect_all_known_peers` would still
    /// leave the supervisor waiting minutes before its next real attempt.
    /// Resetting collapses that to "dial now" — the behaviour a user expects
    /// when they deliberately return to the app or tap reconnect.
    pub async fn force_reconnect_all(&self) {
        {
            let mut rc = self.inner.reconnect.write().await;
            for entry in rc.values_mut() {
                entry.attempts = 0;
                entry.next_retry_at_millis = 0;
            }
        }
        self.reconnect_all_known_peers().await;
        // Also drain the store-and-forward queue now. Foreground/resume is
        // exactly when a peer that was offline for a while comes back: live
        // links reconnect above, but queued deliveries otherwise wait up to the
        // 5-minute puller tick. No-op when no queue client is configured.
        if let Err(e) = self.try_queue_pull_all().await {
            debug!(error = %e, "force_reconnect_all: queue pull failed (non-fatal)");
        }
    }

    /// Start the engine-owned reconnect supervisor: a background task that
    /// every [`RECONNECT_SUPERVISOR_TICK`] redials known peers that are
    /// disconnected, not mid-dial, and past their per-peer backoff window.
    ///
    /// This is the half of reconnect that moved out of the frontend: the
    /// old JS `setInterval` loop died whenever the webview was suspended or
    /// killed (mobile background, OS memory pressure), so a backgrounded
    /// phone would stop reconnecting. The engine task keeps running, so
    /// convergence no longer depends on any UI being mounted.
    ///
    /// Idempotent — aborts any prior supervisor before spawning. Started
    /// from `sync_init_inner` (covers both fresh init and autostart) and
    /// torn down by `stop()`.
    pub async fn start_reconnect_supervisor(&self) {
        // SYN-B6: serialize against stop().
        let _life = self.inner.lifecycle.lock().await;
        if self.is_stopped() {
            return;
        }
        if let Some(task) = self.inner.reconnect_task.lock().await.take() {
            task.abort();
        }
        let engine = self.clone();
        // SYN-I4: tracked + cancel-aware. The tick loop breaks on cancel so
        // `stop()` drains it; the stored handle is belt-and-suspenders.
        let task = self.spawn_tracked(move |token| async move {
            let mut interval = tokio::time::interval(RECONNECT_SUPERVISOR_TICK);
            interval.tick().await; // drop the immediate first tick
            loop {
                tokio::select! {
                    _ = token.cancelled() => break,
                    _ = interval.tick() => {}
                }
                engine.reconnect_due_peers().await;
                // Redeliver any live changes dropped by a momentarily-full peer
                // control channel. Runs on this always-on timer (not the
                // queue-gated puller) so it works in strict-P2P / no-queue mode.
                engine.drain_live_pending().await;
                // SYN-B8: re-attempt any tombstone-trash that failed on disk
                // (read-only roost / disk-full), so a committed delete converges
                // to disk once the condition clears — the live-path analogue of
                // the queue's retain→re-pull. Also on the always-on timer.
                engine.retry_pending_trashes().await;
            }
        });
        *self.inner.reconnect_task.lock().await = Some(task);
    }

    /// One supervisor pass: dial every known peer that is (a) not connected,
    /// (b) not already mid-dial (`Dialing`/`Handshaking`), and (c) past its
    /// backoff `next_retry_at` (a peer with no backoff entry yet has never
    /// been tried, so it's due). Backoff + last-error are tracked centrally
    /// in `set_peer_state`.
    async fn reconnect_due_peers(&self) {
        let now = now_millis();
        let connected_ids: HashSet<DeviceId> =
            self.inner.peers.read().await.keys().copied().collect();
        let in_flight: HashSet<DeviceId> = {
            let states = self.inner.peer_states.read().await;
            states
                .iter()
                .filter(|(_, s)| {
                    matches!(
                        s,
                        PeerConnectionState::Dialing | PeerConnectionState::Handshaking
                    )
                })
                .map(|(d, _)| *d)
                .collect()
        };
        let next_retry: HashMap<DeviceId, (u64, u32)> = {
            let rc = self.inner.reconnect.read().await;
            rc.iter()
                .map(|(d, s)| (*d, (s.next_retry_at_millis, s.attempts)))
                .collect()
        };
        let peer_records = self.inner.peer_store.lock().await.list();
        for record in peer_records {
            let dev = DeviceId(record.device_id);
            if connected_ids.contains(&dev) || in_flight.contains(&dev) {
                continue;
            }
            // Skip records we can't auto-dial at all (inbound-only / no saved
            // address or PIN). Without this the supervisor would call
            // `dial_peer_record` for them every tick only for it to no-op —
            // pure spin. Malformed-but-dialable records (bad PIN, corrupt
            // ticket) DO go through and get marked Failed with backoff.
            if record.last_known_addr.is_none() || record.last_known_pin.is_none() {
                continue;
            }
            // Gate only when we have an entry; a peer never tried (no entry) is
            // due immediately. `should_skip_reconnect` applies the L-2
            // max-attempts cutoff and the backoff window: after
            // MAX_RECONNECT_ATTEMPTS consecutive failures the supervisor stops
            // dialing a dead/removed peer every minute for the install's
            // lifetime; `force_reconnect_all` (app foreground / manual
            // Reconnect) zeroes `attempts`, re-arming a peer that comes back.
            if let Some(&(next, attempts)) = next_retry.get(&dev) {
                if super::engine::should_skip_reconnect(attempts, next, now) {
                    continue;
                }
            }
            self.dial_peer_record(record).await;
        }
    }

    /// Spawn a dial task for one known-peer record, dispatching on the
    /// stored address format: iroh tickets carry a `corvux1:` prefix, legacy
    /// TLS pairings stored a bare `ip:port`. Shared by
    /// `reconnect_all_known_peers` (startup / manual button) and the
    /// supervisor so both transports are covered from one place. The dial
    /// runs the `PeerConnectionState` machine, which feeds backoff via
    /// `set_peer_state`.
    ///
    /// A record that *has* an address + PIN but is **unusable** (PIN won't
    /// parse, corrupt ticket, invalid address, no iroh ticket) is marked
    /// `Failed` with backoff via `set_peer_state` — otherwise it has no
    /// reconnect entry and the supervisor would re-attempt (and log) it every
    /// tick forever. Records with no address/PIN at all are inbound-only and
    /// are filtered out by the supervisor (`can_auto_reconnect`), so they
    /// return quietly here.
    async fn dial_peer_record(&self, record: PeerRecord) {
        let dev = DeviceId(record.device_id);
        // SEC-2: reconnect with the stored resumption token if we have one, so
        // this redial proves the token rather than re-proving the human PIN.
        // Legacy peers (no token) fall back to PIN and get upgraded on success.
        let resumption_token = record.resumption_token;
        let (Some(addr_str), Some(pin_str)) = (
            record.last_known_addr.clone(),
            record.last_known_pin.clone(),
        ) else {
            // Was a SILENT `return` — the #1 reason a paired peer sits grey
            // after relaunch with zero diagnostic (observed on iOS: engine +
            // listener come up, then nothing — no dial, no log). Surface it:
            // `mark_dial_unusable` logs a warning AND sets a visible
            // Failed(reason) so the dot has a cause instead of a mute grey.
            // Recording WHICH credential is missing distinguishes a
            // never-persisted ticket from a wiped one for the follow-up fix.
            let reason = if record.last_known_addr.is_none() && record.last_known_pin.is_none() {
                "no stored redial address or PIN — re-pair needed"
            } else if record.last_known_addr.is_none() {
                "no stored redial address (iroh ticket) — re-pair needed"
            } else {
                "no stored redial PIN — re-pair needed"
            };
            self.mark_dial_unusable(dev, reason).await;
            return;
        };

        if addr_str.starts_with("corvux1:") {
            let Some(pin) = Pin::parse(&pin_str) else {
                self.mark_dial_unusable(dev, "invalid stored iroh PIN — re-pair needed")
                    .await;
                return;
            };
            let peer = match crate::sync::transport::iroh::decode_ticket(&addr_str) {
                Ok(p) => p,
                Err(e) => {
                    debug!(device = %dev, error = %e, "dial_peer_record: ticket decode");
                    self.mark_dial_unusable(dev, "stale or corrupt iroh ticket — re-pair needed")
                        .await;
                    return;
                }
            };
            let engine = self.clone();
            // SYN-I4/B7: tracked + cancel-aware. A redial in flight when the
            // engine stops (roost switch) must not complete a QUIC handshake
            // and register a peer into a torn-down engine — cancel drops it.
            self.spawn_tracked(move |token| async move {
                // Refresh OUR reconnect offer on EVERY redial, not just the
                // initial pair. Previously redials passed `None`, so the peer
                // captured our dialable ticket exactly once (at first pair) and
                // never again — if that capture was missed (e.g. this record
                // was last written by a redial) the peer had no way to dial us
                // back, leaving it grey with "re-pair needed" after its
                // restart (confirmed on-device: the host logged
                // `no stored redial address (iroh ticket)` for us). It also
                // went stale as our address changed. Re-offering here means the
                // peer's stored ticket for us self-heals on the next successful
                // reconnect — no re-pair. `None` only when we aren't hosting a
                // listener (falls back to one-directional, as before).
                let my_offer = engine.build_own_reconnect_offer().await;
                tokio::select! {
                    _ = token.cancelled() => {}
                    r = engine.connect_peer_via_iroh(peer, pin, my_offer, resumption_token, None) => {
                        if let Err(e) = r {
                            warn!(device = %dev, error = %e, "dial_peer_record: iroh redial failed");
                        }
                    }
                }
            });
            return;
        }

        // Legacy TLS path (a bare `ip:port`, not a `corvux1:` ticket). TCP
        // reconnect is a DEAD path: the TCP acceptor preloads no resumption
        // tokens and, with `should_autostart_legacy_tcp_listener() == false`,
        // no TCP listener is even bound — so a dial here could never complete;
        // it would only spin and re-fail (and, if a listener existed, be
        // rejected with `PairingClosed`). Surface a clear, actionable re-pair
        // signal instead of attempting it. Pairing — and therefore reconnect —
        // is iroh-only now. (Was: `connect_peer(addr, pin, fp)`.)
        self.mark_dial_unusable(dev, "legacy TCP pairing — re-pair over iroh")
            .await;
    }

    /// Mark an auto-reconnect record as `Failed` (with a re-pair reason) so
    /// the supervisor backs off instead of retrying a permanently-broken
    /// record every tick, and the UI surfaces *why* it can't reconnect.
    async fn mark_dial_unusable(&self, dev: DeviceId, reason: &str) {
        warn!(device = %dev, reason, "dial_peer_record: record not auto-dialable");
        self.set_peer_state(
            dev,
            PeerConnectionState::Failed {
                reason: reason.to_string(),
                at_millis: now_millis(),
            },
        )
        .await;
    }

    /// Look up the peer_store entry that matches `pinned` and return
    /// its device_id. Used by `connect_peer` to attach dial-state
    /// events to an already-known peer before the handshake returns
    /// the id we'd otherwise have to wait for. AUDIT-003b: `pinned` is
    /// mandatory now, but the lookup can still return `None` if the
    /// caller resolved a fingerprint that isn't in `peer_store` (rare —
    /// e.g. cross-process race between unpair and reconnect).
    async fn expected_device_for_fingerprint(&self, pinned: Fingerprint) -> Option<DeviceId> {
        let store = self.inner.peer_store.lock().await;
        store
            .list()
            .into_iter()
            .find(|r| r.fingerprint == pinned.0)
            .map(|r| DeviceId(r.device_id))
    }

    pub(super) async fn persist_new_peer(
        &self,
        device: DeviceId,
        display_name: String,
        fingerprint: Fingerprint,
    ) -> SyncResult<()> {
        // Credential-preserving: a redial must NOT blank the stored
        // ticket/PIN/token it just used (see PeerStore::ensure_peer — the old
        // `upsert(PeerRecord::new(..))` here burned the redial credentials on
        // every successful outbound connect, so the SECOND relaunch had
        // nothing to dial and sat grey with "re-pair needed").
        self.inner
            .peer_store
            .lock()
            .await
            .ensure_peer(device, fingerprint.0, display_name)?;
        Ok(())
    }
}
