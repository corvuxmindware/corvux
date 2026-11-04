import { For, Show } from "solid-js";
import type { ConnectedPeer, KnownPeer, PeerConnectionState } from "./types";

export interface PeerListProps {
  knownPeers: KnownPeer[];
  connectedPeers: ConnectedPeer[];
  onUnpair: (deviceIdHex: string) => void;
  onReconnectAll: () => void;
}

function dotClass(connected: boolean, state: PeerConnectionState): string {
  if (connected) return "bg-green-400";
  switch (state.kind) {
    case "connected":
      return "bg-green-400";
    case "dialing":
    case "handshaking":
      return "bg-yellow-400 animate-pulse";
    case "failed":
      // Red only for real, action-needed failures; benign retrying states
      // (not reachable / timed out) glow calm amber, not alarm red.
      return humanizeReason(state.reason).severity === "error"
        ? "bg-red-400"
        : "bg-amber-400";
    default:
      return "bg-gray-500";
  }
}

function dotTitle(connected: boolean, state: PeerConnectionState): string {
  if (connected) return "Connected";
  switch (state.kind) {
    case "connected":
      return "Connected";
    case "dialing":
      return "Dialing...";
    case "handshaking":
      return "Authenticating...";
    case "failed":
      return humanizeReason(state.reason).text;
    default:
      return "Disconnected";
  }
}

/// "Xm ago" for a past unix-millis timestamp. Evaluated per render (the
/// panel re-renders on each status refresh), so no live ticking needed.
function relPast(ms: number): string {
  const diff = Date.now() - ms;
  if (diff < 60_000) return "just now";
  const m = Math.floor(diff / 60_000);
  if (m < 60) return `${m}m ago`;
  const h = Math.floor(m / 60);
  if (h < 24) return `${h}h ago`;
  return `${Math.floor(h / 24)}d ago`;
}

/// "retry in Ns/Nm" for a future unix-millis timestamp.
function retryHint(ms: number): string {
  const diff = ms - Date.now();
  if (diff <= 0) return "retrying…";
  const s = Math.ceil(diff / 1000);
  return s < 60 ? `retry in ${s}s` : `retry in ${Math.ceil(s / 60)}m`;
}

/// Map a raw backend failure `reason` to plain, calm, user-facing language.
/// The backend emits debug-flavored strings (e.g. "iroh dial timed out",
/// "iroh endpoint unavailable") that leak internal library names and read as
/// crashes. Users must NEVER see "iroh"/"dial"/etc. `severity` also decides
/// tone: "warn" (amber) for benign, self-healing states the reconnect loop is
/// already retrying; "error" (red) only for failures that need the user to act
/// (re-pair). Unknown reasons collapse to a calm generic — we never fall
/// through to the raw string.
type ReasonSeverity = "warn" | "error";
export function humanizeReason(reason: string): {
  text: string;
  severity: ReasonSeverity;
} {
  const r = reason.toLowerCase();
  // Genuine auth / identity failures — the link can't be restored without the
  // user re-pairing, so this is a real red state that needs action.
  if (
    r.includes("fingerprint") ||
    r.includes("roost id") ||
    r.includes("pin") ||
    r.includes("pair") ||
    r.includes("unauthorized") ||
    r.includes("rejected")
  ) {
    return { text: "This device needs to be re-paired", severity: "error" };
  }
  // Benign, self-healing: not reachable / dropped / timed out. The reconnect
  // supervisor is already retrying, so keep it calm (amber, not red).
  if (
    r.includes("timed out") ||
    r.includes("timeout") ||
    r.includes("unavailable") ||
    r.includes("connection lost") ||
    r.includes("not connected") ||
    r.includes("unreachable")
  ) {
    return {
      text: "Other device isn't reachable right now — retrying automatically",
      severity: "warn",
    };
  }
  // Unknown reason — stay calm and never leak internal jargon.
  return { text: "Connection problem — retrying", severity: "warn" };
}

/// Paired-devices list with status dot, failure reason, and per-row
/// Reconnect/Unpair actions. Hidden entirely when no peers are paired.
export default function PeerList(props: PeerListProps) {
  return (
    <Show when={props.knownPeers.length > 0}>
      <div class="space-y-1.5">
        <div class="text-text-secondary text-xs uppercase tracking-wide">
          Paired devices
        </div>
        <div class="space-y-1">
          <For each={props.knownPeers}>
            {(peer) => {
              const connected = () =>
                props.connectedPeers.some(
                  (c) => c.device_id === peer.device_id,
                );
              return (
                <div class="flex items-center justify-between bg-bg-secondary rounded px-3 py-2">
                  <div class="min-w-0 flex-1">
                    <div class="flex items-center gap-2">
                      <span
                        class={`w-1.5 h-1.5 rounded-full ${dotClass(connected(), peer.connection_state)}`}
                        title={dotTitle(connected(), peer.connection_state)}
                      />
                      <span class="text-text-primary text-sm truncate">
                        {peer.display_name}
                      </span>
                    </div>
                    <div class="text-text-muted text-[10px] font-mono truncate">
                      {peer.device_id.slice(0, 16)}…
                    </div>
                    <Show when={peer.connection_state.kind === "failed"}>
                      {(() => {
                        const info = humanizeReason(
                          (peer.connection_state as { reason: string }).reason,
                        );
                        return (
                          <div
                            class={`text-[10px] truncate ${
                              info.severity === "error"
                                ? "text-red-400"
                                : "text-amber-400"
                            }`}
                          >
                            {info.text}
                          </div>
                        );
                      })()}
                    </Show>
                    <div class="text-text-muted text-[10px] truncate">
                      <Show when={peer.pending_outbound > 0}>
                        <span class="text-amber-400">
                          {peer.pending_outbound} waiting
                        </span>
                      </Show>
                      <Show
                        when={
                          peer.pending_outbound > 0 &&
                          peer.last_connected_millis !== null
                        }
                      >
                        {" · "}
                      </Show>
                      <Show when={peer.last_connected_millis !== null}>
                        connected {relPast(peer.last_connected_millis!)}
                      </Show>
                      <Show
                        when={
                          !connected() &&
                          peer.next_retry_millis !== null &&
                          (peer.pending_outbound > 0 ||
                            peer.last_connected_millis !== null)
                        }
                      >
                        {" · "}
                      </Show>
                      <Show when={!connected() && peer.next_retry_millis !== null}>
                        <span class="text-text-secondary">
                          {retryHint(peer.next_retry_millis!)}
                        </span>
                      </Show>
                    </div>
                  </div>
                  <div class="flex items-center gap-1 ml-2">
                    <Show when={!connected() && peer.can_auto_reconnect}>
                      <button
                        class="text-text-muted hover:text-accent text-xs"
                        onClick={props.onReconnectAll}
                        title="Try to reconnect to this device now"
                      >
                        Reconnect
                      </button>
                    </Show>
                    <button
                      class="text-text-muted hover:text-red-400 text-xs"
                      onClick={() => props.onUnpair(peer.device_id)}
                      title="Remove this device"
                    >
                      Unpair
                    </button>
                  </div>
                </div>
              );
            }}
          </For>
        </div>
      </div>
    </Show>
  );
}
