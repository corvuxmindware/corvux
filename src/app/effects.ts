import { createEffect, on, onCleanup, onMount, type Accessor } from "solid-js";
import { type UnlistenFn } from "@tauri-apps/api/event";
import { commands, events, isTauri, tryRestoreBrowserRoost, unwrap } from "../lib/invoke";
import { roostStore } from "../stores/roost";
import { tabStore } from "../stores/tabs";
import { graphStore } from "../stores/graph";
import { chatStore } from "../stores/chat";
import { syncStatusStore } from "../stores/sync";
import { invalidateImageInEditors } from "../components/Editor/extensions/image-embed";
import { invalidateAudioInEditors } from "../components/Editor/extensions/audio-embed";
import { markAttachmentTooLarge } from "../components/Editor/extensions/attachment-missing-notice";
import { roostSettingsStore } from "../stores/roostSettings";
import { settingsStore } from "../stores/settings";
import { startupStore } from "../stores/startup";
import { updaterStore } from "../stores/updater";
import { bugReportStore } from "../stores/bugReport";
import { GRAPH_TAB_PATH } from "../stores/tabs";
import { tasksStore } from "../stores/tasks";
import { searchStore } from "../stores/search";
import type { SaveOutcome } from "../lib/save";
import { lsGet } from "../lib/storage";
import { log } from "../lib/log";
import { recordSelfWrite, wasRecentSelfWrite } from "../lib/self-write-guard";

/// Wire up the post-save callback on the tab store and start the sync
/// status broadcaster. Idempotent — only runs once on mount.
///
/// AUDIT-053: the prior implementation chained 3 follow-up IPCs after
/// every save (`index_note` → `get_backlinks` → `index_search` with
/// `suggestionsStore.fetchDebounced` after). Saves now fire ONE
/// `save_roost_note` IPC (inside `tabStore.saveTab`), and this
/// callback receives the bundled `SaveOutcome` and dispatches store
/// updates synchronously — no more chained round-trips, no more
/// per-save WebView/JNI bridge cost spike on mobile.
///
/// The same callback fires for every save entry point (auto-save
/// debounce, Cmd+S, command palette, tab close) because saveTab itself
/// invokes it now. Adding a new post-save concern lands in ONE place.
export function installSaveAndIdleHooks() {
  tabStore.setOnAfterSave((path: string, _content: string, outcome: SaveOutcome | null) => {
    const root = roostStore.roostPath();
    const normalizedRoot = root?.replace(/[\\/]+$/, "");
    const normalizedPath = path.replace(/\\/g, "/");
    const rootForCompare = normalizedRoot?.replace(/\\/g, "/");
    if (
      !rootForCompare ||
      (normalizedPath !== rootForCompare &&
        !normalizedPath.startsWith(`${rootForCompare}/`))
    ) return;
    // Mark this as our own write so the filesystem-watcher echo of it
    // (`note_written` / `note_deleted` below) doesn't reload or close the
    // open tab — on mobile a reload drops the soft keyboard, and a
    // delete-echo from the atomic save would close the note mid-typing.
    recordSelfWrite(path);

    // NOTE: we deliberately do NOT bump a "sync outgoing" counter here. A
    // local save is not the same as a change reaching a peer — the old
    // optimistic bump painted the TopBar `↑N` even with sync disabled or no
    // peer connected. The indicator now reflects the backend's durable outbox
    // depth via `outbox_changed` events (see stores/sync.ts), which is 0
    // whenever nothing is actually outbound.

    // Non-markdown save (settings JSON, config file). No graph / search
    // indexing applies; nothing to dispatch.
    if (!outcome) return;

    // Apply the bundled graph stats + backlinks + tasks. One synchronous
    // pass over store state, zero follow-up IPCs.
    graphStore.applySaveOutcome(outcome);

    // FTS5 fail-soft: the bundled IPC tried to update the search index
    // but the search DB hit an error (read-only mount, disk full,
    // search DB not initialised yet). Surface as a debug log — a
    // non-blocking toast would be noisy for a transient condition, and
    // the next save will retry.
    if (outcome.fts5.kind === "failed") {
      log.warn("startup", "FTS5 index update failed (will retry on next save)", outcome.fts5.message);
    }
  });

  syncStatusStore.start();
}

