// Wait state + nonce field for future challenge flow
#![allow(dead_code)]

//! Pairing handshake as an explicit state machine.
//!
//! Both sides run through the same states, just mirrored: the server
//! receives `Hello` and emits `PairingChallenge`; the client sends `Hello`
//! and awaits the challenge. Encoding the flow as enum transitions (rather
//! than ad-hoc booleans) means:
//!
//! - Unreachable combinations don't compile.
//! - A bug in the flow is a panic from `match` exhaustiveness, not a
//!   silent "hang forever".
//! - The UI can render a meaningful status from the current state
//!   (`"Pairing: awaiting PIN"`, `"Pairing: verifying"`, etc.).
//!
//! # State transitions
//!
//! ```text
//!     Client                          Server
//!     ──────                          ──────
//!     ClientState::Init               ServerState::Init
//!     send Hello ──────────────────▶  recv Hello → SawHello
//!                                     send PairingChallenge
//!     recv Challenge → AwaitingPin  ◀──────────────── SentChallenge
//!     (user types PIN)
//!     send PairingProof ───────────▶  recv Proof → verify
//!       → AwaitingResult             success → send PairingOk → Done
//!                                    failure → send PairingFailed → Failed
//!     recv Ok → Done                ◀────────────────
//! ```
//!
//! The *outcome* (success vs failure) is represented as an `Outcome` enum
//! returned from `step`, letting the engine handle next actions without
//! pattern-matching on internal state.

use std::sync::{Arc, Mutex};

use std::collections::HashMap;

use super::{Message, PairingFailure};
use crate::sync::crypto::pairing::{
    derive_session_from_key, first_pair_key, proof_from_key, verify_proof_with_key, FirstPairGate,
    KeyedPairingRateLimiter, Nonce, PairingSecret, Pin, Proof, SessionSecret,
    SharedEnrollmentWindow,
};
use crate::sync::error::{ProtocolError, SyncResult};
use crate::sync::identity::DeviceId;

/// AUDIT-077: shared, engine-scoped pairing rate limiter. The previous
/// design owned a `PairingRateLimiter` per `ServerHandshake`, which is
/// re-instantiated on every accepted connection — so the exponential
/// backoff counter reset every time an attacker reconnected, defeating
/// the brute-force defense. Threading an `Arc<Mutex<…>>` through the
/// handshake lets every connection share one counter for the engine's
/// lifetime.
pub type SharedRateLimiter = Arc<Mutex<KeyedPairingRateLimiter>>;

/// Domain-separation label for the channel-binding transcript. Bump if the
/// transcript layout changes so a proof computed under one layout can never be
/// mistaken for another.
const TRANSCRIPT_DOMAIN: &[u8] = b"corvux-sync-transcript-v1";

/// Build the channel-binding transcript the pairing proof is keyed over. Both
/// peers MUST produce identical bytes, so it is built here in ONE place rather
/// than hand-assembled on each side (a divergence would silently fail every
/// pairing). Binds:
/// - a domain label + the wire `PROTOCOL_VERSION` (a MITM can't downgrade the
///   proof format or replay it in a different protocol context), and
/// - both ephemeral X25519 public keys (the core MITM channel binding —
///   substituting either key changes the transcript and breaks the proof).
///
/// Device/vault identity is deliberately NOT bound here: the server's identity
/// only arrives in `PairingOk`, *after* the proof, so it isn't available to
/// both sides symmetrically at proof time. The post-handshake vault-id compare
/// (accept/dial) covers that.
fn build_transcript(client_eph_pub: &[u8; 32], server_eph_pub: &[u8; 32]) -> Vec<u8> {
    let mut t = Vec::with_capacity(TRANSCRIPT_DOMAIN.len() + 2 + 64);
    t.extend_from_slice(TRANSCRIPT_DOMAIN);
    t.extend_from_slice(&crate::sync::transport::frame::PROTOCOL_VERSION.to_le_bytes());
    t.extend_from_slice(client_eph_pub);
    t.extend_from_slice(server_eph_pub);
    t
}

/// What the handshake wants to happen next.
pub enum Outcome {
    /// Send this message, then call `step` again on the next received one.
    Send(Message),
    /// Handshake succeeded. Session key is ready. Move on to sync phase.
    Done {
        session: SessionSecret,
        peer_device_id: DeviceId,
        peer_display_name: String,
        /// AUDIT-015: the roost id the peer advertised in `Hello`. Caller
        /// persists this with the `PeerRecord` so subsequent reconnects can
        /// detect that the same device is now presenting a different roost.
        peer_vault_id: [u8; 16],
    },
    /// Handshake failed. Tear down the transport.
    Failed(PairingFailure),
    /// Waiting for the next inbound message. No action.
    Wait,
}

impl std::fmt::Debug for Outcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Custom Debug: never print the session secret. Variants are enough
        // to tell the test engine what happened.
        match self {
            Outcome::Send(m) => f.debug_tuple("Send").field(&variant_name_of(m)).finish(),
            Outcome::Done {
                peer_device_id,
                peer_display_name,
                peer_vault_id,
                ..
            } => f
                .debug_struct("Done")
                .field("peer_device_id", peer_device_id)
                .field("peer_display_name", peer_display_name)
                .field("peer_vault_id", &hex::encode(peer_vault_id))
                .finish_non_exhaustive(),
            Outcome::Failed(r) => f.debug_tuple("Failed").field(r).finish(),
            Outcome::Wait => f.write_str("Wait"),
        }
    }
}

fn variant_name_of(m: &Message) -> &'static str {
    msg_name(m)
}

// ── Client side ─────────────────────────────────────────────────────

pub enum ClientState {
    /// Haven't sent our Hello yet.
    Init,
    /// Sent Hello; waiting for server's PairingChallenge.
    SentHello,
    /// Got challenge; PIN is available; sent our proof.
    SentProof {
        nonce: Nonce,
        session: Option<SessionSecret>,
    },
    /// Terminal.
    Done,
    /// Terminal.
    Failed,
}

pub struct ClientHandshake {
    state: ClientState,
    device_id: DeviceId,
    display_name: String,
    roost_name: String,
    vault_id: [u8; 16],
    pin: Pin,
    /// Symmetric pairing: our own (ticket, hosting-pin) offered to the host so
    /// it can dial us back. `None` = don't offer (redials / legacy).
    reconnect_offer: Option<(String, String)>,
    /// SEC-2: if set, authenticate by proving THIS resumption token instead of
    /// the human PIN (reconnect to an already-paired peer). `None` = first pair
    /// → prove the PIN.
    resumption_token: Option<[u8; 32]>,
    /// SRX layer 3: the one-time pairing secret we extracted from the scanned/
    /// pasted ticket. When set, the first-pair proof/session key is derived from
    /// PIN + secret, so a peer that never obtained the ticket can't forge it.
    /// `None` = no ticket secret (legacy / PIN-only fallback).
    pairing_secret: Option<PairingSecret>,
    /// H-2/H-3: our one-shot ephemeral X25519 secret, generated in `start()`
    /// and consumed when the server's ephemeral public arrives. `None` before
    /// `start()` and after the exchange completes.
    eph_secret: Option<x25519_dalek::EphemeralSecret>,
    /// Our ephemeral public bytes (set in `start()`), kept so we can rebuild
    /// the channel-binding transcript once the server's public arrives (the
    /// secret is consumed by the DH and can't re-derive the public).
    client_eph_pub: [u8; 32],
    /// M-3: on a reconnect, the `vault_id` we pinned for this peer. When set,
    /// the client rejects a `PairingChallenge` whose `server_vault_id` differs
    /// — a roost swap — BEFORE deriving the session/proof. `None` on a first
    /// pair (nothing pinned yet), mirroring the server's first-pair case.
    expected_peer_vault_id: Option<[u8; 16]>,
}

impl ClientHandshake {
    pub fn new(
        device_id: DeviceId,
        display_name: String,
        roost_name: String,
        vault_id: [u8; 16],
        pin: Pin,
    ) -> Self {
        Self {
            state: ClientState::Init,
            device_id,
            display_name,
            roost_name,
            vault_id,
            pin,
            reconnect_offer: None,
            resumption_token: None,
            pairing_secret: None,
            eph_secret: None,
            client_eph_pub: [0u8; 32],
            expected_peer_vault_id: None,
        }
    }

    /// SRX layer 3: bind the one-time pairing secret extracted from the ticket
    /// into the first-pair proof/session. Set by the dial path when the joiner
    /// scanned/pasted a ticket carrying a secret.
    pub fn with_pairing_secret(mut self, secret: PairingSecret) -> Self {
        self.pairing_secret = Some(secret);
        self
    }

    /// M-3: pin the `vault_id` we expect the peer (server) to present, so a
    /// reconnect that lands on a *different* roost is rejected before we derive
    /// a session. Set on redials from the stored peer record; unset on a first
    /// pair. Symmetric with the server's `with_expected_peer_vault_ids`.
    pub fn with_expected_peer_vault_id(mut self, vault_id: [u8; 16]) -> Self {
        self.expected_peer_vault_id = Some(vault_id);
        self
    }

    /// SEC-2: authenticate this handshake with a stored resumption token
    /// (reconnect) instead of the human PIN. Set only when we already have a
    /// token for this peer from a prior pairing.
    pub fn with_resumption_token(mut self, token: [u8; 32]) -> Self {
        self.resumption_token = Some(token);
        self
    }

