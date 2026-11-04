/**
 * Block the WebView's built-in *reload* shortcuts.
 *
 * `Ctrl/Cmd+R` and `F5` reload the entire webview — in a shipped desktop
 * app that throws away all in-memory state (open tabs, unsaved editor
 * buffers, scroll position) and flashes a white reload, which no native
 * app should ever do. (Save-as via `Ctrl+S` and print via `Ctrl+P` are
 * already claimed by the app's own shortcuts in `keyboard-shortcuts.ts`,
 * so they never reach the browser default.)
 *
 * This is installed in PRODUCTION ONLY — during development reload is a
 * genuine convenience when HMR misses a change. The dev/prod decision is
 * made by the caller (`index.tsx` gates on `import.meta.env.PROD`); this
 * module stays env-agnostic so it's trivially testable.
 */

/** True when the key event is a browser reload we want to suppress. */
export function isReloadShortcut(e: KeyboardEvent): boolean {
  if (e.key === "F5") return true; // F5, Shift+F5, Ctrl+F5 — all reload
  const mod = e.ctrlKey || e.metaKey;
  // Catches Ctrl/Cmd+R and Ctrl/Cmd+Shift+R (hard reload, "R").
  if (mod && (e.key === "r" || e.key === "R")) return true;
  return false;
}

/**
 * Install the reload guard. Listens in the capture phase and only calls
 * `preventDefault()` — never `stopPropagation()` — so app-level keydown
 * handlers (e.g. Ctrl+Shift+R voice memo) still run. Returns a disposer.
 */
export function installReloadShortcutGuard(
  doc: Document = document,
): () => void {
  const handler = (e: KeyboardEvent) => {
    if (isReloadShortcut(e)) e.preventDefault();
  };
  doc.addEventListener("keydown", handler, { capture: true });
  return () => doc.removeEventListener("keydown", handler, { capture: true });
}