/// One crash prompt per app session. `check_previous_crash` returns
/// process-cached state, so without this the roost-change effect below
/// would re-surface the same dialog on every roost open. Module-scoped:
/// resets only on webview reload (dev) or process restart.
let crashPromptShownThisSession = false;

// Monotonic counter bumped on every roost switch. `runStartupSequence`
// captures the value at entry and bails if it goes stale — i.e. the user
// switched roosts while an earlier sequence was parked on an `await`. This
// stops a stale sequence from firing `syncTryAutostart` against the
// previous roost, which the backend would otherwise initialize against the
// now-current roost's filesystem backend (cross-roost data bleed).
// Defense-in-depth paired with the backend `same_roost_root` guard in
// `sync_init_inner`.
let roostGeneration = 0;

/// Watch for roost changes and run post-open work sequentially.
///
/// The phases run in order, lightest first, so the file tree + editor
/// become usable before heavier subsystems (the sync engine) start
/// competing for the Rust runtime. On a memory-pressed phone,
/// running everything concurrently at startup saturated the IPC dispatcher
/// for 10–30s and made auto-save round-trips queue behind the roost walk.
///
/// Order: roost (already done by openRoost) → settings → chat history →
/// sync. Graph + FTS stay lazy on first use of chat / search / graph tabs.
/// Small inter-phase yields let the renderer paint each phase update before
/// kicking off the next IPC.
export function installRoostChangeEffect() {
  createEffect(
    on(
      () => roostStore.roostPath(),
      (path) => {
        const generation = ++roostGeneration;
        // Clear any prior roost's live sync state so a stale peer count /
        // outbox backlog can't bleed onto a roost with no sync configured
        // (Desktop bug #3, 0.2.1). The new roost re-seeds via autostart below
        // only if it actually has sync enabled.
        syncStatusStore.resetForRoostSwitch();
        chatStore.resetForRoostSwitch();
        roostSettingsStore.clear();
        searchStore.clear();
        tasksStore.clear();
        graphStore.setRoostPath(path);
        if (!path) {
          startupStore.setPhase("idle");
          return;
        }
        void runStartupSequence(path, generation);
      },
    ),
  );
}

/// Yield to the renderer so the phase label has a chance to repaint
/// between IPCs. 16 ms ≈ one frame on a 60 Hz display; on a 90/120 Hz
/// phone it's still well under a single frame budget.
function nextFrame(): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, 16));
}

