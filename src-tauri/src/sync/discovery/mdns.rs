//! LAN peer discovery via mDNS / DNS-SD.
//!
//! Corvux advertises this device as `_corvux-sync._tcp.local.` on the
//! local network and simultaneously browses for others. The device ID
//! and roost name ride along in TXT records so the UI can show
//! "Raj's MacBook — roost MyBrain" without making the user type an IP.
//!
//! # Scope
//!
//! - Desktop: works.
//! - Android: works. Requires a `WifiManager.MulticastLock` (else Android
//!   filters incoming multicast and the browser sees zero peers); that lock
//!   ships via `tauri-plugin-multicast-lock` and is acquired for the whole
//!   sync session in `commands::sync` (landed 2026-06-08). Both announce and
//!   browse run under it.
//! - iOS: the `Info.plist` declares `NSBonjourServices` (`_corvux-sync._tcp`)
//!   plus `NSLocalNetworkUsageDescription`, and no multicast lock is needed.
//!   BUT whether `mdns-sd` actually announces/browses under the iOS sandbox
//!   once the user grants Local Network permission is UNVERIFIED on-device.
//!   Until confirmed on a real iPhone, do not assume same-Wi-Fi LAN discovery
//!   (advertise or browse) functions on iOS — it may silently no-op, and the
//!   iroh dialer then falls back to ticket addrs / internet pkarr.
//! - The browser intentionally filters out *our own* service registration
//!   by device ID. Otherwise the UI would suggest pairing with ourselves.
//! - Roost-name mismatch is enforced at the handshake layer (not here).
//!   We still surface mismatched peers in the discovered list so the UI
//!   can render them greyed-out with a clear reason.
//!
//! # TXT record shape
//!
//! We pack up to six ASCII fields; mdns-sd serializes them as `key=value`
//! segments inside a single TXT record:
//!
//!   device_id    = 32 hex chars  (16-byte DeviceId)
//!   display_name = short UTF-8 (truncated to 63 bytes)
//!   roost_name   = short UTF-8
//!   port         = decimal u16 for the sync listener
//!   node_id      = iroh EndpointId (NodeId), hex -- OMITTED when the
//!                  iroh listener has not bound yet at announce time
//!   iroh_addrs   = comma-joined `ip:port` direct UDP addresses of the
//!                  iroh endpoint -- OMITTED alongside `node_id`
//!
//! `node_id` / `iroh_addrs` exist so a LAN peer's discovery result is
//! directly usable by iroh's dialer (`SyncEngine::feed_lan_endpoint`):
//! iroh 1.0.1 has no built-in LAN/mDNS discovery of its own, so without
//! these two fields, same-Wi-Fi reconnect falls back entirely to stale
//! ticket addresses and internet pkarr discovery. Older peers (or a peer
//! still starting up its iroh listener) simply omit both keys --
//! `parse_peer` tolerates that as "no LAN-fed iroh hint", not a parse
//! error.
//!
//! Anything we can't parse is dropped; a malformed peer can't crash us.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use mdns_sd::{ResolvedService, ServiceDaemon, ServiceEvent, ServiceInfo, TxtProperties};
use tokio::sync::{mpsc, Mutex};
use tracing::{debug, info, warn};

use crate::sync::error::{EngineError, SyncError, SyncResult};
use crate::sync::identity::DeviceId;

/// DNS-SD service type registered on the LAN. Trailing dot is required by
/// the spec so mdns-sd treats it as fully-qualified.
pub const SERVICE_TYPE: &str = "_corvux-sync._tcp.local.";

