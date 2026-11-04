import { Show, createSignal, onCleanup, onMount } from "solid-js";
import { Portal } from "solid-js/web";
import { roostStore } from "../../stores/roost";
import { commands, isTauri, setBrowserRoost, unwrap } from "../../lib/invoke";
import { errMsg } from "../../lib/error";
import { log } from "../../lib/log";
import { isMobile } from "../../lib/platform";
import { toastStore } from "../Toast/Toast";

interface CreateRoostModalProps {
  onClose: () => void;
  onCreated: () => void;
}

const isMobileDevice = isMobile();

export default function CreateRoostModal(props: CreateRoostModalProps) {
  const [name, setName] = createSignal("");
  const [parentPath, setParentPath] = createSignal<string | null>(null);
  // Browser mode: keep the picked parent's FSAccess handle so we can actually
  // create a subfolder inside it on `Create`.
  const [parentHandle, setParentHandle] = createSignal<FileSystemDirectoryHandle | null>(null);
  const [creating, setCreating] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);

  function handleKey(e: KeyboardEvent) {
    if (e.key === "Escape") props.onClose();
    if (e.key === "Enter" && !creating() && canCreate()) handleCreate();
  }
  onMount(() => {
    document.addEventListener("keydown", handleKey);
    onCleanup(() => document.removeEventListener("keydown", handleKey));
  });

  function canCreate(): boolean {
    if (name().trim().length === 0) return false;
    if (!isMobileDevice && !parentPath()) return false;
    return true;
  }

  async function pickParent() {
    if (!isTauri) {
      if (!("showDirectoryPicker" in window)) return;
      try {
        const handle = await (window as Window & { showDirectoryPicker: (opts?: object) => Promise<FileSystemDirectoryHandle> }).showDirectoryPicker({ mode: "readwrite" });
        setParentHandle(handle);
        setParentPath(`/browser-roost/${handle.name}`);
      } catch (e) {
        const msg = errMsg(e);
        if (!/abort/i.test(msg)) log.error("roost", "parent dir picker failed", e);
      }
      return;
    }
    try {
      const { open } = await import("@tauri-apps/plugin-dialog");
      const selected = await open({
        directory: true,
        multiple: false,
        title: "Choose where to create the Roost",
      });
      if (typeof selected === "string" && selected.length > 0) {
        setParentPath(selected);
      }
    } catch (e) {
      log.error("roost", "parent dir picker failed", e);
    }
  }

  async function handleCreate() {
    setError(null);
    setCreating(true);
    try {
      if (isMobileDevice) {
        // App-private: <app_data_dir>/roosts/<slug>
        const r = await commands.createNamedRoost(name().trim());
        if (r.status === "error") throw r.error;
        const path = r.data;
        await roostStore.openRoost(path, name().trim());
        toastStore.show(`Roost created: ${name().trim()}`, "success");
      } else {
        const parent = parentPath();
        if (!parent) {
          setError("Pick a location first");
          return;
        }
        const folderName = name().trim();
        const path = `${parent}/${folderName}`;
        if (!isTauri) {
          const ph = parentHandle();
          if (!ph) {
            setError("Parent folder picker is required in browser dev mode");
            return;
          }
          // Create the subfolder inside the picked parent and bind it as
          // the active browser roost so subsequent file ops touch real disk.
          const subHandle = await ph.getDirectoryHandle(folderName, { create: true });
          setBrowserRoost(subHandle, path);
        } else {
          await unwrap(commands.createFolder(path));
        }
        await roostStore.openRoost(path, folderName);
        toastStore.show(`Roost created: ${folderName}`, "success");
      }
      props.onCreated();
    } catch (e) {
      const msg = errMsg(e);
      setError(msg);
    } finally {
      setCreating(false);
    }
  }

  return (
    // Portal to <body> — see ManageRoostsModal: a transformed mobile-drawer
    // ancestor would otherwise trap this fixed overlay inside the left pane.
    <Portal>
    <div class="fixed inset-0 z-[60] flex items-center justify-center p-4">
      <div class="absolute inset-0 bg-black/60" onClick={props.onClose} />
      <div class="relative w-full max-w-md bg-bg-primary border border-border rounded-lg shadow-xl overflow-hidden">
        <div class="flex items-center gap-2 px-4 py-3 border-b border-border">
          <button
            onClick={props.onClose}
            class="w-8 h-8 flex items-center justify-center rounded hover:bg-bg-hover text-text-muted"
            aria-label="Back"
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
              <path d="M19 12H5M12 19l-7-7 7-7" />
            </svg>
          </button>
          <h2 class="text-text-primary text-base font-medium">Create new Roost</h2>
        </div>

        <div class="p-4 space-y-3">
          <label class="block">
            <div class="text-text-primary text-sm mb-1">Roost name</div>
            <input
              type="text"
              value={name()}
              onInput={(e) => setName(e.currentTarget.value)}
              placeholder="My Notes"
              autofocus
              class="w-full px-3 py-2 bg-bg-secondary border border-border rounded text-text-primary text-sm placeholder:text-text-muted focus:outline-none focus:border-accent"
            />
          </label>

          <Show when={!isMobileDevice}>
            <div>
              <div class="text-text-primary text-sm mb-1">Location</div>
              <div class="flex gap-2">
                <div class="flex-1 px-3 py-2 bg-bg-secondary border border-border rounded text-text-secondary text-xs truncate">
                  {parentPath() ?? "— pick a parent folder —"}
                </div>
                <button
                  onClick={pickParent}
                  class="px-3 py-2 text-xs rounded border border-border text-text-primary hover:bg-bg-hover"
                >
                  Choose…
                </button>
              </div>
            </div>
          </Show>

          <Show when={isMobileDevice}>
            <div class="text-text-muted text-xs bg-bg-secondary border border-border rounded p-2 leading-snug">
              Stored inside the app's private folder. Use <b>Add existing
              folder…</b> instead to point at a folder you already keep in a
              cloud-sync service (iCloud, Dropbox, Syncthing).
            </div>
          </Show>

          <Show when={error()}>
            <div class="text-red-400 text-xs">{error()}</div>
          </Show>
        </div>

        <div class="flex justify-end gap-2 px-4 py-3 border-t border-border">
          <button
            onClick={props.onClose}
            class="px-3 py-2 text-sm rounded border border-border text-text-primary hover:bg-bg-hover"
          >
            Cancel
          </button>
          <button
            onClick={handleCreate}
            disabled={!canCreate() || creating()}
            class="px-4 py-2 text-sm rounded bg-accent text-white hover:bg-accent-hover disabled:opacity-50 disabled:cursor-not-allowed"
          >
            {creating() ? "Creating…" : "Create"}
          </button>
        </div>
      </div>
    </div>
    </Portal>
  );
}