async function runStartupSequence(path: string, generation: number) {
  // The user may switch roosts while this sequence is parked on an `await`.
  // A newer generation supersedes us — bail so we never run a later phase
  // (above all, sync autostart) against a roost that is no longer active.
  const superseded = () => generation !== roostGeneration;

  startupStore.setPhase("settings");
  await nextFrame();
  if (superseded()) return;
  try {
    await roostSettingsStore.load(path);
    if (superseded()) return;
    const roostTheme = roostSettingsStore.theme();
    const deviceTheme = settingsStore.theme();
    // First-install onboarding case: user just picked a theme on the
    // theme-picker step, hasn't completed onboarding yet. Skip the
    // roost → device override and instead push the device's choice
    // INTO the roost so the picked theme stays consistent across all
    // onboarding pages. After Finish, normal "roost wins" resumes.
    const onboardingComplete =
      lsGet("corvux:onboarded") === "true";
    if (roostTheme && roostTheme !== deviceTheme) {
      if (onboardingComplete) {
        settingsStore.setTheme(roostTheme);
      } else {
        await roostSettingsStore.save({ appearance: { theme: deviceTheme } });
        if (superseded()) return;
      }
    }
  } catch (e) {
    log.warn("startup", "roost settings load failed", e);
  }

  if (superseded()) return;
  startupStore.setPhase("history");
  await nextFrame();
  if (superseded()) return;
  try {
    await chatStore.loadChatHistory(path);
  } catch (e) {
    log.warn("startup", "chat history load failed", e);
  }

  // Only pay the autostart IPC + app-data read cost when the user
  // has actually enabled sync at least once. Stops cold-start work
  // on every launch for users who never turned sync on.
  if (isTauri && lsGet("corvux:syncEnabled") === "1") {
    if (superseded()) return;
    startupStore.setPhase("sync");
    await nextFrame();
    // Final gate right before the autostart IPC: this is the exact call the
    // backend guard also protects. Bailing here means a roost switch that
    // happened during the yields above never reaches the wrong roost.
    if (superseded()) return;
    try {
      // `deviceName` is no longer part of the Rust signature; specta
      // would have caught the extra arg the old call site was passing
      // (Tauri silently dropped it pre-typing).
      await unwrap(commands.syncTryAutostart(path));
    } catch (e) {
      log.warn("startup", "sync_try_autostart failed", e);
    }
  }

  if (superseded()) return;
  startupStore.setPhase("ready");

  // Post-crash prompt. `check_previous_crash` returns state cached at
  // process setup — it stays `crashed: true` for the whole session, and
  // this startup routine re-runs on every roost open / reload. Without
  // the once-guard the same dialog re-fires after every dismissal until
  // the process restarts (observed: prompt on open, again on next roost
  // open, again after a webview reload). Guard is process-scoped: one
  // prompt per app session is the correct frequency for one crash.
  // We open the bug-report modal directly (rather than via a toast →
  // click → modal hop) because dismissal is one Esc / backdrop click
  // away — non-aggressive — and going straight to the modal saves the
  // user a step when their attention is already on "what just
  // happened?". Browser-dev mode skips this — the IPC isn't available
  // off Tauri.
  if (isTauri && !crashPromptShownThisSession) {
    setTimeout(() => {
      void unwrap(commands.checkPreviousCrash())
        .then((info) => {
          if (!info.crashed) return;
          if (crashPromptShownThisSession) return;
          crashPromptShownThisSession = true;
          bugReportStore.show({
            kind: "previous-crash",
            initialDescription: "",
            crashExcerpt: info.crash_excerpt ?? undefined,
          });
        })
        .catch((err) => {
          // Non-fatal — log + drop. Crash detection itself must never
          // crash the boot path.
          log.warn("startup", "check_previous_crash failed", err);
        });
    }, 1500);
  }

  // Schedule the post-launch updater check. The store self-throttles
  // (no more than once an hour) and waits 30s after this call so the
  // first roost interaction is never blocked by a network round-trip
  // to the manifest endpoint. Mobile / browser builds early-return.
  updaterStore.scheduleStartupCheck();

  // Background graph init AFTER the main startup phases are done. The
  // editor is already usable; this just makes sure the graph DB is live
  // before the user's first auto-save (which fires 2s after the last
  // keystroke). Without it, edits made before the user opens any
  // graph-touching pane (Tasks / History / Search / Chat / Graph) are
  // not recorded as `:edit_event` rows and don't appear in the History
  // pane. Warm starts complete in ~30ms, so this is a no-op cost.
  //
  // Wrapped in an immediate `void` — failures here are non-fatal and
  // surface through `graphStore.initError` if anything goes wrong.
  void graphStore.ensureGraphReady();
}

