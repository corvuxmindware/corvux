import { Show, createSignal } from "solid-js";
import BottomSheet from "../BottomSheet/BottomSheet";
import { roostStore } from "../../stores/roost";
import { appInvoke as invoke } from "../../lib/invoke";
import { log } from "../../lib/log";

/**
 * Shown when `open_roost` returns `SCOPE_EXPIRED` — an iOS security-scoped
 * bookmark the OS won't resume (stale after an app update / iCloud change /
 * revoked grant). Instead of a dead-end permission error, we ask the user
 * to re-grant access by re-running the document picker for the SAME roost,
 * which mints a fresh bookmark. The roost store drives visibility via
 * `reauthRequest`; this is effectively iOS-only (the backend only emits
 * SCOPE_EXPIRED there).
 */
export default function ReauthPrompt() {
  const [busy, setBusy] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);

  async function reGrant() {
    const req = roostStore.reauthRequest();
    if (!req) return;
    setBusy(true);
    setError(null);
    try {
      const picked = await invoke<{
        path: string;
        displayName: string;
        bookmark: string;
      }>("plugin:ios-picker|pick_folder");
      await roostStore.openRoost(
        picked.path,
        req.displayName || picked.displayName,
        picked.bookmark,
      );
      roostStore.clearReauthRequest();
    } catch (e) {
      log.warn("reauth", "re-grant failed", e);
      setError(
        "Couldn't open that folder. Pick the same roost folder you used before.",
      );
    } finally {
      setBusy(false);
    }
  }

  return (
    <Show when={roostStore.reauthRequest()}>
      {(req) => (
        <BottomSheet
          onClose={() => roostStore.clearReauthRequest()}
          title="Re-grant folder access"
          ariaLabel="Re-grant access to your roost"
        >
          <div class="p-4 space-y-3">
            <p class="text-text-secondary text-sm leading-relaxed">
              Corvux lost access to{" "}
              <span class="text-text-primary font-medium">{req().displayName}</span>.
              This can happen after an app update or an iCloud change. Choose the
              same folder again to re-grant access — your notes are untouched.
            </p>
            <Show when={error()}>
              <div class="text-[12px] text-red-400" role="alert">
                {error()}
              </div>
            </Show>
            <div class="flex justify-end gap-2">
              <button
                onClick={() => roostStore.clearReauthRequest()}
                class="px-3 py-1.5 rounded-md text-text-secondary hover:bg-bg-hover text-sm transition-colors"
              >
                Not now
              </button>
              <button
                onClick={() => void reGrant()}
                disabled={busy()}
                class="px-3 py-1.5 rounded-md bg-accent text-bg-primary text-sm font-medium disabled:opacity-40 transition-opacity"
              >
                {busy() ? "Opening…" : "Choose folder"}
              </button>
            </div>
          </div>
        </BottomSheet>
      )}
    </Show>
  );
}
