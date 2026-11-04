import { For, Show, createSignal, onCleanup, onMount } from "solid-js";
import { commands, events, isTauri, unwrap } from "../../lib/invoke";
import { errMsg } from "../../lib/error";
import { toastStore } from "../Toast/Toast";
import { bugReportStore } from "../../stores/bugReport";
import { roostStore } from "../../stores/roost";

interface LogEntry {
  ts: string;
  level: string;
  target: string;
  message: string;
}

interface DiagnosticReport {
  text: string;
  app_version: string;
  os: string;
  arch: string;
  log_count: number;
}

const LEVEL_STYLES: Record<string, string> = {
  ERROR: "text-red",
  WARN: "text-yellow",
  INFO: "text-text-primary",
  DEBUG: "text-text-muted",
  TRACE: "text-text-muted opacity-70",
};

const LEVEL_FILTERS = ["ALL", "ERROR", "WARN", "INFO", "DEBUG"] as const;
type LevelFilter = (typeof LEVEL_FILTERS)[number];

/**
 * Diagnostics — privacy-respecting, all-local observability surface.
 *
 * Three things live here:
 * 1. **Copy diagnostic info** button — bundles app version, OS, recent
 *    logs, and crash.log tail into one markdown blob the user can paste
 *    into a bug report. Nothing leaves the device unless they paste.
 * 2. **Live log viewer** — last 200 entries from the in-memory ring
 *    buffer, auto-updating via Tauri `log:event` events. Filterable by
 *    level and free-text. Tail-follow mode pins to bottom on new entries.
 * 3. **Clear** — wipes the in-memory buffer (does NOT touch crash.log).
 */
interface GraphStatsResult {
  notes: number;
  links: number;
  tags: number;
}

