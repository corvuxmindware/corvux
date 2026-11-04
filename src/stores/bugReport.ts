import { createSignal, createRoot } from "solid-js";
import { commands, isTauri, unwrap } from "../lib/invoke";
import { errMsg } from "../lib/error";
import { log, type LogEntry } from "../lib/log";
import { settingsStore } from "./settings";

/**
 * Bug-report flow — captures a user description, bundles the diagnostic
 * report, and routes the whole thing through the user's configured
 * support endpoint (mailto: or https:// URL).
 *
 * Two reasons to open the modal:
 *   - User-initiated from Settings → Diagnostics or a toast "Report" button
 *     (`open()` with no args).
 *   - Post-crash, after `check_previous_crash` returns `crashed: true` on
 *     next launch (`open({ context: "previous-crash", excerpt })`). In that
 *     case the modal pre-fills "Corvux closed unexpectedly during the
 *     previous session" so the user only types the *what were you doing?*
 *     part.
 *
 * Routing strategy:
 *   1. Build the diagnostic report via `get_diagnostic_report` IPC.
 *   2. Concatenate `userDescription + "\n\n---\n\n" + diagnosticReport`.
 *   3. Always copy the full payload to clipboard — it's longer than most
 *      mailto: clients accept inline, and clipboard delivery is universal.
 *   4. If `support_endpoint` is `mailto:...`: open with a *summary* body
 *      saying "Diagnostic report copied to clipboard — paste below."
 *   5. If it's `https://...`: open the URL in a new tab; user pastes the
 *      report into whatever form is on the other side.
 *   6. If unset: skip step 4/5, just confirm the copy succeeded and tell
 *      the user where to paste it.
 *
 * No outbound HTTP from the app — the user's choice of mail client or
 * browser is what does the network I/O. Honors the local-first stance.
 */

interface DiagnosticReport {
  text: string;
  app_version: string;
  os: string;
  arch: string;
  log_count: number;
}

export interface BugReportContext {
  /** What surfaced the modal — drives the pre-filled headline. */
  kind: "manual" | "toast-error" | "previous-crash";
  /** Initial user-description text. Pre-fill for the post-crash path so
   *  the user only adds repro details. */
  initialDescription?: string;
  /** Crash log excerpt to pre-include in the description. Only used by
   *  the "previous-crash" path. */
  crashExcerpt?: string;
}

export interface BugReportSubmitResult {
  /** True when the diagnostic report was successfully copied. The caller
   *  can show a different success message if false (the IPC failed). */
  copied: boolean;
  /** Which channel the user was routed to. `null` when no support
   *  endpoint is configured — caller should show a "paste into your
   *  channel" hint. */
  routed: "mailto" | "browser" | null;
  /** Friendly error message, when something went wrong end-to-end. */
  error?: string;
}