    /// Offer our own redial credentials (dialable ticket + our hosting PIN) in
    /// the Hello, so the host can dial us back after a restart. Used only on
    /// the initial pair; redials don't re-offer (the host already stored them).
    pub fn with_reconnect_offer(mut self, ticket: String, pin: String) -> Self {
        self.reconnect_offer = Some((ticket, pin));
        self
    }

    /// Produce the opening message. Call once, before any `step`.
    pub fn start(&mut self) -> Message {
        self.state = ClientState::SentHello;
        let (dialer_ticket, dialer_pin) = match &self.reconnect_offer {
            Some((t, p)) => (Some(t.clone()), Some(p.clone())),
            None => (None, None),
        };
        // H-2/H-3: mint the one-shot ephemeral keypair for this handshake.
        let (eph_secret, client_eph_pub) = crate::sync::crypto::pairing::generate_ephemeral();
        self.eph_secret = Some(eph_secret);
        self.client_eph_pub = client_eph_pub;
        Message::Hello {
            device_id: self.device_id.0,
            protocol_version: super::super::transport::frame::PROTOCOL_VERSION,
            display_name: self.display_name.clone(),
            roost_name: self.roost_name.clone(),
            vault_id: self.vault_id,
            dialer_ticket,
            dialer_pin,
            client_eph_pub,
        }
    }

    pub fn step(&mut self, incoming: Message) -> SyncResult<Outcome> {
        match (&self.state, incoming) {
            // The server may reject the Hello (roost mismatch, rate limit,
            // unsupported version) before ever sending a challenge. Surface
            // that reason instead of the generic "unexpected message" error.
            (
                ClientState::SentHello | ClientState::SentProof { .. },
                Message::PairingFailed { reason },
            ) => {
                self.state = ClientState::Failed;
                Ok(Outcome::Failed(reason))
            }
            (
                ClientState::SentHello,
                Message::PairingChallenge {
                    nonce,
                    server_eph_pub,
                    server_vault_id,
                },
            ) => {
                // M-3: on a reconnect we pinned this peer's vault_id — reject a
                // roost swap HERE, before deriving any session/proof material
                // (the post-handshake compare in dial.rs remains as a backstop).
                // First pairs pin nothing, so this is a no-op then.
                if let Some(expected) = self.expected_peer_vault_id {
                    if expected != server_vault_id {
                        self.state = ClientState::Failed;
                        return Ok(Outcome::Failed(PairingFailure::RoostIdMismatch));
                    }
                }
                let nonce = Nonce(nonce);
                // H-2/H-3: complete the ephemeral X25519 exchange with our
                // stored one-shot secret (consumed here), yielding the same
                // ECDH secret the server derived, plus forward secrecy.
                let eph = self
                    .eph_secret
                    .take()
                    .ok_or(ProtocolError::UnexpectedMessage {
                        state: "SentHello",
                        actual: "challenge before ephemeral key was generated",
                    })?;
                let ecdh = crate::sync::crypto::pairing::ecdh_shared(eph, &server_eph_pub)?;
                // Channel-binding transcript, identical on both sides absent a
                // MITM (built by the shared helper so the two never diverge).
                let transcript = build_transcript(&self.client_eph_pub, &server_eph_pub);
                // SEC-2: authenticate with the resumption token if we hold one
                // for this peer (reconnect); otherwise prove the human PIN
                // (first pair). The session secret is derived from whichever
                // key both sides used (plus the ECDH secret), so the envelope
                // stays consistent.
                let (proof, session) = match &self.resumption_token {
                    Some(tok) => (
                        proof_from_key(tok, &nonce, &transcript)?,
                        derive_session_from_key(tok, &nonce, &ecdh)?,
                    ),
                    None => {
                        // SRX layer 3: first-pair key = PIN folded with the
                        // ticket secret (if we scanned one). Absent a secret this
                        // is the PIN bytes — identical to the pre-layer-3 path.
                        let key = first_pair_key(&self.pin, self.pairing_secret.as_ref());
                        (
                            proof_from_key(&key, &nonce, &transcript)?,
                            derive_session_from_key(&key, &nonce, &ecdh)?,
                        )
                    }
                };
                // Move the session into our state; we hand it out only on Done.
                self.state = ClientState::SentProof {
                    nonce,
                    session: Some(session),
                };
                Ok(Outcome::Send(Message::PairingProof { proof: proof.0 }))
            }
            (
                ClientState::SentProof { .. },
                Message::PairingOk {
                    device_id,
                    display_name,
                    vault_id,
                },
            ) => {
                let mut s = ClientState::Done;
                std::mem::swap(&mut self.state, &mut s);
                // Extract the session secret from the prior state.
                let session = match s {
                    ClientState::SentProof { session, .. } => session,
                    _ => unreachable!(),
                }
                .ok_or(ProtocolError::UnexpectedMessage {
                    state: "SentProof",
                    actual: "session already taken",
                })?;
                Ok(Outcome::Done {
                    session,
                    peer_device_id: DeviceId(device_id),
                    peer_display_name: display_name,
                    peer_vault_id: vault_id,
                })
            }
            (_, other) => Err(ProtocolError::UnexpectedMessage {
                state: client_state_name(&self.state),
                actual: msg_name(&other),
            }
            .into()),
        }
    }
}

// ── Server side ─────────────────────────────────────────────────────

pub enum ServerState {
    /// Waiting for the client's Hello.
    AwaitingHello,
    /// Sent challenge; waiting for proof. Peer identity captured from Hello.
    SentChallenge {
        nonce: Nonce,
        session: SessionSecret,
        peer_device_id: DeviceId,
        peer_display_name: String,
        peer_vault_id: [u8; 16],
        /// SEC-2: `Some(token)` if we're authenticating this connection by a
        /// stored resumption token (reconnect); `None` = verify the PIN.
        auth_token: Option<[u8; 32]>,
        /// H-1/H-2: the channel-binding transcript (see `build_transcript`) the
        /// proof is bound to. Stored so the proof can be verified against the
        /// exact transcript we issued the challenge under.
        transcript: Vec<u8>,
    },
    /// Terminal.
    Done,
    /// Terminal.
    Failed,
}

pub struct ServerHandshake {
    state: ServerState,
    device_id: DeviceId,
    display_name: String,
    roost_name: String,
    vault_id: [u8; 16],
    pin: Pin,
    /// AUDIT-077: shared across handshakes for the engine's lifetime. See
    /// [`SharedRateLimiter`]. Defaults to a fresh per-instance limiter
    /// (preserves test ergonomics); production callers must inject the
    /// engine's shared instance via [`Self::with_shared_rate_limiter`].
    rate_limiter: SharedRateLimiter,
    /// AUDIT-015: if we've paired with this peer before, the roost id we
    /// recorded then. A `Hello` from the same device carrying a *different*
    /// roost id is rejected with `PairingFailure::RoostIdMismatch`. `None`
    /// means "first pair attempt with this peer — accept any roost id".
    expected_peer_vault_id: Option<[u8; 16]>,
    /// AUDIT-015 (server side): roost ids we recorded for previously-paired
    /// devices, keyed by device id. The acceptor doesn't know *which* device
    /// is dialing until the `Hello` arrives, so — unlike the single
    /// `expected_peer_vault_id` a dialer can set — the server looks the
    /// expected id up by the just-received device id and rejects a mismatch
    /// *before* deriving the session. Pre-loaded by the accept path from the
    /// peer store. See M-3.
    expected_peer_vault_ids: HashMap<DeviceId, [u8; 16]>,
    /// Symmetric pairing: the dialer's own redial credentials, captured from
    /// its `Hello`. The caller (`run_handshake` → accept path) reads this via
    /// [`Self::peer_reconnect`] after a successful handshake and stores it so
    /// WE can dial the dialer back. `None` if the dialer didn't offer.
    peer_reconnect: Option<(String, String)>,
    /// SEC-2: resumption tokens we hold, keyed by device id. When an incoming
    /// `Hello` names a device we have a token for, we authenticate that
    /// connection by verifying the token (reconnect) instead of the human PIN.
    /// Pre-loaded by the accept path from the peer store.
    resumption_tokens: HashMap<DeviceId, [u8; 32]>,
    /// M-11: the source key (remote IP) this connection is rate-limited under.
    /// The shared limiter tracks backoff per key, so one abusive source can't
    /// lock out pairing for everyone. Empty string = ungrouped (tests / the
    /// rare case where no peer address is available).
    rate_limit_key: String,
    /// SRX brute-force fix: the engine-shared first-pair enrollment window. When
    /// set (production accept paths inject it), an inbound FIRST-PAIR handshake
    /// is refused unless the window is open, and a source-independent global cap
    /// trips a hard lockout during the window — defeating the fresh-NodeId
    /// spraying that beats the per-key rate limiter. `None` (tests) leaves
    /// first-pair ungated, preserving existing test ergonomics. Reconnects
    /// (resumption token) are never gated regardless.
    enrollment_window: Option<SharedEnrollmentWindow>,
    /// SRX layer 3: the one-time secret we minted for this "Show code" session
    /// and embedded in the ticket. When set, the first-pair proof/session key is
    /// derived from PIN + secret, so a caller that didn't obtain the ticket
    /// (only the public NodeId) derives a different key and its proof fails —
    /// this is what makes ticket pairing non-brute-forceable. `None` = PIN-only
    /// first pair (legacy / no-ticket fallback).
    pairing_secret: Option<PairingSecret>,
    /// B4: the peer device this connection is allowed to first-pair OUTSIDE the
    /// enrollment window — set ONLY when the accept path has already
    /// cryptographically authenticated the peer (the iroh QUIC NodeId) AND finds
    /// that device already in our peer store. It exists solely to let a device
    /// paired BEFORE resumption tokens shipped (`resumption_token == None`)
    /// reconnect and acquire a token, without forcing the user to re-open
    /// pairing. It MUST be derived from the authenticated NodeId, NEVER from the
    /// self-asserted `Hello.device_id` (which is spoofable) — otherwise a
    /// stranger could claim a known id to skip the gate and brute-force the PIN.
    /// The exemption skips only the enrollment window; the per-source rate
    /// limiter, the PIN/secret proof, and the post-handshake NodeId weld all
    /// still apply. `None` = fully gated (the default; the TLS accept path,
    /// which has no NodeId weld, always leaves it `None`).
    enrollment_exempt_device: Option<DeviceId>,
}