/// Refresh the file tree when the sync engine writes a file we didn't know
/// about (new file from a peer) or when the window regains focus (catches
/// anything that slipped through while backgrounded — crucial on Android
/// SAF where there's no live filesystem watcher).
export function installSyncAndFocusListeners() {
  onMount(() => {
    if (!isTauri) return;
    let unlistenSync: UnlistenFn | undefined;
    // Debounce so a burst of inbound sync messages collapses to one
    // file-tree re-read instead of N.
    //
    // `force` matters for DELETIONS: refreshFiles() unforced is a no-op
    // inside roostStore's 10s REFRESH_DEBOUNCE window, and a reconnect
    // burst almost always refreshes the tree seconds before the
    // tombstones land — so the delete's soft refresh was swallowed and
    // the tree kept ghost entries (notes deleted on the other device
    // while offline stayed listed, but wouldn't open because the file
    // was already trashed). A deletion must force the re-read; a
    // pending forced request wins over soft ones in the same batch.
    let refreshTimer: number | null = null;
    let refreshForce = false;
    const scheduleRefresh = (opts?: { force?: boolean }) => {
      if (opts?.force) refreshForce = true;
      if (refreshTimer !== null) return;
      refreshTimer = window.setTimeout(() => {
        refreshTimer = null;
        const force = refreshForce;
        refreshForce = false;
        void roostStore.refreshFiles(force ? { force: true } : undefined);
      }, 400) as unknown as number;
    };

    // CO-4: debounced refresh of a specific note's backlinks. A sync burst can
    // fire many note_written events; collapse them to one backlinks query for
    // the currently-open note (whichever it is when the timer fires).
    let backlinkRefreshTimer: number | null = null;
    let backlinkRefreshPath: string | null = null;
    const scheduleActiveBacklinkRefresh = (path: string) => {
      backlinkRefreshPath = path;
      if (backlinkRefreshTimer !== null) return;
      backlinkRefreshTimer = window.setTimeout(() => {
        backlinkRefreshTimer = null;
        const target = backlinkRefreshPath;
        backlinkRefreshPath = null;
        // Only refresh if it's still the active note when the timer fires.
        if (target && tabStore.activeTabPath() === target) {
          void graphStore.refreshBacklinks(target);
        }
      }, 500) as unknown as number;
    };
    // Phase B: backend background-revalidates the SAF tree cache after
    // every warm `open_roost` and emits this event when the on-disk
    // tree differs from the cached snapshot (e.g., user edited via
    // another app between sessions). Patch the store directly so the
    // sidebar updates without re-walking the roost on the frontend.
    let unlistenTree: UnlistenFn | undefined;
    let unlistenGraphChanged: UnlistenFn | undefined;
    events.roostTreeChanged
      .listen((e) => {
        roostStore.applyTreeFromBackend(e.payload);
      })
      .then((u) => (unlistenTree = u))
      .catch((e) => log.warn("startup", "tree-changed listener failed", e));
    events.graphChanged
      .listen(() => graphStore.backendGraphChanged())
      .then((u) => (unlistenGraphChanged = u))
      .catch((e) => log.warn("startup", "graph-changed listener failed", e));

    // AUDIT-027: subscribe via the syncStatusStore fan-out instead of
    // opening a second Tauri `sync://event` listener. Same payloads, one
    // deserialise per backend emit.
    // Does a candidate path (tab path / file-tree entry — absolute or
    // SAF tree-uri form) refer to the same note as the roost-relative path
    // `rel` the sync engine emits? Suffix-match handles desktop; SAF URIs are
    // URL-encoded (`%2FCorvux%2Ffoo.md`) so we decode once before matching.
    const matchesRel = (candidate: string, rel: string): boolean => {
      let decoded = candidate;
      try {
        decoded = decodeURIComponent(candidate);
      } catch {
        // Not URI-encoded (or malformed) — fall through to the raw form.
      }
      return (
        candidate === rel ||
        decoded === rel ||
        candidate.endsWith(`/${rel}`) ||
        candidate.endsWith(`\\${rel}`) ||
        decoded.endsWith(`/${rel}`) ||
        decoded.endsWith(`\\${rel}`)
      );
    };

    // Find a note's display-path (the form the file tree + FTS index are
    // keyed under) from the file tree, given the roost-relative path.
    const findDisplayPath = (
      entries: { path: string; is_dir: boolean; children: unknown[] }[],
      rel: string,
    ): string | undefined => {
      for (const e of entries) {
        if (e.is_dir) {
          const found = findDisplayPath(
            e.children as typeof entries,
            rel,
          );
          if (found) return found;
        } else if (matchesRel(e.path, rel)) {
          return e.path;
        }
      }
      return undefined;
    };

    unlistenSync = syncStatusStore.onEvent((ev) => {
      if (ev.kind === "attachment_written") {
        // A requested blob just landed on disk. Re-decorate any open
        // editor's embed for it (📎 "not synced yet" → the image/player)
        // without reloading the note (a reload drops the mobile keyboard).
        // The path could be an image or a small audio clip; ask both — each
        // only rebuilds if it actually has the path cached.
        invalidateImageInEditors(ev.rel);
        invalidateAudioInEditors(ev.rel);
        return;
      }
      if (ev.kind === "attachment_too_large") {
        // Permanent: the attachment is over the sync cap. Mark it so the
        // embed shows "too large to sync", and re-decorate to flip the copy.
        markAttachmentTooLarge(ev.rel);
        invalidateImageInEditors(ev.rel);
        invalidateAudioInEditors(ev.rel);
        return;
      }
      if (ev.kind !== "note_written" && ev.kind !== "note_deleted") return;
      // Deletions force the tree re-read past roostStore's 10s debounce —
      // an entry that must VANISH can't wait for the next organic refresh
      // (ghost-note bug); writes keep the cheap soft path.
      scheduleRefresh(ev.kind === "note_deleted" ? { force: true } : undefined);
      const p = ev.path;
      if (!p) return;
      // Sync emits a roost-relative path (`Corvux/foo.md`); tabs hold the
      // absolute / tree-uri form. Resolve the open tab (if any) via the
      // shared matcher.
      const openTab = tabStore.tabs().find((t) => matchesRel(t.path, p));
      const absPath = openTab?.path ?? p;

      // Self-write echo: the filesystem watcher is reporting the app's OWN
      // save (see self-write-guard). Reloading would replace the CodeMirror
      // doc and drop the mobile keyboard; a delete-echo from the atomic
      // `write tmp → rename` would close the note mid-typing.
      //
      // IP-13: the guard must gate only the UI-disruptive actions (reload /
      // delete-close), NOT the reindex. A genuine peer edit that MERGES into a
      // note we also just saved lands within the 4s window and looks like a
      // self-write; suppressing its reindex left the graph pointing at
      // pre-merge content (silent divergence). So: note_deleted still bails on
      // a self-write echo, but note_written always reindexes below — it only
      // skips the editor reload.
      const isSelfWrite = wasRecentSelfWrite(openTab?.path ?? p) || wasRecentSelfWrite(p);

      if (ev.kind === "note_deleted") {
        if (isSelfWrite) return;
        // A peer deleted this note; the backend already trashed the file on
        // disk. Mirror the local-delete cleanup so the receiving device
        // doesn't keep a ghost in the graph / search / UI.
        const runDeleteCleanup = () => {
          // 1. Close the tab if the deleted note was open.
          if (openTab) void tabStore.onFileDeleted(openTab.path);
          // 2. Purge the graph (backlinks, tags, :task rows, node). `note_id`
          //    is form-tolerant, so the roost-relative path resolves correctly.
          //    CO-3: refresh the cached stats + bump the graph revision once
          //    the purge lands, so the node/link/tag totals and any open graph
          //    overlay stop showing the deleted note.
          void unwrap(commands.purgeNoteFromGraph(p))
            .then(() => graphStore.notePurged())
            .catch((err) => log.warn("startup", `note_deleted graph purge failed for ${p}`, err));
          // 3. Remove from the FTS5 index. It's keyed by the display path, so
          //    resolve that from the file tree (still present pre-refresh),
          //    falling back to the open tab path.
          const display = findDisplayPath(roostStore.files(), p) ?? openTab?.path;
          if (display) {
            // Capture the external delete into edit history BEFORE removing it
            // from the search index — record_external_delete reads the
            // last-indexed content as the deletion's "before" snapshot, which
            // removeFromSearch is about to erase. This handler only runs for
            // genuine external deletes (self-write echoes bailed above).
            void unwrap(commands.recordExternalDelete(display))
              .catch((err) =>
                log.warn("startup", `record external delete failed for ${display}`, err),
              )
              .finally(() => {
                void unwrap(commands.removeFromSearch(display)).catch((err) =>
                  log.warn("startup", `note_deleted FTS remove failed for ${display}`, err),
                );
              });
          }
        };
        // Defensive: an atomic-save rename can surface as a removal on some
        // platforms. Only run the destructive cleanup once we've confirmed
        // the file is genuinely gone — if it still reads, the "delete" was a
        // spurious watcher echo and closing the note would lose the user's place.
        //
        // Probe by the DISPLAY path, not the raw sync path: for a note with
        // no open tab, `absPath` is the roost-RELATIVE form, and readFile
        // rejects relative paths outright ("Access denied" — same trap as
        // the note_written display-path resolution below). Probing with it
        // made every unopened note's delete look "genuinely gone", so the
        // spurious-echo guard only ever protected open tabs. Resolve from
        // the (pre-refresh) tree; if the note is in neither a tab nor the
        // tree, there's nothing user-visible left — cleanup is idempotent
        // and safe to run.
        const probePath =
          openTab?.path ?? findDisplayPath(roostStore.files(), p);
        if (!probePath) {
          runDeleteCleanup();
          return;
        }
        void unwrap(commands.readFile(probePath)).then(
          () =>
            log.warn(
              "startup",
              `note_deleted ignored — ${probePath} still exists (spurious echo)`,
            ),
          () => runDeleteCleanup(),
        );
        return;
      }

      // ── note_written ──────────────────────────────────────────────────
      // If the note is open in a tab, reload it from disk so the editor shows
      // the incoming edit immediately — but NOT for our own save echo (that
      // would replace the CodeMirror doc mid-edit and drop the mobile
      // keyboard). The reindex below runs regardless (IP-13).
      if (openTab && !isSelfWrite) {
        void tabStore.reloadTabFromDisk(openTab.path);
      }

      // Reindex the note so the graph (and the Tasks pane derived from
      // `:task` rows) reflects the new content. External edits (vim,
      // Obsidian, agent shells) and incoming peer-sync writes must flow into
      // the indexer, not just in-app editor saves.
      //
      // CRITICAL: index by the DISPLAY path (the form the file tree, FTS
      // index, and graph node ids are keyed under) — NOT the raw sync path.
      // Sync emits a roost-RELATIVE path; `read_file`/`index_note` strip the
      // roost root off an absolute path, so passing the relative form fails
      // ("Access denied") and the note was silently never indexed for any
      // note not already open in a tab. Resolve the display path from the file
      // tree; a brand-new synced note may not be in the tree yet, so refresh
      // once and retry before giving up.
      void (async () => {
        let display = openTab?.path ?? findDisplayPath(roostStore.files(), p);
        if (!display) {
          await roostStore.refreshFiles({ force: true });
          display = findDisplayPath(roostStore.files(), p);
        }
        if (!display) {
          log.warn(
            "startup",
            `note_written: could not resolve a display path for ${p}; graph/tasks not updated`,
          );
          return;
        }
        if (!(display.endsWith(".md") || display.endsWith(".markdown"))) return;
        try {
          const content = await unwrap(commands.readFile(display));
          // Capture this write into edit history BEFORE reindexing. External
          // edits (MCP, Obsidian, git, peer sync) are otherwise invisible to
          // the History pane. record_external_edit reads the pre-write content
          // from the search index as its diff baseline, so it must run before
          // anything advances that baseline; its after_hash dedup makes it a
          // no-op for our own save echoes (IP-13: we call it regardless of the
          // self-write guard and let the content-based dedup decide, so a peer
          // edit landing inside the 4s self-write window is still captured).
          try {
            await unwrap(commands.recordExternalEdit(display, content));
          } catch (err) {
            log.warn("startup", `record external edit failed for ${display}`, err);
          }
          await graphStore.indexNote(display, content);
        } catch (err) {
          log.warn("startup", `note_written reindex failed for ${display}`, err);
        }
      })();

      // CO-4: the note that just changed may now link TO the note we're
      // reading, so the active note's backlinks panel can be stale. Refresh it
      // (debounced — a sync burst fires many note_written events). Skip when
      // the changed note IS the active note: indexNote already refreshed its
      // own outgoing view, and backlinks-of-self didn't change from its edit.
      const active = tabStore.activeTabPath();
      if (active && !matchesRel(active, p)) {
        scheduleActiveBacklinkRefresh(active);
      }
    });

    const onForeground = () => {
      // Refresh the file tree so any notes peers wrote while we were
      // backgrounded show up. Do NOT rescan the whole roost — that walks
      // every file and is painful on mobile.
      roostStore.refreshFiles();
      // Kick an immediate sync reconnect. On mobile the process was likely
      // suspended while backgrounded: the QUIC link idled out and the
      // backend reconnect supervisor's timer was frozen, so nothing redials
      // until its next tick (and per-peer backoff may have climbed to
      // minutes). syncReconnectAll resets backoff and dials now; it's a
      // cheap no-op when sync isn't running. This is the primary
      // foreground-reconnect trigger on mobile — the webview visibility
      // event is more reliable there than the native RunEvent::Resumed.
      void commands.syncReconnectAll().catch(() => {});
    };
    // `visibilitychange` is the reliable background→foreground signal on
    // mobile webviews; `focus` covers desktop window refocus. Either one
    // returning us to visible triggers the reconnect.
    const onVisibility = () => {
      if (document.visibilityState === "visible") {
        onForeground();
      } else if (document.visibilityState === "hidden") {
        // Mobile background safety: the OS may freeze or kill us mid-debounce.
        // Flush every modified tab now (saveAllTabs also commits a focused
        // table cell) so an edit made in the last 500ms isn't lost on
        // background. Best-effort — errors are swallowed like the reconnect.
        void tabStore.saveAllTabs().catch(() => {});
      }
    };
    window.addEventListener("focus", onForeground);
    document.addEventListener("visibilitychange", onVisibility);
    onCleanup(() => {
      window.removeEventListener("focus", onForeground);
      document.removeEventListener("visibilitychange", onVisibility);
      if (refreshTimer !== null) window.clearTimeout(refreshTimer);
      unlistenSync?.();
      unlistenTree?.();
      unlistenGraphChanged?.();
    });
  });
}