/// A peer the browser has observed on the LAN. Emitted to the frontend so
/// users can tap-to-pair instead of typing IP addresses.
#[derive(Debug, Clone)]
pub struct DiscoveredPeer {
    pub device_id: DeviceId,
    pub display_name: String,
    pub roost_name: String,
    pub addr: String,
    pub port: u16,
    /// iroh EndpointId (NodeId) the peer advertised, if its iroh listener
    /// was bound at announce time. `None` for an older Corvux build or a
    /// peer whose iroh endpoint has not bound yet this session -- the LAN
    /// address feed (`SyncEngine::feed_lan_endpoint`) is just skipped for
    /// that peer; it still falls back to ticket addrs / pkarr as before.
    pub node_id: Option<String>,
    /// Direct UDP addresses the peer's iroh endpoint is reachable on.
    /// Empty when `node_id` is `None`. Fed into
    /// `SyncEngine::feed_lan_endpoint` so the iroh dialer can resolve this
    /// peer on the LAN without depending on stale ticket addrs or internet
    /// pkarr discovery.
    pub iroh_addrs: Vec<SocketAddr>,
}

/// What the browser emits. `Lost` fires when a peer stops responding to
/// queries (e.g. their Corvux quit or WiFi dropped).
#[derive(Debug, Clone)]
pub enum DiscoveryEvent {
    Found(DiscoveredPeer),
    Lost { device_id: DeviceId },
}

/// Handle on a running discovery subsystem (announce + browse). Drop to
/// stop both. The handle is cheap to clone because internal state lives
/// behind `Arc`s.
#[derive(Clone)]
pub struct Discovery {
    inner: Arc<DiscoveryInner>,
}

struct DiscoveryInner {
    daemon: ServiceDaemon,
    /// Our own device ID so the browser can skip our own advertisements.
    our_device_id: DeviceId,
    /// Cache of peers we've seen, keyed by device ID. Keeps `Lost` events
    /// from firing when mDNS briefly republishes the same service.
    seen: Mutex<HashMap<DeviceId, DiscoveredPeer>>,
    /// Full service name we registered; needed to unregister cleanly on
    /// shutdown. None if we never announced.
    registered_full_name: Mutex<Option<String>>,
}

impl Discovery {
    /// Start the daemon. No announcements or browse are active until you
    /// call [`Self::announce`] or [`Self::browse`] explicitly.
    pub fn start(our_device_id: DeviceId) -> SyncResult<Self> {
        let daemon = ServiceDaemon::new()
            .map_err(|e| SyncError::Engine(EngineError::Discovery(format!("mdns daemon: {e}"))))?;
        Ok(Self {
            inner: Arc::new(DiscoveryInner {
                daemon,
                our_device_id,
                seen: Mutex::new(HashMap::new()),
                registered_full_name: Mutex::new(None),
            }),
        })
    }

    /// Publish this device on the LAN. `port` is the sync listener port
    /// (from `SyncEngine::start_listening`). `node_id` is this device's
    /// iroh EndpointId as a string, and `iroh_addrs` its current direct
    /// UDP addresses -- pass an empty string / empty slice when the iroh
    /// listener has not bound yet, and both TXT keys are omitted rather
    /// than sent empty (see [`build_txt_properties`]). Safe to call once
    /// per engine session; calling again replaces the prior registration.
    pub async fn announce(
        &self,
        display_name: &str,
        roost_name: &str,
        port: u16,
        node_id: &str,
        iroh_addrs: &[SocketAddr],
    ) -> SyncResult<()> {
        // Hostname has to be unique on the LAN; device ID hex is plenty.
        let hex = self.inner.our_device_id.as_hex();
        let instance = format!("corvux-{}", &hex[..16]);
        let host = format!("{instance}.local.");
        let txt = build_txt_properties(
            self.inner.our_device_id,
            display_name,
            roost_name,
            port,
            node_id,
            iroh_addrs,
        );

        // We pass no explicit IPs; mdns-sd picks the device's primary
        // v4 + v6 addresses itself via `if-addrs`. That's what we want --
        // both sides of the LAN see us on every reachable interface.
        let info = ServiceInfo::new(SERVICE_TYPE, &instance, &host, "", port, Some(txt))
            .map_err(|e| SyncError::Engine(EngineError::Discovery(format!("mdns info: {e}"))))?
            .enable_addr_auto();

        // Stash the full name before registering so the Unregister path
        // has it even if the caller drops the handle racily.
        let full_name = info.get_fullname().to_string();
        *self.inner.registered_full_name.lock().await = Some(full_name);
        self.inner.daemon.register(info).map_err(|e| {
            SyncError::Engine(EngineError::Discovery(format!("mdns register: {e}")))
        })?;
        info!(%instance, port, "mdns: announced");
        Ok(())
    }

