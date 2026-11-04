/// Edit history store — Phase 2.2 (the trust gate before agent CRUD).
///
/// Reads `:edit_event` rows for the active note, drives the History pane,
/// and routes revert through `revert_edit`. The store is keyed on the
/// currently-loaded note path; switching tabs reloads the log.
///
/// Mirrors the shape of `tasks.ts` so the pane can subscribe to
/// `events()` / `loaded()` / `error()` the same way TasksPanel does.

import { createSignal, createRoot } from "solid-js";
import { commands, unwrap } from "../lib/invoke";
import { log } from "../lib/log";
import type { EditEventDto } from "../lib/bindings";

export interface EditEvent {
  noteId: string;
  eventId: string;
  actorType: "user" | "agent" | "system" | string;
  actorId: string;
  intent: string;
  /// Unix epoch seconds (Float in CozoDB).
  timestamp: number;
  /// Unified diff text (`similar` crate output). Pre-formatted with
  /// `before` / `after` headers + a 3-line context radius.
  diff: string;
  linesAdded: number;
  linesRemoved: number;
  /// Empty string when the event hasn't been reverted; otherwise the
  /// `eventId` of the revert event that undid it.
  revertedBy: string;
  notePath: string;
  /// Hard block: the event has already been reverted. Pane disables the
  /// Revert button. Distinct from `needsForce`.
  revertConflicts: boolean;
  /// EH-1: reverting this event would discard work (a later unreverted
  /// edit exists, or the file changed outside Corvux's history). Revert
  /// stays enabled but goes through the force-confirm dialog; the pane
  /// shows a "will discard newer changes" hint. Most non-latest rows are
  /// `true`, so it must NOT disable the button.
  needsForce: boolean;
}

type RawEditEvent = EditEventDto;

function createEditHistoryStore() {
  const [events, setEvents] = createSignal<EditEvent[]>([]);
  const [loaded, setLoaded] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);
  const [activePath, setActivePath] = createSignal<string | null>(null);
  const [reverting, setReverting] = createSignal<string | null>(null);

  function fromRaw(r: RawEditEvent): EditEvent {
    return {
      noteId: r.note_id,
      eventId: r.event_id,
      actorType: r.actor_type,
      actorId: r.actor_id,
      intent: r.intent,
      // `timestamp` is `f64` on the Rust side; tauri-specta surfaces it as
      // `number | null` (NaN/Infinity can't survive JSON). Coalesce to 0
      // — UI just renders "—" for a zero timestamp.
      timestamp: r.timestamp ?? 0,
      diff: r.diff,
      linesAdded: r.lines_added,
      linesRemoved: r.lines_removed,
      revertedBy: r.reverted_by,
      notePath: r.note_path,
      revertConflicts: r.revert_conflicts,
      needsForce: r.needs_force,
    };
  }

  /// Load history for `notePath`. Caller usually passes the active tab
  /// path; the pane re-runs this whenever the tab changes. Cleared
  /// state when `notePath` is empty so the pane shows an idle message
  /// rather than stale rows from the previous note.
  async function load(notePath: string | null): Promise<void> {
    setError(null);
    setActivePath(notePath);
    if (!notePath) {
      setEvents([]);
      setLoaded(true);
      return;
    }
    setLoaded(false);
    try {
      // The `:edit_event` relation lives in CozoDB, which is lazily
      // initialised on first demand (graph view / tasks pane / now
      // history pane). Without this guard, opening the History pane
      // on a fresh roost open before any other graph-touching feature
      // ran would fail with "Graph DB not initialized".
      const { graphStore } = await import("./graph");
      await graphStore.ensureGraphReady();

      const raw = await unwrap(commands.listEditEvents(notePath, 100));
      setEvents(raw.map(fromRaw));
      setLoaded(true);
    } catch (e) {
      log.error("editHistory", "list failed", e);
      setError(String(e));
      setEvents([]);
      setLoaded(true);
    }
  }

  /// Refresh the current note's log. Called by save flows after an edit
  /// has just been recorded so the pane is live without a tab switch.
  async function refresh(): Promise<void> {
    const path = activePath();
    if (!path) return;
    await load(path);
  }

  /// Revert one event. `force = true` re-issues after the backend rejected
  /// the first attempt with a "subsequent edits would be overwritten"
  /// error — the pane surfaces a confirmation dialog and re-calls.
  /// Returns the new file content so the caller can update the open tab.
  async function revert(
    eventId: string,
    force: boolean,
  ): Promise<string | null> {
    const path = activePath();
    if (!path) return null;
    setReverting(eventId);
    try {
      const newContent = await unwrap(commands.revertEdit(path, eventId, force));
      // Reload the tab from disk so the editor reflects the revert.
      const { tabStore } = await import("./tabs");
      await tabStore.reloadTabFromDisk(path);
      await refresh();
      return newContent;
    } catch (e) {
      const msg = String(e);
      // Surface the conflict error verbatim — UI uses it to decide
      // whether to show the "Revert anyway" confirmation.
      setError(msg);
      throw e;
    } finally {
      setReverting(null);
    }
  }

  function clearError() {
    setError(null);
  }

  return {
    events,
    loaded,
    error,
    activePath,
    reverting,
    load,
    refresh,
    revert,
    clearError,
  };
}

/// Singleton — `createRoot` keeps the signals alive for the lifetime of
/// the app, matching the pattern used by `tasksStore`.
export const editHistoryStore = createRoot(createEditHistoryStore);
