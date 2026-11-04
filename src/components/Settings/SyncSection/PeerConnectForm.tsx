import { Show } from "solid-js";

export interface PeerConnectFormProps {
  peerAddr: string;
  setPeerAddr: (v: string) => void;
  peerPin: string;
  setPeerPin: (v: string) => void;
  connecting: boolean;
  connectError: string | null;
  connectPeer: () => void;
  pinInputRef: (el: HTMLInputElement) => void;
}

/// "Connect to another device" outbound-pair form. Address + PIN entry,
/// validation, and an error slot. The PIN input is exposed via a ref so
/// the Nearby list can focus it after the user picks a discovered peer.
export default function PeerConnectForm(props: PeerConnectFormProps) {
  return (
    <div class="space-y-1.5">
      <div class="text-text-secondary text-xs uppercase tracking-wide">
        Connect to another device
      </div>
      <input
        class="w-full bg-bg-secondary border border-border rounded px-2 py-1 text-sm font-mono"
        placeholder="192.168.1.5:7337"
        value={props.peerAddr}
        onInput={(e) => props.setPeerAddr(e.currentTarget.value)}
      />
      <input
        ref={props.pinInputRef}
        class="w-full bg-bg-secondary border border-border rounded px-2 py-1 text-sm font-mono tracking-widest text-center"
        placeholder="6-digit PIN from other device"
        maxLength={6}
        value={props.peerPin}
        onInput={(e) =>
          props.setPeerPin(e.currentTarget.value.replace(/\D/g, ""))
        }
      />
      <button
        class="w-full bg-bg-secondary hover:bg-bg-tertiary border border-border rounded px-3 py-1.5 text-sm disabled:opacity-50"
        onClick={props.connectPeer}
        disabled={props.connecting || props.peerPin.length !== 6}
      >
        {props.connecting ? "Connecting…" : "Pair"}
      </button>
      <Show when={props.connectError}>
        <div class="text-red-400 text-xs">{props.connectError}</div>
      </Show>
    </div>
  );
}
