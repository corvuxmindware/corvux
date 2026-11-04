//! LAN peer discovery. Currently just mDNS / DNS-SD; later phases might
//! add signaling-server based discovery for NAT-traversal setups (parked).

pub mod mdns;

pub use mdns::{DiscoveredPeer, Discovery, DiscoveryEvent};
