import { createSignal, createRoot, For, Show } from "solid-js";
import { errMsg } from "../../lib/error";
import { bugReportStore } from "../../stores/bugReport";

export type ToastType = "info" | "success" | "error" | "warning";

interface ToastItem {
  id: number;
  message: string;
  type: ToastType;
  /** Optional structured details — error message + module + stack-ish
   *  blob the user can copy and paste into a bug report. Only error/warning
   *  toasts use this; info/success toasts pass it as undefined. */
  details?: ToastDetails;
}

/** Structured error context attached to a toast. The string fields are
 *  what gets copied to the clipboard when the user clicks "Copy details".
 *  Keep this shape narrow — anything that wouldn't be useful in a bug
 *  report doesn't belong here. */
export interface ToastDetails {
  /** Short error code, e.g. "ROOST_OPEN_FAIL" or "EMBED_TIMEOUT". */
  code?: string;
  /** Module/source the error originated in, e.g. "roost.open" or
   *  "ai.embedding". Helps a maintainer route the report. */
  module?: string;
  /** Raw error string from the IPC call or thrown exception. */
  error: string;
}

let nextId = 0;

function createToastStore() {
  const [toasts, setToasts] = createSignal<ToastItem[]>([]);

  function show(
    message: string,
    type: ToastType = "info",
    duration = 4000,
    details?: ToastDetails,
  ) {
    const id = nextId++;
    setToasts((prev) => [...prev, { id, message, type, details }]);
    // Errors that include details persist longer — the user needs time
    // to read the message and click "Copy details" before it auto-dismisses.
    const effectiveDuration =
      details && (type === "error" || type === "warning") ? Math.max(duration, 8000) : duration;
    setTimeout(() => dismiss(id), effectiveDuration);
  }

  function showError(message: string, error: unknown, opts?: { code?: string; module?: string }) {
    const errStr = errMsg(error);
    show(message, "error", 8000, {
      code: opts?.code,
      module: opts?.module,
      error: errStr,
    });
  }

  function dismiss(id: number) {
    setToasts((prev) => prev.filter((t) => t.id !== id));
  }

  return { toasts, show, showError, dismiss };
}

export const toastStore = createRoot(createToastStore);

const typeStyles: Record<ToastType, string> = {
  info: "border-accent/30 bg-accent/10 text-accent",
  success: "border-green/30 bg-green/10 text-green",
  error: "border-red/30 bg-red/10 text-red",
  warning: "border-yellow/30 bg-yellow/10 text-yellow",
};

const typeIcons: Record<ToastType, string> = {
  info: "M12 2C6.48 2 2 6.48 2 12s4.48 10 10 10 10-4.48 10-10S17.52 2 12 2zm1 15h-2v-6h2v6zm0-8h-2V7h2v2z",
  success: "M20 6L9 17l-5-5",
  error: "M18 6L6 18M6 6l12 12",
  warning: "M12 9v4m0 4h.01M10.29 3.86L1.82 18a2 2 0 001.71 3h16.94a2 2 0 001.71-3L13.71 3.86a2 2 0 00-3.42 0z",
};

/** Format a toast's details into a copy-pasteable text block. Includes
 *  the visible message, the error string, code and module if present,
 *  plus app version + UA so a bug report has the bare minimum context. */
function formatDetailsForCopy(toast: ToastItem): string {
  const lines: string[] = [];
  lines.push(`# Corvux error report`);
  lines.push(``);
  lines.push(`**Message:** ${toast.message}`);
  if (toast.details) {
    if (toast.details.code) lines.push(`**Code:** ${toast.details.code}`);
    if (toast.details.module) lines.push(`**Module:** ${toast.details.module}`);
    lines.push(``);
    lines.push(`**Error:**`);
    lines.push("```");
    lines.push(toast.details.error);
    lines.push("```");
  }
  lines.push(``);
  lines.push(`**When:** ${new Date().toISOString()}`);
  if (typeof navigator !== "undefined") {
    lines.push(`**UA:** ${navigator.userAgent}`);
  }
  lines.push(``);
  lines.push(
    `_Tip: open Settings → Diagnostics → Copy diagnostic info for the full log + crash trace._`,
  );
  return lines.join("\n");
}

