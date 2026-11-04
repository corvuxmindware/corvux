import { createSignal } from "solid-js";
import { errMsg } from "../../lib/error";
import { roostStore } from "../../stores/roost";
import { tabStore } from "../../stores/tabs";
import BottomSheet from "../BottomSheet/BottomSheet";
import { newNoteDestDir, relFolderLabel } from "../../lib/new-note-dest";

export interface NewNotePromptProps {
  onClose: () => void;
}

/**
 * Compact "create a note" bottom sheet — title input + Create / Cancel.
 * Extracted from MobileBottomBar so it can be re-used from the tab-switcher
 * footer and (later) from empty-state CTAs without duplicating the flow.
 */
export default function NewNotePrompt(props: NewNotePromptProps) {
  const [name, setName] = createSignal("");
  const [busy, setBusy] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);

  const canCreate = () => name().trim().length > 0 && !busy();

  // Where the note will be created — the open note's folder, else root.
  // Mirrors roostStore.createNewNote so the label matches the outcome.
  const destLabel = () => {
    const root = roostStore.roostPath() ?? "";
    return relFolderLabel(newNoteDestDir(tabStore.activeTabPath(), root), root);
  };

  async function handleCreate() {
    if (!canCreate()) return;
    const trimmed = name().trim();
    setBusy(true);
    setError(null);
    try {
      const path = await roostStore.createNewNote(trimmed);
      // Brand-new empty note — always land in write mode. Without this the
      // mobile "+ note" flow dropped the user on a LOCKED blank page and
      // there was no obvious way to start typing.
      if (path) tabStore.openTab(path, { readOnly: false });
      props.onClose();
    } catch (e) {
      const msg = errMsg(e);
      setError(msg || "Failed to create note");
    } finally {
      setBusy(false);
    }
  }

  return (
    <BottomSheet
      onClose={props.onClose}
      title="New note"
      ariaLabel="Create a new note"
    >
      <div class="p-4 space-y-3">
        <input
          type="text"
          placeholder="Note title…"
          value={name()}
          onInput={(e) => {
            setName(e.currentTarget.value);
            if (error()) setError(null);
          }}
          onKeyDown={(e) => {
            if (e.key === "Enter") void handleCreate();
          }}
          class="w-full px-3 py-2 rounded-md bg-bg-tertiary border border-border text-text-primary text-sm focus:outline-none focus:border-accent"
          autofocus
          aria-label="Note title"
          aria-invalid={error() !== null}
        />
        <div class="text-[12px] text-text-muted truncate" title={destLabel()}>
          New note in <span class="text-text-secondary">{destLabel()}</span>
        </div>
        {error() && (
          <div class="text-[12px] text-red-400" role="alert">
            {error()}
          </div>
        )}
        <div class="flex justify-end gap-2">
          <button
            onClick={props.onClose}
            class="px-3 py-1.5 rounded-md text-text-secondary hover:bg-bg-hover text-sm transition-colors"
          >
            Cancel
          </button>
          <button
            onClick={() => void handleCreate()}
            disabled={!canCreate()}
            class="px-3 py-1.5 rounded-md bg-accent text-bg-primary text-sm font-medium disabled:opacity-40 transition-opacity"
          >
            {busy() ? "Creating…" : "Create"}
          </button>
        </div>
      </div>
    </BottomSheet>
  );
}
