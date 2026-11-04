import { createSignal, Show } from "solid-js";

/// "How sync works" explainer — a collapsed-by-default help box shown near
/// the top of the Sync settings panel, so a user can read it *before*
/// deciding to enable sync (the rest of the panel only renders once sync is
/// on). Mirrors the "If sync gets stuck" recovery box's visual pattern.
///
/// The privacy copy is deliberately honest about the transport: notes are
/// end-to-end encrypted (no server can ever read them), but device discovery
/// + relay currently ride a third-party discovery service that can see the
/// devices' network addresses. We describe *what* is visible, not the vendor
/// name (which is slated to move to self-hosted). Do NOT regress this into
/// "no relays are ever used" — that was the old, inaccurate claim from the
/// pre-relay "strict P2P only" era and it does not match the shipped build
/// (iroh `RelayMode::Default` + `discovery_n0`). See engine/dial.rs.
export default function HowSyncWorks() {
  const [open, setOpen] = createSignal(false);

  return (
    <div class="border border-border rounded">
      <button
        class="w-full flex items-center justify-between px-3 py-2 text-xs text-text-secondary hover:bg-bg-hover"
        onClick={() => setOpen((v) => !v)}
        aria-expanded={open()}
      >
        <span class="font-medium">How sync works</span>
        <span class="text-text-muted">{open() ? "−" : "+"}</span>
      </button>
      <Show when={open()}>
        <div class="px-3 pb-3 text-[11px] text-text-muted leading-relaxed space-y-2 border-t border-border pt-2">
          <p>
            <strong class="text-text-secondary">What it does.</strong> Sync
            copies your notes between your own devices. No account, no cloud
            storage — your notes live on your devices, not on our servers.
          </p>
          <div>
            <strong class="text-text-secondary">How to set it up.</strong>
            <ol class="list-decimal list-inside mt-1 space-y-0.5">
              <li>
                On this device, turn on sync. It shows a QR code and a 6-digit
                PIN.
              </li>
              <li>
                On your other device: Settings → Sync → Scan, point it at the QR
                code, type the PIN.
              </li>
              <li>
                Done. After the first pairing, your devices reconnect on their
                own whenever both are open.
              </li>
            </ol>
          </div>
          <p>
            <strong class="text-text-secondary">Is it private?</strong> Yes. Your
            notes are end-to-end encrypted — no server can read them, ever. By
            default Corvux syncs <strong>directly between your own devices</strong>,
            with no Corvux server in the path. To help your devices find each
            other it does publish their network addresses to a third-party
            discovery service (it sees that your devices are online and their
            addresses — never your notes). Two optional features, both off by
            default and toggled in Sync settings, route your <em>encrypted</em>
            data through Corvux-operated infrastructure when you turn them on: a
            relay (for reaching a device on a different network) and a
            store-and-forward queue (for delivering to a device that's offline).
            Neither can ever read your notes.
          </p>
          <div>
            <strong class="text-text-secondary">Current limits.</strong>
            <ul class="list-disc list-inside mt-1 space-y-0.5">
              <li>Markdown notes and referenced image/audio attachments sync.</li>
              <li>Attachments over 8 MB stay local.</li>
              <li>
                Both devices need to be online around the same time unless the
                optional queue is configured.
              </li>
            </ul>
          </div>
        </div>
      </Show>
    </div>
  );
}