impl ServerHandshake {
    pub fn new(
        device_id: DeviceId,
        display_name: String,
        roost_name: String,
        vault_id: [u8; 16],
        pin: Pin,
    ) -> Self {
        Self {
            state: ServerState::AwaitingHello,
            device_id,
            display_name,
            roost_name,
            vault_id,
            pin,
            rate_limiter: Arc::new(Mutex::new(KeyedPairingRateLimiter::new())),
            expected_peer_vault_id: None,
            expected_peer_vault_ids: HashMap::new(),
            peer_reconnect: None,
            resumption_tokens: HashMap::new(),
            rate_limit_key: String::new(),
            enrollment_window: None,
            pairing_secret: None,
            enrollment_exempt_device: None,
        }
    }

    /// SRX layer 3: inject the one-time secret this session minted (also embedded
    /// in the ticket) so the server derives the first-pair key from PIN + secret.
    /// The accept path sets this from the engine's current session secret.
    pub fn with_pairing_secret(mut self, secret: PairingSecret) -> Self {
        self.pairing_secret = Some(secret);
        self
    }

    /// SRX brute-force fix: inject the engine-shared first-pair enrollment
    /// window so inbound first-pair handshakes are gated on an active, time-boxed
    /// pairing window + a source-independent global failure cap. Production
    /// accept paths (`sync::engine::accept`) must call this; tests can skip it
    /// and leave first-pair ungated.
    pub fn with_enrollment_window(mut self, window: SharedEnrollmentWindow) -> Self {
        self.enrollment_window = Some(window);
        self
    }

    /// M-11: set the source key (remote IP) this connection is rate-limited
    /// under. Set by the accept path from the peer's address.
    pub fn with_rate_limit_key(mut self, key: String) -> Self {
        self.rate_limit_key = key;
        self
    }

    /// AUDIT-015 (server side): supply the roost ids we recorded for
    /// previously-paired devices, keyed by device id, so an incoming `Hello`
    /// whose roost id changed since first pair is rejected before a session is
    /// derived. Pre-loaded by the accept path from the peer store. See M-3.
    pub fn with_expected_peer_vault_ids(mut self, ids: HashMap<DeviceId, [u8; 16]>) -> Self {
        self.expected_peer_vault_ids = ids;
        self
    }

    /// SEC-2: supply the resumption tokens we hold (keyed by device id) so an
    /// incoming reconnect from a known peer authenticates by proving its token
    /// rather than re-proving the human PIN.
    pub fn with_resumption_tokens(mut self, tokens: HashMap<DeviceId, [u8; 32]>) -> Self {
        self.resumption_tokens = tokens;
        self
    }

    /// B4: mark the QUIC-authenticated peer device as exempt from the enrollment
    /// window for a one-time token-acquisition first-pair. The accept path MUST
    /// pass the device derived from the authenticated iroh NodeId (and only if
    /// it's already in the peer store), never the self-asserted `Hello`
    /// device_id — see the field docs. `None` keeps first-pair fully gated.
    pub fn with_enrollment_exempt_device(mut self, device: Option<DeviceId>) -> Self {
        self.enrollment_exempt_device = device;
        self
    }

    /// The dialer's redial credentials captured from its `Hello`, if it offered
    /// them (symmetric pairing). Valid only after a successful handshake.
    pub fn peer_reconnect(&self) -> Option<(String, String)> {
        self.peer_reconnect.clone()
    }

    /// AUDIT-077: inject the engine's shared rate limiter so the
    /// exponential-backoff counter spans every accepted connection rather
    /// than resetting on each new handshake. Production accept paths
    /// (`sync::engine::accept`) must call this; tests can skip it and
    /// get a fresh per-instance limiter from [`Self::new`].
    pub fn with_shared_rate_limiter(mut self, shared: SharedRateLimiter) -> Self {
        self.rate_limiter = shared;
        self
    }

    /// Set the roost id we expect the peer to present (from a prior
    /// `PeerRecord`). Calling this is what enables the AUDIT-015
    /// reconnect-time check.
    pub fn with_expected_peer_vault_id(mut self, expected: [u8; 16]) -> Self {
        self.expected_peer_vault_id = Some(expected);
        self
    }

    /// The server's own device id — needed to build the PairingOk frame
    /// (client pins this). Previously the engine accidentally echoed the
    /// *client's* id back, so both sides ended up storing the same name
    /// in their peer lists.
    pub fn own_device_id(&self) -> DeviceId {
        self.device_id
    }

    /// The server's own human-readable display name — sent to the client
    /// in PairingOk so the client can show "Paired: <server>".
    pub fn own_display_name(&self) -> &str {
        &self.display_name
    }

    /// AUDIT-015: the server's own roost id — echoed in `PairingOk` so the
    /// client can pin it for the symmetric reconnect-time check.
    pub fn own_vault_id(&self) -> [u8; 16] {
        self.vault_id
    }