    /// Unregister our advertisement. Called on `stop()`. Safe to call
    /// multiple times; later calls are no-ops.
    pub async fn unannounce(&self) {
        let mut guard = self.inner.registered_full_name.lock().await;
        if let Some(full) = guard.take() {
            if let Err(e) = self.inner.daemon.unregister(&full) {
                warn!(error = %e, "mdns: unregister failed");
            } else {
                debug!(%full, "mdns: unregistered");
            }
        }
    }

    /// Start a browser and forward `DiscoveryEvent`s on `out`. The returned
    /// task handle runs until the daemon is shut down; callers normally
    /// keep it alive inside a `JoinHandle` field on the engine.
    pub fn browse(&self, out: mpsc::UnboundedSender<DiscoveryEvent>) -> SyncResult<()> {
        let receiver =
            self.inner.daemon.browse(SERVICE_TYPE).map_err(|e| {
                SyncError::Engine(EngineError::Discovery(format!("mdns browse: {e}")))
            })?;
        let inner = self.inner.clone();
        tokio::spawn(async move {
            while let Ok(event) = receiver.recv_async().await {
                match event {
                    ServiceEvent::ServiceResolved(info) => {
                        match parse_peer(&info, inner.our_device_id) {
                            Ok(Some(peer)) => {
                                let mut seen = inner.seen.lock().await;
                                seen.insert(peer.device_id, peer.clone());
                                drop(seen);
                                if out.send(DiscoveryEvent::Found(peer)).is_err() {
                                    return;
                                }
                            }
                            Ok(None) => {
                                // Our own advertisement — skip silently.
                            }
                            Err(reason) => {
                                debug!(reason, "mdns: ignoring malformed peer");
                            }
                        }
                    }
                    ServiceEvent::ServiceRemoved(_, full_name) => {
                        // mdns-sd gives us the full service name at removal
                        // time; recover the peer via a lookup in the seen
                        // cache. If we don't have it cached, nothing to
                        // emit.
                        let mut seen = inner.seen.lock().await;
                        let removed_id = seen
                            .iter()
                            .find(|(_, p)| full_name_matches(p, &full_name))
                            .map(|(id, _)| *id);
                        if let Some(id) = removed_id {
                            seen.remove(&id);
                            drop(seen);
                            if out.send(DiscoveryEvent::Lost { device_id: id }).is_err() {
                                return;
                            }
                        }
                    }
                    _ => {
                        // Searcher started, search ended, etc. — not interesting.
                    }
                }
            }
        });
        Ok(())
    }

    /// Shut down the daemon entirely. After this the handle is dead; make
    /// a new `Discovery` to resume.
    pub async fn shutdown(&self) {
        self.unannounce().await;
        if let Err(e) = self.inner.daemon.shutdown() {
            warn!(error = %e, "mdns: shutdown signal failed");
        }
    }
}

/// Build the TXT record properties for [`Discovery::announce`]. Split out
/// (free function, no `&self`) so a unit test can drive the exact same
/// encoding `announce` uses -- via `ServiceInfo::new(..).get_properties()`
/// -- without spinning up a real mdns daemon.
///
/// `node_id` empty is the "iroh listener not bound yet" case: both
/// `node_id` and `iroh_addrs` TXT keys are omitted entirely (not sent
/// empty), so `parse_peer` on the receiving end sees exactly what an
/// older, pre-iroh-LAN-lookup peer would have sent.
fn build_txt_properties(
    our_device_id: DeviceId,
    display_name: &str,
    roost_name: &str,
    port: u16,
    node_id: &str,
    iroh_addrs: &[SocketAddr],
) -> HashMap<String, String> {
    let mut txt: HashMap<String, String> = HashMap::new();
    txt.insert("device_id".into(), our_device_id.as_hex());
    // Truncate to stay inside the 63-byte DNS label cap comfortably.
    txt.insert("display_name".into(), truncate_bytes(display_name, 48));
    txt.insert("roost_name".into(), truncate_bytes(roost_name, 48));
    txt.insert("port".into(), port.to_string());
    if !node_id.is_empty() {
        txt.insert("node_id".into(), node_id.to_string());
        let addrs_str = iroh_addrs
            .iter()
            .map(|a| a.to_string())
            .collect::<Vec<_>>()
            .join(",");
        txt.insert("iroh_addrs".into(), addrs_str);
    }
    txt
}

