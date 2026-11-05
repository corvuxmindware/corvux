// peer_node_id retained for diagnostics + future routing
#![allow(dead_code)]

//! Iroh QUIC transport implementing the [`Transport`] trait.
//!
//! Replaces the TCP+TLS path for cross-network sync. Iroh handles three
//! things the legacy `tcp_tls` path could not:
//!
//! - **LAN discovery** — built-in mDNS-style discovery for same-Wi-Fi peers.
//! - **NAT hole-punching** — when peers are on different networks, Iroh
//!   probes for direct UDP paths via STUN-like negotiation.
//! - **Relay fallback** — when hole-punching fails (~20% of NATs), traffic
//!   transparently falls back to a relay. Beta uses Number0's public
//!   relays (default); production swaps to self-hosted EC2 relays via
//!   [`iroh::RelayMode::Custom`].
//!
//! # Identity reuse
//!
//! Iroh's `NodeId` is an Ed25519 public key — exactly the same primitive
//! the rest of Corvux uses for [`DeviceId`]. We seed Iroh's `SecretKey`
//! from the existing [`Identity`] keypair, so there's only one identity
//! to persist and one keypair to protect. `DeviceId` and `NodeId` are
//! mathematically derivable from each other; users see one canonical
//! identity per device.
//!
//! # Authentication
//!
//! Iroh's QUIC handshake authenticates the remote `NodeId` via the
//! Ed25519 signature embedded in the TLS 1.3 cert. Spoofing the NodeId
//! requires the private key. The TOFU-cert-pinning we did with
//! `tcp_tls.rs` becomes redundant: the user pairs against a NodeId
//! (32-byte public key) which IS the identity. We still run the
//! HMAC-PIN proof inside the connection as defence-in-depth and to
//! prove the human-on-the-other-end intent.
//!
//! # Framing
//!
//! Re-uses [`super::frame::write_frame`] / [`read_frame`] verbatim. Iroh
//! `SendStream` / `RecvStream` implement `tokio::io::AsyncWrite` /
//! `AsyncRead`, so the existing length-prefixed framing works without
//! modification.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use async_trait::async_trait;
use iroh::endpoint::{Connection, RecvStream, SendStream};
use iroh::{Endpoint, EndpointAddr as NodeAddr, EndpointId as NodeId, RelayMode, SecretKey};
use tokio::io::AsyncWriteExt;
use zeroize::Zeroize;

use crate::sync::error::TransportError;
use crate::sync::identity::{DeviceId, Identity};
use crate::sync::protocol::{self, Message};

use super::frame::{read_frame, write_frame, PROTOCOL_VERSION};
use super::Transport;

/// ALPN advertised on Iroh QUIC connections.
///
/// Tied to the wire-protocol generation, NOT the per-frame protocol version.
/// Bumping the ALPN forces an immediate hard cut-over (peers with the old
/// ALPN can't connect at all). Per-frame `PROTOCOL_VERSION` handles
/// finer-grained backwards-compatible upgrades inside an established
/// session — use that path unless the wire grammar itself changes.
pub const SYNC_ALPN: &[u8] = b"corvux/sync/1";

/// Build an Iroh `SecretKey` seeded from our existing Ed25519 device key.
///
/// The returned `SecretKey` and our `Identity` produce the same NodeId /
/// DeviceId pair — there's only one keypair on disk. The local `bytes`
/// buffer is zeroed before returning so secret material doesn't outlive
/// the call frame on the stack.
fn iroh_secret_from_identity(id: &Identity) -> SecretKey {
    let mut bytes = id.signing_key_bytes();
    let secret = SecretKey::from_bytes(&bytes);
    bytes.zeroize();
    secret
}

/// Convert an Iroh `NodeId` to our internal `DeviceId`.
///
/// `NodeId` is the raw Ed25519 verifying key; `DeviceId` is the first
/// 16 bytes of `SHA256(verifying_key)`. Same input, fully deterministic
/// — every device gets the same DeviceId regardless of which side of
/// the connection observes it.
pub fn device_id_from_node_id(node_id: NodeId) -> Result<DeviceId, TransportError> {
    use ed25519_dalek::VerifyingKey;
    let bytes: [u8; 32] = *node_id.as_bytes();
    let vk = VerifyingKey::from_bytes(&bytes)
        .map_err(|e| TransportError::Iroh(format!("invalid NodeId pubkey: {e}")))?;
    Ok(DeviceId::from_pubkey(&vk))
}

