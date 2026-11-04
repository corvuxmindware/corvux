/**
 * Append an attachment link to the active note.
 *
 * Both the voice-memo flow (RecorderSheet) and the image-attach flow
 * (image-attach.ts) land their files the same way: save to roost
 * `.attachments/`, then append `![filename](.attachments/filename)` to the
 * active note's markdown. The editor's image/audio embed extensions
 * recognize that pattern by extension and render <img>/<audio> inline.
 *
 * Insertion goes through `tabStore.updateContent` — NOT a CodeMirror
 * dispatch — because that's the single write path the editor, autosave, and
 * sync all observe. The audio flow established this; the image flow reuses
 * it verbatim so spacing/behaviour can never drift between the two.
 */
import { tabStore } from "../stores/tabs";

/**
 * Pure: produce `existing` content with an attachment link appended.
 *
 * Spacing rule (mirrors the original RecorderSheet logic): no separator if
 * the note is empty or already ends in a newline, otherwise a blank line so
 * the embed starts its own paragraph. Always terminates with a trailing
 * newline so a subsequent append starts clean.
 */
export function appendAttachmentMarkdown(existing: string, filename: string): string {
  const sep = existing.endsWith("\n") || existing.length === 0 ? "" : "\n\n";
  return `${existing}${sep}![${filename}](.attachments/${filename})\n`;
}

/**
 * Append an attachment link to the active file note. Returns `"no-tab"` when
 * there's no active note to link from (no tab, or a non-file tab such as the
 * graph view — those use the `corvux://` scheme), `"ok"` otherwise.
 */
export function appendAttachmentToActiveNote(filename: string): "ok" | "no-tab" {
  const path = tabStore.activeTabPath();
  if (!path || path.startsWith("corvux://")) return "no-tab";
  const tab = tabStore.activeTab();
  if (!tab) return "no-tab";
  tabStore.updateContent(appendAttachmentMarkdown(tab.content, filename));
  return "ok";
}
