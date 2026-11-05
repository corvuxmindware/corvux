import { createSignal, onMount, onCleanup, Show } from "solid-js";
import { commands, isTauri, unwrap } from "../../../lib/invoke";
import { confirmDialog } from "../../../lib/confirm";
import { log } from "../../../lib/log";
import { isMobile } from "../../../lib/platform";
import { lsSet, lsRemove, lsKeysWithPrefix } from "../../../lib/storage";
import { roostStore } from "../../../stores/roost";
import { syncStatusStore } from "../../../stores/sync";

import HowSyncWorks from "./HowSyncWorks";
import IrohPairingPanel from "./IrohPairingPanel";
import PeerList from "./PeerList";
import QueuePanel from "./QueuePanel";
import ScanActions from "./ScanActions";
import {
  EMPTY_STATUS,
  defaultDeviceName,
  type Banner,
  type SyncEventPayload,
  type SyncStatus,
} from "./types";

/// Settings → Sync section.
///
/// Stripped down to a single pairing path (iroh, cross-network) after
/// the LAN-only TLS form was confusing users — they couldn't tell when
/// to use the IP-based pair vs the QR pair, and the two halves overlapped
/// in semantics anyway (iroh handles LAN automatically). Legacy TLS
/// commands still exist on the backend so previously-paired devices keep
/// working; we just don't expose new pairings through them.
export default function SyncSection() {
  // ── Engine state ──────────────────────────────────────────────────
  const [status, setStatus] = createSignal<SyncStatus>(EMPTY_STATUS);
  const [initialized, setInitialized] = createSignal(false);
  const [initError, setInitError] = createSignal<string | null>(null);
  const [deviceName, setDeviceName] = createSignal(defaultDeviceName());

  // ── Banner + scan progress ────────────────────────────────────────
  const [banner, setBanner] = createSignal<Banner | null>(null);
  let bannerTimer: number | null = null;
  const [scan, setScan] = createSignal<
    { processed: number; total: number } | null
  >(null);
  const [rescanning, setRescanning] = createSignal(false);

  let unlisten: (() => void) | null = null;

  const refresh = async () => {
    try {
      const s = await unwrap(commands.syncStatus());
      setStatus(s);
    } catch {
      // Not initialized yet — leave EMPTY_STATUS.
    }
  };

  const flashBanner = (b: Banner, ms = 5000) => {
    if (bannerTimer !== null) window.clearTimeout(bannerTimer);
    setBanner(b);
    bannerTimer = window.setTimeout(
      () => setBanner(null),
      ms,
    ) as unknown as number;
  };

  const handleSyncEvent = (ev: SyncEventPayload) => {
    // Any event is a signal that status may have changed — re-fetch
    // immediately instead of waiting for the next poll tick.
    void refresh();
    switch (ev.kind) {
      case "listener_bound":
        // Legacy TLS listener — still wired in case an old paired peer
        // dials in over TLS. No banner; iroh is the one users see.
        break;
      case "iroh_listener_bound":
        flashBanner({ tone: "info", text: "Pairing endpoint ready" });
        break;
      case "peer_connected":
        flashBanner({
          tone: "success",
          text: `Connected to ${ev.display_name}`,
        });
        break;
      case "peer_disconnected":
        flashBanner({ tone: "info", text: "A peer disconnected" });
        break;
      case "pairing_failed":
        flashBanner({ tone: "error", text: `Pairing failed: ${ev.reason}` }, 8000);
        break;
      case "scan_progress":
        setScan({ processed: ev.processed, total: ev.total });
        break;
      case "scan_complete":
        setScan(null);
        flashBanner({
          tone: "success",
          text: `Scanned ${ev.count} note${ev.count === 1 ? "" : "s"} into sync store`,
        });
        break;
      case "note_written":
      case "note_deleted":
        // App.tsx handles file-tree refresh — this panel only mirrors
        // peer state, which `refresh()` above covers.
        break;
      case "peer_status_changed":
        if (ev.state.kind === "failed") {
          flashBanner(
            { tone: "error", text: `Reconnect failed: ${ev.state.reason}` },
            8000,
          );
        }
        break;
      case "outbox_changed":
        // QueuePanel owns the pending-count display via its own
        // subscription; nothing for the parent panel to do.
        break;
      case "stopped":
        flashBanner({ tone: "info", text: "Sync stopped" });
        break;
    }
  };

  onMount(async () => {
    await refresh();
    if (isTauri) {
      // AUDIT-027: subscribe via the syncStatusStore fan-out — single
      // Tauri listener handles every consumer in-process.
      unlisten = syncStatusStore.onEvent((ev) => handleSyncEvent(ev as SyncEventPayload));
    }
  });

  onCleanup(() => {
    if (bannerTimer !== null) window.clearTimeout(bannerTimer);
    if (unlisten) unlisten();
  });

  const initEngine = async () => {
    setInitError(null);
    try {
      const roost = roostStore.roostPath();
      if (!roost) {
        setInitError("Open a roost first.");
        return;
      }
      const s = await unwrap(commands.syncInit(roost, deviceName()));
      setStatus(s);
      setInitialized(true);
      // Mark sync as enabled so next launch's auto-resume runs even on
      // a dialer-only device.
      lsSet("corvux:syncEnabled", "1");
    } catch (e) {
      setInitError(String(e));
    }
  };

  const unpair = async (deviceIdHex: string) => {
    try {
      await unwrap(commands.syncUnpair(deviceIdHex));
      await refresh();
    } catch (e) {
      log.error("sync", "unpair failed", e);
    }
  };

  const rescanRoost = async () => {
    if (rescanning()) return;
    setRescanning(true);
    setScan({ processed: 0, total: 0 });
    try {
      const count = await unwrap(commands.syncRescanRoost());
      flashBanner({
        tone: "success",
        text: `Rescan complete — ${count} notes ingested`,
      });
    } catch (e) {
      flashBanner({ tone: "error", text: `Rescan failed: ${e}` }, 8000);
    } finally {
      setRescanning(false);
      setScan(null);
    }
  };

  const reconnectAll = async () => {
    try {
      await unwrap(commands.syncReconnectAll());
      flashBanner({ tone: "info", text: "Reconnecting to paired devices…" });
      await refresh();
    } catch (e) {
      flashBanner({ tone: "error", text: `Reconnect failed: ${e}` }, 8000);
    }
  };

  /// Full sync reset: wipe local Automerge state + unpair every device +
  /// clear the localStorage keys the front-end retry loop reads. This is
  /// the "I want a clean slate" path. After this, the user must re-pair
  /// each device — but that's the safe outcome when sync state is corrupt
  /// or you're starting from scratch.
  ///
  /// Why ALL peers: when one device's CRDT is corrupted, peers that hold
  /// the bad history will keep pushing it back. Unpairing forces a fresh
  /// pair, which means a fresh handshake and a fresh CRDT bootstrap. Half-
  /// measures (reset CRDT but keep pairings) caused the recurrence we
  /// just diagnosed.
  ///
  /// Why clear localStorage: legacy TLS pairings stored `syncAddr:<id>` +
  /// `syncPin:<id>` keys for auto-reconnect. Even after backend `unpair`
  /// + `reset_docstore`, the front-end retry loop reads those keys and
  /// fires sync_connect (TLS path) with the now-wrong PIN, producing the
  /// `WrongPin` spam in diagnostics.
  const resetDocstore = async () => {
    const ok = await confirmDialog(
      "Reset all sync state on this device?\n\n" +
      "• Wipes the local Automerge / sync history.\n" +
      "• Forgets every paired device.\n" +
      "• Clears auto-reconnect cache.\n\n" +
      "Your roost files are NOT touched. You'll need to re-pair each device after.\n\n" +
      "Use this when sync gets stuck or after a corruption error.",
    );
    if (!ok) return;
    try {
      // Unpair every known peer first. Keeps the backend's peer_store and
      // the front-end's localStorage cache from disagreeing.
      const known = status().known_peers;
      for (const peer of known) {
        try {
          await unwrap(commands.syncUnpair(peer.device_id));
        } catch (e) {
          log.warn("sync", `unpair ${peer.device_id} failed`, e);
        }
      }
      await unwrap(commands.syncResetDocstore());

      // Sweep stale localStorage entries from prior pairings. The retry
      // loop in stores/sync.ts reads any `corvux:syncPin:<id>` /
      // `corvux:syncAddr:<id>` keys it finds, so a stale one from a
      // previous test session re-triggers wrong-PIN dials forever.
      const stale = [
        ...lsKeysWithPrefix("corvux:syncPin:"),
        ...lsKeysWithPrefix("corvux:syncAddr:"),
      ];
      for (const key of stale) lsRemove(key);

      flashBanner({
        tone: "success",
        text: "Sync state cleared. Re-pair each device to start sync again.",
      });
      await refresh();
      syncStatusStore.kickRetry();
    } catch (e) {
      flashBanner({ tone: "error", text: `Reset failed: ${e}` }, 8000);
    }
  };

  const stopEngine = async () => {
    try {
      await unwrap(commands.syncStop());
      lsRemove("corvux:syncEnabled");
      setInitialized(false);
      setStatus(EMPTY_STATUS);
    } catch (e) {
      log.error("sync", "stop failed", e);
    }
  };

  const [showRecovery, setShowRecovery] = createSignal(false);
  const mobile = isMobile();

  return (
    <section>
      <div class="flex items-center justify-between mb-2">
        <h3 class="text-text-secondary text-xs font-semibold uppercase tracking-wider">
          Sync
        </h3>
        <span class="inline-flex items-center gap-1 text-[10px] uppercase font-semibold rounded px-1.5 py-0.5 border border-amber-400/30 bg-amber-400/10 text-amber-400">
          Beta
        </span>
      </div>

      {/* Scope statement — what works today, what doesn't. The trust gap
          this closes: users were treating sync as if it were "just works"
          for any combination of devices. It isn't yet, and pretending
          otherwise produces bug reports that read like product failures. */}
      <div class="text-[11px] text-text-muted leading-relaxed mb-3 space-y-1">
        <p>
          Pair two of your own devices — same network or across the internet.
          Your notes are{" "}
          <strong class="text-text-secondary">end-to-end encrypted</strong>; no
          server can read them.
        </p>
        <Show when={mobile}>
          <p class="text-amber-400/80">
            Mobile sync is experimental. Reconnect after backgrounding may
            require manual retry; offline-queue convergence isn't guaranteed.
          </p>
        </Show>
      </div>

      <Show
        when={isTauri}
        fallback={
          <div class="text-text-muted text-xs italic">
            Sync is only available in the desktop app — browser dev mode has no
            network transport.
          </div>
        }
      >
        <div class="space-y-3">
          {/* Visible before AND after enabling — a new user reads it while
              deciding, since the rest of the panel only renders once sync
              is on. */}
          <HowSyncWorks />

          <Show
            when={status().our_device_id || initialized()}
            fallback={
              <div class="space-y-2">
                <div class="flex items-center gap-2">
                  <label class="text-text-secondary text-xs w-24">
                    Device name
                  </label>
                  <input
                    class="flex-1 bg-bg-secondary border border-border rounded px-2 py-1 text-sm"
                    value={deviceName()}
                    onInput={(e) => setDeviceName(e.currentTarget.value)}
                  />
                </div>
                <button
                  class="w-full bg-bg-secondary hover:bg-bg-tertiary border border-border rounded px-3 py-1.5 text-sm"
                  onClick={initEngine}
                >
                  Enable sync on this roost
                </button>
                <Show when={initError()}>
                  <div class="text-red-400 text-xs">{initError()}</div>
                </Show>
              </div>
            }
          >
            {/* Status pill + transient banner. */}
            <div class="flex items-center gap-2 text-xs">
              <span
                class={`inline-flex items-center gap-1.5 rounded-full px-2 py-0.5 ${
                  status().connected_peers.length > 0
                    ? "bg-green-400/10 text-green-400"
                    : status().running
                      ? "bg-blue-400/10 text-blue-400"
                      : "bg-bg-secondary text-text-muted"
                }`}
              >
                <span
                  class={`w-1.5 h-1.5 rounded-full ${
                    status().connected_peers.length > 0
                      ? "bg-green-400 animate-pulse"
                      : status().running
                        ? "bg-blue-400 animate-pulse"
                        : "bg-gray-500"
                  }`}
                />
                {status().connected_peers.length > 0
                  ? `${status().connected_peers.length} peer${status().connected_peers.length > 1 ? "s" : ""} connected`
                  : status().running
                    ? "Ready to pair"
                    : "Idle"}
              </span>
              {/* PT-1: relay reachability. Only shown when a relay is
                  configured but currently unreachable — so cross-network
                  reconnect is degraded (same-network devices still sync
                  directly). Previously this only appeared as silent log spam. */}
              <Show when={status().relay_connected === false}>
                <span
                  class="inline-flex items-center gap-1.5 rounded-full px-2 py-0.5 bg-amber-400/10 text-amber-400"
                  title="Can't reach the relay server. Devices on the same network still sync directly; cross-network sync may be delayed until the relay is reachable."
                >
                  <span class="w-1.5 h-1.5 rounded-full bg-amber-400" />
                  Relay unreachable
                </span>
              </Show>
              <Show when={banner()}>
                {(b) => (
                  <span
                    class={`flex-1 truncate ${
                      b().tone === "error"
                        ? "text-red-400"
                        : b().tone === "success"
                          ? "text-green-400"
                          : "text-text-secondary"
                    }`}
                  >
                    {b().text}
                  </span>
                )}
              </Show>
            </div>

            <IrohPairingPanel />

            <PeerList
              knownPeers={status().known_peers}
              connectedPeers={status().connected_peers}
              onUnpair={unpair}
              onReconnectAll={reconnectAll}
            />

            <ScanActions
              scan={scan()}
              rescanning={rescanning()}
              onRescan={rescanRoost}
              onResetDocstore={resetDocstore}
            />

            <QueuePanel />

            {/* Recovery — collapsed by default. Reads like a quick-ref
                runbook so a user whose sync got weird has somewhere to
                start before opening a bug report. */}
            <div class="border border-border rounded">
              <button
                class="w-full flex items-center justify-between px-3 py-2 text-xs text-text-secondary hover:bg-bg-hover"
                onClick={() => setShowRecovery((v) => !v)}
              >
                <span class="font-medium">If sync gets stuck</span>
                <span class="text-text-muted">{showRecovery() ? "−" : "+"}</span>
              </button>
              <Show when={showRecovery()}>
                <div class="px-3 pb-3 text-[11px] text-text-muted leading-relaxed space-y-2 border-t border-border">
                  <p>
                    <strong class="text-text-secondary">A peer won't reconnect.</strong>{" "}
                    Click the peer in the list above and unpair it, then re-pair from
                    both ends. The PIN is fresh each time — old PINs won't be reused.
                  </p>
                  <p>
                    <strong class="text-text-secondary">A note has the wrong content
                    on one device.</strong> The most recent edit on either device wins
                    after the next sync. To force a direction, open the note on the
                    "good" device, save once (any whitespace edit), and the change
                    propagates. CRDT merges line-level — if both sides edited the
                    same line concurrently, both versions appear and you reconcile
                    manually.
                  </p>
                  <p>
                    <strong class="text-text-secondary">Sync looks idle even though both
                    devices are online.</strong> Use{" "}
                    <strong class="text-text-secondary">Reconnect all</strong> in the
                    peer list. If that doesn't help and the diagnostics show recurring
                    errors, use{" "}
                    <strong class="text-text-secondary">Reset sync state</strong> below
                    Scan — your roost files stay intact, you just re-pair each device.
                  </p>
                  <p>
                    <strong class="text-text-secondary">Cross-network on a hotel/cafe
                    network.</strong> Most public networks block peer-to-peer. Use
                    Tailscale (or any VPN that routes both devices into the same
                    private subnet) and pair as if you were on a home network.
                  </p>
                </div>
              </Show>
            </div>

            <button
              class="w-full text-text-muted hover:text-red-400 text-xs pt-2"
              onClick={stopEngine}
            >
              Stop sync and forget session
            </button>
          </Show>
        </div>
      </Show>
    </section>
  );
}
