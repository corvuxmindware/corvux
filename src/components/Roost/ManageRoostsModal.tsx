import { For, Show, createSignal, onCleanup, onMount } from "solid-js";
import { Portal } from "solid-js/web";
import { roostRegistry, type RoostEntry } from "../../stores/roosts";
import { roostStore } from "../../stores/roost";
import { appInvoke as invoke, isTauri, setBrowserRoost, commands, unwrap } from "../../lib/invoke";
import { errMsg } from "../../lib/error";
import { isAndroid as platformIsAndroid, isIOS as platformIsIOS } from "../../lib/platform";
import { toastStore } from "../Toast/Toast";
import CreateRoostModal from "./CreateRoostModal";

interface ManageRoostsModalProps {
  onClose: () => void;
}

const isAndroid = platformIsAndroid();
const isIOS = platformIsIOS();

export default function ManageRoostsModal(props: ManageRoostsModalProps) {
  const [createOpen, setCreateOpen] = createSignal(false);

  function handleKey(e: KeyboardEvent) {
    if (e.key === "Escape") props.onClose();
  }
  onMount(() => {
    document.addEventListener("keydown", handleKey);
    onCleanup(() => document.removeEventListener("keydown", handleKey));
  });

  async function handleOpen(entry: RoostEntry) {
    await roostStore.openRoost(entry.path, entry.displayName, entry.bookmark);
    props.onClose();
  }

  async function handleRemove(entry: RoostEntry) {
    const isActive =
      roostRegistry.activeId() === entry.id || roostStore.roostPath() === entry.path;
    const msg = isActive
      ? `Remove '${entry.displayName}' from the list? This is your active roost — the files on disk stay, but the app will close the roost.`
      : `Remove '${entry.displayName}' from the list? Files on disk are not deleted.`;
    if (!confirm(msg)) return;

    // MR-B3: offer to reclaim the roost's local index (graph.db + search.db +
    // file-tree cache) so removing a roost doesn't orphan a multi-MB index.
    // Only for non-active roosts — the active roost holds its DB files open,
    // which blocks deletion on Windows. The user's notes are never touched.
    if (isTauri && !isActive) {
      const alsoDelete = confirm(
        `Also delete Corvux's local index for '${entry.displayName}'? This frees disk space and rebuilds automatically if you reopen the roost. Your notes are not affected.`,
      );
      if (alsoDelete) {
        try {
          await unwrap(commands.deleteRoostIndex(entry.path));
        } catch (e) {
          // Non-fatal — an orphaned index is harmless; still drop the entry.
          toastStore.show(`Couldn't delete local index: ${errMsg(e)}`, "error");
        }
      }
    }

    if (isActive) {
      try {
        await roostStore.closeRoost();
      } catch (e) {
        toastStore.show(`Couldn't close active roost: ${errMsg(e)}`, "error");
        return;
      }
    }
    roostRegistry.remove(entry.id);
  }

  async function handleAddExisting() {
    if (!isTauri) {
      if (!("showDirectoryPicker" in window)) {
        toastStore.show("Folder picker not supported in this browser", "info");
        return;
      }
      try {
        const handle = await (window as Window & { showDirectoryPicker: (opts?: object) => Promise<FileSystemDirectoryHandle> }).showDirectoryPicker({ mode: "readwrite" });
        const roostPath = `/browser-roost/${handle.name}`;
        setBrowserRoost(handle, roostPath);
        await roostStore.openRoost(roostPath, handle.name);
        toastStore.show(`Roost opened: ${handle.name}`, "success");
        props.onClose();
      } catch (e) {
        const msg = errMsg(e);
        if (/abort/i.test(msg)) return;
        toastStore.show(`Add roost failed: ${msg}`, "error");
      }
      return;
    }
    try {
      if (isAndroid) {
        const picked = await invoke<{ uri: string; displayName: string }>(
          "plugin:saf|pick_folder",
        );
        await roostStore.openRoost(picked.uri, picked.displayName);
        toastStore.show(`Roost opened: ${picked.displayName}`, "success");
        props.onClose();
        return;
      }
      if (isIOS) {
        const picked = await invoke<{
          path: string;
          displayName: string;
          bookmark: string;
        }>("plugin:ios-picker|pick_folder");
        await roostStore.openRoost(picked.path, picked.displayName, picked.bookmark);
        toastStore.show(`Roost opened: ${picked.displayName}`, "success");
        props.onClose();
        return;
      }
      const { open } = await import("@tauri-apps/plugin-dialog");
      const selected = await open({
        directory: true,
        multiple: false,
        title: "Choose a Corvux Roost folder",
      });
      if (typeof selected === "string" && selected.length > 0) {
        await roostStore.openRoost(selected);
        props.onClose();
      }
    } catch (e) {
      const msg = errMsg(e);
      if (/cancel/i.test(msg)) return;
      toastStore.show(`Add roost failed: ${msg}`, "error");
    }
  }

  function typeLabel(v: RoostEntry): string {
    if (v.type === "saf") return "Android folder";
    if (v.type === "ios-scoped") return "iOS picked folder";
    return v.path;
  }

  return (
    // Portal to <body> so the fixed overlay centers on the viewport. Mounted
    // inside the Sidebar subtree, a transformed mobile-drawer ancestor would
    // otherwise trap `fixed` positioning inside the left pane.
    <Portal>
    <div class="fixed inset-0 z-50 flex items-center justify-center p-4">
      <div class="absolute inset-0 bg-black/60" onClick={props.onClose} />
      <div class="relative w-full max-w-lg max-h-[85vh] flex flex-col bg-bg-primary border border-border rounded-lg shadow-xl overflow-hidden">
        <div class="flex items-center justify-between px-4 py-3 border-b border-border">
          <h2 class="text-text-primary text-base font-medium">Manage Roosts</h2>
          <button
            onClick={props.onClose}
            class="w-8 h-8 flex items-center justify-center rounded hover:bg-bg-hover text-text-muted"
            aria-label="Close"
          >
            <svg
              width="18"
              height="18"
              viewBox="0 0 24 24"
              fill="none"
              stroke="currentColor"
              stroke-width="2"
              stroke-linecap="round"
              stroke-linejoin="round"
            >
              <path d="M18 6L6 18M6 6l12 12" />
            </svg>
          </button>
        </div>

        <div class="flex-1 overflow-y-auto">
          <Show
            when={roostRegistry.roosts().length > 0}
            fallback={
              <div class="px-4 py-10 text-center text-text-muted text-sm">
                No Roosts yet. Add an existing folder or create a new one.
              </div>
            }
          >
            <ul class="py-1">
              <For each={roostRegistry.roosts()}>
                {(entry) => {
                  const isActive = () =>
                    roostRegistry.activeId() === entry.id;
                  return (
                    <li class="flex items-center gap-3 px-4 py-3 hover:bg-bg-hover">
                      <div class="shrink-0 text-text-muted">
                        <Show
                          when={entry.type === "saf"}
                          fallback={
                            <svg
                              width="20"
                              height="20"
                              viewBox="0 0 24 24"
                              fill="none"
                              stroke="currentColor"
                              stroke-width="1.75"
                              stroke-linecap="round"
                              stroke-linejoin="round"
                            >
                              <path d="M22 19a2 2 0 01-2 2H4a2 2 0 01-2-2V5a2 2 0 012-2h5l2 3h9a2 2 0 012 2z" />
                            </svg>
                          }
                        >
                          <svg
                            width="20"
                            height="20"
                            viewBox="0 0 24 24"
                            fill="none"
                            stroke="currentColor"
                            stroke-width="1.75"
                            stroke-linecap="round"
                            stroke-linejoin="round"
                          >
                            <path d="M21 16V8a2 2 0 00-1-1.73l-7-4a2 2 0 00-2 0l-7 4A2 2 0 003 8v8a2 2 0 001 1.73l7 4a2 2 0 002 0l7-4A2 2 0 0021 16z" />
                          </svg>
                        </Show>
                      </div>
                      <div class="flex-1 min-w-0">
                        <div class="flex items-center gap-2">
                          <span class="text-[14px] text-text-primary truncate">
                            {entry.displayName}
                          </span>
                          <Show when={isActive()}>
                            <span class="text-[10px] px-1.5 py-0.5 rounded bg-accent/20 text-accent uppercase tracking-wide">
                              Active
                            </span>
                          </Show>
                        </div>
                        <div class="text-[11px] text-text-muted truncate">
                          {typeLabel(entry)}
                        </div>
                      </div>
                      <div class="flex gap-1 shrink-0">
                        <Show when={!isActive()}>
                          <button
                            onClick={() => handleOpen(entry)}
                            class="px-2.5 py-1 text-xs rounded border border-border text-text-primary hover:bg-bg-hover"
                          >
                            Open
                          </button>
                        </Show>
                        <button
                          onClick={() => handleRemove(entry)}
                          class="px-2.5 py-1 text-xs rounded border border-border text-text-muted hover:bg-red-500/20 hover:text-red-400 hover:border-red-500/40"
                          title="Remove from list (does not delete files)"
                        >
                          Remove
                        </button>
                      </div>
                    </li>
                  );
                }}
              </For>
            </ul>
          </Show>
        </div>

        <div class="flex gap-2 px-4 py-3 border-t border-border">
          <button
            onClick={handleAddExisting}
            class="flex-1 px-3 py-2 text-sm rounded border border-border text-text-primary hover:bg-bg-hover"
          >
            Add existing folder…
          </button>
          <button
            onClick={() => setCreateOpen(true)}
            class="flex-1 px-3 py-2 text-sm rounded bg-accent text-white hover:bg-accent-hover"
          >
            Create new Roost…
          </button>
        </div>
      </div>

      <Show when={createOpen()}>
        <CreateRoostModal
          onClose={() => setCreateOpen(false)}
          onCreated={() => {
            setCreateOpen(false);
            props.onClose();
          }}
        />
      </Show>
    </div>
    </Portal>
  );
}
