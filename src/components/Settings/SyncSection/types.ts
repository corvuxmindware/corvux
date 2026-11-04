/// View-side helpers + UI-only types for the sync settings panel.
/// `SyncStatus`, `ConnectedPeer`, `KnownPeer`, `PeerConnectionState`
/// come from the generated tauri-specta bindings — single source of
/// truth on the Rust side, no hand-maintained mirrors here.
import { isIOS as platformIsIOS } from "../../../lib/platform";
import type {
  ConnectedPeer,
  KnownPeer,
  PeerConnectionState,
  SyncStatus,
} from "../../../lib/bindings";

export type { ConnectedPeer, KnownPeer, PeerConnectionState, SyncStatus };

export type SyncEventPayload =
  | { kind: "listener_bound"; port: number }
  | { kind: "iroh_listener_bound"; node_id: string }
  | { kind: "peer_connected"; device_id: string; display_name: string }
  | { kind: "peer_disconnected"; device_id: string }
  | { kind: "pairing_failed"; reason: string }
  | { kind: "scan_progress"; processed: number; total: number }
  | { kind: "scan_complete"; count: number }
  | { kind: "note_written"; path: string }
  | { kind: "note_deleted"; path: string }
  | {
      kind: "peer_status_changed";
      device_id: string;
      state: PeerConnectionState;
    }
  | { kind: "outbox_changed"; total_pending: number }
  | { kind: "stopped" };

export type DiscoveryEventPayload =
  | {
      kind: "found";
      device_id: string;
      display_name: string;
      roost_name: string;
      addr: string;
      port: number;
    }
  | { kind: "lost"; device_id: string };

export type NearbyPeer = {
  device_id: string;
  display_name: string;
  roost_name: string;
  addr: string;
  port: number;
};

export type Banner = { tone: "info" | "success" | "error"; text: string };

export const EMPTY_STATUS: SyncStatus = {
  running: false,
  listen_port: null,
  our_device_id: "",
  our_fingerprint_words: "",
  connected_peers: [],
  known_peers: [],
  queue_active: false,
  total_pending_outbound: 0,
  relay_connected: null,
};

export function randomPin(): string {
  return Math.floor(100000 + Math.random() * 900000).toString();
}

export function defaultDeviceName(): string {
  // Platform-agnostic best-effort. User can override.
  // Check iOS FIRST: iPadOS 13+ reports "Macintosh" in the UA string, so
  // a naive /Mac/ test mis-labels iPads as "Mac". platformIsIOS() handles
  // the masquerade via maxTouchPoints.
  const ua = typeof navigator !== "undefined" ? navigator.userAgent : "";
  if (platformIsIOS()) return "iOS";
  if (/Mac/i.test(ua)) return "Mac";
  if (/Windows/i.test(ua)) return "Windows PC";
  if (/Linux/i.test(ua)) return "Linux";
  if (/Android/i.test(ua)) return "Android";
  return "This device";
}