/// AUDIT-081: enforce that a peer's self-asserted handshake `device_id`
/// matches the DeviceId derived from the iroh `NodeId` that QUIC
/// authenticated.
///
/// The pairing handshake proves the peer knows the PIN and yields a shared
/// session secret, but the `device_id` carried in the Hello is an
/// *unauthenticated label* — nothing in the handshake binds it to a key. On
/// iroh, QUIC independently authenticates the peer's `NodeId` (its Ed25519
/// key), and `DeviceId` is a deterministic digest of that same key. So a
/// peer who knows the PIN could otherwise connect with its real NodeId yet
/// claim *someone else's* device_id, and we would persist its NodeId and
/// per-pair queue key under that spoofed identity — corrupting routing and
/// welding unrelated CRDT history together.
///
/// Returns the authenticated DeviceId on success so callers can use it
/// directly; `Err` means the peer is claiming an identity it has not proven
/// possession of.
pub fn verify_node_device_binding(
    claimed: DeviceId,
    node_id: NodeId,
) -> Result<DeviceId, TransportError> {
    let authenticated = device_id_from_node_id(node_id)?;
    if claimed != authenticated {
        return Err(TransportError::Iroh(format!(
            "device id does not match authenticated node id: handshake claimed {}, NodeId authenticates {}",
            hex::encode(claimed.0),
            hex::encode(authenticated.0),
        )));
    }
    Ok(authenticated)
}

/// One Iroh-backed message channel. Wraps a single QUIC bidirectional
/// stream over an `iroh::Endpoint` connection.
///
/// The `Connection` is held alongside the streams so the QUIC session
/// stays alive — dropping the `Connection` closes every stream on it.
pub struct IrohTransport {
    /// Held to keep the QUIC connection alive while either stream is in
    /// use. Dropped on `close()` or when the transport is dropped.
    connection: Connection,
    send: SendStream,
    recv: RecvStream,
    peer_node_id: NodeId,
}

impl IrohTransport {
    /// NodeId of the peer on the far end of this connection. Authenticated
    /// by Iroh's QUIC handshake — the peer cannot have completed the
    /// handshake without holding the corresponding Ed25519 private key.
    pub fn peer_node_id(&self) -> NodeId {
        self.peer_node_id
    }

    /// Server-side accept: takes an inbound `Connection` and opens the
    /// single bi-stream we use as the message channel.
    ///
    /// Convention: the **dialer** opens the bi-stream; the **listener**
    /// accepts it. This mirrors the `TcpStream::accept` shape and keeps
    /// `peer::run_handshake` direction-symmetric.
    pub async fn accept(connection: Connection) -> Result<Self, TransportError> {
        // iroh 1.x: `remote_id()` returns `EndpointId` directly (no
        // Result) — guaranteed-set post-handshake. `accept_bi` below
        // would have failed on a pre-handshake connection, so by the
        // time we'd use peer_node_id the value is sound.
        let peer_node_id = connection.remote_id();
        let (send, recv) = connection
            .accept_bi()
            .await
            .map_err(|e| TransportError::Iroh(format!("accept_bi: {e}")))?;
        Ok(Self {
            connection,
            send,
            recv,
            peer_node_id,
        })
    }

    /// Client-side dial. Resolves the peer through Iroh's discovery /
    /// hole-punching / relay machinery and opens the bi-stream we'll
    /// use for the rest of the session.
    ///
    /// `peer` carries everything Iroh needs: the `NodeId` (identity) plus
    /// optional direct addresses + relay URL hints. For QR pairing the
    /// receiver typically only knows the NodeId; Iroh's discovery service
    /// resolves the rest.
    pub async fn connect(endpoint: &Endpoint, peer: NodeAddr) -> Result<Self, TransportError> {
        let peer_node_id = peer.id;
        let connection = endpoint
            .connect(peer, SYNC_ALPN)
            .await
            .map_err(|e| TransportError::Iroh(format!("connect: {e}")))?;
        let (send, recv) = connection
            .open_bi()
            .await
            .map_err(|e| TransportError::Iroh(format!("open_bi: {e}")))?;
        Ok(Self {
            connection,
            send,
            recv,
            peer_node_id,
        })
    }
}