/// Pull a [`DiscoveredPeer`] out of a resolved mDNS record, or explain
/// why it should be ignored. Returns `Ok(None)` for *our own* advertisement
/// (legitimate skip), `Err(reason)` for malformed peers.
//
// mdns-sd 0.19 splits the resolved-side type from the registration-side
// `ServiceInfo`: browse events now carry `ResolvedService`, which is a
// non-exhaustive struct with the same getter surface (`get_properties`,
// `get_port`, `get_addresses`) plus a typed `get_addresses_v4` we use
// below to avoid the prior `.is_ipv4()` check on the enum-shaped `ScopedIp`.
fn parse_peer(
    info: &ResolvedService,
    our_id: DeviceId,
) -> Result<Option<DiscoveredPeer>, &'static str> {
    // Prefer an IPv4 address for broadest compatibility. Clients can dial
    // either form, but the UI displays this string directly so IPv6's
    // `[…]:port` form is a papercut we'd rather skip in MVP.
    let addr = info
        .get_addresses_v4()
        .iter()
        .next()
        .map(|a| a.to_string())
        .or_else(|| info.get_addresses().iter().next().map(|a| a.to_string()))
        .ok_or("no addresses")?;
    parse_peer_props(info.get_properties(), our_id, addr, info.get_port())
}

/// The TXT-decoding half of [`parse_peer`], split out so it is unit
/// testable against a hand-built [`TxtProperties`] -- `ResolvedService` is
/// `#[non_exhaustive]` and can't be constructed outside `mdns-sd`, but
/// `ServiceInfo::new(..).get_properties()` (the exact encode path
/// `Discovery::announce` uses) gives us a real `TxtProperties` to decode.
fn parse_peer_props(
    props: &TxtProperties,
    our_id: DeviceId,
    addr: String,
    port: u16,
) -> Result<Option<DiscoveredPeer>, &'static str> {
    let device_id_hex = props
        .get_property_val_str("device_id")
        .ok_or("missing device_id")?;
    let device_id = parse_device_id(device_id_hex).ok_or("bad device_id hex")?;
    if device_id == our_id {
        return Ok(None);
    }
    let display_name = props
        .get_property_val_str("display_name")
        .unwrap_or("Unknown")
        .to_string();
    let roost_name = props
        .get_property_val_str("roost_name")
        .unwrap_or("")
        .to_string();
    let port = props
        .get_property_val_str("port")
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(port);
    // Tolerate a missing (or empty) `node_id`: an older Corvux build, or a
    // peer whose iroh listener had not bound yet at announce time, simply
    // has no LAN-fed iroh hint. That's not malformed -- the peer still
    // surfaces via mDNS for manual/ticket-based pairing, it just does not
    // get the LAN address-lookup fast path.
    let node_id = props
        .get_property_val_str("node_id")
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    let iroh_addrs = props
        .get_property_val_str("iroh_addrs")
        .unwrap_or("")
        .split(',')
        .filter_map(|s| s.parse::<SocketAddr>().ok())
        // This is a LAN discovery source: only accept private / link-local
        // addresses. A public/routable address advertised over local mDNS is
        // suspicious (a rogue advertiser trying to redirect our dial off-LAN)
        // and never useful for same-Wi-Fi resolution -- drop it.
        .filter(|sa| is_lan_addr(&sa.ip()))
        .collect();
    Ok(Some(DiscoveredPeer {
        device_id,
        display_name,
        roost_name,
        addr,
        port,
        node_id,
        iroh_addrs,
    }))
}