/// Whenever the active tab changes, refresh backlinks for the new path.
///
/// Backlinks are debounced 400ms because rapid tab-flicking (e.g. opening
/// multiple notes from the sidebar in quick succession) was firing one
/// `get_backlinks` IPC per switch and queuing them all behind SAF/FUSE on
/// Android. 400ms collapses bursts into a single IPC for the tab the user
/// actually settles on.
export function installActiveTabEffect() {
  let backlinksTimer: number | null = null;
  createEffect(
    on(
      () => tabStore.activeTabPath(),
      (path) => {
        if (backlinksTimer !== null) {
          window.clearTimeout(backlinksTimer);
          backlinksTimer = null;
        }
        if (path && path !== GRAPH_TAB_PATH) {
          backlinksTimer = window.setTimeout(() => {
            backlinksTimer = null;
            graphStore.refreshBacklinks(path);
          }, 400) as unknown as number;
        }
      },
    ),
  );
  onCleanup(() => {
    if (backlinksTimer !== null) window.clearTimeout(backlinksTimer);
  });
}

/// First-run gate: show onboarding if the user has never completed it AND
/// no roost is saved. Otherwise restore the last roost. Returns the signal
/// setter so App can pass it back to the Onboarding component.
export function installOnboardingGate(setShowOnboarding: (v: boolean) => void) {
  onMount(async () => {
    const onboarded = lsGet("corvux:onboarded");
    const lastRoost = lsGet("corvux:lastVault");
    const isBrowserRoost =
      !isTauri && lastRoost?.startsWith("/browser-roost/") === true;

    // Browser dev mode: FSAccess folder handles don't survive page reload, but
    // the handle itself is stored in IndexedDB. If the browser still grants
    // permission (typical for same-session reloads), rebind silently — no
    // re-pick. Critical for automated testing and good for daily refresh UX.
    if (isBrowserRoost && lastRoost) {
      const restored = await tryRestoreBrowserRoost(lastRoost);
      if (restored) {
        await roostStore.restoreLastRoost();
        return;
      }
      // IDB had nothing or permission was revoked — fall through to onboarding.
      setShowOnboarding(true);
      return;
    }

    if (!onboarded && !lastRoost) {
      setShowOnboarding(true);
    } else {
      roostStore.restoreLastRoost();
    }
  });
}

