import { Show } from "solid-js";
import type { SyncStatus } from "./types";
import { randomPin } from "./types";

export interface HostingPanelProps {
  status: SyncStatus;
  hostingPort: string;
  setHostingPort: (v: string) => void;
  hostingPin: string;
  setHostingPin: (v: string) => void;
  hostingError: string | null;
  startHosting: () => void;
}

/// "Host this device" form when the listener is offline, plus the
/// pairing-PIN reveal once the listener is bound. The PIN is rendered
/// large + spaced so the user can read it off the screen onto the other
/// device without cycling between windows.
export default function HostingPanel(props: HostingPanelProps) {
  return (
    <div class="space-y-1.5">
      <div class="text-text-secondary text-xs uppercase tracking-wide">
        Host this device
      </div>
      <Show
        when={props.status.running}
        fallback={
          <>
            <div class="flex gap-2">
              <input
                class="w-24 bg-bg-secondary border border-border rounded px-2 py-1 text-sm font-mono"
                placeholder="Port"
                value={props.hostingPort}
                onInput={(e) => props.setHostingPort(e.currentTarget.value)}
              />
              <input
                class="flex-1 bg-bg-secondary border border-border rounded px-2 py-1 text-sm font-mono tracking-widest text-center"
                placeholder="6-digit PIN"
                maxLength={6}
                value={props.hostingPin}
                onInput={(e) =>
                  props.setHostingPin(e.currentTarget.value.replace(/\D/g, ""))
                }
              />
              <button
                class="bg-bg-secondary hover:bg-bg-tertiary border border-border rounded px-2 py-1 text-xs"
                onClick={() => props.setHostingPin(randomPin())}
                title="Generate random PIN"
              >
                ⟳
              </button>
            </div>
            <button
              class="w-full bg-bg-secondary hover:bg-bg-tertiary border border-border rounded px-3 py-1.5 text-sm"
              onClick={props.startHosting}
              disabled={props.hostingPin.length !== 6}
            >
              Start listening
            </button>
            <Show when={props.hostingError}>
              <div class="text-red-400 text-xs">{props.hostingError}</div>
            </Show>
          </>
        }
      >
        <div class="bg-bg-secondary rounded px-3 py-2 space-y-2">
          <div class="flex items-center justify-between">
            <div class="text-sm text-text-primary">
              Listening on port{" "}
              <span class="font-mono">{props.status.listen_port}</span>
            </div>
            <div class="w-2 h-2 rounded-full bg-green-400 animate-pulse" />
          </div>
          <Show when={props.hostingPin.length === 6}>
            <div class="border-t border-border pt-2">
              <div class="text-text-secondary text-[10px] uppercase tracking-wide mb-1">
                Pairing PIN
              </div>
              <div class="text-text-primary text-2xl font-mono tracking-[0.4em] text-center select-all">
                {props.hostingPin}
              </div>
              <div class="text-text-muted text-[10px] mt-1 text-center">
                Enter this on the other device to pair
              </div>
            </div>
          </Show>
        </div>
      </Show>
    </div>
  );
}