/// True for addresses that belong on a local network: RFC1918 private +
/// link-local IPv4, and unique-local (`fc00::/7`) + link-local (`fe80::/10`)
/// IPv6. Loopback and public/routable addresses are excluded -- an mDNS
/// discovery source should only ever surface same-LAN reachable addresses,
/// so anything else is dropped as untrustworthy noise. Uses only stable std
/// APIs (the IPv6 `is_unique_local`/`is_unicast_link_local` helpers are still
/// nightly-only), so the two IPv6 ranges are checked by hand.
fn is_lan_addr(ip: &std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => v4.is_private() || v4.is_link_local(),
        std::net::IpAddr::V6(v6) => {
            let seg0 = v6.segments()[0];
            let oct0 = v6.octets()[0];
            // fe80::/10 link-local  OR  fc00::/7 unique-local
            (seg0 & 0xffc0) == 0xfe80 || (oct0 & 0xfe) == 0xfc
        }
    }
}

fn parse_device_id(hex_str: &str) -> Option<DeviceId> {
    if hex_str.len() != 32 {
        return None;
    }
    let bytes = hex::decode(hex_str).ok()?;
    let mut arr = [0u8; 16];
    if bytes.len() != 16 {
        return None;
    }
    arr.copy_from_slice(&bytes);
    Some(DeviceId(arr))
}