/// One-call wrapper for App.tsx that installs every effect/listener. The
/// individual installers are exported too in case future code needs to wire
/// them in a different order.
export function installAppEffects(opts: {
  setShowOnboarding: (v: boolean) => void;
  registerKeyboardShortcuts: () => () => void;
}) {
  installOnboardingGate(opts.setShowOnboarding);
  onMount(() => {
    // Track the on-screen keyboard height so the editor / chat can keep their
    // caret / input above it under `interactive-widget=overlays-content`.
    void import("../lib/keyboard-inset").then(({ installKeyboardInset }) =>
      installKeyboardInset(),
    );
    installSaveAndIdleHooks();
    const dispose = opts.registerKeyboardShortcuts();
    onCleanup(dispose);
    // Register the two-action notification type once at startup so
    // every task-due notification fired by the Rust scheduler can
    // surface Complete / Snooze buttons. Fires-and-forgets — failure
    // degrades to plain single-tap notifications, which is the old
    // behavior.
    void import("../lib/notification-actions").then(
      ({ installTaskNotificationActions }) => {
        void installTaskNotificationActions();
      },
    );
  });
  installRoostChangeEffect();
  installSyncAndFocusListeners();
  installActiveTabEffect();
}

// `_` import to keep tree-shaking from dropping the Accessor type — kept
// because external callers pattern-match on it via TS.
export type _Accessor<T> = Accessor<T>;
