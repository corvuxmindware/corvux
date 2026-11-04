/**
 * Suppress the WebView's built-in right-click menu.
 *
 * On Windows the WebView2 (Edge) engine shows its own context menu —
 * "Back / Refresh / Save as / Print / More tools → Share" — none of which
 * belong in a desktop notes app. It reads as "this is a web page," not a
 * native app, and exposes actions (Save as, Print, Share, Inspect) that
 * make no sense for Corvux.
 *
 * The native menu is KEPT inside editable contexts (text inputs and the
 * CodeMirror editor) so users still get copy / paste / spellcheck
 * suggestions where those genuinely matter. Everywhere else — window
 * chrome, tabs, sidebar, the rendered note body — the menu is blocked.
 *
 * App-owned menus (e.g. the file-tree ContextMenu) already call
 * `preventDefault()` themselves before this listener runs, so this guard
 * never fights them — and even if it runs first, suppressing the native
 * menu on those targets is exactly what we want anyway.
 */

/**
 * True when the native context menu should be allowed for this target —
 * i.e. the click landed inside a text input, textarea, a `contenteditable`
 * region, or the CodeMirror editor (`.cm-editor`, whose `.cm-content` is
 * itself contenteditable).
 */
export function isEditableTarget(target: EventTarget | null): boolean {
  const el = target instanceof Element ? target : null;
  if (!el) return false;
  return !!el.closest(
    'input, textarea, [contenteditable=""], [contenteditable="true"], .cm-editor',
  );
}

/**
 * Install the global `contextmenu` guard. Returns a disposer that removes
 * the listener (handy for tests / HMR).
 */
export function installNativeContextMenuGuard(
  doc: Document = document,
): () => void {
  const handler = (e: MouseEvent) => {
    // App-owned menus already called preventDefault — leave them alone.
    if (e.defaultPrevented) return;
    // Keep the native menu where right-click is genuinely useful.
    if (isEditableTarget(e.target)) return;
    e.preventDefault();
  };
  doc.addEventListener("contextmenu", handler);
  return () => doc.removeEventListener("contextmenu", handler);
}
