/**
 * Copy shown when a referenced attachment (image/audio) isn't on disk.
 *
 * From the editor's side they all look identical — the loader returns null —
 * but the *cause* differs, and so should the message:
 *
 *  - **Too large to sync:** a peer told us this attachment exceeds the sync
 *    cap (>8 MiB — e.g. a big GIF or long audio), so it will never arrive via
 *    sync. Say so honestly instead of a forever-"not synced yet" that implies
 *    it's coming. Tracked in `tooLargePaths`, set from the sync event layer.
 *  - **Sync is ON, not too large:** the file most likely lives on another
 *    paired device and just hasn't arrived yet (in transit, or that device is
 *    offline). "Not synced yet" — not lost, just not here. The old "⚠ Missing"
 *    reads as "Corvux ate my file" (sync audit A-1).
 *  - **Sync is OFF:** a missing file really is missing (moved / deleted /
 *    typo'd path), so we keep the honest warning, path included.
 */
export interface AttachmentMissingNotice {
  text: string;
  /** Drives styling: a neutral "not synced" hint vs a red "missing" error. */
  variant: "unsynced" | "missing";
}

/**
 * Roost-relative attachment paths a peer reported as over the sync cap.
 * Populated from the `attachment_too_large` sync event (see app/effects.ts);
 * consulted here so the embed shows an honest "too large" notice instead of a
 * never-resolving "not synced yet".
 */
const tooLargePaths = new Set<string>();

/** Record that `rel` exceeds the sync cap (from a peer's BlobUnavailable). */
export function markAttachmentTooLarge(rel: string): void {
  tooLargePaths.add(rel);
}

export function attachmentMissingNotice(opts: {
  syncOn: boolean;
  path: string;
  kind: "image" | "audio";
}): AttachmentMissingNotice {
  if (tooLargePaths.has(opts.path)) {
    return { text: "📎 Too large to sync (over 8 MB)", variant: "unsynced" };
  }
  if (opts.syncOn) {
    // Attachments DO sync now (capped, P2P, both devices online); an absent
    // one is in transit or its source device is offline — not "never".
    return { text: "📎 Not synced yet", variant: "unsynced" };
  }
  return { text: `⚠ Missing: ${opts.path}`, variant: "missing" };
}
