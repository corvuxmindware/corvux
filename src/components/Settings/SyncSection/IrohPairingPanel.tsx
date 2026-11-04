import {
  createSignal,
  createMemo,
  createEffect,
  onCleanup,
  Show,
  Match,
  Switch,
} from "solid-js";
import qrcode from "qrcode-generator";
import { commands, unwrap } from "../../../lib/invoke";
import { syncStatusStore } from "../../../stores/sync";
import QrScanner from "./QrScanner";

/// Cross-network sync pairing panel (iroh transport).
///
/// State machine, NOT a "show both halves and let the user figure it out"
/// layout. Pairing is asymmetric: one device shows a code, the other
/// enters it. Showing both options on both devices was demonstrably
/// confusing — users assumed they had to do both halves.
///
/// Modes:
///
/// - `choose`     — landing state. Two big buttons: show OR enter.
/// - `showing`    — host generated a code; render QR + PIN + waiting indicator.
/// - `entering`   — joiner is filling out the ticket + PIN form.
/// - `connecting` — joiner submitted; awaiting handshake completion.
/// - `connected`  — pair succeeded. Same surface used by both sides; the
///                  host transitions here when it receives `peer_connected`,
///                  the joiner transitions here when its connect call
///                  resolves successfully.
///
/// We listen on `sync://event` for `peer_connected` so the host's
/// "waiting" view updates automatically when someone scans + types the
/// PIN — that's the popup-style feedback the user asked for, just
/// inline in the panel instead of a modal.

type Mode =
  | { kind: "choose" }
  | { kind: "showing"; ticket: string; pin: string }
  | { kind: "entering" }
  | { kind: "connecting" }
  | { kind: "connected"; peer: string };

// The hosting PIN is now owned by the backend (sticky, get-or-create) and
// returned by `syncListenIroh` — the panel no longer mints one client-side,
// because a fresh PIN on every "Show code" click silently broke reconnect for
// every already-paired device.