function createBugReportStore() {
  const [open, setOpen] = createSignal<BugReportContext | null>(null);
  const [submitting, setSubmitting] = createSignal(false);

  /** Surface the modal. Multiple calls collapse to the most recent
   *  context — there's only one bug-report modal at a time. */
  function show(ctx: BugReportContext = { kind: "manual" }) {
    setOpen(ctx);
  }

  function dismiss() {
    setOpen(null);
  }

  /** Build + copy + open. Returns a result the modal renders inline. */
  async function submit(userDescription: string): Promise<BugReportSubmitResult> {
    setSubmitting(true);
    try {
      let report: DiagnosticReport | null = null;
      if (isTauri) {
        try {
          // S-01: support / clipboard-bound path — always redacted.
          // Defaults to redacted server-side now, but pass explicit
          // `true` so the intent is obvious at the call site.
          report = await unwrap(commands.getDiagnosticReport(true));
        } catch (e) {
          // Diagnostic IPC failure shouldn't block the user from sending
          // their description — fall through with a stub report.
          log.warn("bug-report", "get_diagnostic_report failed", e);
        }
      }

      const ctx = open();
      const headline = headlineFor(ctx);
      // B3: snapshot the frontend ring buffer at submit time. Drains a
      // copy so future log calls (e.g. from the modal itself closing)
      // don't perturb what the user sees in their report.
      const frontendLogs = log.drain();
      const fullPayload = buildPayload({
        headline,
        userDescription,
        crashExcerpt: ctx?.crashExcerpt,
        report,
        frontendLogs,
      });

      let copied = false;
      try {
        await navigator.clipboard.writeText(fullPayload);
        copied = true;
      } catch {
        // Headless browser dev / clipboard permission denied — fall
        // back to the textarea trick, same shape as Toast.tsx.
        const ta = document.createElement("textarea");
        ta.value = fullPayload;
        document.body.appendChild(ta);
        ta.select();
        try {
          copied = document.execCommand("copy");
        } catch {
          copied = false;
        }
        document.body.removeChild(ta);
      }

      const endpoint = settingsStore.settings().support_endpoint?.trim() ?? "";
      let routed: BugReportSubmitResult["routed"] = null;
      if (endpoint) {
        const launched = launchSupportEndpoint({
          endpoint,
          headline,
          userDescription,
          appVersion: report?.app_version ?? "",
          os: report?.os ?? "",
        });
        routed = launched;
      }

      return { copied, routed };
    } catch (e) {
      const message = errMsg(e);
      return { copied: false, routed: null, error: message };
    } finally {
      setSubmitting(false);
    }
  }

  return { open, submitting, show, dismiss, submit };
}

function headlineFor(ctx: BugReportContext | null): string {
  if (!ctx) return "Bug report";
  switch (ctx.kind) {
    case "previous-crash":
      // Distinguish a real crash (we captured a crash.log excerpt) from an
      // unclean shutdown with no crash artifact — force-quit, OOM, power
      // loss. Don't over-claim "crash" for the latter.
      return ctx.crashExcerpt
        ? "Corvux crashed last time"
        : "Corvux didn't shut down cleanly";
    case "toast-error":
      return "Error in Corvux";
    case "manual":
    default:
      return "Bug report";
  }
}

interface PayloadInput {
  headline: string;
  userDescription: string;
  crashExcerpt?: string;
  report: DiagnosticReport | null;
  frontendLogs: readonly LogEntry[];
}

/** Markdown-formatted blob the user pastes into their support channel.
 *  Order: headline → user narrative → crash excerpt (when relevant) →
 *  full backend diagnostic report → frontend log ring (B3). */
function buildPayload(input: PayloadInput): string {
  const lines: string[] = [];
  lines.push(`# ${input.headline}`);
  lines.push("");

  if (input.userDescription.trim()) {
    lines.push("## What happened");
    lines.push(input.userDescription.trim());
    lines.push("");
  }

  if (input.crashExcerpt && input.crashExcerpt.trim()) {
    lines.push("## Crash excerpt");
    lines.push("```");
    lines.push(redactDiagnosticText(input.crashExcerpt.trim()));
    lines.push("```");
    lines.push("");
  }

  if (input.report) {
    lines.push(input.report.text.trim());
  } else {
    lines.push("_(diagnostic report unavailable — IPC failed or running in browser dev)_");
  }

  // B3: frontend log ring snapshot. Lives at the bottom so it doesn't
  // push the user description below the fold when the report is long.
  if (input.frontendLogs.length > 0) {
    lines.push("");
    lines.push(`## Frontend Logs (last ${input.frontendLogs.length} entries)`);
    lines.push("```");
    for (const entry of input.frontendLogs) {
      lines.push(redactDiagnosticText(formatLogEntry(entry)));
    }
    lines.push("```");
  }

  return lines.join("\n");
}

/** Privacy boundary for renderer-owned diagnostic fragments. The backend
 * report is redacted server-side, but the frontend log ring and crash-popup
 * excerpt never cross that boundary before we append them to the clipboard.
 * Keep recognizable structure while removing paths and credential-shaped
 * tokens. */
