/**
 * Voice-memo capture sheet — opens fullscreen on mobile, modal on desktop.
 *
 * Flow: mount → request mic → record until user taps Stop → save to roost
 * `.attachments/` → append `![filename](.attachments/...)` to active tab.
 *
 * Locked Decision #11: user explicitly starts (by opening this sheet) and
 * explicitly stops (Stop button). No auto-stop, no background re-arming.
 * The recording indicator is unmistakable while the mic is hot.
 */
import { onMount, onCleanup, Show, createSignal } from "solid-js";
import { tabStore } from "../../stores/tabs";
import { roostStore } from "../../stores/roost";
import { saveAudio } from "../../lib/attachments";
import { appendAttachmentMarkdown } from "../../lib/insert-attachment";
import { createAudioRecorder, formatDuration } from "../../lib/audio-recorder";
import { toastStore } from "../Toast/Toast";
import { log } from "../../lib/log";
import RecorderBubble from "./RecorderBubble";

export interface RecorderSheetProps {
  onClose: () => void;
}

export default function RecorderSheet(props: RecorderSheetProps) {
  const recorder = createAudioRecorder();
  const [saving, setSaving] = createSignal(false);

  // Auto-start on mount — opening the sheet IS the "begin recording" intent.
  // Failure surfaces immediately via recorder.error() + a toast; sheet stays
  // open so the user can retry or cancel.
  onMount(() => {
    recorder.start().catch((e) => {
      const msg = e instanceof Error ? e.message : String(e);
      toastStore.show(`Mic unavailable: ${msg}`, "error", 4000);
    });
  });

  // Belt-and-suspenders: if the sheet unmounts while still recording (back
  // button, app backgrounded, route change), release the mic stream so we
  // never leak a hot mic out from under the user.
  onCleanup(() => {
    if (recorder.state() === "recording") recorder.cancel();
  });

  async function finishAndInsert() {
    if (recorder.state() !== "recording") return;
    setSaving(true);
    const blob = await recorder.stop();
    if (!blob) {
      setSaving(false);
      toastStore.show("Nothing recorded", "warning", 2500);
      props.onClose();
      return;
    }
    const roostPath = roostStore.roostPath();
    if (!roostPath) {
      setSaving(false);
      toastStore.show("Open a roost before recording", "error");
      props.onClose();
      return;
    }
    const filename = await saveAudio(roostPath, blob);
    setSaving(false);
    if (!filename) {
      toastStore.show("Failed to save recording", "error");
      props.onClose();
      return;
    }

    // Append markdown to the active tab. Standard `![alt](.attachments/...)`
    // shape mirrors the image-paste flow; the audio-embed editor extension
    // recognizes audio extensions in that pattern and renders <audio>.
    const tab = tabStore.tabs().find((t) => t.path === tabStore.activeTabPath());
    if (!tab) {
      toastStore.show(`Saved ${filename} — no active note to link from`, "info");
      props.onClose();
      return;
    }
    try {
      tabStore.updateContent(appendAttachmentMarkdown(tab.content, filename));
      toastStore.show(`Recording saved: ${filename}`, "success", 2500);
    } catch (e) {
      log.error("recorder", "failed to append link", e);
      toastStore.show("Saved audio but could not link it", "error");
    }
    props.onClose();
  }

  function discardAndClose() {
    recorder.cancel();
    props.onClose();
  }

  const isRecording = () => recorder.state() === "recording";
  const isRequesting = () => recorder.state() === "requesting";
  const isErrored = () => recorder.state() === "error";

  return (
    <div
      role="dialog"
      aria-label="Voice memo"
      aria-modal="true"
      class="fixed inset-0 z-50 flex items-center justify-center bg-black/60 backdrop-blur-sm"
      onClick={(e) => {
        // Tap outside the card cancels — same gesture as other modals.
        if (e.target === e.currentTarget) discardAndClose();
      }}
    >
      <div class="relative w-full max-w-sm mx-4 rounded-2xl bg-bg-secondary border border-border shadow-xl p-6 flex flex-col items-center gap-5">
        {/* Close X — top-right of the sheet. Same path as Discard: drops
            the in-flight recording (if any) and closes the modal. Sits
            in the corner so it's discoverable without competing with
            the prominent Stop & Save / Discard buttons at the bottom. */}
        <button
          type="button"
          onClick={discardAndClose}
          aria-label="Close voice memo"
          class="absolute top-3 right-3 w-7 h-7 flex items-center justify-center rounded-full text-text-muted hover:text-text-primary hover:bg-bg-hover transition-colors"
        >
          <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round">
            <line x1="6" y1="6" x2="18" y2="18" />
            <line x1="18" y1="6" x2="6" y2="18" />
          </svg>
        </button>

        <div class="text-sm font-medium text-text-secondary uppercase tracking-wider">
          {isRecording()
            ? "Recording"
            : isRequesting()
              ? "Requesting mic..."
              : isErrored()
                ? "Microphone error"
                : saving()
                  ? "Saving..."
                  : "Voice memo"}
        </div>

        <RecorderBubble recorder={recorder} isRecording={isRecording} />

        <div class="text-3xl font-mono tabular-nums text-text-primary">
          {formatDuration(recorder.durationMs())}
        </div>

        <Show when={isErrored()}>
          <div class="text-sm text-red-400 text-center px-2">
            {recorder.error() ?? "Microphone unavailable"}
          </div>
        </Show>

        <div class="flex gap-3 w-full">
          <button
            type="button"
            onClick={discardAndClose}
            disabled={saving()}
            class="flex-1 h-11 rounded-full bg-bg-tertiary text-text-secondary hover:text-text-primary active:bg-bg-hover transition-colors disabled:opacity-40"
          >
            Discard
          </button>
          <button
            type="button"
            onClick={finishAndInsert}
            disabled={!isRecording() || saving()}
            class="flex-1 h-11 rounded-full bg-accent text-white font-medium hover:opacity-90 active:opacity-80 transition-opacity disabled:opacity-40"
          >
            {saving() ? "Saving..." : "Stop & Save"}
          </button>
        </div>
      </div>
    </div>
  );
}