export default function IrohPairingPanel() {
  const [mode, setMode] = createSignal<Mode>({ kind: "choose" });
  const [error, setError] = createSignal<string | null>(null);

  // Form state for the "entering" mode. Lives outside `mode` so the
  // user's typing doesn't get reset by an unrelated state transition
  // (e.g. a `peer_connected` event arriving while they're typing).
  const [peerTicket, setPeerTicket] = createSignal("");
  const [peerPin, setPeerPin] = createSignal("");
  // Camera QR-scan sub-state of the "entering" mode. When true, the panel
  // shows the live camera scanner instead of the paste form; a successful
  // scan fills `peerTicket` and flips back. Paste stays available as a
  // fallback for devices/users where the camera isn't an option.
  const [scanning, setScanning] = createSignal(false);

  // Subscribe to engine events so the host's "showing" panel auto-flips
  // to "connected" when the other device finishes pairing. Without
  // this, the host would have to refresh status manually to see what
  // happened — defeats the "just works" pairing UX.
  // AUDIT-027: subscribe via the syncStatusStore fan-out — one Tauri
  // listener feeds every consumer instead of opening a fourth one here.
  let unlisten: (() => void) | null = null;
  createEffect(() => {
    const m = mode();
    if (m.kind !== "showing") return;
    unlisten = syncStatusStore.onEvent((ev) => {
      if (ev.kind === "peer_connected") {
        setMode({
          kind: "connected",
          peer: ev.display_name || "the other device",
        });
      }
    });
    onCleanup(() => {
      if (unlisten) {
        unlisten();
        unlisten = null;
      }
    });
  });

  const showCode = async () => {
    setError(null);
    try {
      // rotate=false → reuse the persisted (sticky) hosting PIN so re-opening
      // this panel shows the SAME code and existing pairings keep working.
      const info = await unwrap(commands.syncListenIroh(false));
      setMode({ kind: "showing", ticket: info.ticket, pin: info.pin });
    } catch (e) {
      setError(String(e));
    }
  };

  const startEntering = () => {
    setError(null);
    setPeerTicket("");
    setPeerPin("");
    setScanning(false);
    setMode({ kind: "entering" });
  };

  const submitEntered = async () => {
    const ticket = peerTicket().trim();
    if (ticket.length === 0) {
      setError("Paste the code from the other device first");
      return;
    }
    if (peerPin().length !== 6) {
      setError("PIN must be 6 digits");
      return;
    }
    setError(null);
    setMode({ kind: "connecting" });
    try {
      await unwrap(commands.syncConnectIroh(ticket, peerPin()));
      // The success transition relies on the connect call returning a
      // peer device id; if we got here without an error, pairing
      // landed. Swap to "connected" so the user sees confirmation
      // immediately rather than waiting for the next status poll.
      setMode({ kind: "connected", peer: "the other device" });
    } catch (e) {
      setError(String(e));
      setMode({ kind: "entering" });
    }
  };

  const reset = () => {
    setError(null);
    setPeerTicket("");
    setPeerPin("");
    setScanning(false);
    setMode({ kind: "choose" });
  };

  // QR encoding lives at the panel level (not inside a `showing` branch)
  // so we don't recompute it whenever the user types in another field.
  // `qrcode-generator` allocates per call; gating with a memo keeps the
  // SVG stable across unrelated re-renders.
  const qrSvg = createMemo(() => {
    const m = mode();
    if (m.kind !== "showing") return null;
    // "L" error correction (vs "M"): the ticket is long (~287 chars), so the
    // QR is already dense. Lower EC = fewer modules = larger, easier for the
    // other device's camera to resolve. The link is shown on-screen at close
    // range, so the extra error-correction redundancy buys little here.
    const qr = qrcode(0, "L");
    qr.addData(m.ticket);
    qr.make();
    return qr.createSvgTag({ scalable: true, margin: 2 });
  });

  return (
    <div class="border border-border rounded p-3 space-y-3">
      <div class="flex items-baseline justify-between">
        <div class="text-text-secondary text-xs uppercase tracking-wide">
          Pair a device
        </div>
        <span class="text-text-muted text-[10px]">cross-network · beta</span>
      </div>

      <Switch>
        {/* ── Choose role ─────────────────────────────────────────── */}
        <Match when={mode().kind === "choose"}>
          <div class="space-y-2">
            <button
              class="w-full bg-accent/10 hover:bg-accent/20 border border-accent/40 text-accent rounded px-3 py-2.5 text-sm font-medium"
              onClick={showCode}
            >
              Show pairing code
              <div class="text-[10px] font-normal text-text-muted mt-0.5">
                On THIS device — the other one will scan
              </div>
            </button>
            <button
              class="w-full bg-bg-secondary hover:bg-bg-tertiary border border-border rounded px-3 py-2.5 text-sm font-medium"
              onClick={startEntering}
            >
              Enter pairing code
              <div class="text-[10px] font-normal text-text-muted mt-0.5">
                Paste a code another device just showed
              </div>
            </button>
            <Show when={error()}>
              <div class="text-red-400 text-xs">{error()}</div>
            </Show>
          </div>
        </Match>

        {/* ── Showing (host) ───────────────────────────────────────── */}
        <Match
          when={(() => {
            const m = mode();
            return m.kind === "showing" ? m : null;
          })()}
        >
          {(host) => (
            <div class="space-y-2">
              <div class="flex gap-3 items-center">
                <div
                  class="bg-white p-2 rounded shrink-0"
                  style={{ width: "200px", height: "200px" }}
                  innerHTML={qrSvg() ?? ""}
                />
                <div class="flex-1 space-y-2 min-w-0">
                  <div>
                    <div class="text-text-muted text-[10px] uppercase tracking-wide">
                      PIN
                    </div>
                    <div class="text-text-primary font-mono text-2xl tracking-widest">
                      {host().pin}
                    </div>
                  </div>
                  <button
                    class="text-accent text-xs hover:underline"
                    onClick={() => {
                      void navigator.clipboard.writeText(host().ticket);
                    }}
                  >
                    Copy code
                  </button>
                </div>
              </div>
              <div class="flex items-center gap-2 text-text-muted text-[11px]">
                <span class="w-2 h-2 rounded-full bg-blue-400 animate-pulse" />
                Waiting for the other device to scan and enter the PIN…
              </div>
              <button
                class="text-text-muted text-xs hover:text-text-secondary"
                onClick={reset}
              >
                Cancel
              </button>
            </div>
          )}
        </Match>

        {/* ── Entering (joiner) ────────────────────────────────────── */}
        <Match when={mode().kind === "entering" && scanning()}>
          <QrScanner
            onResult={(ticket) => {
              setPeerTicket(ticket);
              setScanning(false);
            }}
            onError={(msg) => {
              setError(msg);
              setScanning(false);
            }}
            onCancel={() => setScanning(false)}
          />
        </Match>

        <Match when={mode().kind === "entering" && !scanning()}>
          <div class="space-y-1.5">
            <div class="text-text-muted text-[11px]">
              Scan the QR on the other device, or paste its code — then type the
              PIN it shows.
            </div>
            <button
              class="w-full bg-accent/10 hover:bg-accent/20 border border-accent/40 text-accent rounded px-3 py-2 text-sm font-medium flex items-center justify-center gap-2"
              onClick={() => {
                setError(null);
                setScanning(true);
              }}
            >
              <span aria-hidden="true">⌖</span> Scan QR code
            </button>
            <div class="text-text-muted text-[10px] text-center">— or paste —</div>
            <textarea
              class="w-full bg-bg-secondary border border-border rounded px-2 py-1 text-[11px] font-mono resize-none"
              rows="2"
              placeholder="Paste pairing code"
              value={peerTicket()}
              onInput={(e) => setPeerTicket(e.currentTarget.value)}
            />
            <input
              class="w-full bg-bg-secondary border border-border rounded px-2 py-1 text-sm font-mono tracking-widest text-center"
              placeholder="6-digit PIN"
              maxLength={6}
              value={peerPin()}
              onInput={(e) =>
                setPeerPin(e.currentTarget.value.replace(/\D/g, ""))
              }
            />
            <button
              class="w-full bg-accent/10 hover:bg-accent/20 border border-accent/40 text-accent rounded px-3 py-1.5 text-sm font-medium disabled:opacity-50"
              onClick={submitEntered}
              disabled={
                peerPin().length !== 6 || peerTicket().trim().length === 0
              }
            >
              Connect
            </button>
            <button
              class="text-text-muted text-xs hover:text-text-secondary pt-1"
              onClick={reset}
            >
              Cancel
            </button>
            <Show when={error()}>
              <div class="text-red-400 text-xs">{error()}</div>
            </Show>
          </div>
        </Match>

        {/* ── Connecting ───────────────────────────────────────────── */}
        <Match when={mode().kind === "connecting"}>
          <div class="flex items-center gap-2 text-text-secondary text-sm py-3">
            <span class="w-2 h-2 rounded-full bg-blue-400 animate-pulse" />
            Connecting…
          </div>
        </Match>

        {/* ── Connected (success) ──────────────────────────────────── */}
        <Match
          when={(() => {
            const m = mode();
            return m.kind === "connected" ? m : null;
          })()}
        >
          {(done) => (
            <div class="space-y-2">
              <div class="flex items-center gap-2 text-green-400 text-sm py-1">
                <span class="w-2 h-2 rounded-full bg-green-400" />
                Paired with {done().peer}
              </div>
              <button
                class="w-full bg-bg-secondary hover:bg-bg-tertiary border border-border rounded px-3 py-1.5 text-sm"
                onClick={reset}
              >
                Done
              </button>
            </div>
          )}
        </Match>
      </Switch>
    </div>
  );
}