    pub fn step(&mut self, incoming: Message) -> SyncResult<Outcome> {
        match (&self.state, incoming) {
            (
                ServerState::AwaitingHello,
                Message::Hello {
                    device_id,
                    protocol_version,
                    display_name,
                    roost_name,
                    vault_id,
                    dialer_ticket,
                    dialer_pin,
                    client_eph_pub,
                },
            ) => {
                // Symmetric pairing: stash the dialer's redial offer (if both
                // halves present) so the accept path can store it post-success.
                if let (Some(t), Some(p)) = (dialer_ticket, dialer_pin) {
                    self.peer_reconnect = Some((t, p));
                }
                if protocol_version != super::super::transport::frame::PROTOCOL_VERSION {
                    self.state = ServerState::Failed;
                    return Ok(Outcome::Send(Message::PairingFailed {
                        reason: PairingFailure::UnsupportedVersion,
                    }));
                }
                if roost_name != self.roost_name {
                    self.state = ServerState::Failed;
                    return Ok(Outcome::Send(Message::PairingFailed {
                        reason: PairingFailure::RoostMismatch {
                            expected: self.roost_name.clone(),
                            got: roost_name,
                        },
                    }));
                }
                // AUDIT-015: if we've paired with this peer before, the
                // roost id is part of the identity we pinned. A different
                // roost id from the same device means the user pointed
                // Corvux at a different folder (or an attacker swapped the
                // bytes). Reject before deriving a session key.
                // Resolve the expected roost id from either a dialer-set single
                // value or the server's device-keyed map (M-3). The map is the
                // production accept path: it looks up by the device the `Hello`
                // just named, so the weld is rejected before session derivation.
                let expected_vault = self.expected_peer_vault_id.or_else(|| {
                    self.expected_peer_vault_ids
                        .get(&DeviceId(device_id))
                        .copied()
                });
                if let Some(expected) = expected_vault {
                    if expected != vault_id {
                        self.state = ServerState::Failed;
                        return Ok(Outcome::Send(Message::PairingFailed {
                            reason: PairingFailure::RoostIdMismatch,
                        }));
                    }
                }
                // SRX brute-force fix: gate FIRST-PAIR (not reconnect) on the
                // enrollment window BEFORE the per-key rate limiter. A reconnect
                // (we hold a resumption token for this device) authenticates by
                // token and is never gated. First-pair is refused outside an
                // active pairing window, and a source-independent global cap
                // trips inside it — closing the fresh-NodeId spraying hole that
                // beats the per-key limiter. `check_and_reserve_first_pair`
                // reserves the slot at challenge-issue time (mirroring the
                // per-key limiter) so concurrently-opened connections can't all
                // pass a read-only check. Poisoned mutex → fail closed (reject).
                let is_first_pair = !self.resumption_tokens.contains_key(&DeviceId(device_id));
                // B4: a device the transport already authenticated (iroh NodeId)
                // AND that we already hold in the peer store is exempt from the
                // enrollment window — this is how a pre-resumption-token peer
                // reconnects to acquire a token without the user re-opening
                // pairing. The exemption is bound to `enrollment_exempt_device`,
                // which the accept path derives from the AUTHENTICATED NodeId, so
                // matching it against the Hello's `device_id` here is safe: a
                // stranger who spoofs a known `device_id` in its Hello still
                // won't equal the exempt device unless it also holds that
                // device's NodeId private key (which the accept path required to
                // set the exemption). Everything else — the rate limiter below,
                // the PIN/secret proof, the post-handshake weld — still applies.
                let enrollment_exempt = self.enrollment_exempt_device == Some(DeviceId(device_id));
                if is_first_pair && !enrollment_exempt {
                    if let Some(window) = &self.enrollment_window {
                        let gate = match window.lock() {
                            Ok(mut w) => w.check_and_reserve_first_pair(),
                            Err(_) => FirstPairGate::Closed,
                        };
                        match gate {
                            FirstPairGate::Allow => {}
                            FirstPairGate::Closed => {
                                self.state = ServerState::Failed;
                                return Ok(Outcome::Send(Message::PairingFailed {
                                    reason: PairingFailure::PairingClosed,
                                }));
                            }
                            FirstPairGate::Locked => {
                                self.state = ServerState::Failed;
                                return Ok(Outcome::Send(Message::PairingFailed {
                                    reason: PairingFailure::RateLimited,
                                }));
                            }
                        }
                    }
                }
                // AUDIT-077: rate-limit check on the engine-shared limiter
                // (not a fresh per-handshake one). Mutex poisoning would
                // only happen if a prior holder panicked mid-update; treat
                // that as "deny" so we fail closed rather than wide-open.
                // Reserve the rate-limit slot HERE (at challenge issue), not
                // just check it — otherwise concurrently-opened connections all
                // pass a read-only check before any records a failure, letting
                // an attacker brute-force the PIN in parallel. Poisoned mutex →
                // fail closed (deny).
                let limited = match self.rate_limiter.lock() {
                    Ok(mut rl) => rl.check_and_reserve(&self.rate_limit_key).is_err(),
                    Err(_) => true,
                };
                if limited {
                    self.state = ServerState::Failed;
                    return Ok(Outcome::Send(Message::PairingFailed {
                        reason: PairingFailure::RateLimited,
                    }));
                }
                let nonce = Nonce::generate();
                // H-2/H-3: complete the ephemeral X25519 exchange. Our one-shot
                // secret is consumed here (never stored), so the resulting
                // session key can't be reconstructed later even if the PIN /
                // token leaks — that is the forward secrecy.
                let (server_eph_secret, server_eph_pub) =
                    crate::sync::crypto::pairing::generate_ephemeral();
                let ecdh =
                    crate::sync::crypto::pairing::ecdh_shared(server_eph_secret, &client_eph_pub)?;
                // Transcript the proof is channel-bound to — same shared helper
                // as the client, so the two are byte-identical by construction.
                let transcript = build_transcript(&client_eph_pub, &server_eph_pub);
                // SEC-2: if we hold a resumption token for this device, this is
                // a reconnect — authenticate + derive the session from the
                // token. Otherwise it's a first pair — use the human PIN.
                let auth_token = self.resumption_tokens.get(&DeviceId(device_id)).copied();
                let session = match &auth_token {
                    Some(tok) => derive_session_from_key(tok, &nonce, &ecdh)?,
                    None => {
                        // SRX layer 3: first-pair key = PIN folded with the
                        // secret we minted + embedded in the ticket. A caller
                        // that never obtained the ticket derives a PIN-only key
                        // and its proof will not match this.
                        let key = first_pair_key(&self.pin, self.pairing_secret.as_ref());
                        derive_session_from_key(&key, &nonce, &ecdh)?
                    }
                };
                self.state = ServerState::SentChallenge {
                    nonce,
                    session,
                    peer_device_id: DeviceId(device_id),
                    peer_display_name: display_name,
                    peer_vault_id: vault_id,
                    auth_token,
                    transcript,
                };
                Ok(Outcome::Send(Message::PairingChallenge {
                    nonce: nonce.0,
                    server_eph_pub,
                    server_vault_id: self.vault_id,
                }))
            }
            (
                ServerState::SentChallenge {
                    nonce,
                    auth_token,
                    transcript,
                    ..
                },
                Message::PairingProof { proof },
            ) => {
                let nonce = *nonce;
                let auth_token = *auth_token;
                let transcript = transcript.clone();
                let claimed = Proof(proof);
                // Verify against the resumption token (reconnect) or the PIN
                // (first pair) — whichever we chose when issuing the challenge —
                // channel-bound to the ephemeral-DH transcript (H-1/H-2).
                let verified = match &auth_token {
                    Some(tok) => verify_proof_with_key(tok, &nonce, &transcript, &claimed),
                    None => {
                        // SRX layer 3: verify against PIN + ticket secret (the
                        // same combined key the challenge's session derived).
                        let key = first_pair_key(&self.pin, self.pairing_secret.as_ref());
                        verify_proof_with_key(&key, &nonce, &transcript, &claimed)
                    }
                };
                match verified {
                    Ok(()) => {
                        if let Ok(mut rl) = self.rate_limiter.lock() {
                            rl.record_success(&self.rate_limit_key);
                        }
                        // SRX: a successful FIRST pair clears the global
                        // enrollment counter (reserved at challenge issue). A
                        // token reconnect never touched it.
                        if auth_token.is_none() {
                            if let Some(window) = &self.enrollment_window {
                                if let Ok(mut w) = window.lock() {
                                    w.record_success();
                                }
                            }
                        }
                        // Swap out the session secret + peer identity we
                        // stashed in SentChallenge.
                        let mut s = ServerState::Done;
                        std::mem::swap(&mut self.state, &mut s);
                        let (session, peer_device_id, peer_display_name, peer_vault_id) = match s {
                            ServerState::SentChallenge {
                                session,
                                peer_device_id,
                                peer_display_name,
                                peer_vault_id,
                                ..
                            } => (session, peer_device_id, peer_display_name, peer_vault_id),
                            _ => unreachable!(),
                        };
                        Ok(Outcome::Done {
                            session,
                            peer_device_id,
                            peer_display_name,
                            peer_vault_id,
                        })
                    }
                    Err(_) => {
                        // No record_failure here: the attempt was already
                        // counted by check_and_reserve at challenge-issue time.
                        // (A wrong proof, an abandoned handshake, and a
                        // completed-but-wrong attempt all cost exactly one
                        // increment, which record_success clears on the happy
                        // path.)
                        self.state = ServerState::Failed;
                        Ok(Outcome::Send(Message::PairingFailed {
                            reason: PairingFailure::WrongPin,
                        }))
                    }
                }
            }
            (_, other) => Err(ProtocolError::UnexpectedMessage {
                state: server_state_name(&self.state),
                actual: msg_name(&other),
            }
            .into()),
        }
    }
}

fn client_state_name(s: &ClientState) -> &'static str {
    match s {
        ClientState::Init => "Init",
        ClientState::SentHello => "SentHello",
        ClientState::SentProof { .. } => "SentProof",
        ClientState::Done => "Done",
        ClientState::Failed => "Failed",
    }
}

fn server_state_name(s: &ServerState) -> &'static str {
    match s {
        ServerState::AwaitingHello => "AwaitingHello",
        ServerState::SentChallenge { .. } => "SentChallenge",
        ServerState::Done => "Done",
        ServerState::Failed => "Failed",
    }
}

