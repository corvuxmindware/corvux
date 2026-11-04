/**
 * Pick an image from the device and embed it in the active note.
 *
 * Mobile gap this closes: on desktop you paste or drag an image into the
 * editor, but on phones there's no paste affordance. This routes the OS
 * photo/file picker through the SAME `pasteImage` -> `save_attachment` path
 * the desktop paste flow already uses, so there's no new IPC or native code.
 *
 * Mechanism: a transient `<input type="file" accept="image/*">`. wry 0.55
 * forwards this to the native gallery/document picker on Android (System
 * WebView `onShowFileChooser`) and iOS (WKWebView handles it internally), and
 * desktop opens the OS file dialog. The picker hands us a real `File`.
 *
 * SCOPE (v1): gallery / photo-library pick only. Live-camera capture
 * (`<input ... capture="environment">`) is a deliberate follow-up — it needs
 * `NSCameraUsageDescription` on iOS (hard crash without it) and a `CAMERA`
 * manifest permission on Android, so it ships as its own change.
 */
import { roostStore } from "../stores/roost";
import { toastStore } from "../components/Toast/Toast";
import { pasteImage } from "./attachments";
import { appendAttachmentToActiveNote } from "./insert-attachment";

/**
 * Open the OS picker for a single image. Resolves to the chosen `File`, or
 * `null` if the dialog returned nothing. Cancellation has no reliable DOM
 * event, so a cancelled dialog simply never resolves — harmless here because
 * no resource (mic/camera/stream) is held open, unlike the recorder.
 */
export function pickImageFile(): Promise<File | null> {
  return new Promise((resolve) => {
    const input = document.createElement("input");
    input.type = "file";
    input.accept = "image/*";
    input.onchange = () => resolve(input.files?.[0] ?? null);
    input.click();
  });
}

/**
 * Save an already-picked image into the roost and link it into the active
 * note. Split out from `pickAndInsertImage` so the save+link+toast path is
 * unit-testable without driving a real DOM file dialog.
 */
export async function insertImageFromFile(roostPath: string, file: File): Promise<void> {
  toastStore.show("Adding image…", "info", 1500);
  const filename = await pasteImage(roostPath, file);
  if (!filename) {
    toastStore.show("Failed to save image", "error");
    return;
  }
  if (appendAttachmentToActiveNote(filename) === "no-tab") {
    toastStore.show(`Saved ${filename} — no active note to link from`, "info");
    return;
  }
  toastStore.show(`Image added: ${filename}`, "success", 2500);
}

/** Full flow: guard a roost is open, pick an image, save + link it. */
export async function pickAndInsertImage(): Promise<void> {
  const roostPath = roostStore.roostPath();
  if (!roostPath) {
    toastStore.show("Open a roost before adding an image", "error");
    return;
  }
  const file = await pickImageFile();
  if (!file) return; // cancelled or empty selection
  await insertImageFromFile(roostPath, file);
}