export function redactDiagnosticText(input: string): string {
  let out = input.replace(
    /(?:\\\\[?.]\\|[A-Za-z]:[\\/]|(?:^|[\s('"`])\/)(?:[^\s)'"`<>]+[\\/])+[^\s)'"`<>]*/gm,
    (match) => {
      const prefix = match.match(/^[\s('"`]/)?.[0] ?? "";
      return `${prefix}<redacted:path>`;
    },
  );
  out = out.replace(
    /\b(?:sk-(?:ant-|live_|test_)?|rk_live_|ya29\.|AIza|gh[op]_|github_pat_|xox[bp]-|Bearer\s+)[A-Za-z0-9_./+=-]{12,}/gi,
    "<redacted:secret>",
  );
  // Last-resort entropy guard for bare tokens accidentally logged without a
  // known provider prefix. Require 32 characters so ordinary IDs and prose
  // remain readable.
  return out.replace(
    /\b[A-Za-z0-9_+/-]{32,}={0,2}\b/g,
    "<redacted:secret>",
  );
}

/** Format a LogEntry as a single grep-friendly line.
 *  `<iso-ts> <LEVEL5> <scope>: <message>[ — <details>]` */
function formatLogEntry(entry: LogEntry): string {
  const ts = new Date(entry.ts).toISOString();
  const level = entry.level.toUpperCase().padEnd(5);
  const head = `${ts} ${level} ${entry.scope}: ${entry.message}`;
  if (entry.details === undefined) return head;
  return `${head} — ${stringifyDetails(entry.details)}`;
}

function stringifyDetails(details: unknown): string {
  if (details instanceof Error) {
    return `${details.name}: ${details.message}`;
  }
  if (typeof details === "string") return details;
  try {
    return JSON.stringify(details);
  } catch {
    return String(details);
  }
}

interface LaunchInput {
  endpoint: string;
  headline: string;
  userDescription: string;
  appVersion: string;
  os: string;
}

/** Open the user's configured channel. Returns the channel kind so the
 *  modal can show "Email opened — paste from clipboard" vs. "Browser
 *  opened — paste from clipboard". */
function launchSupportEndpoint(input: LaunchInput): "mailto" | "browser" | null {
  const subject = `Corvux bug report${
    input.appVersion ? ` [v${input.appVersion}` : ""
  }${input.os ? `, ${input.os}` : ""}${input.appVersion ? "]" : ""}`;
  // Email body is intentionally short — full payload goes through the
  // clipboard, not URL params. Some mail clients truncate at 2 KB.
  const shortBody = [
    `# ${input.headline}`,
    "",
    input.userDescription.trim() || "_(no description provided)_",
    "",
    "---",
    "Diagnostic report copied to clipboard — paste below this line.",
  ].join("\n");

  if (input.endpoint.startsWith("mailto:")) {
    const sep = input.endpoint.includes("?") ? "&" : "?";
    const url = `${input.endpoint}${sep}subject=${encodeURIComponent(
      subject,
    )}&body=${encodeURIComponent(shortBody)}`;
    safeOpen(url);
    return "mailto";
  }

  if (/^https?:\/\//i.test(input.endpoint)) {
    // GitHub issue templates accept ?title=&body= so we pass them through
    // (the 8 KB GitHub URL cap is well above the short body). Generic
    // URLs ignore unknown query params, so this is safe to send blindly.
    const sep = input.endpoint.includes("?") ? "&" : "?";
    const url = `${input.endpoint}${sep}title=${encodeURIComponent(
      subject,
    )}&body=${encodeURIComponent(shortBody)}`;
    safeOpen(url);
    return "browser";
  }

  // Unrecognised scheme — don't try to launch anything.
  return null;
}

function safeOpen(url: string) {
  try {
    window.open(url, "_blank", "noopener,noreferrer");
  } catch (e) {
    log.warn("bug-report", "failed to open support endpoint", e);
  }
}

export const bugReportStore = createRoot(createBugReportStore);
