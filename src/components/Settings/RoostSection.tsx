import { Show } from "solid-js";
import { roostStore } from "../../stores/roost";
import { graphStore } from "../../stores/graph";
import { appInvoke as invoke, isTauri } from "../../lib/invoke";
import { log } from "../../lib/log";
import { isAndroid as platformIsAndroid, isIOS as platformIsIOS } from "../../lib/platform";
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import { toastStore } from "../Toast/Toast";

/**
 * Roost section: current path, "Change Roost…" (duplicate of the sidebar
 * dropdown's Manage Roosts flow — kept for discoverability in Settings),
 * graph stats, sync + backup tips.
 */
export default function RoostSection() {
  async function handleChangeRoost() {
    if (!isTauri) {
      toastStore.show("Folder picker only works in the desktop app", "info");
      return;
    }
    const isAndroid = platformIsAndroid();
    const isIOS = platformIsIOS();
    try {
      if (isAndroid) {
        const picked = await invoke<{ uri: string; displayName: string }>(
          "plugin:saf|pick_folder",
        );
        await roostStore.openRoost(picked.uri, picked.displayName);
        toastStore.show(
          `Roost opened: ${picked.displayName || picked.uri}`,
          "success",
          4000,
        );
        return;
      }
      if (isIOS) {
        const picked = await invoke<{
          path: string;
          displayName: string;
          bookmark: string;
        }>("plugin:ios-picker|pick_folder");
        await roostStore.openRoost(picked.path, picked.displayName, picked.bookmark);
        toastStore.show(
          `Roost opened: ${picked.displayName || picked.path}`,
          "success",
          4000,
        );
        return;
      }
      const picked = await openDialog({
        directory: true,
        multiple: false,
        title: "Choose a Corvux Roost folder",
      });
      if (typeof picked !== "string" || picked.length === 0) return;
      await roostStore.openRoost(picked);
      toastStore.show(
        "Roost changed. Graph + search will reindex from markdown.",
        "info",
      );
    } catch (e) {
      const msg = typeof e === "string" ? e : String(e);
      if (/cancel/i.test(msg)) return;
      log.error("roost-section", "change roost failed", e);
      toastStore.show(`Change Roost failed: ${msg}`, "error");
    }
  }

  const stats = () => graphStore.stats();

  return (
    <section>
      <h3 class="text-text-secondary text-xs font-semibold uppercase tracking-wider mb-3">
        Roost
      </h3>
      <div class="space-y-2">
        <Show when={roostStore.roostPath()}>
          <div class="flex items-center justify-between">
            <div class="text-text-primary text-sm">Path</div>
            <div class="text-text-secondary text-xs bg-bg-secondary px-3 py-1 rounded-lg border border-border max-w-[280px] truncate">
              {roostStore.roostPath()}
            </div>
          </div>
        </Show>
        <div class="flex items-center justify-between">
          <div>
            <div class="text-text-primary text-sm">Roost folder</div>
            <div class="text-text-muted text-xs">
              The folder Corvux reads and writes this roost's Markdown files.
            </div>
          </div>
          <button
            onClick={handleChangeRoost}
            class="px-3 py-1.5 bg-bg-secondary border border-border text-text-primary rounded-md text-xs font-medium hover:bg-bg-hover transition-colors"
          >
            Change Roost...
          </button>
        </div>
        <Show when={graphStore.isReady()}>
          <div class="flex items-center justify-between">
            <div class="text-text-primary text-sm">Graph</div>
            <div class="text-text-secondary text-xs">
              {stats().notes} notes, {stats().links} links, {stats().tags} tags
            </div>
          </div>
        </Show>
        <div class="text-text-muted text-xs bg-bg-secondary border border-border rounded-md p-2 leading-snug space-y-2">
          <p>
            <span class="font-medium text-text-secondary">Sync:</span> to sync
            across your devices, open Settings → Sync and pair them with a QR
            code — your edits then flow directly between devices. Syncing across
            different networks or to a device that's offline is a Pro feature.
          </p>
          <p>
            <span class="font-medium text-text-secondary">Your data:</span> notes
            are plain Markdown files at the path above. To back up, copy that
            folder anywhere — to Dropbox, an external drive, or compress with your
            OS's right-click → archive. If you also keep the folder in a cloud-sync
            service (iCloud, Dropbox, Syncthing), Corvux writes a{" "}
            <code class="text-text-secondary">.stignore</code> so its local
            graph/search index stays device-private. The index rebuilds from your
            Markdown automatically; you don't need a separate export.
          </p>
        </div>
      </div>
    </section>
  );
}
