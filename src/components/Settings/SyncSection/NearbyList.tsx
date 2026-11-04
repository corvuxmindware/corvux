import { For, Show } from "solid-js";
import { roostStore } from "../../../stores/roost";
import type { NearbyPeer } from "./types";

export interface NearbyListProps {
  nearby: Record<string, NearbyPeer>;
  onPick: (peer: NearbyPeer) => void;
}

/// Mirror the Rust-side `roost_folder_name` helper exactly: SAF
/// `content://` URIs URL-decode then take the last segment; regular
/// paths just take the last separator-split segment. The two MUST agree
/// or the Pair button greys out for a roost that the handshake would
/// actually accept.
function ourRoostFolderName(): string {
  const path = roostStore.roostPath();
  if (!path) return "";
  if (path.startsWith("content://")) {
    const afterScheme = path.slice("content://".length);
    let decoded: string;
    try {
      decoded = decodeURIComponent(afterScheme);
    } catch {
      decoded = afterScheme;
    }
    const segs = decoded.split("/").filter(Boolean);
    return segs[segs.length - 1] ?? "";
  }
  return path.split(/[\\/]/).filter(Boolean).pop() ?? "";
}

/// mDNS-discovered peers on the same WiFi. Only renders when at least
/// one peer has been advertised — silent when the list is empty so the
/// user doesn't see an empty section heading.
export default function NearbyList(props: NearbyListProps) {
  return (
    <Show when={Object.keys(props.nearby).length > 0}>
      <div class="space-y-1.5">
        <div class="text-text-secondary text-xs uppercase tracking-wide">
          Nearby on this WiFi
        </div>
        <div class="space-y-1">
          <For each={Object.values(props.nearby)}>
            {(peer) => {
              const ourRoost = () => ourRoostFolderName();
              const roostMatches = () => peer.roost_name === ourRoost();
              return (
                <div class="flex items-center justify-between bg-bg-secondary rounded px-3 py-2">
                  <div class="min-w-0 flex-1">
                    <div class="text-text-primary text-sm truncate">
                      {peer.display_name}
                    </div>
                    <div class="text-text-muted text-[10px] font-mono truncate">
                      {peer.addr}:{peer.port} · roost {peer.roost_name}
                      <Show when={!roostMatches()}>
                        <span class="text-amber-400 ml-1">(mismatch)</span>
                      </Show>
                    </div>
                  </div>
                  <button
                    class="ml-2 text-xs px-2 py-1 rounded border border-border hover:bg-bg-tertiary disabled:opacity-40 disabled:cursor-not-allowed"
                    onClick={() => props.onPick(peer)}
                    disabled={!roostMatches()}
                    title={
                      roostMatches()
                        ? "Use this address"
                        : `Roost name must match — this device has "${peer.roost_name}", you have "${ourRoost()}"`
                    }
                  >
                    Pair
                  </button>
                </div>
              );
            }}
          </For>
        </div>
      </div>
    </Show>
  );
}
