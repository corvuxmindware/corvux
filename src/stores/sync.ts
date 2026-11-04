import { createSignal, createRoot } from "solid-js";
import { createStore, produce } from "solid-js/store";
import { type UnlistenFn } from "@tauri-apps/api/event";
import { commands, events, isTauri, unwrap } from "../lib/invoke";
import { isIOS as platformIsIOS } from "../lib/platform";
import { lsGet, lsKeysWithPrefix, lsRemove } from "../lib/storage";
import { log } from "../lib/log";
import { roostStore } from "./roost";

type PeerConnectionStateMsg =
  | { kind: "disconnected" }
  | { kind: "dialing" }
  | { kind: "handshaking" }
  | { kind: "connected" }
  | { kind: "failed"; reason: string; at_millis: number };

// Backend-defined event payload shapes — single source of truth lives
// in Rust (`events.rs` + `commands/sync.rs`). Re-exported so the rest
// of the store + downstream subscribers can refer to them by the
// historical names without spelling the `SyncEventPayload` /
// `DiscoveryEventPayload` import twice.
//
// Until this commit these were hand-mirrored above and drifted (the
// hand-written copy was missing the `note_deleted` variant added on
// the backend in C10). Generated types fail to compile on drift —
// that's the whole point of the typed-IPC migration.
import type {
  SyncEventPayload as SyncEventPayloadBindings,
  DiscoveryEventPayload as DiscoveryEventPayloadBindings,
  SyncStatus,
} from "../lib/bindings";

export type SyncEventPayload = SyncEventPayloadBindings;
type DiscoveryEventPayload = DiscoveryEventPayloadBindings;

function deviceNameFromUA(): string {
  const ua = typeof navigator !== "undefined" ? navigator.userAgent : "";
  if (/Android/i.test(ua)) return "Android";
  if (platformIsIOS()) return "iOS";
  if (/Mac/i.test(ua)) return "Mac";
  if (/Windows/i.test(ua)) return "Windows PC";
  if (/Linux/i.test(ua)) return "Linux";
  return "This device";
}