fn msg_name(m: &Message) -> &'static str {
    match m {
        Message::Hello { .. } => "Hello",
        Message::PairingChallenge { .. } => "PairingChallenge",
        Message::PairingProof { .. } => "PairingProof",
        Message::PairingOk { .. } => "PairingOk",
        Message::PairingFailed { .. } => "PairingFailed",
        Message::SyncMessage { .. } => "SyncMessage",
        Message::NoteList { .. } => "NoteList",
        Message::RequestFullDoc { .. } => "RequestFullDoc",
        Message::FullDoc { .. } => "FullDoc",
        Message::FullDocChunk { .. } => "FullDocChunk",
        Message::NoteDelete { .. } => "NoteDelete",
        Message::Ping => "Ping",
        Message::Pong => "Pong",
        Message::Bye => "Bye",
        Message::EncryptedEnvelope { .. } => "EncryptedEnvelope",
        Message::RequestBlob { .. } => "RequestBlob",
        Message::Blob { .. } => "Blob",
        Message::BlobUnavailable { .. } => "BlobUnavailable",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::crypto::pairing::Pin;

    fn dev(n: u8) -> DeviceId {
        DeviceId([n; 16])
    }

    #[test]
    fn transcript_binds_both_keys_domain_and_version() {
        let c = [1u8; 32];
        let s = [2u8; 32];
        let t = build_transcript(&c, &s);
        // Domain label + 2-byte version + both 32-byte keys.
        assert_eq!(t.len(), TRANSCRIPT_DOMAIN.len() + 2 + 64);
        assert!(t.starts_with(TRANSCRIPT_DOMAIN));
        assert_eq!(
            &t[TRANSCRIPT_DOMAIN.len()..TRANSCRIPT_DOMAIN.len() + 2],
            &crate::sync::transport::frame::PROTOCOL_VERSION.to_le_bytes()
        );
        // Order-sensitive: swapping the two ephemeral keys yields a different
        // transcript (so a reflected challenge can't reuse the same proof).
        assert_ne!(t, build_transcript(&s, &c));
        // Changing either key changes the transcript (channel binding).
        assert_ne!(t, build_transcript(&[9u8; 32], &s));
        assert_ne!(t, build_transcript(&c, &[9u8; 32]));
    }

    fn pin() -> Pin {
        Pin::parse("424242").unwrap()
    }

    fn vid(n: u8) -> [u8; 16] {
        [n; 16]
    }

    #[test]
    fn happy_path_completes() {
        let mut client =
            ClientHandshake::new(dev(1), "laptop".into(), "roost".into(), vid(1), pin());
        let mut server =
            ServerHandshake::new(dev(2), "phone".into(), "roost".into(), vid(2), pin());

        // 1. client opens
        let hello = client.start();
        // 2. server responds with challenge
        let Outcome::Send(challenge) = server.step(hello).unwrap() else {
            panic!("expected challenge");
        };
        // 3. client sends proof
        let Outcome::Send(proof) = client.step(challenge).unwrap() else {
            panic!("expected proof");
        };
        // 4. server verifies → done (no outbound here because our impl returns
        //    Done rather than Send(Ok)). In real wire flow, the engine
        //    wrapping this sends `PairingOk` explicitly after seeing `Done`.
        let server_out = server.step(proof).unwrap();
        match server_out {
            Outcome::Done { peer_vault_id, .. } => {
                assert_eq!(peer_vault_id, vid(1), "server must surface client roost id");
            }
            other => panic!("expected Done, got {other:?}"),
        }

        // 5. simulate server sending PairingOk, client finalizes.
        let ok = Message::PairingOk {
            device_id: dev(2).0,
            display_name: "phone".into(),
            vault_id: vid(2),
        };
        let client_out = client.step(ok).unwrap();
        match client_out {
            Outcome::Done { peer_vault_id, .. } => {
                assert_eq!(peer_vault_id, vid(2), "client must surface server roost id");
            }
            other => panic!("expected Done, got {other:?}"),
        }
    }

    /// H-2/H-3: the same PIN across two handshakes must yield DIFFERENT session
    /// keys, because each mixes in a fresh ephemeral ECDH secret — the
    /// forward-secrecy property (a leaked PIN can't reconstruct a past key).
    #[test]
    fn ephemeral_keys_make_each_session_unique() {
        let run = || -> [u8; 32] {
            let mut c = ClientHandshake::new(dev(1), "a".into(), "v".into(), vid(1), pin());
            let mut s = ServerHandshake::new(dev(2), "b".into(), "v".into(), vid(2), pin());
            let hello = c.start();
            let Outcome::Send(challenge) = s.step(hello).unwrap() else {
                panic!("expected challenge");
            };
            let Outcome::Send(proof) = c.step(challenge).unwrap() else {
                panic!("expected proof");
            };
            match s.step(proof).unwrap() {
                Outcome::Done { session, .. } => session.0,
                other => panic!("expected Done, got {other:?}"),
            }
        };
        assert_ne!(
            run(),
            run(),
            "same PIN but fresh ephemerals must produce distinct session keys"
        );
    }

    /// M-3: a reconnect that pinned the peer's vault_id must reject a challenge
    /// carrying a DIFFERENT roost id — BEFORE deriving any session/proof.
    #[test]
    fn dial_rejects_vault_id_swap_before_session() {
        let mut client = ClientHandshake::new(dev(1), "a".into(), "MyBrain".into(), vid(1), pin())
            .with_expected_peer_vault_id(vid(2));
        // Server presents a vault id we did NOT pin.
        let mut server = ServerHandshake::new(dev(2), "b".into(), "MyBrain".into(), vid(9), pin());
        let hello = client.start();
        let Outcome::Send(challenge) = server.step(hello).unwrap() else {
            panic!("expected challenge");
        };
        match client.step(challenge).unwrap() {
            Outcome::Failed(PairingFailure::RoostIdMismatch) => {}
            other => panic!("expected pre-session RoostIdMismatch, got {other:?}"),
        }
        // The client must not have produced a proof.
        assert!(matches!(client.state, ClientState::Failed));
    }

    /// M-3: a matching pinned vault_id proceeds normally to the proof.
    #[test]
    fn dial_accepts_matching_vault_id() {
        let mut client = ClientHandshake::new(dev(1), "a".into(), "MyBrain".into(), vid(1), pin())
            .with_expected_peer_vault_id(vid(2));
        let mut server = ServerHandshake::new(dev(2), "b".into(), "MyBrain".into(), vid(2), pin());
        let hello = client.start();
        let Outcome::Send(challenge) = server.step(hello).unwrap() else {
            panic!("expected challenge");
        };
        assert!(
            matches!(
                client.step(challenge).unwrap(),
                Outcome::Send(Message::PairingProof { .. })
            ),
            "a matching vault id must proceed to the proof"
        );
    }

    /// H-1/H-2: an active MITM that substitutes its own ephemeral public key
    /// makes the two sides bind the proof to different transcripts, so the
    /// relayed proof fails verification — the MITM can't pair without the PIN.
    #[test]
    fn mitm_substituting_ephemeral_key_fails_proof() {
        let mut client = ClientHandshake::new(dev(1), "a".into(), "MyBrain".into(), vid(1), pin());
        let mut server = ServerHandshake::new(dev(2), "b".into(), "MyBrain".into(), vid(2), pin());

        let hello = client.start();
        let Outcome::Send(challenge) = server.step(hello).unwrap() else {
            panic!("expected challenge");
        };
        // MITM swaps the server's ephemeral pubkey before the client sees it.
        let tampered = match challenge {
            Message::PairingChallenge {
                nonce,
                server_vault_id,
                ..
            } => Message::PairingChallenge {
                nonce,
                server_eph_pub: [0xAB; 32],
                server_vault_id,
            },
            other => panic!("expected challenge, got {other:?}"),
        };
        let Outcome::Send(proof) = client.step(tampered).unwrap() else {
            panic!("expected proof");
        };
        match server.step(proof).unwrap() {
            Outcome::Send(Message::PairingFailed { reason }) => assert!(
                matches!(reason, PairingFailure::WrongPin),
                "MITM key substitution must fail the proof, got {reason:?}"
            ),
            other => panic!("expected PairingFailed, got {other:?}"),
        }
    }

    #[test]
    fn first_pair_derives_matching_resumption_token_both_sides() {
        use crate::sync::crypto::pairing::{derive_subkey, RESUMPTION_TOKEN_LABEL};
        let mut client = ClientHandshake::new(dev(1), "l".into(), "roost".into(), vid(1), pin());
        let mut server = ServerHandshake::new(dev(2), "p".into(), "roost".into(), vid(2), pin());
        let hello = client.start();
        let Outcome::Send(challenge) = server.step(hello).unwrap() else {
            panic!("expected challenge")
        };
        let Outcome::Send(proof) = client.step(challenge).unwrap() else {
            panic!("expected proof")
        };
        let server_session = match server.step(proof).unwrap() {
            Outcome::Done { session, .. } => session,
            o => panic!("expected Done, got {o:?}"),
        };
        let ok = Message::PairingOk {
            device_id: dev(2).0,
            display_name: "p".into(),
            vault_id: vid(2),
        };
        let client_session = match client.step(ok).unwrap() {
            Outcome::Done { session, .. } => session,
            o => panic!("expected Done, got {o:?}"),
        };
        let ct = *derive_subkey(&client_session, RESUMPTION_TOKEN_LABEL)
            .unwrap()
            .expose();
        let st = *derive_subkey(&server_session, RESUMPTION_TOKEN_LABEL)
            .unwrap()
            .expose();
        assert_eq!(
            ct, st,
            "both sides must derive the SAME resumption token from the shared session"
        );
    }

    #[test]
    fn reconnect_authenticates_by_token_not_pin() {
        let tok = [0x5a; 32];
        // Deliberately DIFFERENT PINs on each side: authentication must succeed
        // on the token alone — proving the human PIN is no longer the reconnect
        // credential (SEC-2's whole point).
        let mut client = ClientHandshake::new(
            dev(1),
            "l".into(),
            "roost".into(),
            vid(1),
            Pin::parse("111111").unwrap(),
        )
        .with_resumption_token(tok);
        let mut server = ServerHandshake::new(
            dev(2),
            "p".into(),
            "roost".into(),
            vid(2),
            Pin::parse("222222").unwrap(),
        )
        .with_resumption_tokens(HashMap::from([(dev(1), tok)]));
        let hello = client.start();
        let Outcome::Send(challenge) = server.step(hello).unwrap() else {
            panic!("expected challenge")
        };
        let Outcome::Send(proof) = client.step(challenge).unwrap() else {
            panic!("expected proof")
        };
        assert!(
            matches!(server.step(proof).unwrap(), Outcome::Done { .. }),
            "a matching resumption token must authenticate regardless of PIN"
        );
    }

    #[test]
    fn reconnect_with_wrong_token_is_rejected() {
        let mut client = ClientHandshake::new(dev(1), "l".into(), "roost".into(), vid(1), pin())
            .with_resumption_token([0xAA; 32]);
        let mut server = ServerHandshake::new(dev(2), "p".into(), "roost".into(), vid(2), pin())
            .with_resumption_tokens(HashMap::from([(dev(1), [0xBB; 32])]));
        let hello = client.start();
        let Outcome::Send(challenge) = server.step(hello).unwrap() else {
            panic!("expected challenge")
        };
        let Outcome::Send(proof) = client.step(challenge).unwrap() else {
            panic!("expected proof")
        };
        match server.step(proof).unwrap() {
            Outcome::Send(Message::PairingFailed { reason }) => {
                assert!(matches!(reason, PairingFailure::WrongPin));
            }
            o => panic!("expected PairingFailed, got {o:?}"),
        }
    }

    #[test]
    fn wrong_pin_rejected() {
        let mut client = ClientHandshake::new(
            dev(1),
            "a".into(),
            "v".into(),
            vid(1),
            Pin::parse("111111").unwrap(),
        );
        let mut server = ServerHandshake::new(
            dev(2),
            "b".into(),
            "v".into(),
            vid(2),
            Pin::parse("222222").unwrap(),
        );

        let hello = client.start();
        let Outcome::Send(challenge) = server.step(hello).unwrap() else {
            panic!()
        };
        let Outcome::Send(proof) = client.step(challenge).unwrap() else {
            panic!()
        };
        let out = server.step(proof).unwrap();
        match out {
            Outcome::Send(Message::PairingFailed { reason }) => {
                assert!(matches!(reason, PairingFailure::WrongPin));
            }
            _ => panic!("expected WrongPin failure"),
        }
    }

    #[test]
    fn unsupported_protocol_version_rejected() {
        let mut server = ServerHandshake::new(dev(2), "b".into(), "v".into(), vid(2), pin());
        let hello = Message::Hello {
            device_id: [1u8; 16],
            protocol_version: 999,
            display_name: "x".into(),
            roost_name: "v".into(),
            vault_id: vid(1),
            dialer_ticket: None,
            dialer_pin: None,
            client_eph_pub: [0u8; 32],
        };
        let out = server.step(hello).unwrap();
        match out {
            Outcome::Send(Message::PairingFailed { reason }) => {
                assert!(matches!(reason, PairingFailure::UnsupportedVersion));
            }
            _ => panic!("expected UnsupportedVersion"),
        }
    }

    #[test]
    fn server_captures_dialer_reconnect_offer() {
        let mut client = ClientHandshake::new(dev(1), "a".into(), "v".into(), vid(1), pin())
            .with_reconnect_offer("corvux1:xyz".into(), "654321".into());
        let hello = client.start();
        let mut server = ServerHandshake::new(dev(2), "b".into(), "v".into(), vid(1), pin());
        // Processing the Hello issues a challenge AND stashes the offer.
        let _ = server.step(hello).unwrap();
        assert_eq!(
            server.peer_reconnect(),
            Some(("corvux1:xyz".to_string(), "654321".to_string())),
            "server must capture the dialer's redial offer for symmetric reconnect"
        );
    }

    #[test]
    fn server_has_no_offer_when_dialer_omits_it() {
        let mut client = ClientHandshake::new(dev(1), "a".into(), "v".into(), vid(1), pin());
        let hello = client.start();
        let mut server = ServerHandshake::new(dev(2), "b".into(), "v".into(), vid(1), pin());
        let _ = server.step(hello).unwrap();
        assert_eq!(server.peer_reconnect(), None);
    }

    #[test]
    fn unexpected_message_in_init_errors() {
        let mut server = ServerHandshake::new(dev(2), "b".into(), "v".into(), vid(2), pin());
        let err = server.step(Message::Ping).unwrap_err();
        // Should be a ProtocolError wrapped in SyncError.
        let s = err.to_string();
        assert!(s.contains("protocol"));
    }

    #[test]
    fn roost_mismatch_rejected() {
        let mut client = ClientHandshake::new(dev(1), "a".into(), "MyBrain".into(), vid(1), pin());
        let mut server =
            ServerHandshake::new(dev(2), "b".into(), "OtherRoost".into(), vid(2), pin());

        let hello = client.start();
        let out = server.step(hello).unwrap();
        match out {
            Outcome::Send(Message::PairingFailed { reason }) => match reason {
                PairingFailure::RoostMismatch { expected, got } => {
                    assert_eq!(expected, "OtherRoost");
                    assert_eq!(got, "MyBrain");
                }
                other => panic!("expected RoostMismatch, got {other:?}"),
            },
            _ => panic!("expected PairingFailed"),
        }
    }

    #[test]
    fn matching_roosts_pass_handshake_start() {
        let mut client = ClientHandshake::new(dev(1), "a".into(), "MyBrain".into(), vid(1), pin());
        let mut server = ServerHandshake::new(dev(2), "b".into(), "MyBrain".into(), vid(2), pin());

        let hello = client.start();
        let out = server.step(hello).unwrap();
        // Matching roosts should progress to challenge, not fail.
        assert!(matches!(
            out,
            Outcome::Send(Message::PairingChallenge { .. })
        ));
    }

    /// AUDIT-015 headline test: same roost name, different roost id, and the
    /// server was constructed with an expectation (i.e. we've paired this
    /// peer before under a different roost id) → the handshake refuses
    /// before deriving a session key.
    #[test]
    fn vault_id_mismatch_rejected_on_reconnect() {
        let mut client = ClientHandshake::new(dev(1), "a".into(), "MyBrain".into(), vid(7), pin());
        let mut server = ServerHandshake::new(dev(2), "b".into(), "MyBrain".into(), vid(2), pin())
            .with_expected_peer_vault_id(vid(99));

        let hello = client.start();
        let out = server.step(hello).unwrap();
        match out {
            Outcome::Send(Message::PairingFailed { reason }) => {
                assert!(
                    matches!(reason, PairingFailure::RoostIdMismatch),
                    "expected RoostIdMismatch, got {reason:?}"
                );
            }
            other => panic!("expected PairingFailed::RoostIdMismatch, got {other:?}"),
        }
    }

    /// M-3: the production accept path pre-loads a device-keyed map of expected
    /// roost ids. A `Hello` from a device in that map carrying a different
    /// roost id is rejected before session derivation — same property as the
    /// single-value dialer case, but keyed by the just-received device id.
    #[test]
    fn vault_id_mismatch_rejected_via_device_keyed_map() {
        let mut client = ClientHandshake::new(dev(1), "a".into(), "MyBrain".into(), vid(7), pin());
        let mut map = HashMap::new();
        map.insert(dev(1), vid(99)); // we recorded dev(1) under a different roost
        let mut server = ServerHandshake::new(dev(2), "b".into(), "MyBrain".into(), vid(2), pin())
            .with_expected_peer_vault_ids(map);

        let hello = client.start();
        let out = server.step(hello).unwrap();
        match out {
            Outcome::Send(Message::PairingFailed { reason }) => assert!(
                matches!(reason, PairingFailure::RoostIdMismatch),
                "expected RoostIdMismatch, got {reason:?}"
            ),
            other => panic!("expected PairingFailed::RoostIdMismatch, got {other:?}"),
        }
    }

    /// M-3: a matching device-keyed roost id proceeds normally (no false
    /// rejection of a legitimate reconnect).
    #[test]
    fn vault_id_match_via_device_keyed_map_proceeds() {
        let mut client = ClientHandshake::new(dev(1), "a".into(), "MyBrain".into(), vid(7), pin());
        let mut map = HashMap::new();
        map.insert(dev(1), vid(7)); // recorded roost id matches what dev(1) sends
        let mut server = ServerHandshake::new(dev(2), "b".into(), "MyBrain".into(), vid(2), pin())
            .with_expected_peer_vault_ids(map);

        let hello = client.start();
        let out = server.step(hello).unwrap();
        assert!(matches!(
            out,
            Outcome::Send(Message::PairingChallenge { .. })
        ));
    }

    /// First-pair case: server has no stored expectation, so any roost id is
    /// accepted and the handshake proceeds.
    #[test]
    fn vault_id_accepted_when_no_prior_expectation() {
        let mut client = ClientHandshake::new(dev(1), "a".into(), "MyBrain".into(), vid(7), pin());
        let mut server = ServerHandshake::new(dev(2), "b".into(), "MyBrain".into(), vid(2), pin());

        let hello = client.start();
        let out = server.step(hello).unwrap();
        assert!(matches!(
            out,
            Outcome::Send(Message::PairingChallenge { .. })
        ));
    }

    /// AUDIT-077: the brute-force defense's invariant — a failed pairing
    /// on one connection must increment the counter that the *next*
    /// connection sees. The pre-fix `ServerHandshake` owned its limiter,
    /// so every accepted connection got a fresh zero-count one and
    /// exponential backoff never engaged.
    #[test]
    fn shared_rate_limiter_blocks_replayed_brute_force() {
        // Both handshakes default to the empty rate-limit key → same bucket,
        // so the shared limiter still blocks a replayed brute force from one
        // source (M-11 keys by source; same source still accumulates).
        let shared = Arc::new(Mutex::new(KeyedPairingRateLimiter::new()));

        // Connection #1: client guesses wrong PIN.
        let mut client_a = ClientHandshake::new(
            dev(1),
            "a".into(),
            "v".into(),
            vid(1),
            Pin::parse("111111").unwrap(),
        );
        let mut server_a = ServerHandshake::new(
            dev(2),
            "b".into(),
            "v".into(),
            vid(2),
            Pin::parse("222222").unwrap(),
        )
        .with_shared_rate_limiter(shared.clone());

        let Outcome::Send(challenge_a) = server_a.step(client_a.start()).unwrap() else {
            panic!("expected challenge");
        };
        let Outcome::Send(proof_a) = client_a.step(challenge_a).unwrap() else {
            panic!("expected proof");
        };
        let out_a = server_a.step(proof_a).unwrap();
        assert!(
            matches!(
                out_a,
                Outcome::Send(Message::PairingFailed {
                    reason: PairingFailure::WrongPin
                })
            ),
            "first attempt with wrong PIN must report WrongPin, got {out_a:?}"
        );

        // Connection #2: brand-new ServerHandshake — same shared limiter.
        // The Hello message must NOT advance to a challenge; we must see
        // RateLimited because the prior failure persists across the
        // connection boundary now.
        let mut client_b = ClientHandshake::new(
            dev(1),
            "a".into(),
            "v".into(),
            vid(1),
            Pin::parse("111111").unwrap(),
        );
        let mut server_b = ServerHandshake::new(
            dev(2),
            "b".into(),
            "v".into(),
            vid(2),
            Pin::parse("222222").unwrap(),
        )
        .with_shared_rate_limiter(shared.clone());

        let out_b = server_b.step(client_b.start()).unwrap();
        assert!(
            matches!(
                out_b,
                Outcome::Send(Message::PairingFailed {
                    reason: PairingFailure::RateLimited
                })
            ),
            "second connection on the shared limiter must be rate-limited, got {out_b:?}"
        );
    }

    /// M-11: two connections from DIFFERENT sources share the engine limiter
    /// but must NOT lock each other out. A failed attempt from source A leaves
    /// source B able to pair — the whole point of keying the backoff per
    /// source rather than globally.
    #[test]
    fn shared_rate_limiter_isolates_distinct_sources() {
        let shared = Arc::new(Mutex::new(KeyedPairingRateLimiter::new()));

        // Source A guesses the wrong PIN, accruing a failure on key "ip-a".
        let mut client_a = ClientHandshake::new(
            dev(1),
            "a".into(),
            "v".into(),
            vid(1),
            Pin::parse("111111").unwrap(),
        );
        let mut server_a = ServerHandshake::new(
            dev(2),
            "b".into(),
            "v".into(),
            vid(2),
            Pin::parse("222222").unwrap(),
        )
        .with_shared_rate_limiter(shared.clone())
        .with_rate_limit_key("ip-a".into());
        let Outcome::Send(challenge_a) = server_a.step(client_a.start()).unwrap() else {
            panic!("expected challenge");
        };
        let Outcome::Send(proof_a) = client_a.step(challenge_a).unwrap() else {
            panic!("expected proof");
        };
        let _ = server_a.step(proof_a).unwrap(); // WrongPin, accrues on "ip-a"

        // Source B (different key) on the SAME shared limiter must still get a
        // challenge, not RateLimited.
        let mut client_b = ClientHandshake::new(
            dev(3),
            "c".into(),
            "v".into(),
            vid(3),
            Pin::parse("222222").unwrap(),
        );
        let mut server_b = ServerHandshake::new(
            dev(4),
            "d".into(),
            "v".into(),
            vid(4),
            Pin::parse("222222").unwrap(),
        )
        .with_shared_rate_limiter(shared.clone())
        .with_rate_limit_key("ip-b".into());
        let out_b = server_b.step(client_b.start()).unwrap();
        assert!(
            matches!(out_b, Outcome::Send(Message::PairingChallenge { .. })),
            "a failure from another source must not rate-limit a distinct source, got {out_b:?}"
        );
    }

    /// SRX layer 3 headline: an attacker with the PUBLIC NodeId and even the
    /// CORRECT 6-digit PIN, but who never obtained the ticket (so has no secret),
    /// CANNOT complete a first pair against a host that minted one. Brute-forcing
    /// the PIN is useless without the 128-bit ticket secret.
    #[test]
    fn first_pair_without_ticket_secret_fails_against_secret_host() {
        let secret = PairingSecret::from_bytes([0x44; 32]);
        // Attacker: correct PIN, but NO ticket secret.
        let mut attacker = ClientHandshake::new(dev(1), "a".into(), "roost".into(), vid(1), pin());
        let mut server = ServerHandshake::new(dev(2), "p".into(), "roost".into(), vid(2), pin())
            .with_pairing_secret(secret);
        let Outcome::Send(challenge) = server.step(attacker.start()).unwrap() else {
            panic!("expected challenge");
        };
        let Outcome::Send(proof) = attacker.step(challenge).unwrap() else {
            panic!("expected proof");
        };
        match server.step(proof).unwrap() {
            Outcome::Send(Message::PairingFailed { reason }) => assert!(
                matches!(reason, PairingFailure::WrongPin),
                "a first pair with the right PIN but no ticket secret must fail, got {reason:?}"
            ),
            other => panic!("expected PairingFailed, got {other:?}"),
        }
    }

    /// SRX layer 3: the legitimate joiner who scanned the ticket presents the
    /// matching secret (and PIN) and completes the pair.
    #[test]
    fn first_pair_with_matching_ticket_secret_succeeds() {
        let secret = PairingSecret::from_bytes([0x33; 32]);
        let mut client = ClientHandshake::new(dev(1), "l".into(), "roost".into(), vid(1), pin())
            .with_pairing_secret(secret.clone());
        let mut server = ServerHandshake::new(dev(2), "p".into(), "roost".into(), vid(2), pin())
            .with_pairing_secret(secret);
        let Outcome::Send(challenge) = server.step(client.start()).unwrap() else {
            panic!("expected challenge");
        };
        let Outcome::Send(proof) = client.step(challenge).unwrap() else {
            panic!("expected proof");
        };
        assert!(
            matches!(server.step(proof).unwrap(), Outcome::Done { .. }),
            "a matching ticket secret + PIN must complete the first pair"
        );
    }

    /// SRX layer 3: a joiner presenting the WRONG secret (e.g. from a different
    /// or stale ticket) is rejected even with the right PIN.
    #[test]
    fn first_pair_with_wrong_ticket_secret_fails() {
        let mut client = ClientHandshake::new(dev(1), "a".into(), "roost".into(), vid(1), pin())
            .with_pairing_secret(PairingSecret::from_bytes([0xAA; 32]));
        let mut server = ServerHandshake::new(dev(2), "p".into(), "roost".into(), vid(2), pin())
            .with_pairing_secret(PairingSecret::from_bytes([0xBB; 32]));
        let Outcome::Send(challenge) = server.step(client.start()).unwrap() else {
            panic!("expected challenge");
        };
        let Outcome::Send(proof) = client.step(challenge).unwrap() else {
            panic!("expected proof");
        };
        assert!(
            matches!(
                server.step(proof).unwrap(),
                Outcome::Send(Message::PairingFailed {
                    reason: PairingFailure::WrongPin
                })
            ),
            "a mismatched ticket secret must fail the proof"
        );
    }

    /// SRX layer 3: PIN-only first pair still works when NEITHER side has a
    /// secret (the no-ticket fallback / pre-layer-3 interop). Guards against the
    /// secret binding accidentally becoming mandatory on the fallback path.
    #[test]
    fn first_pair_pin_only_still_works_without_secrets() {
        let mut client = ClientHandshake::new(dev(1), "l".into(), "roost".into(), vid(1), pin());
        let mut server = ServerHandshake::new(dev(2), "p".into(), "roost".into(), vid(2), pin());
        let Outcome::Send(challenge) = server.step(client.start()).unwrap() else {
            panic!("expected challenge");
        };
        let Outcome::Send(proof) = client.step(challenge).unwrap() else {
            panic!("expected proof");
        };
        assert!(
            matches!(server.step(proof).unwrap(), Outcome::Done { .. }),
            "PIN-only first pair (no secret on either side) must still succeed"
        );
    }

    /// SRX brute-force fix: with an enrollment window injected but CLOSED (no
    /// active pairing), an inbound FIRST-PAIR Hello is refused with
    /// `PairingClosed` before any challenge is issued — the attacker never even
    /// gets to guess a PIN. This is the property that shrinks the first-pair
    /// attack surface from "whenever sync is on" to "while the user is pairing".
    #[test]
    fn first_pair_rejected_when_enrollment_window_closed() {
        use crate::sync::crypto::pairing::EnrollmentWindow;
        let window = Arc::new(Mutex::new(EnrollmentWindow::new())); // closed by default
        let mut client = ClientHandshake::new(dev(1), "a".into(), "v".into(), vid(1), pin());
        let mut server = ServerHandshake::new(dev(2), "b".into(), "v".into(), vid(2), pin())
            .with_enrollment_window(window);
        let out = server.step(client.start()).unwrap();
        match out {
            Outcome::Send(Message::PairingFailed { reason }) => assert!(
                matches!(reason, PairingFailure::PairingClosed),
                "a closed window must refuse first-pair with PairingClosed, got {reason:?}"
            ),
            other => panic!("expected PairingFailed::PairingClosed, got {other:?}"),
        }
    }

    /// SRX: with the window OPEN, a first-pair Hello proceeds to a challenge as
    /// normal — the gate doesn't break legitimate pairing.
    #[test]
    fn first_pair_proceeds_when_enrollment_window_open() {
        use crate::sync::crypto::pairing::{EnrollmentWindow, ENROLLMENT_WINDOW};
        let mut w = EnrollmentWindow::new();
        w.open(ENROLLMENT_WINDOW);
        let window = Arc::new(Mutex::new(w));
        let mut client = ClientHandshake::new(dev(1), "a".into(), "v".into(), vid(1), pin());
        let mut server = ServerHandshake::new(dev(2), "b".into(), "v".into(), vid(2), pin())
            .with_enrollment_window(window);
        assert!(
            matches!(
                server.step(client.start()).unwrap(),
                Outcome::Send(Message::PairingChallenge { .. })
            ),
            "an open window must let a legitimate first-pair reach the challenge"
        );
    }

    /// SRX: a RECONNECT (we hold a resumption token for this device) is NEVER
    /// gated by the enrollment window — even with the window closed, a known
    /// peer authenticates by token and reaches a challenge. This proves the gate
    /// can't lock out already-paired devices.
    #[test]
    fn reconnect_not_gated_by_enrollment_window() {
        use crate::sync::crypto::pairing::EnrollmentWindow;
        let tok = [0x5a; 32];
        let window = Arc::new(Mutex::new(EnrollmentWindow::new())); // closed
        let mut client = ClientHandshake::new(dev(1), "l".into(), "roost".into(), vid(1), pin())
            .with_resumption_token(tok);
        let mut server = ServerHandshake::new(dev(2), "p".into(), "roost".into(), vid(2), pin())
            .with_enrollment_window(window)
            .with_resumption_tokens(HashMap::from([(dev(1), tok)]));
        assert!(
            matches!(
                server.step(client.start()).unwrap(),
                Outcome::Send(Message::PairingChallenge { .. })
            ),
            "a token reconnect must not be gated by a closed enrollment window"
        );
    }

    /// B4: a device paired before resumption tokens shipped has no token, so it
    /// looks like a first-pair and a closed enrollment window would reject its
    /// reconnect. When the accept path has authenticated the peer's NodeId AND
    /// found it in the store, it marks the device enrollment-exempt — the closed
    /// window is then skipped and the handshake reaches a challenge and (with
    /// matching PINs) completes, letting the peer acquire a token. Proves the
    /// migration path works without the user re-opening pairing.
    #[test]
    fn known_authenticated_device_bypasses_closed_enrollment_window() {
        use crate::sync::crypto::pairing::EnrollmentWindow;
        let window = Arc::new(Mutex::new(EnrollmentWindow::new())); // closed
        let mut client = ClientHandshake::new(dev(1), "l".into(), "roost".into(), vid(1), pin());
        // No resumption token (pre-SEC-2 peer); exempt set to the client's device
        // as the accept path would after authenticating its NodeId + store hit.
        let mut server = ServerHandshake::new(dev(2), "p".into(), "roost".into(), vid(2), pin())
            .with_enrollment_window(window)
            .with_enrollment_exempt_device(Some(dev(1)));
        let Outcome::Send(challenge) = server.step(client.start()).unwrap() else {
            panic!("exempt known device must reach a challenge, not PairingClosed");
        };
        assert!(matches!(challenge, Message::PairingChallenge { .. }));
        let Outcome::Send(proof) = client.step(challenge).unwrap() else {
            panic!("expected proof");
        };
        assert!(
            matches!(server.step(proof).unwrap(), Outcome::Done { .. }),
            "an exempt known device with a matching PIN must complete the handshake"
        );
    }

    /// B4 (safety): the exemption must NOT change the stranger path. With no
    /// exempt device set (the default, and what the TLS accept path always
    /// passes), a first-pair against a closed window is still refused.
    #[test]
    fn unknown_device_still_gated_when_enrollment_window_closed() {
        use crate::sync::crypto::pairing::EnrollmentWindow;
        let window = Arc::new(Mutex::new(EnrollmentWindow::new())); // closed
        let mut client = ClientHandshake::new(dev(1), "a".into(), "v".into(), vid(1), pin());
        let mut server = ServerHandshake::new(dev(2), "b".into(), "v".into(), vid(2), pin())
            .with_enrollment_window(window)
            .with_enrollment_exempt_device(None); // no known-device exemption
        match server.step(client.start()).unwrap() {
            Outcome::Send(Message::PairingFailed { reason }) => assert!(
                matches!(reason, PairingFailure::PairingClosed),
                "an unexempted first-pair must still be refused, got {reason:?}"
            ),
            other => panic!("expected PairingClosed, got {other:?}"),
        }
    }

    /// B4 (the critical anti-spoof property): the exemption keys on the
    /// AUTHENTICATED device, matched against the Hello's claimed device_id — so a
    /// connection cannot ride device X's exemption while its Hello claims to be
    /// device Y. Here the exempt device is dev(1) (what the NodeId authenticated
    /// to) but the client's Hello claims dev(2); the mismatch means the gate
    /// still applies. This is the guard that stops a future refactor from
    /// sliding into matching a spoofable, unauthenticated known-id set.
    #[test]
    fn mismatched_hello_device_id_is_not_exempt() {
        use crate::sync::crypto::pairing::EnrollmentWindow;
        let window = Arc::new(Mutex::new(EnrollmentWindow::new())); // closed
                                                                    // Client's Hello claims dev(2)...
        let mut client = ClientHandshake::new(dev(2), "a".into(), "v".into(), vid(2), pin());
        // ...but only dev(1) is exempt (that's who the NodeId would authenticate to).
        let mut server = ServerHandshake::new(dev(3), "b".into(), "v".into(), vid(3), pin())
            .with_enrollment_window(window)
            .with_enrollment_exempt_device(Some(dev(1)));
        match server.step(client.start()).unwrap() {
            Outcome::Send(Message::PairingFailed { reason }) => assert!(
                matches!(reason, PairingFailure::PairingClosed),
                "a Hello claiming a device other than the exempt one must stay gated, got {reason:?}"
            ),
            other => panic!("expected PairingClosed, got {other:?}"),
        }
    }

    /// SRX: fresh-NodeId spraying — the attack the per-key limiter can't stop —
    /// must trip the source-independent global cap. Simulate distinct sources
    /// (distinct rate-limit keys) each making a first-pair attempt against ONE
    /// shared, open enrollment window; after the cap, the next first-pair is
    /// `RateLimited` regardless of its (fresh) source key.
    #[test]
    fn fresh_source_spray_trips_global_enrollment_cap() {
        use crate::sync::crypto::pairing::{
            EnrollmentWindow, ENROLLMENT_WINDOW, GLOBAL_FIRST_PAIR_CAP,
        };
        let mut w = EnrollmentWindow::new();
        w.open(ENROLLMENT_WINDOW);
        let window = Arc::new(Mutex::new(w));
        // Each "spray" uses a brand-new rate-limit key (a fresh NodeId) so the
        // per-key limiter never bites; only the global cap can stop it.
        for i in 0..GLOBAL_FIRST_PAIR_CAP {
            let mut client = ClientHandshake::new(dev(1), "a".into(), "v".into(), vid(1), pin());
            let mut server = ServerHandshake::new(dev(2), "b".into(), "v".into(), vid(2), pin())
                .with_enrollment_window(window.clone())
                .with_rate_limit_key(format!("iroh:fresh-node-{i}"));
            assert!(
                matches!(
                    server.step(client.start()).unwrap(),
                    Outcome::Send(Message::PairingChallenge { .. })
                ),
                "attempt {i} (under the cap) should still reach a challenge"
            );
        }
        // The (cap+1)th fresh source is refused by the GLOBAL cap.
        let mut client = ClientHandshake::new(dev(1), "a".into(), "v".into(), vid(1), pin());
        let mut server = ServerHandshake::new(dev(2), "b".into(), "v".into(), vid(2), pin())
            .with_enrollment_window(window)
            .with_rate_limit_key("iroh:fresh-node-final".into());
        match server.step(client.start()).unwrap() {
            Outcome::Send(Message::PairingFailed { reason }) => assert!(
                matches!(reason, PairingFailure::RateLimited),
                "fresh-NodeId spray past the global cap must be RateLimited, got {reason:?}"
            ),
            other => panic!("expected PairingFailed::RateLimited, got {other:?}"),
        }
    }

    /// AUDIT-077: companion to the test above — a freshly-defaulted
    /// (non-shared) limiter on each handshake does NOT inherit the prior
    /// counter. This is the pre-fix behavior we're guarding against
    /// regressing back into. It's a sanity check that the test scaffold
    /// is testing what we think it is.
    #[test]
    fn fresh_per_handshake_limiter_does_not_carry_state() {
        // No `.with_shared_rate_limiter` → each handshake gets its own.
        let mut client_a = ClientHandshake::new(
            dev(1),
            "a".into(),
            "v".into(),
            vid(1),
            Pin::parse("111111").unwrap(),
        );
        let mut server_a = ServerHandshake::new(
            dev(2),
            "b".into(),
            "v".into(),
            vid(2),
            Pin::parse("222222").unwrap(),
        );

        let Outcome::Send(challenge_a) = server_a.step(client_a.start()).unwrap() else {
            panic!()
        };
        let Outcome::Send(proof_a) = client_a.step(challenge_a).unwrap() else {
            panic!()
        };
        let _ = server_a.step(proof_a).unwrap(); // fails: wrong PIN

        // Second handshake — new limiter, gets through to challenge.
        let mut client_b = ClientHandshake::new(
            dev(1),
            "a".into(),
            "v".into(),
            vid(1),
            Pin::parse("111111").unwrap(),
        );
        let mut server_b = ServerHandshake::new(
            dev(2),
            "b".into(),
            "v".into(),
            vid(2),
            Pin::parse("222222").unwrap(),
        );
        let out_b = server_b.step(client_b.start()).unwrap();
        assert!(
            matches!(out_b, Outcome::Send(Message::PairingChallenge { .. })),
            "without a shared limiter, fresh handshakes have a clean counter (this is the \
             pre-AUDIT-077 behavior — guarded so we notice if it stops being true)"
        );
    }
}