#[async_trait]
impl Transport for IrohTransport {
    async fn send(&mut self, msg: &Message) -> Result<(), TransportError> {
        let bytes =
            protocol::encode(msg).map_err(|e| TransportError::Iroh(format!("encode: {e}")))?;
        write_frame(&mut self.send, PROTOCOL_VERSION, &bytes).await
    }

    async fn recv(&mut self) -> Result<Message, TransportError> {
        let (ver, bytes) = read_frame(&mut self.recv).await?;
        if ver != PROTOCOL_VERSION {
            return Err(TransportError::UnsupportedProtocolVersion {
                expected: PROTOCOL_VERSION,
                found: ver,
            });
        }
        protocol::decode(&bytes).map_err(|e| TransportError::Iroh(format!("decode: {e}")))
    }

    async fn close(&mut self) -> Result<(), TransportError> {
        // FIN the send side cleanly so the peer's `recv()` returns EOF,
        // not a reset. If the peer already hung up, ignore the error —
        // we're in a teardown path either way.
        self.send.shutdown().await.ok();
        // Best-effort QUIC close. Code 0 = normal closure; reason is
        // human-readable for debugging from packet captures.
        self.connection.close(0u32.into(), b"corvux: shutting down");
        Ok(())
    }
}

/// Public resolvers iroh falls back to when the system resolver can't answer.
///
/// Some ISPs refuse queries for iroh's discovery host: Reliance Jio's resolver
/// answers REFUSED for `dns.iroh.link` (seen 2026-11-05), so on Jio the N0
/// pkarr publish and DNS address lookup failed on a loop and a device that
/// changed networks could no longer be re-found. The system resolver stays
/// first; hickory moves on to these when it refuses (verified on a Jio
/// connection: system-only fails, system + these resolves). This resolver is
/// only used for iroh's own relay/discovery hostnames, never for note content.
const SYNC_DNS_FALLBACKS: [SocketAddr; 2] = [
    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1)), 53),
    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)), 53),
];

/// The system DNS configuration plus [`SYNC_DNS_FALLBACKS`].
fn sync_dns_resolver() -> iroh::dns::DnsResolver {
    iroh::dns::DnsResolver::builder()
        .with_system_defaults()
        .with_nameservers(SYNC_DNS_FALLBACKS.map(|addr| (addr, iroh::dns::DnsProtocol::Udp)))
        .build()
}