// Lightweight sync state for the TopBar indicator. The Settings panel
// owns the authoritative SyncStatus; this store tracks the parts the
// indicator needs (listening/peers/activity) and runs the auto-reconnect
// loop so paired peers re-link silently whenever both sides come up.
function createSyncStatusStore() {
  const [listening, setListening] = createSignal(false);
  const [connectedPeers, setConnectedPeers] = createSignal(0);
  const [incoming, setIncoming] = createSignal(0);
  const [outgoing, setOutgoing] = createSignal(0);
  const [lastActivityAt, setLastActivityAt] = createSignal(0);
  // True while actively attempting to reach a peer (auto-reconnect or
  // user-triggered). Drives the spin state for the TopBar indicator.
  const [connecting, setConnecting] = createSignal(false);

  // H-6: per-note sync failures, keyed by roost-relative note path. The
  // backend emits `note_sync_failed` when a peer's change couldn't be applied
  // locally (decode/merge/write error) — previously swallowed with no user
  // signal, so a note could silently stop converging. We surface it here so
  // the UI can mark the note as having sync issues. Cleared when that note
  // next writes or is deleted successfully, so a transient failure that later
  // recovers doesn't leave a stale warning.
  const [noteSyncFailures, setNoteSyncFailures] = createStore<
    Record<string, { reason: string; at: number }>
  >({});
  function clearNoteSyncFailure(path: string) {
    if (path in noteSyncFailures) {
      setNoteSyncFailures(produce((s) => void delete s[path]));
    }
  }

  let unlistenEvent: UnlistenFn | undefined;
  let unlistenDiscovery: UnlistenFn | undefined;
  let started = false;

  // AUDIT-027: single Tauri `sync://event` listener for the whole app.
  // Other code paths (app effects, Settings panels, the iroh pairing UI)
  // used to each `listen()` themselves — every payload was deserialised
  // three to four times and a 50-note rescan turned into 150-200 marshals
  // for the same data. Now they all subscribe through `onEvent` and the
  // single listener fans out in-process.
  const eventSubscribers = new Set<(ev: SyncEventPayload) => void>();
  function fanOutEvent(ev: SyncEventPayload) {
    for (const handler of eventSubscribers) {
      try {
        handler(ev);
      } catch (e) {
        log.warn("sync", "onEvent handler threw", e);
      }
    }
  }

  // Track which peers we've already tried to auto-dial in this session
  // so the mDNS loop doesn't hammer the same peer. Cleared on `stopped`.
  const autoDialedInFlight = new Set<string>();
  // Cache of last-known peer addresses by device_id. Populated from
  // `sync://discovered` events + localStorage (for Android where mDNS
  // multicast is blocked without a MulticastLock and "found" never fires).
  // On disconnect we redial from here instead of waiting on mDNS.
  const lastKnownAddr = new Map<
    string,
    { addr: string; port: number; display_name: string }
  >();
  // Per-peer exponential backoff. Without this, a peer that's offline
  // gets dialed every 10s forever, each attempt blocking up to the TCP
  // connect timeout. On Android that thrash visibly hangs the UI thread.
  // Schedule: 0s (first try) → 30s → 60s → 2m → 5m (cap). Reset on a
  // successful peer_connected event.
  const peerNextRetryAt = new Map<string, number>();
  const peerConsecutiveFails = new Map<string, number>();
  const BACKOFF_STEPS_MS = [30_000, 60_000, 120_000, 300_000];
  function nextBackoffMs(fails: number): number {
    const idx = Math.min(fails, BACKOFF_STEPS_MS.length) - 1;
    return idx < 0 ? 0 : BACKOFF_STEPS_MS[idx];
  }

  // Seed address cache from localStorage entries saved on successful
  // pair. Lets Android (no multicast) still reconnect after restart:
  // we have the address AND the PIN, mDNS isn't required. Idempotent,
  // so callers are free to re-invoke this any time they might need an
  // up-to-date view — we call it on start, on each retry tick, and on
  // peer_disconnected to pick up pairings that happened mid-session.
  // SYN-M10: purge legacy plaintext pairing PINs from localStorage. Older
  // builds persisted `corvux:syncPin:<id>` to drive a frontend auto-dial; no
  // code writes them anymore, and reconnect is now owned by the backend's
  // engine reconnect supervisor (resumption tokens + pinned TLS fingerprint,
  // which survive webview suspend/kill — see dial.rs::start_reconnect_supervisor).
  // A reusable PIN sitting in plaintext localStorage is a needless secret to
  // leave lying around, so sweep every residual entry once on init. After this
  // `tryAutoDial` finds no PIN and no-ops; the backend covers reconnect.
  function purgeLegacySyncPins() {
    for (const key of lsKeysWithPrefix("corvux:syncPin:")) lsRemove(key);
  }
  // One-time sweep at store init — the store is a singleton created once via
  // `createRoot` below, so this runs exactly once per app session.
  purgeLegacySyncPins();

  function hydrateAddrsFromStorage() {
    for (const key of lsKeysWithPrefix("corvux:syncAddr:")) {
      const deviceId = key.slice("corvux:syncAddr:".length);
      const addrStr = lsGet(key);
      if (!addrStr || !addrStr.includes(":")) continue;
      const [ip, portStr] = addrStr.split(":");
      const port = Number(portStr) | 0;
      if (!ip || !port) continue;
      // Don't clobber a richer entry from mDNS discovery (which carries
      // display_name) — only fill if missing.
      if (!lastKnownAddr.has(deviceId)) {
        lastKnownAddr.set(deviceId, { addr: ip, port, display_name: "" });
      }
    }
  }

  let decayTimer: number | null = null;
  // Decays only the inbound activity pulse. `outgoing` is NOT reset here: it
  // is the durable outbox depth (see the `outbox_changed` handler), authored
  // solely by backend outbox events + the startup seed, and cleared only when
  // the engine stops. Zeroing it on a timer would hide a real unsynced backlog.
  function scheduleDecay() {
    if (decayTimer !== null) return;
    decayTimer = window.setTimeout(() => {
      decayTimer = null;
      setIncoming(0);
    }, 30000) as unknown as number;
  }

  function recordIncoming() {
    setIncoming((n) => n + 1);
    setLastActivityAt(Date.now());
    scheduleDecay();
  }

  // Set the peer count from the backend's authoritative snapshot. The backend
  // `peers` map is keyed by device, so `connected_peers.length` is the TRUE
  // count of unique connected devices. We use it instead of a naive +1/-1
  // delta because the delta over-counts: symmetric pairing (protocol v5) opens
  // two connections to the SAME device (each side dials), firing two
  // `peer_connected` events → the badge showed "2 peers" for one phone. It also
  // under-counts, because a superseded duplicate connection emits
  // `peer_disconnected` while the device is still live via a newer handle.
  // Refetching the deduped backend count is immune to both.
  async function refreshPeerCount() {
    try {
      const st = await unwrap(commands.syncStatus());
      setConnectedPeers(st.connected_peers.length);
    } catch {
      /* transient IPC failure — keep the last-known count */
    }
  }

  // Attempt a silent reconnect to a paired peer. Requires:
  //   - Peer appears in our known_peers (already paired at least once)
  //   - We have the peer's PIN saved locally (stored on first manual pair)
  //   - We're not already connected to this peer
  //   - No other in-flight attempt for this device_id
  // On success, peer_connected event will fire and update the UI; on
  // failure we just log and move on — user can still manually re-pair.
  async function tryAutoDial(peer: {
    device_id: string;
    display_name: string;
    addr: string;
    port: number;
  }) {
    if (autoDialedInFlight.has(peer.device_id)) return;
    const dueAt = peerNextRetryAt.get(peer.device_id) ?? 0;
    if (Date.now() < dueAt) return;
    let status: SyncStatus;
    try {
      status = await unwrap(commands.syncStatus());
    } catch {
      return;
    }
    if (!status.running) return;
    const already = status.connected_peers.some(
      (p) => p.device_id === peer.device_id,
    );
    if (already) return;
    const known = status.known_peers.some(
      (p) => p.device_id === peer.device_id,
    );
    if (!known) return;
    // SYN-M10: legacy frontend PIN-dial. No build writes `corvux:syncPin:` any
    // more and `purgeLegacySyncPins` sweeps any residue at init, so this is now
    // always null → this path no-ops and reconnect is handled entirely by the
    // backend engine reconnect supervisor (resumption-token based). Kept as an
    // inert guard rather than ripped out; retiring the whole frontend auto-dial
    // loop is a separate tidy-up.
    const pin = lsGet(`corvux:syncPin:${peer.device_id}`);
    if (!pin) return;
    autoDialedInFlight.add(peer.device_id);
    setConnecting(true);
    try {
      // AUDIT-003b: backend now resolves the pinned fingerprint from
      // peer_store by addr (see `resolve_pinned_fingerprint` in
      // commands/sync.rs). Passing `pinnedFingerprintHex: null` is
      // still accepted for callers that want explicit control, but
      // the auto-reconnect path leaves resolution to the backend so
      // any future peer-store schema changes (e.g. dropping
      // `last_known_addr` in favour of a routing table) only need
      // updating in one place.
      await unwrap(
        commands.syncConnect(`${peer.addr}:${peer.port}`, pin, null),
      );
      peerConsecutiveFails.delete(peer.device_id);
      peerNextRetryAt.delete(peer.device_id);
    } catch (e) {
      const fails = (peerConsecutiveFails.get(peer.device_id) ?? 0) + 1;
      peerConsecutiveFails.set(peer.device_id, fails);
      peerNextRetryAt.set(peer.device_id, Date.now() + nextBackoffMs(fails));
      log.warn(
        "sync",
        `auto-reconnect to ${peer.display_name || peer.device_id} failed (attempt ${fails}, next in ${Math.round(nextBackoffMs(fails) / 1000)}s)`,
        e,
      );
    } finally {
      autoDialedInFlight.delete(peer.device_id);
      setConnecting(false);
    }
  }

  // One-shot reconnect sweep: if the engine is running but has no
  // connected peers, walk known_peers from sync_status and try dialing
  // each via its cached (localStorage / mDNS) address. Called once on
  // engine start and on demand via `kickRetry` (after pairing / reset).
  // The *periodic* version of this now lives in the Rust engine's
  // reconnect supervisor so it survives the webview being backgrounded.
  async function retryKnownPeers() {
    let status: SyncStatus;
    try {
      status = await unwrap(commands.syncStatus());
    } catch {
      return;
    }
    if (!status.running) return;
    if (status.connected_peers.length > 0) return;
    // Re-read localStorage — the user might have paired during this
    // session, in which case a new `corvux:syncAddr:*` entry exists
    // that we don't yet have in `lastKnownAddr`.
    hydrateAddrsFromStorage();
    for (const kp of status.known_peers) {
      const addr = lastKnownAddr.get(kp.device_id);
      if (!addr) continue;
      void tryAutoDial({
        device_id: kp.device_id,
        display_name: kp.display_name,
        addr: addr.addr,
        port: addr.port,
      });
    }
  }

  // Reduce a single backend sync event into store state, then fan it out to
  // subscribers. Extracted from the Tauri listener so the state machine is
  // unit-testable without a live `sync://event` channel (the listener only
  // runs under `isTauri`). Callers: the real listener in `start()`, and tests.
  function handleSyncEvent(ev: SyncEventPayload) {
    switch (ev.kind) {
      case "listener_bound":
        setListening(true);
        break;
      case "iroh_listener_bound":
        // Iroh path also counts as "listening" for the TopBar
        // indicator — same green-dot UX whether the connection
        // came in over LAN TLS or cross-network iroh QUIC.
        setListening(true);
        break;
      case "peer_connected":
        // Authoritative device-deduped count, not a naive +1 (see
        // refreshPeerCount — symmetric pairing can double-count a device).
        void refreshPeerCount();
        setLastActivityAt(Date.now());
        peerConsecutiveFails.delete(ev.device_id);
        peerNextRetryAt.delete(ev.device_id);
        break;
      case "peer_disconnected":
        // Authoritative refetch, not `n-1`: a superseded duplicate
        // connection emits this while the device is still connected via a
        // newer handle, so a blind decrement would wrongly drop the count.
        void refreshPeerCount();
        // Transport dropped — redial from cached address. If we
        // don't have one cached yet (e.g. user paired during this
        // session so the localStorage entry is newer than the
        // cache), re-hydrate from storage before giving up.
        {
          let last = lastKnownAddr.get(ev.device_id);
          if (!last) {
            hydrateAddrsFromStorage();
            last = lastKnownAddr.get(ev.device_id);
          }
          if (last) {
            window.setTimeout(() => {
              void tryAutoDial({
                device_id: ev.device_id,
                display_name: last!.display_name,
                addr: last!.addr,
                port: last!.port,
              });
            }, 1500);
          }
        }
        break;
      case "note_written":
        recordIncoming();
        // A successful write for this path means it converged — clear any
        // prior sync-failure marker so a recovered note stops warning.
        clearNoteSyncFailure(ev.path);
        break;
      case "note_sync_failed":
        // A peer's change for this note couldn't be applied locally. Record it
        // so the UI can surface a per-note "sync issue" marker.
        setNoteSyncFailures(ev.path, { reason: ev.reason, at: Date.now() });
        break;
      case "note_deleted":
        // The note is gone — a stale failure marker for it is meaningless.
        clearNoteSyncFailure(ev.path);
        break;
      case "attachment_written":
        // A synced attachment blob arrived — counts as inbound activity.
        // The editor re-decoration is handled by app/effects.ts.
        recordIncoming();
        break;
      case "outbox_changed":
        // The `↑N` indicator is the durable outbox depth — the count of
        // changes actually awaiting delivery to a peer — NOT a per-save
        // pulse. The backend only enqueues when the queue client is running
        // AND the peer is deliverable (paired, has NodeId + pair key), so
        // `total_pending` is 0 whenever sync is off or no peer exists. This is
        // the authoritative source; the frontend never guesses "outbound"
        // from local saves anymore (that painted `↑1` with sync disabled).
        {
          const prev = outgoing();
          setOutgoing(ev.total_pending);
          // Spin the "syncing now" icon only when the outbox DRAINS (count
          // dropped = a change actually delivered to a peer). An INCREASE is a
          // change being queued because the peer is offline — backlog growth,
          // not live traffic — so it must not pulse (otherwise a bulk enqueue
          // to a stuck peer spins the icon continuously, reading as "syncing"
          // when nothing is moving).
          if (ev.total_pending < prev) setLastActivityAt(Date.now());
        }
        break;
      case "peer_status_changed":
        // Drive the TopBar `connecting()` signal so the spinning
        // icon reflects backend dial-state in real time. We only
        // toggle during dial/handshake — connected/disconnected/
        // failed are handled by the dedicated events above.
        if (ev.state.kind === "dialing" || ev.state.kind === "handshaking") {
          setConnecting(true);
        } else {
          setConnecting(false);
        }
        break;
      case "stopped":
        setListening(false);
        setConnectedPeers(0);
        setIncoming(0);
        setOutgoing(0);
        autoDialedInFlight.clear();
        peerConsecutiveFails.clear();
        peerNextRetryAt.clear();
        setNoteSyncFailures(
          produce((s) => {
            for (const k of Object.keys(s)) delete s[k];
          }),
        );
        break;
    }
    // Fan out to subscribers AFTER the store has already updated its
    // own state. Handlers downstream see a consistent view.
    fanOutEvent(ev);
  }

  async function start() {
    if (started || !isTauri) return;
    hydrateAddrsFromStorage();
    started = true;
    try {
      unlistenEvent = await events.syncEvent.listen((e) => {
        handleSyncEvent(e.payload);
      });
    } catch (e) {
      log.warn("sync", "sync-event listener failed", e);
    }
    try {
      unlistenDiscovery = await events.syncDiscovered.listen((e) => {
          const ev = e.payload;
          if (ev.kind === "found") {
            lastKnownAddr.set(ev.device_id, {
              addr: ev.addr,
              port: ev.port,
              display_name: ev.display_name,
            });
            void tryAutoDial({
              device_id: ev.device_id,
              display_name: ev.display_name,
              addr: ev.addr,
              port: ev.port,
            });
          } else if (ev.kind === "lost") {
            // Do NOT delete from lastKnownAddr — on Android the "lost"
            // event never fires anyway because mDNS multicast isn't
            // received, and even on desktop the peer might just be
            // briefly off-network. We'd rather keep trying the cached
            // address every 20s than forget it.
          }
        });
    } catch (e) {
      log.warn("sync", "sync-discovery listener failed", e);
    }

    // Seed the reactive signals from the backend's authoritative snapshot.
    // Everything above is delta-mutated from events starting at listening=false
    // / connectedPeers=0. After a webview reload (very common on mobile
    // foreground) the backend engine may already be connected, so without this
    // seed the TopBar shows "off / 0 peers" while sync is actually live, and
    // the editor's "will this attachment sync?" checks mis-decide. Seed AFTER
    // subscribing so we don't miss transitions during the fetch.
    try {
      const st = await unwrap(commands.syncStatus());
      setListening(st.listen_port !== null || st.known_peers.length > 0);
      setConnectedPeers(st.connected_peers.length);
      // Seed the `↑N` outbound-backlog indicator from the durable outbox
      // depth so a webview reload with changes still waiting to deliver
      // shows the true count instead of 0 until the next event.
      setOutgoing(st.total_pending_outbound);
    } catch (e) {
      log.warn("sync", "seed from sync_status failed", e);
    }

    // One-shot kick so a freshly-started engine with a paired peer
    // reconnects immediately (snappy LAN UX). The *periodic* retry now
    // lives in the Rust engine's reconnect supervisor — it keeps running
    // while this webview is suspended or killed (mobile background), which
    // the old 10s JS setInterval could not. We keep the event-driven dials
    // here (mDNS-found, on-disconnect, post-pair `kickRetry`) because they
    // use localStorage/mDNS address sources the backend peer_store may not
    // have yet.
    void retryKnownPeers();
  }

  // Kick the retry loop from outside — used right after a successful
  // pair or after clicking "Reset sync history" so the user doesn't
  // have to wait for the next 10s tick to see reconnection.
  function kickRetry() {
    void retryKnownPeers();
  }

  // Zero the indicator + reconnect state when the active roost changes. The
  // signals live in a singleton (createRoot below) shared across roosts, so
  // without this a previous roost's peer count / outbox backlog would bleed
  // into a roost with no sync configured — painting a misleading "connected /
  // ↑N" on the TopBar. Called from the roost-change effect BEFORE the new
  // roost's engine (maybe) autostarts and re-seeds. (Desktop bug #3, 0.2.1.)
  function resetForRoostSwitch() {
    setListening(false);
    setConnectedPeers(0);
    setIncoming(0);
    setOutgoing(0);
    setConnecting(false);
    autoDialedInFlight.clear();
    peerConsecutiveFails.clear();
    peerNextRetryAt.clear();
    setNoteSyncFailures(
      produce((s) => {
        for (const k of Object.keys(s)) delete s[k];
      }),
    );
  }

  function stop() {
    unlistenEvent?.();
    unlistenEvent = undefined;
    unlistenDiscovery?.();
    unlistenDiscovery = undefined;
    started = false;
    if (decayTimer !== null) {
      window.clearTimeout(decayTimer);
      decayTimer = null;
    }
  }

  // User-initiated "kick sync" — what the TopBar indicator runs on click.
  // Ensures the engine is running (autostarting it if needed) and then
  // forces an immediate scan of known peers via sync_status so we don't
  // wait up to 10s for the next mDNS beacon. Returns true if sync is
  // live afterwards (either was already, or we started it). The UI uses
  // the return to decide whether to open Settings as a fallback when
  // the user has never paired anything.
  async function triggerSync(): Promise<
    | "running"
    | "autostarted"
    | "needs-setup"
    | "error"
  > {
    if (!isTauri) return "error";
    setConnecting(true);
    try {
      let status: SyncStatus;
      try {
        status = await unwrap(commands.syncStatus());
      } catch {
        return "error";
      }
      // Engine already up — just force a fresh rescan to re-announce
      // our notes so any newly-connected peer can pick them up.
      if (status.running) {
        commands.syncRescanRoost().catch(() => {});
        return "running";
      }
      // Engine is down. If the user has never paired anyone OR has no
      // persisted sync prefs, nudge them into Settings to set it up.
      // `corvux:syncEnabled` is our fast local-only hint; the backend
      // prefs.json is the real source of truth and `sync_try_autostart`
      // will silently no-op if it's missing.
      if (lsGet("corvux:syncEnabled") !== "1") {
        return "needs-setup";
      }
      const roost = roostStore.roostPath();
      if (!roost) return "needs-setup";
      try {
        // Note: `deviceName` is no longer accepted by the Rust signature;
        // specta caught the extra arg the old call site was passing.
        await unwrap(commands.syncTryAutostart(roost));
      } catch (e) {
        log.warn("sync", "triggerSync autostart failed", e);
        return "error";
      }
      return "autostarted";
    } finally {
      setConnecting(false);
    }
  }

  function onEvent(handler: (ev: SyncEventPayload) => void): () => void {
    eventSubscribers.add(handler);
    return () => {
      eventSubscribers.delete(handler);
    };
  }

  return {
    listening,
    connectedPeers,
    incoming,
    outgoing,
    lastActivityAt,
    connecting,
    noteSyncFailures,
    clearNoteSyncFailure,
    triggerSync,
    kickRetry,
    resetForRoostSwitch,
    start,
    stop,
    onEvent,
    // Exposed for unit tests: drive the reducer directly without a live
    // `sync://event` channel. Not called from app code (the listener owns it).
    handleSyncEvent,
  };
}

// Exported for tests — construct an isolated store instance under a fresh
// reactive root. App code uses the `syncStatusStore` singleton below.
export { createSyncStatusStore };

export const syncStatusStore = createRoot(createSyncStatusStore);
