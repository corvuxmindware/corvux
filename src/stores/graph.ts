import { createSignal, createRoot } from "solid-js";
import { commands, unwrap } from "../lib/invoke";
import { log } from "../lib/log";
import { toastStore } from "../components/Toast/Toast";
import { tasksStore } from "./tasks";

export interface BacklinkEntry {
  title: string;
  path: string;
}

export interface GraphStats {
  notes: number;
  links: number;
  tags: number;
}

function createGraphStore() {
  const [backlinks, setBacklinks] = createSignal<BacklinkEntry[]>([]);
  // Structural equality: the backend hands back a fresh stats object on
  // every save, but a no-op save (content edited, links/tags unchanged)
  // produces identical counts. Without this, the identity change still
  // notifies subscribers — most expensively GraphView's 5s reload timer,
  // which would re-settle the whole layout for a save that changed nothing
  // structural. Comparing fields makes unchanged saves a true no-op.
  const [stats, setStats] = createSignal<GraphStats>(
    { notes: 0, links: 0, tags: 0 },
    { equals: (a, b) => a.notes === b.notes && a.links === b.links && a.tags === b.tags },
  );

  // ── Graph layout continuity ──────────────────────────────────────
  // Module-level (survives GraphView remounts — tab switches, stats
  // reloads, mode flips) snapshot of where nodes settled + the last
  // zoom/pan transform. GraphView writes this on every reload/unmount and
  // seeds the next render from it, so the graph no longer re-randomizes
  // and re-settles from scratch every time. Not persisted to disk — a
  // fresh app start gets a fresh layout, which is fine.
  let _layoutPositions = new Map<string, { x: number; y: number }>();
  let _layoutTransform: { x: number; y: number; k: number } | null = null;
  function saveGraphLayout(
    positions: Map<string, { x: number; y: number }>,
    transform: { x: number; y: number; k: number },
  ) {
    _layoutPositions = positions;
    _layoutTransform = transform;
  }
  function getGraphLayout(): {
    positions: Map<string, { x: number; y: number }>;
    transform: { x: number; y: number; k: number } | null;
  } {
    return { positions: _layoutPositions, transform: _layoutTransform };
  }
  const [isReady, setIsReady] = createSignal(false);
  const [initError, setInitError] = createSignal<string | null>(null);
  // CO-3: a monotonic counter bumped on EVERY graph mutation (index, save,
  // delete). Consumers that cache derived graph state (the graph view's
  // PageRank/community overlay cache) key their freshness on this instead of
  // on note/link/tag COUNTS. Counts miss count-neutral edits — retargeting a
  // link (`[[X]]`→`[[Y]]`) or renaming a tag leaves the three totals
  // unchanged, so an overlay cache keyed on counts never noticed the graph
  // actually changed and served stale PageRank sizes / cluster colors.
  const [graphRevision, setGraphRevision] = createSignal(0);
  function bumpRevision() {
    setGraphRevision((n) => n + 1);
  }
  // CO-5: the last note whose backlinks were requested. refreshBacklinks
  // no-ops before the graph is ready (returns early), and nothing replayed the
  // request once it became ready — so the backlinks panel showed a false "No
  // backlinks yet" for the note open at startup until the user switched tabs.
  // We stash the request here and replay it the moment isReady flips true.
  let _pendingBacklinkPath: string | null = null;
  // DB-1: true when the last init failed specifically because graph.db is
  // corrupt (`__db_corrupt__:` prefix from the Rust side). Distinct from a
  // transient init error because the recovery is different — rebuild from the
  // markdown on disk, not "retry". The Graph view offers a Repair button when
  // this is set.
  const [initCorrupt, setInitCorrupt] = createSignal(false);
  const CORRUPT_PREFIX = "__db_corrupt__:";

  // Track whether search index is initialized (separate from graph).
  const [isSearchReady, setIsSearchReady] = createSignal(false);

  // Counter that bumps every time a "start find-path" request fires.
  // GraphView watches it via createEffect and enters pickSource phase.
  // Counter (not boolean) so consecutive presses still trigger.
  const [findPathRequest, setFindPathRequest] = createSignal(0);
  function requestFindPath() {
    setFindPathRequest((n) => n + 1);
  }

  // One-shot mode preference for the next graph-tab open. Callers (e.g.
  // the floating-bar graph button on desktop, the IconRail context menu)
  // set this to "local" or "full" right before flipping to the graph
  // tab; GraphView reads + clears it on mount/load. Null means "respect
  // the user's last-picked mode" (defaults to full).
  const [pendingMode, setPendingMode] = createSignal<"full" | "local" | null>(null);
  function requestGraphMode(mode: "full" | "local") {
    setPendingMode(mode);
  }
  function consumeGraphMode(): "full" | "local" | null {
    const m = pendingMode();
    if (m !== null) setPendingMode(null);
    return m;
  }

  // One-shot path to highlight when the graph tab next opens. Set by the
  // PathFinder modal's "Show in graph" action (alongside requestGraphMode)
  // right before flipping to the graph tab. GraphView consumes + clears it
  // on mount and replays it as a `showing` path. Ids are internal note
  // ids — exactly what the renderer's highlightPath expects.
  const [pendingPath, setPendingPath] = createSignal<string[] | null>(null);
  function requestHighlightPath(ids: string[]) {
    setPendingPath(ids.length > 0 ? ids : null);
  }
  function consumeHighlightPath(): string[] | null {
    const p = pendingPath();
    if (p !== null) setPendingPath(null);
    return p;
  }

  // Track the roost path so lazy init can run when needed.
  let _roostPath: string | null = null;
  let _roostGeneration = 0;
  // Guard against concurrent init calls.
  let _graphInitPromise: Promise<void> | null = null;
  let _searchInitPromise: Promise<void> | null = null;

  /// Set the roost path for deferred init — does NOT trigger any indexing.
  /// The file tree + editor are immediately usable; graph and search
  /// initialise lazily when the user first needs them.
  function setRoostPath(path: string | null) {
    _roostGeneration += 1;
    _roostPath = path;
    // Reset readiness when roost changes (user switched roosts).
    setIsReady(false);
    setIsSearchReady(false);
    setInitError(null);
    setInitCorrupt(false);
    setBacklinks([]);
    setStats({ notes: 0, links: 0, tags: 0 });
    _pendingBacklinkPath = null;
    _layoutPositions = new Map();
    _layoutTransform = null;
    _graphInitPromise = null;
    _searchInitPromise = null;
  }

  /// Lazily initialise the graph DB. Called the first time the user opens
  /// Graph View. On warm start (DB already indexed) this returns in <100ms.
  async function ensureGraphReady(): Promise<void> {
    if (isReady()) return;
    if (_graphInitPromise) return _graphInitPromise;
    _graphInitPromise = _doInitGraph();
    return _graphInitPromise;
  }

  async function _doInitGraph() {
    if (!_roostPath) return;
    const path = _roostPath;
    const generation = _roostGeneration;
    const superseded = () => generation !== _roostGeneration || path !== _roostPath;
    setInitError(null);
    setInitCorrupt(false);
    try {
      const result = await unwrap(commands.initGraph(path));
      if (superseded()) return;
      setStats(result);
      setIsReady(true);
      bumpRevision();
      // CO-5: replay the backlinks request that arrived before the graph was
      // ready (the note open at startup), so its panel fills in instead of
      // showing a stale "No backlinks yet".
      if (_pendingBacklinkPath) {
        const path = _pendingBacklinkPath;
        _pendingBacklinkPath = null;
        void refreshBacklinks(path);
      }
      // The bulk init scan populates `:task` rows for every existing
      // checkbox in the roost. Pull them into the Tasks pane store now so
      // the user sees the full list the moment they open the panel.
      void tasksStore.refresh();
    } catch (e) {
      if (superseded()) return;
      const raw = typeof e === "string" ? e : (e as Error)?.message ?? String(e);
      // DB-1: the Rust side tags an unrecoverable/corrupt graph.db with the
      // `__db_corrupt__:` prefix (init_db → InitDbError::Corrupt). Retrying
      // won't help — the fix is to rebuild the DB from the markdown on disk.
      // Flag it so the Graph view shows a "Repair database" button instead of
      // a dead "Retry", and strip the internal prefix from the shown message.
      const corrupt = raw.startsWith(CORRUPT_PREFIX);
      const msg = corrupt ? raw.slice(CORRUPT_PREFIX.length) : raw;
      log.error("graph", "failed to init graph", msg);
      setInitError(msg);
      setInitCorrupt(corrupt);
      toastStore.showError(
        corrupt
          ? `Graph database is corrupt. Open Graph view to repair it. (${msg})`
          : `Graph init failed: ${msg}`,
        e,
        { code: corrupt ? "GRAPH_DB_CORRUPT" : "GRAPH_INIT_FAIL", module: "stores.graph.init" },
      );
      // Allow retry on next call.
      _graphInitPromise = null;
    }
  }

  /// DB-1: rebuild graph.db from scratch by re-scanning the markdown on disk.
  /// The recovery path for a corrupt DB (and a manual "reindex" escape hatch).
  /// Nukes the persisted graph.db and runs a fresh cold index, so it can be
  /// slow on a large roost — the caller shows progress. Returns true on
  /// success. On failure the corrupt state is left intact so the button stays.
  async function repairDatabase(): Promise<boolean> {
    if (!_roostPath) return false;
    const path = _roostPath;
    const generation = _roostGeneration;
    _graphInitPromise = null;
    setIsReady(false);
    try {
      const result = await unwrap(commands.rebuildDatabase(path));
      if (generation !== _roostGeneration || path !== _roostPath) return false;
      setStats(result);
      setInitError(null);
      setInitCorrupt(false);
      setIsReady(true);
      // IP-A5: rebuildDatabase also deletes search.db, so the FTS index must
      // re-init cold on the next search. Readiness is otherwise sticky and
      // ensureSearchReady() would short-circuit, leaving search on the grep
      // fallback until an app restart.
      setIsSearchReady(false);
      _searchInitPromise = null;
      void tasksStore.refresh();
      toastStore.show("Graph database rebuilt from your notes.", "success");
      return true;
    } catch (e) {
      if (generation !== _roostGeneration || path !== _roostPath) return false;
      const msg = typeof e === "string" ? e : (e as Error)?.message ?? String(e);
      log.error("graph", "failed to repair graph db", msg);
      toastStore.showError(`Database repair failed: ${msg}`, e, {
        code: "GRAPH_DB_REPAIR_FAIL",
        module: "stores.graph.repair",
      });
      return false;
    }
  }

  /// Lazily initialise the FTS5 search index. Called on first search query
  /// or when the Search panel opens. Warm start skips the roost walk.
  async function ensureSearchReady(): Promise<void> {
    if (isSearchReady()) return;
    if (_searchInitPromise) return _searchInitPromise;
    _searchInitPromise = _doInitSearch();
    return _searchInitPromise;
  }

  async function _doInitSearch() {
    if (!_roostPath) return;
    const path = _roostPath;
    const generation = _roostGeneration;
    try {
      await unwrap(commands.initSearch(path));
      if (generation !== _roostGeneration || path !== _roostPath) return;
      setIsSearchReady(true);
    } catch (e) {
      if (generation !== _roostGeneration || path !== _roostPath) return;
      log.error("graph", "failed to init search index", e);
      // Allow retry on next call.
      _searchInitPromise = null;
    }
  }

  /// Legacy eager init — kept for backward compatibility and for contexts
  /// where both graph + search should warm up together (e.g., a manual
  /// "reindex roost" action).
  async function initGraph(roostPath: string) {
    setRoostPath(roostPath);
    await ensureGraphReady();
    await ensureSearchReady();
  }

  async function refreshBacklinks(notePath: string) {
    // CO-5: remember the request even when we can't serve it yet, so
    // `_doInitGraph` can replay it once the graph is ready.
    if (!isReady()) {
      _pendingBacklinkPath = notePath;
      return;
    }
    const generation = _roostGeneration;
    try {
      const result = await unwrap(commands.getBacklinks(notePath));
      if (generation !== _roostGeneration) return;
      setBacklinks(result);
    } catch (e) {
      if (generation !== _roostGeneration) return;
      log.error("graph", "failed to get backlinks", e);
      setBacklinks([]);
    }
  }

  /**
   * AUDIT-053: apply the bundled `SaveOutcome` from `save_roost_note`.
   * Replaces the chained `indexNote` + `refreshBacklinks` pair so the
   * post-save UI update is one synchronous pass over store state
   * instead of two more IPCs. Tasks delegate to tasksStore; FTS5
   * outcome flows up to the caller (effects.ts handles the toast on
   * fail-soft).
   */
  function applySaveOutcome(outcome: import("../lib/save").SaveOutcome) {
    setStats(outcome.stats);
    setBacklinks(outcome.backlinks);
    tasksStore.applyTasksFromBackend(outcome.tasks);
    bumpRevision(); // CO-3: a save can change links without changing counts.
  }

  async function indexNote(path: string, content: string) {
    if (!isReady()) return;
    const generation = _roostGeneration;
    try {
      // The Rust handler returns stats AND the full task list in a
      // single response. Pre-Round 4 the save path fired three IPCs:
      // index_note + get_graph_stats + list_tasks. The latter two are
      // both folded in here — on Android each round-trip hops the
      // WebView/JNI bridge, so collapsing the trio is a measurable
      // mobile win.
      const resp = await unwrap(commands.indexNote(path, content));
      if (generation !== _roostGeneration) return;
      setStats(resp.stats);
      tasksStore.applyTasksFromBackend(resp.tasks);
      bumpRevision(); // CO-3: content may have retargeted links (count-neutral).
      // Edit-history pane stays live without a manual refresh. Dynamic
      // import breaks the graph → editHistory → tabs → graph cycle that a
      // top-level import would create. The store is a no-op when no
      // markdown tab is active.
      void import("./editHistory").then(({ editHistoryStore }) => {
        if (editHistoryStore.activePath() === path) {
          void editHistoryStore.refresh();
        }
      });
    } catch (e) {
      log.error("graph", "failed to index note", e);
    }
  }

  /**
   * Read a markdown file from disk and (re)index it into the graph.
   * The single chokepoint for "a note's path just appeared or changed —
   * make sure the graph has a node for it." Used after create / move /
   * rename, which write (or relocate) a file on disk but don't index it
   * themselves. Non-markdown paths and a not-yet-ready graph are no-ops
   * (the lazy graph-init full-scan catches anything created while it was
   * cold). Reading the content back rather than passing a known string
   * keeps us drift-proof against whatever create_file seeds on disk.
   */
  async function indexPath(path: string) {
    if (!isReady()) return;
    if (!/\.(?:md|markdown)$/i.test(path)) return;
    const generation = _roostGeneration;
    try {
      const content = await unwrap(commands.readFile(path));
      if (generation !== _roostGeneration) return;
      await indexNote(path, content);
    } catch (e) {
      log.error("graph", "failed to index path", { path, e });
    }
  }

  async function refreshStats() {
    if (!isReady()) return;
    const generation = _roostGeneration;
    try {
      const result = await unwrap(commands.getGraphStats());
      if (generation !== _roostGeneration) return;
      setStats(result);
    } catch (e) {
      log.error("graph", "failed to get graph stats", e);
    }
  }

  /// CO-3: call after a note was purged from the graph (a peer/local delete).
  /// The delete happens Rust-side (purge_note_from_graph); the frontend never
  /// updated its cached stats, so the node/link/tag totals stayed stale and
  /// the graph overlay never noticed the deletion. Refresh the counts and bump
  /// the revision so open views re-derive.
  function notePurged() {
    bumpRevision();
    void refreshStats();
  }

  /** Backend warm reconciliation changed graph rows after init returned. */
  function backendGraphChanged() {
    bumpRevision();
    void refreshStats();
    void tasksStore.refresh();
  }

  return {
    backlinks,
    stats,
    isReady,
    isSearchReady,
    initError,
    initCorrupt,
    repairDatabase,
    setRoostPath,
    ensureGraphReady,
    ensureSearchReady,
    initGraph,
    refreshBacklinks,
    indexNote,
    indexPath,
    applySaveOutcome,
    refreshStats,
    graphRevision,
    notePurged,
    backendGraphChanged,
    findPathRequest,
    requestFindPath,
    requestGraphMode,
    consumeGraphMode,
    requestHighlightPath,
    consumeHighlightPath,
    saveGraphLayout,
    getGraphLayout,
  };
}

export const graphStore = createRoot(createGraphStore);