/// Build an Iroh [`Endpoint`] for this device.
///
/// The endpoint is the long-lived object that owns the QUIC socket and
/// the relay subscriptions; one per running `SyncEngine`. Both the
/// listener (accept loop) and outbound dials use the same endpoint.
///
/// `relay_mode` lets the engine pick between:
/// - [`RelayMode::Default`] — use Number0's public relays (beta).
/// - [`RelayMode::Custom`]  — use Corvux-operated relays (post-launch).
/// - [`RelayMode::Disabled`] — LAN-only (test fixtures, no cross-net).
///
/// `discovery_n0` is enabled unconditionally: the host publishes its
/// NodeAddr (relay URL + direct addrs) into Number0's discovery service
/// so a peer holding only a NodeId can still resolve and dial. Without
/// this, `endpoint.connect(NodeAddr::new(node_id), ...)` fails with
/// "no addressing information for NodeId" because the client has nowhere
/// to look up where the host actually lives. The pairing ticket carries
/// addrs as a fast-path; discovery is the fallback when those go stale
/// (device moved networks, IP changed, etc.).
///
/// `lan_lookup` is an additional address-lookup source fed by our own
/// mDNS browser (see `SyncEngine::feed_lan_endpoint`). Passing it to
/// `.address_lookup(..)` below is additive, not a replacement -- it
/// combines with the N0 preset's own DNS-based lookup rather than
/// overriding it, so this only adds a same-Wi-Fi fast path; internet
/// pkarr discovery via Number0 keeps working exactly as before. iroh
/// 1.0.1 has no built-in LAN/mDNS discovery of its own, which is the gap
/// this closes -- without it, a same-Wi-Fi reconnect depends entirely on
/// stale ticket addrs and the internet pkarr record, and times out when
/// either goes stale (peer IP changed, or pkarr has not republished
/// after e.g. iOS backgrounding).
pub async fn build_endpoint(
    identity: &Identity,
    relay_mode: RelayMode,
    lan_lookup: iroh::address_lookup::MemoryLookup,
) -> Result<Endpoint, TransportError> {
    let secret = iroh_secret_from_identity(identity);
    // iroh 1.x: `builder()` takes a preset that bundles discovery +
    // relay + address-lookup defaults. `N0` is the Number0 preset which
    // is the moral equivalent of the previous explicit
    // `.discovery_n0()` call — DNS-based address lookup against
    // Number0's discovery service, plus Number0's public relay map. We
    // override `relay_mode` below and add `lan_lookup` on top; the
    // preset's own discovery is preserved (address_lookup is additive).
    //
    // AUDIT-072: the discovery half publishes the device's NodeId →
    // current address (LAN + relay) into Number0's namespace. Anyone
    // with the NodeId can resolve the device's address; pairing still
    // gates whether they can actually open an authenticated session.
    // Privacy surface + tradeoffs are documented for the website
    // privacy page in `docs/iroh-and-obsidian-sync-explainer.md` §
    // "Discovery via Number0 (privacy disclosure)".
    // QUIC keepalive is DISABLED by default in iroh/quinn, and the default
    // idle timeout is 30s. With no keepalive a connection that goes briefly
    // silent (mobile radio blip, Wi-Fi→cellular handoff, NAT rebind) can
    // idle out needlessly, and a truly dead peer isn't noticed until the
    // 30s idle window elapses. A 10s keepalive holds a healthy connection
    // (and its NAT binding) open across short interruptions, while dead-peer
    // detection stays bounded to the 30s idle timeout. Keepalive must be
    // strictly less than the idle timeout of BOTH peers to be effective, so
    // 10s < 30s holds symmetrically.
    let transport_config = iroh::endpoint::QuicTransportConfig::builder()
        .keep_alive_interval(std::time::Duration::from_secs(10))
        .max_idle_timeout(Some(
            std::time::Duration::from_secs(30)
                .try_into()
                .map_err(|e| TransportError::Iroh(format!("idle timeout: {e}")))?,
        ))
        .build();
    Endpoint::builder(iroh::endpoint::presets::N0)
        .secret_key(secret)
        .alpns(vec![SYNC_ALPN.to_vec()])
        .relay_mode(relay_mode)
        .dns_resolver(sync_dns_resolver())
        .address_lookup(lan_lookup)
        .transport_config(transport_config)
        .bind()
        .await
        .map_err(|e| TransportError::Iroh(format!("bind: {e}")))
}

// ── Pairing ticket ──────────────────────────────────────────────────
//
// What lives on the wire / inside a QR code. Wraps a NodeAddr so the
// joining device gets enough info (node_id + relay URL + LAN addrs) to
// connect immediately, without round-tripping through discovery for the
// first connect.
//
// Format: `corvux1:<base64url-no-pad>(<json>)`.
//
//   - `corvux1:` prefix gives us a clear scheme identifier so a future
//     `corvux2:` can change the inner format without a flag day.
//   - JSON payload is hand-keyed (`nid`/`a`/`r`) for compactness and to
//     decouple from `iroh::NodeAddr`'s internal serde shape, which has
//     historically broken across iroh minor versions.
//   - URL-safe base64 (no padding) keeps the result QR-friendly and
//     copy-pasteable.

/// Wire format of a pairing ticket. Decoupled from `iroh::NodeAddr`'s
/// own serde derive so we don't break tickets when iroh tweaks its
/// internal types between releases.
#[derive(serde::Serialize, serde::Deserialize)]
struct TicketWire {
    /// NodeId as 64-char lowercase hex.
    nid: String,
    /// Direct LAN/WAN addresses the host advertised, formatted as
    /// `ip:port`. Lets the joining device skip discovery on the same
    /// network.
    #[serde(default)]
    a: Vec<String>,
    /// Relay URL (e.g. `https://...`) the host is currently homed on.
    /// `None` if the host bound with `RelayMode::Disabled`.
    #[serde(default)]
    r: Option<String>,
    /// SRX layer 3: base64url of the 32-byte one-time [`PairingSecret`] the host
    /// minted for this pairing session. Carried out of band inside the ticket
    /// (like the PIN, never on the wire); the joiner folds it into the first-pair
    /// key so ticket pairing can't be brute-forced. `None` on legacy tickets and
    /// on non-QR listeners that never minted one. `#[serde(default)]` keeps old
    /// tickets decodable and old decoders tolerant of the new field.
    #[serde(default)]
    s: Option<String>,
}