/// Byte-safe string truncation that won't split a UTF-8 codepoint.
fn truncate_bytes(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

/// Best-effort match of a removed service's fully-qualified name against
/// a cached peer. We stored the device-id as part of the instance name at
/// announce time, so substring match is safe here.
fn full_name_matches(peer: &DiscoveredPeer, full_name: &str) -> bool {
    let marker = format!("corvux-{}", &peer.device_id.as_hex()[..16]);
    full_name.contains(&marker)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_bytes_is_codepoint_safe() {
        // "abc" ≤ 10 bytes → unchanged.
        assert_eq!(truncate_bytes("abc", 10), "abc");
        // Multi-byte char must not split. "é" is 2 bytes; truncating at 1
        // byte should back up to empty.
        assert_eq!(truncate_bytes("é", 1), "");
        // Truncating at exactly 2 is fine.
        assert_eq!(truncate_bytes("é", 2), "é");
    }

    #[test]
    fn parse_device_id_roundtrip() {
        let id = DeviceId([0xab; 16]);
        let parsed = parse_device_id(&id.as_hex()).unwrap();
        assert_eq!(parsed, id);
    }

    #[test]
    fn parse_device_id_rejects_bad_length() {
        assert!(parse_device_id("too short").is_none());
        assert!(parse_device_id(&"a".repeat(31)).is_none());
    }

    /// LAN reconnect fix: the exact TXT encoding `announce` produces --
    /// via `build_txt_properties` + a real `ServiceInfo` -- must decode
    /// back into a `DiscoveredPeer` carrying the iroh node_id + direct
    /// addrs, so `spawn_discovery_bridge` can feed
    /// `SyncEngine::feed_lan_endpoint`.
    #[test]
    fn txt_round_trip_includes_node_id_and_iroh_addrs() {
        let our_id = DeviceId([0x11; 16]);
        let peer_id = DeviceId([0x22; 16]);
        let addrs: Vec<SocketAddr> = vec![
            "192.168.1.5:4001".parse().unwrap(),
            "10.0.0.7:5000".parse().unwrap(),
        ];
        let node_id_str = "a".repeat(64); // shape of an iroh EndpointId hex string

        let txt = build_txt_properties(
            peer_id,
            "Raj's MacBook",
            "MyBrain",
            9000,
            &node_id_str,
            &addrs,
        );
        let info = ServiceInfo::new(
            SERVICE_TYPE,
            "corvux-test-roundtrip",
            "corvux-test-roundtrip.local.",
            "",
            9000,
            Some(txt),
        )
        .unwrap();

        let peer = parse_peer_props(info.get_properties(), our_id, "192.168.1.5".into(), 9000)
            .unwrap()
            .expect("distinct device id must not be treated as our own advert");

        assert_eq!(peer.device_id, peer_id);
        assert_eq!(peer.display_name, "Raj's MacBook");
        assert_eq!(peer.roost_name, "MyBrain");
        assert_eq!(peer.node_id.as_deref(), Some(node_id_str.as_str()));
        assert_eq!(peer.iroh_addrs, addrs);
    }

    /// A peer that has not bound its iroh listener yet (or predates this
    /// change) omits `node_id`/`iroh_addrs` entirely. `parse_peer_props`
    /// must tolerate that -- no error, just empty iroh hints -- rather
    /// than reject the whole record.
    #[test]
    fn txt_tolerates_missing_node_id() {
        let our_id = DeviceId([0x11; 16]);
        let peer_id = DeviceId([0x33; 16]);
        let txt = build_txt_properties(peer_id, "Old Peer", "MyBrain", 9000, "", &[]);
        assert!(
            !txt.contains_key("node_id"),
            "empty node_id must be omitted, not sent empty"
        );
        assert!(!txt.contains_key("iroh_addrs"));

        let info = ServiceInfo::new(
            SERVICE_TYPE,
            "corvux-test-missing",
            "corvux-test-missing.local.",
            "",
            9000,
            Some(txt),
        )
        .unwrap();

        let peer = parse_peer_props(info.get_properties(), our_id, "10.0.0.9".into(), 9000)
            .unwrap()
            .expect("distinct device id must not be treated as our own advert");

        assert_eq!(peer.node_id, None);
        assert!(peer.iroh_addrs.is_empty());
    }

    #[test]
    fn is_lan_addr_accepts_lan_rejects_public_and_loopback() {
        use std::net::IpAddr;
        // LAN: RFC1918 + link-local.
        for ok in ["192.168.1.5", "10.0.0.7", "172.16.3.9", "169.254.1.1"] {
            assert!(is_lan_addr(&ok.parse::<IpAddr>().unwrap()), "{ok} is LAN");
        }
        // IPv6 ULA (fc00::/7) + link-local (fe80::/10).
        assert!(is_lan_addr(&"fd12::1".parse::<IpAddr>().unwrap()));
        assert!(is_lan_addr(&"fe80::1".parse::<IpAddr>().unwrap()));
        // Not LAN: public, loopback, IPv6 loopback, public IPv6.
        for bad in ["8.8.8.8", "1.1.1.1", "127.0.0.1", "::1", "2606:4700::1111"] {
            assert!(
                !is_lan_addr(&bad.parse::<IpAddr>().unwrap()),
                "{bad} not LAN"
            );
        }
    }

    /// A LAN discovery source must never hand the dialer a public/routable
    /// address (a rogue advertiser could use it to redirect our dial
    /// off-LAN). `parse_peer_props` drops non-LAN entries from `iroh_addrs`
    /// while keeping the genuine same-Wi-Fi ones.
    #[test]
    fn parse_drops_non_lan_iroh_addrs() {
        let our_id = DeviceId([0x11; 16]);
        let peer_id = DeviceId([0x44; 16]);
        // One legit LAN addr, one attacker-supplied public addr.
        let advertised: Vec<SocketAddr> = vec![
            "192.168.1.20:4001".parse().unwrap(),
            "8.8.8.8:4001".parse().unwrap(),
        ];
        let node_id_str = "b".repeat(64);
        let txt = build_txt_properties(peer_id, "Peer", "MyBrain", 9000, &node_id_str, &advertised);
        let info = ServiceInfo::new(
            SERVICE_TYPE,
            "corvux-test-nonlan",
            "corvux-test-nonlan.local.",
            "",
            9000,
            Some(txt),
        )
        .unwrap();

        let peer = parse_peer_props(info.get_properties(), our_id, "192.168.1.20".into(), 9000)
            .unwrap()
            .expect("distinct device id");

        assert_eq!(
            peer.iroh_addrs,
            vec!["192.168.1.20:4001".parse::<SocketAddr>().unwrap()],
            "public addr must be filtered out, LAN addr kept"
        );
    }
}