export default function DiagnosticsSection() {
  const [logs, setLogs] = createSignal<LogEntry[]>([]);
  const [filter, setFilter] = createSignal<LevelFilter>("ALL");
  const [search, setSearch] = createSignal("");
  const [follow, setFollow] = createSignal(true);
  const [generating, setGenerating] = createSignal(false);
  const [report, setReport] = createSignal<DiagnosticReport | null>(null);
  // S-01: redaction is on by default. Local-debug-only toggle reveals
  // unredacted paths and log content — surfaced inline so it can't be
  // confused with the support / clipboard-copy path.
  const [revealRaw, setRevealRaw] = createSignal(false);
  // AUDIT-051: Repair-database flow. `repairing` gates the button so a
  // user can't fire two rebuilds back-to-back while the first walks
  // the roost.
  const [repairing, setRepairing] = createSignal(false);

  let scrollEl: HTMLDivElement | undefined;
  let unlisten: (() => void) | null = null;

  async function refresh() {
    try {
      const fresh = await unwrap(commands.getRecentLogs(200));
      setLogs(fresh);
      scrollToBottomIfFollowing();
    } catch {
      // Browser dev mode hits the mock — empty list is fine.
      setLogs([]);
    }
  }

  function scrollToBottomIfFollowing() {
    if (!follow() || !scrollEl) return;
    queueMicrotask(() => {
      if (scrollEl) scrollEl.scrollTop = scrollEl.scrollHeight;
    });
  }

  onMount(async () => {
    // Live log is a DEV-only surface (see the gated section in the JSX). In
    // shipped builds nobody renders the stream — users send logs to us via
    // "Report a problem", which bundles them server-side — so skip the
    // subscription + in-memory buffer entirely in production.
    if (!import.meta.env.DEV) return;
    await refresh();
    if (!isTauri) return;
    // Subscribe to live log events. Each event appends one entry.
    unlisten = await events.logEvent.listen((e) => {
      setLogs((prev) => {
        const next = [...prev, e.payload];
        // Keep the visible buffer aligned with the backend cap.
        return next.length > 500 ? next.slice(next.length - 500) : next;
      });
      scrollToBottomIfFollowing();
    });
  });

  onCleanup(() => {
    if (unlisten) unlisten();
  });

  async function handleCopyReport() {
    setGenerating(true);
    try {
      // S-01: explicit redaction control. `null` would now default to
      // redacted (backend default flipped). Passing the toggle state
      // explicitly makes the intent obvious at the IPC boundary.
      const redact = !revealRaw();
      const r = await unwrap(commands.getDiagnosticReport(redact));
      setReport(r);
      await navigator.clipboard.writeText(r.text);
      toastStore.show(
        `Diagnostic report copied (${r.log_count} log entries${redact ? "" : ", UNREDACTED"})`,
        "success",
      );
    } catch (e) {
      toastStore.show(`Failed to build report: ${e}`, "error");
    } finally {
      setGenerating(false);
    }
  }

  async function handleSendTestNotification() {
    try {
      // macOS / iOS / Android all require explicit permission before
      // `.show()` will actually surface a banner — the Rust call returns
      // Ok regardless, so without this check the toast lies and the user
      // sees nothing in the notification tray. Mirrors the lazy prompt
      // in stores/tasks.ts:ensureNotificationPermission, kept inline so
      // this diagnostic surface doesn't depend on the tasks store.
      if (isTauri) {
        const { isPermissionGranted, requestPermission } = await import(
          "@tauri-apps/plugin-notification"
        );
        let granted = await isPermissionGranted();
        if (!granted) {
          const state = await requestPermission();
          granted = state === "granted";
        }
        if (!granted) {
          toastStore.show(
            "Notifications denied. Enable them in System Settings → Notifications → Corvux, then try again.",
            "error",
          );
          return;
        }
      }
      await unwrap(commands.sendTestNotification());
      toastStore.show(
        "Test notification sent — check your notification tray for Complete + Snooze buttons.",
        "success",
      );
    } catch (e) {
      toastStore.show(`Send failed: ${e}`, "error");
    }
  }

  /// AUDIT-051: rebuild the on-disk graph DB from the roost. Gated by
  /// a `window.confirm` because the rebuild discards the embeddings
  /// cache (they re-populate over the next background drain). Markdown
  /// files are untouched — the IPC only nukes the device-local
  /// `graph.db` and re-walks the roost.
  async function handleRepairDatabase() {
    const path = roostStore.roostPath();
    if (!path) {
      toastStore.show("Open a roost before repairing its index.", "error");
      return;
    }
    if (
      !confirm(
        "Repair the database for this roost?\n\n" +
          "This rebuilds the search index and graph from your Markdown files. " +
          "Your notes are NOT changed. AI embeddings will re-build in the " +
          "background — expect search/RAG to be partial for a few minutes.",
      )
    ) {
      return;
    }
    setRepairing(true);
    try {
      const stats = await unwrap(commands.rebuildDatabase(path));
      toastStore.show(
        `Database rebuilt — ${stats.notes} notes, ${stats.links} links, ${stats.tags} tags re-indexed.`,
        "success",
      );
    } catch (e) {
      const msg = errMsg(e);
      toastStore.show(`Repair failed: ${msg}`, "error");
    } finally {
      setRepairing(false);
    }
  }

  async function handleClear() {
    if (!confirm("Clear in-memory log buffer? Crash log on disk is preserved.")) {
      return;
    }
    try {
      await unwrap(commands.clearLogBuffer());
      setLogs([]);
      toastStore.show("Log buffer cleared", "info");
    } catch (e) {
      toastStore.show(`Clear failed: ${e}`, "error");
    }
  }

  const visible = () => {
    const f = filter();
    const q = search().toLowerCase();
    return logs().filter((l) => {
      if (f !== "ALL" && l.level !== f) return false;
      if (q && !l.message.toLowerCase().includes(q) && !l.target.toLowerCase().includes(q)) {
        return false;
      }
      return true;
    });
  };

  return (
    <div class="space-y-5">
      {/* Privacy disclosure — first-time users should know the rules. */}
      <section>
        <h3 class="text-text-secondary text-xs font-semibold uppercase tracking-wider mb-2">
          Privacy
        </h3>
        <p class="text-text-muted text-[12px] leading-relaxed">
          Everything on this page stays on your device. No telemetry, no remote
          crash reporting, no auto-upload. The "Copy diagnostic info" button
          puts the report on your clipboard — nothing is sent anywhere unless
          you paste it yourself (e.g. into a Discord message or email when
          asking for help).
        </p>
      </section>

      {/* Report bundle */}
      <section>
        <h3 class="text-text-secondary text-xs font-semibold uppercase tracking-wider mb-2">
          Bug report bundle
        </h3>
        <div class="flex flex-col gap-2">
          <button
            onClick={() => bugReportStore.show({ kind: "manual" })}
            class="flex items-center justify-center gap-2 px-3 py-2 rounded-lg bg-accent text-white text-sm font-medium hover:bg-accent-hover transition-colors"
            aria-label="Report a problem with description and diagnostic bundle"
          >
            <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
              <path d="M12 9v4" />
              <path d="M12 17h.01" />
              <path d="M10.29 3.86L1.82 18a2 2 0 001.71 3h16.94a2 2 0 001.71-3L13.71 3.86a2 2 0 00-3.42 0z" />
            </svg>
            <span>Report a problem…</span>
          </button>
          <button
            onClick={handleCopyReport}
            disabled={generating()}
            class="flex items-center justify-center gap-2 px-3 py-2 rounded-lg border border-border bg-bg-secondary/60 text-text-secondary text-sm hover:bg-bg-hover hover:text-text-primary disabled:opacity-50 disabled:cursor-wait transition-colors"
            aria-label="Copy diagnostic info to clipboard"
          >
            <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
              <rect x="9" y="9" width="13" height="13" rx="2" ry="2" />
              <path d="M5 15H4a2 2 0 01-2-2V4a2 2 0 012-2h9a2 2 0 012 2v1" />
            </svg>
            <span>{generating() ? "Building report…" : "Copy diagnostic info only"}</span>
          </button>
          <label class="flex items-center gap-2 text-text-muted text-[11px] mt-1 cursor-pointer">
            <input
              type="checkbox"
              checked={revealRaw()}
              onChange={(e) => setRevealRaw(e.currentTarget.checked)}
              class="accent-accent"
              aria-label="Include raw paths and log content (local debugging only — do not paste this anywhere)"
            />
            <span>
              Reveal raw paths and log content (local debugging only — do not
              paste anywhere)
            </span>
          </label>
          <Show when={report()}>
            {(r) => (
              <p class="text-text-muted text-[11px]">
                Last build: Corvux {r().app_version} · {r().os}/{r().arch} · {r().log_count} log entries
              </p>
            )}
          </Show>
        </div>
      </section>

      {/* On-device notification QA — DEV builds ONLY (same rule as the Live
          log section below). Fires a synthetic task-due notification so we can
          verify the Complete + Snooze 15m buttons render on real devices
          (Android #2968 cosmetic + iOS UNNotificationAction display). The
          sentinel note path means the downstream IPC fails soft if a button is
          actually tapped — the QA signal is whether the buttons appear at all.
          Hidden in every shipped build; we run this check from the live-dev
          build (which is itself a real device), so end users — and
          release-build testers — never see it. */}
      {import.meta.env.DEV && (
      <section>
        <h3 class="text-text-secondary text-xs font-semibold uppercase tracking-wider mb-2">
          Notifications
        </h3>
        <button
          onClick={handleSendTestNotification}
          class="flex items-center justify-center gap-2 px-3 py-2 rounded-lg border border-border bg-bg-secondary/60 text-text-secondary text-sm hover:bg-bg-hover hover:text-text-primary transition-colors"
          aria-label="Fire a test task-due notification to verify on-device button rendering"
        >
          <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
            <path d="M18 8A6 6 0 006 8c0 7-3 9-3 9h18s-3-2-3-9" />
            <path d="M13.73 21a2 2 0 01-3.46 0" />
          </svg>
          <span>Send test notification</span>
        </button>
        <p class="text-text-muted text-[11px] mt-1.5">
          Fires an immediate task-due notification with Complete + Snooze 15m buttons. Useful on iOS / Android to confirm permissions + button rendering before scheduling a real task.
        </p>
      </section>
      )}

      {/* AUDIT-051: roost-index repair. Rare-use button — only
          surfaces after a corruption error, OR when a user is
          troubleshooting "search is missing notes." Non-destructive
          to markdown files; the IPC re-walks the roost from disk
          and rebuilds graph/search/tasks from scratch. */}
      <section>
        <h3 class="text-text-secondary text-xs font-semibold uppercase tracking-wider mb-2">
          Roost index
        </h3>
        <button
          onClick={handleRepairDatabase}
          disabled={repairing() || !roostStore.roostPath()}
          class="flex items-center justify-center gap-2 px-3 py-2 rounded-lg border border-amber-400/40 bg-amber-400/5 text-amber-400 text-sm hover:bg-amber-400/10 disabled:opacity-50 disabled:cursor-wait transition-colors"
          aria-label="Rebuild the search index and graph from your Markdown files"
        >
          <svg
            width="14"
            height="14"
            viewBox="0 0 24 24"
            fill="none"
            stroke="currentColor"
            stroke-width="2"
            stroke-linecap="round"
            stroke-linejoin="round"
            aria-hidden="true"
          >
            <path d="M3 12a9 9 0 019-9 9.75 9.75 0 016.74 2.74L21 8" />
            <path d="M21 3v5h-5" />
            <path d="M21 12a9 9 0 01-9 9 9.75 9.75 0 01-6.74-2.74L3 16" />
            <path d="M8 16H3v5" />
          </svg>
          <span>{repairing() ? "Repairing…" : "Repair database"}</span>
        </button>
        <p class="text-text-muted text-[11px] mt-1.5">
          Last-resort fix when search or the graph behaves strangely. Deletes the
          on-device index for this roost and rebuilds it by re-reading every
          Markdown file. Your notes themselves are never touched. Embeddings
          re-build in the background after.
        </p>
      </section>

      {/* Live log viewer — DEV builds ONLY. End users never see the raw
          streaming console; when they hit "Report a problem" the bundle
          carries the logs to us. import.meta.env.DEV is statically false in
          every shipped build (vite build → PROD) on all platforms, so esbuild
          drops this whole subtree from production bundles. */}
      {import.meta.env.DEV && (
      <section>
        <div class="flex items-center justify-between mb-2">
          <h3 class="text-text-secondary text-xs font-semibold uppercase tracking-wider">
            Live log
          </h3>
          <div class="flex items-center gap-1">
            <button
              onClick={refresh}
              class="text-[11px] px-2 py-0.5 rounded border border-border hover:bg-bg-hover text-text-secondary"
              title="Reload from server"
            >
              Refresh
            </button>
            <button
              onClick={handleClear}
              class="text-[11px] px-2 py-0.5 rounded border border-border hover:bg-bg-hover text-text-secondary"
              title="Clear in-memory buffer"
            >
              Clear
            </button>
          </div>
        </div>

        {/* Filters row */}
        <div class="flex flex-wrap items-center gap-2 mb-2">
          <div class="flex gap-1">
            <For each={LEVEL_FILTERS}>
              {(lvl) => {
                const active = () => filter() === lvl;
                return (
                  <button
                    onClick={() => setFilter(lvl)}
                    class={`text-[11px] px-2 py-0.5 rounded border transition-colors ${
                      active()
                        ? "border-accent bg-accent/15 text-accent"
                        : "border-border bg-bg-secondary/60 text-text-muted hover:text-text-secondary"
                    }`}
                  >
                    {lvl}
                  </button>
                );
              }}
            </For>
          </div>
          <input
            type="text"
            placeholder="Filter…"
            value={search()}
            onInput={(e) => setSearch(e.currentTarget.value)}
            class="flex-1 min-w-[120px] text-[11px] px-2 py-1 rounded border border-border bg-bg-secondary/60 text-text-primary placeholder:text-text-muted focus:outline-none focus:border-accent/60"
          />
          <label class="flex items-center gap-1 text-[11px] text-text-muted cursor-pointer select-none">
            <input
              type="checkbox"
              checked={follow()}
              onChange={(e) => setFollow(e.currentTarget.checked)}
            />
            <span>Tail</span>
          </label>
        </div>

        {/* Log lines — fixed height, scrollable. Monospace for alignment. */}
        <div
          ref={(el) => (scrollEl = el)}
          class="h-[260px] overflow-y-auto rounded-lg border border-border bg-bg-secondary/40 font-mono text-[11px] leading-tight"
        >
          <Show
            when={visible().length > 0}
            fallback={
              <div class="p-3 text-text-muted text-center">
                {logs().length === 0
                  ? "No log entries yet — nothing has happened in this session."
                  : "No entries match the current filter."}
              </div>
            }
          >
            <For each={visible()}>
              {(entry) => (
                <div class="px-2 py-0.5 hover:bg-bg-hover whitespace-pre-wrap break-words">
                  <span class="text-text-muted">{entry.ts.slice(11, 23)}</span>
                  <span class={`ml-2 font-bold ${LEVEL_STYLES[entry.level] ?? ""}`}>
                    {entry.level.padEnd(5)}
                  </span>
                  <span class="ml-2 text-text-muted">{entry.target}</span>
                  <span class="ml-2 text-text-primary">{entry.message}</span>
                </div>
              )}
            </For>
          </Show>
        </div>
        <p class="text-text-muted text-[11px] mt-1">
          Showing {visible().length} of {logs().length} entries (ring buffer cap: 500).
        </p>
      </section>
      )}
    </div>
  );
}