/// Encode a `NodeAddr` to the pairing-ticket string format, optionally carrying
/// the session's one-time [`PairingSecret`] (SRX layer 3).
pub fn encode_ticket(
    addr: &NodeAddr,
    secret: Option<&crate::sync::crypto::pairing::PairingSecret>,
) -> Result<String, TransportError> {
    use base64::Engine;
    // iroh 1.x EndpointAddr fields/methods used here:
    //   .id          — EndpointId (field)
    //   .ip_addrs()  — iterator of direct SocketAddrs
    //   .relay_urls() — iterator of relay URLs (multi-relay support).
    //     We persist only the first relay in the ticket — adequate for
    //     pairing; the receiver re-discovers others via N0 lookup.
    let wire = TicketWire {
        nid: addr.id.to_string(),
        a: addr.ip_addrs().map(|sa| sa.to_string()).collect(),
        r: addr.relay_urls().next().map(|u| u.to_string()),
        s: secret.map(|sec| sec.to_b64()),
    };
    let json = serde_json::to_string(&wire)
        .map_err(|e| TransportError::Iroh(format!("ticket encode: {e}")))?;
    let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json.as_bytes());
    Ok(format!("corvux1:{encoded}"))
}

/// SRX layer 3: extract just the one-time [`PairingSecret`] from a ticket, if it
/// carries one. Kept separate from [`decode_ticket`] so the addr-decoding
/// callers stay unchanged; the joiner calls both. Returns `None` for a bare
/// NodeId, a legacy ticket without the field, or any malformed secret (which
/// then falls back to the PIN-only path).
pub fn decode_ticket_secret(s: &str) -> Option<crate::sync::crypto::pairing::PairingSecret> {
    use base64::Engine;
    let s = s.trim();
    let body = s.strip_prefix("corvux1:")?;
    let json = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(body.as_bytes())
        .ok()?;
    let wire: TicketWire = serde_json::from_slice(&json).ok()?;
    wire.s
        .as_deref()
        .and_then(crate::sync::crypto::pairing::PairingSecret::from_b64)
}

