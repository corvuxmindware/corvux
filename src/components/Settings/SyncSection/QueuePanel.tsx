import { createSignal, onCleanup, onMount, Show } from "solid-js";
import { commands, unwrap } from "../../../lib/invoke";
import type { QueueSettings } from "../../../lib/bindings";
import { syncStatusStore } from "../../../stores/sync";
import { log } from "../../../lib/log";

/// Settings → Sync → Queue panel.
///
/// Wraps the four queue IPC commands into a single panel:
/// - `sync_get_queue_settings`     — endpoint + strict_p2p_only + active
/// - `sync_set_queue_endpoint`     — set/clear endpoint URL
/// - `sync_set_strict_p2p_only`    — toggle strict P2P-only mode
/// - `sync_pull_queue_now`         — manual drain
///
/// The store-and-forward queue is OFF by default; users opt in by typing
/// an endpoint URL. Strict P2P-only mode hard-disables it even with an
/// endpoint set, for users who don't want any Corvux-operated server in
/// their sync path.

type Tone = "info" | "success" | "error";

export default function QueuePanel() {
  const [settings, setSettings] = createSignal<QueueSettings | null>(null);
  const [endpointDraft, setEndpointDraft] = createSignal("");
  const [pulling, setPulling] = createSignal(false);
  const [draining, setDraining] = createSignal(false);
  const [saving, setSaving] = createSignal(false);
  const [pending, setPending] = createSignal(0);
  const [msg, setMsg] = createSignal<{ tone: Tone; text: string } | null>(null);

  const flash = (tone: Tone, text: string) => {
    setMsg({ tone, text });
    window.setTimeout(() => setMsg(null), 5000);
  };

  const refresh = async () => {
    try {
      const s = await unwrap(commands.syncGetQueueSettings());
      setSettings(s);
      setEndpointDraft(s.endpoint);
    } catch (e) {
      // sync_init not yet called — leave panel in unloaded state.
      log.warn("sync", "queue settings unavailable", e);
    }
    try {
      const ob = await unwrap(commands.syncOutboxStatus());
      setPending(ob.total_pending);
    } catch (e) {
      log.warn("sync", "outbox status unavailable", e);
    }
  };

  onMount(() => {
    void refresh();
    // Live pending-count updates: the backend emits `outbox_changed` when a
    // change is queued for retry or one drains. The event carries the total,
    // so no refetch is needed.
    const unlisten = syncStatusStore.onEvent((ev) => {
      if (ev.kind === "outbox_changed") setPending(ev.total_pending);
    });
    onCleanup(unlisten);
  });

  const drainNow = async () => {
    if (draining()) return;
    setDraining(true);
    try {
      const remaining = await unwrap(commands.syncDrainOutboxNow());
      setPending(remaining);
      flash(
        "success",
        remaining === 0
          ? "All pending changes delivered"
          : `${remaining} change${remaining === 1 ? "" : "s"} still waiting — will retry`,
      );
    } catch (e) {
      flash("error", `Retry failed: ${e}`);
    } finally {
      setDraining(false);
    }
  };

  const saveEndpoint = async () => {
    const draft = endpointDraft().trim();
    setSaving(true);
    try {
      await unwrap(commands.syncSetQueueEndpoint(draft));
      await refresh();
      flash(
        "success",
        draft === "" ? "Queue disabled" : "Queue endpoint saved",
      );
    } catch (e) {
      flash("error", `Save failed: ${e}`);
    } finally {
      setSaving(false);
    }
  };

  const toggleStrict = async () => {
    const cur = settings();
    if (!cur) return;
    setSaving(true);
    try {
      await unwrap(commands.syncSetStrictP2pOnly(!cur.strict_p2p_only));
      await refresh();
    } catch (e) {
      flash("error", `Toggle failed: ${e}`);
    } finally {
      setSaving(false);
    }
  };

  const pullNow = async () => {
    if (pulling()) return;
    setPulling(true);
    try {
      const count = await unwrap(commands.syncPullQueueNow());
      flash(
        "success",
        count === 0
          ? "Queue empty — nothing to apply"
          : `Applied ${count} message${count === 1 ? "" : "s"} from queue`,
      );
      await refresh();
    } catch (e) {
      flash("error", `Pull failed: ${e}`);
    } finally {
      setPulling(false);
    }
  };

  return (
    <Show when={settings()}>
      {(s) => (
        <div class="border border-border rounded p-3 space-y-2">
          <div class="flex items-center justify-between">
            <div class="text-text-secondary text-xs font-semibold">
              Store-and-forward queue
            </div>
            <div class="flex items-center gap-1.5">
              <Show when={pending() > 0}>
                <span
                  class="text-[10px] rounded-full px-2 py-0.5 bg-amber-400/10 text-amber-400"
                  title="Local changes waiting to reach an offline device"
                >
                  {pending()} waiting
                </span>
              </Show>
              <span
                class={`text-[10px] rounded-full px-2 py-0.5 ${
                  s().active
                    ? "bg-green-400/10 text-green-400"
                    : "bg-bg-secondary text-text-muted"
                }`}
              >
                {s().active ? "active" : "off"}
              </span>
            </div>
          </div>

          <p class="text-text-muted text-[11px] leading-snug">
            Buffers encrypted change-packets when a paired device is offline.
            We can't read your notes — payloads are end-to-end encrypted with
            a per-pair key derived during pairing.
          </p>

          <div class="space-y-1.5">
            <label class="text-text-secondary text-xs">Endpoint URL</label>
            <div class="flex gap-1.5">
              <input
                class="flex-1 bg-bg-secondary border border-border rounded px-2 py-1 text-xs font-mono"
                placeholder="https://queue.example.com"
                value={endpointDraft()}
                onInput={(e) => setEndpointDraft(e.currentTarget.value)}
                disabled={saving() || s().strict_p2p_only}
              />
              <button
                class="bg-bg-secondary hover:bg-bg-tertiary border border-border rounded px-2.5 py-1 text-xs disabled:opacity-50"
                onClick={saveEndpoint}
                disabled={
                  saving() ||
                  s().strict_p2p_only ||
                  endpointDraft().trim() === s().endpoint
                }
              >
                Save
              </button>
            </div>
            <Show when={endpointDraft().trim() === "" && s().endpoint !== ""}>
              <div class="text-text-muted text-[10px]">
                Saving an empty URL disables the queue.
              </div>
            </Show>
          </div>

          <label class="flex items-center gap-2 cursor-pointer group">
            <span class="relative inline-flex shrink-0">
              <input
                type="checkbox"
                checked={s().strict_p2p_only}
                onChange={toggleStrict}
                disabled={saving()}
                class="sr-only peer"
              />
              <span
                class={`w-9 h-5 rounded-full transition-colors ${
                  s().strict_p2p_only
                    ? "bg-purple"
                    : "bg-bg-tertiary border border-border"
                } peer-focus-visible:ring-2 peer-focus-visible:ring-purple/40 peer-disabled:opacity-50`}
                aria-hidden="true"
              />
              <span
                class={`absolute top-0.5 left-0.5 w-4 h-4 rounded-full bg-white shadow transition-transform ${
                  s().strict_p2p_only ? "translate-x-4" : "translate-x-0"
                }`}
                aria-hidden="true"
              />
            </span>
            <span class="text-xs text-text-secondary">
              Only sync directly between my devices
            </span>
          </label>
          <Show when={s().strict_p2p_only}>
            <div class="text-text-muted text-[10px] pl-5 -mt-1 space-y-1">
              <p>
                Live sync only. Devices must be online together for changes to
                propagate.
              </p>
              <p>
                This turns off the Corvux queue and relay forwarding. It does
                not yet disable device discovery: to help your devices find each
                other, their NodeID and network addresses are still published to
                a third-party discovery service. Your notes stay end-to-end
                encrypted and are never exposed — but the fact that your devices
                are online, and their network addresses, remain visible to that
                service. For zero third-party visibility, run your devices on
                your own private network (e.g. a VPN such as Tailscale).
              </p>
            </div>
          </Show>

          <div class="flex items-center justify-between pt-1">
            <div class="flex items-center gap-1.5">
              <button
                class="text-xs bg-bg-secondary hover:bg-bg-tertiary border border-border rounded px-2.5 py-1 disabled:opacity-50"
                onClick={pullNow}
                disabled={pulling() || !s().active}
                title={
                  s().active
                    ? "Fetch any pending messages from the queue now"
                    : "Set an endpoint and disable strict P2P to enable"
                }
              >
                {pulling() ? "Pulling…" : "Sync queue now"}
              </button>
              <Show when={pending() > 0 && s().active}>
                <button
                  class="text-xs bg-bg-secondary hover:bg-bg-tertiary border border-border rounded px-2.5 py-1 disabled:opacity-50"
                  onClick={drainNow}
                  disabled={draining()}
                  title="Retry sending changes still waiting for an offline device"
                >
                  {draining() ? "Retrying…" : "Retry pending"}
                </button>
              </Show>
            </div>
            <Show when={msg()}>
              {(m) => (
                <span
                  class={`text-[11px] truncate ml-2 ${
                    m().tone === "error"
                      ? "text-red-400"
                      : m().tone === "success"
                        ? "text-green-400"
                        : "text-text-secondary"
                  }`}
                >
                  {m().text}
                </span>
              )}
            </Show>
          </div>
        </div>
      )}
    </Show>
  );
}
