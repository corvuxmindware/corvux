import { createSignal, createRoot } from "solid-js";
import { commands, isTauri, unwrap } from "../lib/invoke";
import { errMsg } from "../lib/error";

/**
 * Hidden developer perf panel store. Toggled via the Ctrl+Alt+P chord
 * (intentionally not advertised in the keyboard-shortcut help). Polls
 * `get_perf_snapshot` while open, idle when closed.
 *
 * Scope is "triage tool for the founder", not user-facing instrumentation.
 * No Settings UI, no localStorage persistence, no analytics — opens fresh
 * each session.
 */

export interface ChatSendBreakdown {
  at: string;
  prompt_ms: number;
  setup_ms: number;
  system_prompt_chars: number;
  profile: string;
}

export interface PerfCounters {
  chat_send_total: number;
  roost_open_total: number;
  last_chat_send: ChatSendBreakdown | null;
}

export interface PerfSnapshot {
  counters: PerfCounters;
  rss_mb: number | null;
  free_ram_mb: number;
  at: string;
}

const POLL_INTERVAL_MS = 2000;

function createDevPerfStore() {
  const [open, setOpen] = createSignal(false);
  const [snapshot, setSnapshot] = createSignal<PerfSnapshot | null>(null);
  const [error, setError] = createSignal<string | null>(null);

  let timer: number | null = null;

  async function refresh() {
    if (!isTauri) {
      setError("Not running under Tauri — perf snapshot unavailable.");
      return;
    }
    try {
      const snap = await unwrap(commands.getPerfSnapshot());
      setSnapshot(snap);
      setError(null);
    } catch (e) {
      const msg = errMsg(e);
      setError(msg);
    }
  }

  function start() {
    if (timer !== null) return;
    void refresh();
    timer = window.setInterval(() => void refresh(), POLL_INTERVAL_MS) as unknown as number;
  }

  function stop() {
    if (timer === null) return;
    window.clearInterval(timer);
    timer = null;
  }

  function show() {
    if (open()) return;
    setOpen(true);
    start();
  }

  function hide() {
    if (!open()) return;
    setOpen(false);
    stop();
  }

  function toggle() {
    if (open()) hide();
    else show();
  }

  return { open, snapshot, error, show, hide, toggle, refresh };
}

export const devPerfStore = createRoot(createDevPerfStore);