async function copyDetails(toast: ToastItem) {
  const text = formatDetailsForCopy(toast);
  try {
    await navigator.clipboard.writeText(text);
    // Re-use the toast surface — show a small confirmation. Don't spawn
    // another toast with details, otherwise we recurse.
    toastStore.show("Error details copied to clipboard", "success", 2500);
  } catch {
    // Clipboard API gated by permissions in some browsers — fall back
    // to a textarea trick for browser dev mode.
    const ta = document.createElement("textarea");
    ta.value = text;
    document.body.appendChild(ta);
    ta.select();
    try {
      document.execCommand("copy");
      toastStore.show("Error details copied to clipboard", "success", 2500);
    } catch {
      toastStore.show("Could not copy to clipboard", "error", 3000);
    }
    document.body.removeChild(ta);
  }
}

export default function ToastContainer() {
  return (
    <div class="fixed bottom-4 right-4 z-[100] flex flex-col gap-2 pointer-events-none">
      <For each={toastStore.toasts()}>
        {(toast) => (
          <div
            class={`pointer-events-auto flex items-start gap-2 px-4 py-2.5 rounded-lg border shadow-lg text-sm animate-slide-in ${typeStyles[toast.type]}`}
            style={{ "backdrop-filter": "blur(8px)" }}
            role={toast.type === "error" ? "alert" : "status"}
          >
            <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" class="shrink-0 mt-0.5" aria-hidden="true">
              <path d={typeIcons[toast.type]} />
            </svg>
            <div class="flex-1 min-w-0">
              <div class="break-words">{toast.message}</div>
              <Show when={toast.details}>
                {(d) => (
                  <Show when={d().code || d().module}>
                    <div class="text-[10px] opacity-70 mt-0.5 font-mono">
                      {d().code ?? ""}
                      {d().code && d().module ? " · " : ""}
                      {d().module ?? ""}
                    </div>
                  </Show>
                )}
              </Show>
            </div>
            <Show when={toast.details}>
              <button
                onClick={() => copyDetails(toast)}
                class="ml-1 shrink-0 px-2 py-0.5 rounded border border-current/30 text-[11px] opacity-80 hover:opacity-100 transition-opacity"
                aria-label="Copy error details"
                title="Copy error details to clipboard"
              >
                Copy details
              </button>
              <Show when={toast.type === "error"}>
                <button
                  onClick={() => {
                    const d = toast.details;
                    const intro = d
                      ? `An error popped up: "${toast.message}". ${
                          d.code ? `Code: ${d.code}. ` : ""
                        }${d.module ? `Module: ${d.module}. ` : ""}Raw: ${d.error}.`
                      : `An error popped up: "${toast.message}".`;
                    bugReportStore.show({
                      kind: "toast-error",
                      initialDescription: intro,
                    });
                    toastStore.dismiss(toast.id);
                  }}
                  class="ml-1 shrink-0 px-2 py-0.5 rounded border border-current/30 text-[11px] opacity-80 hover:opacity-100 transition-opacity"
                  aria-label="Open bug report with this error pre-filled"
                  title="Send a bug report with this error attached"
                >
                  Report
                </button>
              </Show>
            </Show>
            <button
              onClick={() => toastStore.dismiss(toast.id)}
              class="ml-1 shrink-0 opacity-50 hover:opacity-100 transition-opacity mt-0.5"
              aria-label="Dismiss"
            >
              <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
                <path d="M18 6L6 18M6 6l12 12" />
              </svg>
            </button>
          </div>
        )}
      </For>
    </div>
  );
}