/// Decode a pairing-ticket string back into a `NodeAddr`. Accepts both
/// the canonical `corvux1:...` form and a bare 64-char NodeId hex (in
/// case a user pastes only the ID — we degrade gracefully and rely on
/// discovery for resolution).
pub fn decode_ticket(s: &str) -> Result<NodeAddr, TransportError> {
    use base64::Engine;
    let s = s.trim();

    // Bare NodeId fallback. Lets a user paste just the 64-char hex from
    // the host card without the ticket envelope; discovery_n0 handles
    // the lookup once they connect.
    if !s.starts_with("corvux1:") {
        if s.len() == 64 && s.chars().all(|c| c.is_ascii_hexdigit()) {
            let node_id: NodeId = s
                .parse()
                .map_err(|e| TransportError::Iroh(format!("bare NodeId: {e}")))?;
            return Ok(NodeAddr::new(node_id));
        }
        return Err(TransportError::Iroh(
            "invalid pairing code (expected `corvux1:...` or 64 hex NodeId)".into(),
        ));
    }

    let body = &s["corvux1:".len()..];
    let json = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(body.as_bytes())
        .map_err(|e| TransportError::Iroh(format!("ticket b64: {e}")))?;
    let wire: TicketWire = serde_json::from_slice(&json)
        .map_err(|e| TransportError::Iroh(format!("ticket json: {e}")))?;

    let node_id: NodeId = wire
        .nid
        .parse()
        .map_err(|e| TransportError::Iroh(format!("ticket node_id: {e}")))?;
    let mut node_addr = NodeAddr::new(node_id);

    if let Some(relay_str) = wire.r {
        let relay_url: iroh::RelayUrl = relay_str
            .parse()
            .map_err(|e| TransportError::Iroh(format!("ticket relay: {e}")))?;
        node_addr = node_addr.with_relay_url(relay_url);
    }

    // iroh 1.x: `with_ip_addr(SocketAddr)` adds one direct address at a
    // time (no bulk `with_direct_addresses` setter) -- chain it per address.
    let addrs: Vec<std::net::SocketAddr> =
        wire.a.into_iter().filter_map(|s| s.parse().ok()).collect();
    for ip in addrs {
        node_addr = node_addr.with_ip_addr(ip);
    }

    Ok(node_addr)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A NodeId derived from a known Ed25519 keypair must yield the same
    /// DeviceId we'd derive directly via `Identity`.
    #[test]
    fn node_id_to_device_id_matches_identity_derivation() {
        use ed25519_dalek::SigningKey;
        let sk_bytes = [7u8; 32];
        let sk = SigningKey::from_bytes(&sk_bytes);
        let vk = sk.verifying_key();

        let our_device_id = DeviceId::from_pubkey(&vk);

        let iroh_secret = SecretKey::from_bytes(&sk_bytes);
        let node_id = iroh_secret.public();
        let derived = device_id_from_node_id(node_id).unwrap();

        assert_eq!(our_device_id, derived);
    }

    /// Helper: the NodeId for a fixed-seed Ed25519 key.
    fn node_id_for(seed: u8) -> NodeId {
        SecretKey::from_bytes(&[seed; 32]).public()
    }

    /// SRX layer 3: a ticket carrying a pairing secret must round-trip both the
    /// NodeAddr AND the secret, so the joiner can fold the secret into the proof.
    #[test]
    fn ticket_round_trips_pairing_secret() {
        use crate::sync::crypto::pairing::PairingSecret;
        let node_id = node_id_for(5);
        let addr = NodeAddr::new(node_id);
        let secret = PairingSecret::from_bytes([0x7c; 32]);
        let ticket = encode_ticket(&addr, Some(&secret)).unwrap();

        // The addr still decodes (existing callers unaffected).
        assert_eq!(decode_ticket(&ticket).unwrap().id, node_id);
        // And the secret comes back intact.
        let got = decode_ticket_secret(&ticket).expect("secret must round-trip");
        assert_eq!(got.as_bytes(), secret.as_bytes());
    }

    /// SRX layer 3: a ticket WITHOUT a secret (legacy / non-QR listener), and a
    /// bare-NodeId paste, both yield `None` — the PIN-only fallback path.
    #[test]
    fn ticket_without_secret_yields_none() {
        let node_id = node_id_for(6);
        let ticket = encode_ticket(&NodeAddr::new(node_id), None).unwrap();
        assert!(decode_ticket_secret(&ticket).is_none());
        // A bare 64-hex NodeId paste has no envelope, so no secret.
        assert!(decode_ticket_secret(&node_id.to_string()).is_none());
    }

    /// SRX layer 3: a NEW decoder must still read an OLD ticket that predates the
    /// secret field (forward/backward compatibility via `#[serde(default)]`).
    #[test]
    fn legacy_ticket_without_secret_field_still_decodes() {
        use base64::Engine;
        // Hand-craft a pre-layer-3 ticket JSON with no `s` field.
        let node_id = node_id_for(7);
        let json = format!(r#"{{"nid":"{node_id}","a":[],"r":null}}"#);
        let body = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json.as_bytes());
        let ticket = format!("corvux1:{body}");
        assert_eq!(decode_ticket(&ticket).unwrap().id, node_id);
        assert!(decode_ticket_secret(&ticket).is_none());
    }

    /// AUDIT-081: when the peer's claimed device_id is the one its NodeId
    /// authenticates, the binding check passes and returns that DeviceId.
    #[test]
    fn node_device_binding_accepts_matching_claim() {
        let node_id = node_id_for(11);
        let honest = device_id_from_node_id(node_id).unwrap();

        let out = verify_node_device_binding(honest, node_id).unwrap();
        assert_eq!(out, honest);
    }

    /// AUDIT-081: a peer that completes the QUIC handshake with one key but
    /// claims a *different* device_id in the Hello must be rejected — this is
    /// the spoof the weld exists to stop. Without it, the impostor's NodeId
    /// and per-pair queue key would be persisted under the victim's id.
    #[test]
    fn node_device_binding_rejects_spoofed_claim() {
        let attacker_node = node_id_for(11);
        // A victim's id derived from a *different* key — what the attacker
        // wants to impersonate.
        let victim_id = device_id_from_node_id(node_id_for(22)).unwrap();
        // Sanity: distinct keys really do yield distinct device ids, so the
        // comparison is meaningful (no truncation collision).
        assert_ne!(
            device_id_from_node_id(attacker_node).unwrap(),
            victim_id,
            "distinct NodeIds must map to distinct DeviceIds"
        );

        let result = verify_node_device_binding(victim_id, attacker_node);
        assert!(
            result.is_err(),
            "claiming a device_id not backed by the authenticated NodeId must be rejected"
        );
    }
}
