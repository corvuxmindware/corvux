import { Show, createEffect, createMemo, createSignal, onCleanup } from "solid-js";
import { syncStatusStore } from "../../stores/sync";
import { isTauri } from "../../lib/invoke";
import { isMobile } from "../../lib/platform";

// The ↓N ↑N transfer readout must only appear when a peer is actually
// connected — then a number honestly means "moving data right now". With no
// peer, `outgoing()` is just the durable outbox backlog (changes waiting for a
// peer that isn't there), and painting "↑N" reads as "uploading N" on a roost
// the user may never have synced — misleading, and corrosive to the local-first
// trust promise. (Desktop bug #3, 0.2.1.)
export function shouldShowTransferCounts(
  connectedPeers: number,
  incoming: number,
  outgoing: number,
): boolean {
  return connectedPeers > 0 && (incoming > 0 || outgoing > 0);
}

interface SyncIndicatorProps {
  // Called only when the user has never set sync up (no paired peers,
  // no saved prefs). Indicates we should drop them into Settings →
  // Sync to go through first-time pairing. A plain click on an already-
  // configured indicator never opens settings.
  onNeedsSetup: () => void;
}

// Compact sync status chip rendered in TopBar — desktop + mobile. States:
//   - off      : engine not running, no prior pairing — opens settings on click
//   - listening: engine up, no peers connected yet
//   - syncing  : activity within the last few seconds — icon spins
//   - connected: peers paired — accent tint + peer count
// A click never opens Settings automatically unless the user has nothing
// to sync to (never paired). It kicks the engine/reconnect path instead.
export default function SyncIndicator(props: SyncIndicatorProps) {
  // Re-render once per second so the "spin while recent activity" derived
  // value can settle back to false without us having to track a timer.
  // createMemo caches the comparison so Solid only actually repaints
  // when the derived truthiness flips.
  const [now, setNow] = createSignal(Date.now());
  const tick = window.setInterval(() => setNow(Date.now()), 1000);
  onCleanup(() => window.clearInterval(tick));

  // Active = icon should spin. Spin whenever:
  //   - We're actively attempting to connect (user tap or mDNS auto-dial)
  //   - A sync event fired in the last 3 seconds (peer exchange in-flight)
  const active = createMemo(() => {
    if (syncStatusStore.connecting()) return true;
    const last = syncStatusStore.lastActivityAt();
    if (!last) return false;
    return now() - last < 3000;
  });

  const state = createMemo(() => {
    if (syncStatusStore.connectedPeers() > 0) return "connected";
    if (syncStatusStore.listening()) return "listening";
    return "off";
  });

  // Presence-dot colour (Tailwind theme token). Empty string = no dot.
  // Connecting wins over the base state so a mid-dial reconnect reads as
  // "working on it" (yellow, pulsing) rather than a flat gray/green.
  const dotColor = createMemo(() => {
    if (syncStatusStore.connecting()) return "bg-yellow";
    switch (state()) {
      case "connected":
        return "bg-green";
      case "listening":
        return "bg-text-muted";
      default:
        return "";
    }
  });

  const title = createMemo(() => {
    const peers = syncStatusStore.connectedPeers();
    const inn = syncStatusStore.incoming();
    const out = syncStatusStore.outgoing();
    if (syncStatusStore.connecting()) return "Sync · reconnecting…";
    if (!syncStatusStore.listening() && peers === 0) {
      return "Sync off — tap to set up or reconnect";
    }
    // Only assert in/out traffic when a peer is actually connected; with no
    // peer the outbox depth is a dormant backlog, not live upload (bug #3).
    if (peers === 0) return "Sync · listening · no peers yet";
    const activity = active() ? " · syncing now" : "";
    return `Sync · ${peers} peer${peers === 1 ? "" : "s"} · ${inn} in · ${out} out${activity}`;
  });

  // Transient tap feedback — shows a short "…" suffix after a click so
  // the user sees something happened even when the backend takes a
  // second to respond (typical mDNS round-trip on Android).
  const [tapFlash, setTapFlash] = createSignal<string | null>(null);
  let flashTimer: number | null = null;
  function flash(msg: string, ms = 2000) {
    if (flashTimer !== null) window.clearTimeout(flashTimer);
    setTapFlash(msg);
    flashTimer = window.setTimeout(() => setTapFlash(null), ms) as unknown as number;
  }
  onCleanup(() => {
    if (flashTimer !== null) window.clearTimeout(flashTimer);
  });

  // Keep tapFlash in sync with connecting state — any time we start
  // connecting, surface "reconnecting…" right away without waiting on
  // the click handler to set it.
  createEffect(() => {
    if (syncStatusStore.connecting()) flash("Reconnecting…", 4000);
  });

  async function handleClick() {
    const result = await syncStatusStore.triggerSync();
    switch (result) {
      case "running":
        flash("Sync is live");
        break;
      case "autostarted":
        flash("Reconnecting…", 4000);
        break;
      case "needs-setup":
        // Only case where we route through Settings — the user has no
        // paired peer to reconnect to yet. First-run bootstrap.
        props.onNeedsSetup();
        break;
      case "error":
        flash("Sync unavailable");
        break;
    }
  }

  return (
    <Show when={isTauri}>
      <button
        onClick={handleClick}
        class={`flex items-center justify-center gap-1.5 px-2 py-1 min-h-[44px] min-w-[44px] sm:min-h-0 sm:min-w-0 rounded text-xs transition-colors ${
          state() === "connected"
            ? "text-accent hover:bg-bg-hover"
            : state() === "listening"
              ? "text-text-secondary hover:bg-bg-hover"
              : "text-text-muted hover:text-text-secondary hover:bg-bg-hover opacity-60"
        }`}
        title={title()}
        aria-label={title()}
      >
        <span class="relative inline-flex">
          <svg
            width="14"
            height="14"
            viewBox="0 0 24 24"
            fill="none"
            stroke="currentColor"
            stroke-width="2"
            stroke-linecap="round"
            stroke-linejoin="round"
            class={active() ? "animate-pulse" : ""}
            style={active() ? { "animation-duration": "1.2s" } : undefined}
          >
            {/* Incoming: down arrow (left). Outgoing: up arrow (right).
                Shafts bow outward so the pair reads as a soft circular
                sync glyph — NOT a single circular-arrow "refresh/reload"
                symbol, which users kept tapping expecting a reload. */}
            <path d="M9 4 C5 8 5 13 7.5 16.5" />
            <polyline points="4 13 7.5 16.5 11 13" />
            <path d="M15 20 C19 16 19 11 16.5 7.5" />
            <polyline points="13 11 16.5 7.5 20 11" />
          </svg>
          {/* Presence dot on the glyph corner: at-a-glance "is the other
              device online?". Green = a peer link is live (for a 2-device
              pair, the other device is reachable right now); yellow-pulse =
              dialing/handshaking; gray = engine up but nobody connected. No
              dot when sync is off entirely, so an unconfigured indicator
              stays visually quiet. `ring` in the top-bar background colour
              keeps the dot legible where it overlaps the arrows. */}
          <Show when={dotColor()}>
            {(color) => (
              <span
                class={`absolute -top-0.5 -right-0.5 w-1.5 h-1.5 rounded-full ring-2 ring-bg-primary ${color()} ${
                  syncStatusStore.connecting() ? "animate-pulse" : ""
                }`}
                style={syncStatusStore.connecting() ? { "animation-duration": "1.2s" } : undefined}
              />
            )}
          </Show>
        </span>
        <Show when={syncStatusStore.connectedPeers() > 0}>
          <span class="tabular-nums">{syncStatusStore.connectedPeers()}</span>
        </Show>
        <Show
          when={
            !isMobile() &&
            shouldShowTransferCounts(
              syncStatusStore.connectedPeers(),
              syncStatusStore.incoming(),
              syncStatusStore.outgoing(),
            )
          }
        >
          <span class="text-[10px] text-text-muted tabular-nums">
            ↓{syncStatusStore.incoming()} ↑{syncStatusStore.outgoing()}
          </span>
        </Show>
        <Show when={!isMobile() && tapFlash()}>
          <span class="text-[10px] text-text-muted">{tapFlash()}</span>
        </Show>
      </button>
    </Show>
  );
}
