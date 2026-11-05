/**
 * `window.confirm` that always waits for the user's answer.
 *
 * On desktop and iOS, tauri-plugin-dialog replaces `window.confirm` with an
 * async function (Tauri has no synchronous IPC), so calling it returns a
 * Promise — and a Promise is truthy. Code written as
 * `if (!window.confirm(msg)) return;` therefore carried on immediately, before
 * the user answered, and pressing Cancel changed nothing (found 2026-11-05:
 * note and folder deletes, the sync reset and the MCP connect consent all went
 * ahead on Cancel). Android keeps the WebView's native, synchronous confirm,
 * and the browser test harness uses the browser's. Awaiting handles both.
 *
 * Use this instead of `window.confirm` everywhere; a unit test enforces it.
 */
export async function confirmDialog(message: string): Promise<boolean> {
  const answer = window.confirm(message) as unknown as boolean | Promise<boolean>;
  return (await answer) === true;
}
