import { createSignal, createEffect, on, onMount, onCleanup, Show, For, createMemo } from "solid-js";
import { graphStore } from "../../stores/graph";
import { createForceGraph, type GraphNode, type GraphEdge, type ForceGraphHandle } from "./ForceGraph";
import {
  noteIdFor as noteIdForPure,
  prettyPath as prettyPathPure,
  labelForId,
  computeCommunityRanks,
  dedupeEdges,
  noteHoverPreview,
} from "./graph-style";
import { graphSettingsStore, GRAPH_SLIDER_RANGES } from "../../stores/graph-settings";
import { commands, unwrap } from "../../lib/invoke";
import type { FullGraphResult } from "../../lib/bindings";
import { log } from "../../lib/log";
import { isMobile } from "../../lib/platform";
import { createAllNotes } from "../../app/derived";
import { roostStore } from "../../stores/roost";
import { createDoubleTapDetector } from "../../lib/double-tap";
import { lsGet, lsSet } from "../../lib/storage";

const GRAPH_MODE_KEY = "corvux.graph.mode";
const GRAPH_LOCAL_DEPTH_KEY = "corvux.graph.localDepth";

// Settings-popover layout config. Keys are typed against GraphSettings so
// the store setters stay type-safe.
const GRAPH_TOGGLES = [
  { key: "showTags", label: "Tags" },
  { key: "showOrphans", label: "Orphans" },
  { key: "showArrows", label: "Arrows" },
] as const;
const GRAPH_SLIDERS = [
  { key: "nodeScale", label: "Node size" },
  { key: "linkDistance", label: "Link distance" },
  { key: "repelForce", label: "Repel force" },
  { key: "labelDensity", label: "Label density" },
] as const;

interface FullGraphData {
  nodes: Array<{ id: string; title: string; path: string; kind: string; link_count: number }>;
  edges: Array<{ source: string; target: string }>;
}

interface CommunityEntry {
  note_id: string;
  community: number;
}

interface PageRankEntry {
  note_id: string;
  // specta surface returns `number | null` for f64 fields (NaN/Inf
  // can't survive JSON). Existing call sites already coalesce via
  // `?? 0` — widening here so the memo cache assignment compiles.
  rank: number | null;
}

interface ShortestPathEntry {
  from_id: string;
  to_id: string;
  cost: number;
  path: string[];
}

type FindPathPhase = "off" | "pickSource" | "pickTarget" | "showing" | "noPath";

interface GraphViewProps {
  activeNotePath?: string;
  onNodeClick: (path: string) => void;
  onClose: () => void;
}

export default function GraphView(props: GraphViewProps) {
  // Hoisted so the node-click handler (defined below) can branch on it.
  const mobile = isMobile();
  const [loading, setLoading] = createSignal(true);
  const [error, setError] = createSignal<string | null>(null);
  const [nodeCount, setNodeCount] = createSignal(0);
  const [noteNodeCount, setNoteNodeCount] = createSignal(0);
  const [edgeCount, setEdgeCount] = createSignal(0);
  // Mode resolution order: (1) a one-shot hint posted right before this
  // tab opened (e.g. floating bar's graph button asks for "local" so the
  // user lands on the current-note neighbourhood, not the whole-roost
  // hairball) — consumeGraphMode is single-read and clears itself; then
  // (2) the user's last-picked mode persisted across restarts; then
  // (3) full as the default.
  const persistedMode = lsGet(GRAPH_MODE_KEY);
  const [mode, setMode] = createSignal<"full" | "local">(
    graphStore.consumeGraphMode() ??
      (persistedMode === "local" || persistedMode === "full" ? persistedMode : "full"),
  );
  // Persist the manual mode choice so it survives a remount / restart
  // (persist-view-state rule).
  createEffect(() => lsSet(GRAPH_MODE_KEY, mode()));

  // Live view settings (force sliders, visibility toggles). Read as a
  // reactive snapshot; the renderer applies them via updateSettings, and
  // the node-set toggles trigger a reload (see effects below).
  const settings = graphSettingsStore.settings;
  const [settingsOpen, setSettingsOpen] = createSignal(false);

  // Local-mode hop depth — replaces the old hardcoded LOCAL_DEPTH=2,
  // reusing the same 1/2/3 control the search subgraph uses. Persisted.
  const persistedDepth = Number(lsGet(GRAPH_LOCAL_DEPTH_KEY));
  const [localDepth, setLocalDepth] = createSignal<1 | 2 | 3>(
    persistedDepth === 1 || persistedDepth === 3 ? (persistedDepth as 1 | 3) : 2,
  );
  createEffect(() => lsSet(GRAPH_LOCAL_DEPTH_KEY, String(localDepth())));

  // One-shot path replay from the PathFinder modal's "Show in graph".
  const pendingPathIds = graphStore.consumeHighlightPath();

  // Desktop hover preview card. `hoverCard` is the anchor + node identity
  // (updates live as the cursor moves); `hoverData` is the lazily-fetched
  // note preview (tags + excerpt), keyed by id so a stale fetch for a
  // node we've left can't paint over the current one.
  const [hoverCard, setHoverCard] = createSignal<{
    id: string;
    title: string;
    path: string;
    kind: "note" | "tag";
    linkCount: number;
    left: number;
    top: number;
  } | null>(null);
  const [hoverData, setHoverData] = createSignal<
    { id: string; tags: string[]; excerpt: string } | null
  >(null);
  let hoverFetchTimer: ReturnType<typeof setTimeout> | undefined;

  function onGraphHover(node: GraphNode | null, x: number, y: number) {
    if (!node) {
      setHoverCard(null);
      return;
    }
    // Clamp the card inside the canvas using size estimates (card is
    // ~280×150). Anchors near the cursor, flips off the right/bottom edge.
    const w = graphRef?.clientWidth ?? 800;
    const h = graphRef?.clientHeight ?? 600;
    const left = Math.max(8, Math.min(x + 14, w - 288));
    const top = Math.max(8, Math.min(y + 14, h - 158));
    setHoverCard({
      id: node.id,
      title: node.title,
      path: node.path,
      kind: node.kind,
      linkCount: node.linkCount,
      left,
      top,
    });

    if (node.kind === "note") {
      if (hoverData()?.id !== node.id) {
        setHoverData(null);
        clearTimeout(hoverFetchTimer);
        const id = node.id;
        const path = node.path;
        hoverFetchTimer = setTimeout(() => {
          void unwrap(commands.readFile(path))
            .then((content) => setHoverData({ id, ...noteHoverPreview(content) }))
            .catch(() => setHoverData({ id, tags: [], excerpt: "" }));
        }, 220);
      }
    } else {
      setHoverData(null);
    }
  }
  onCleanup(() => clearTimeout(hoverFetchTimer));
  // Find-path UX is a tiny state machine: button click moves it forward.
  // We intentionally keep state at the GraphView level (not inside the D3
  // handle) so the status bar + button label can react to phase changes
  // without poking back into the imperative graph layer.
  const [findPath, setFindPath] = createSignal<FindPathPhase>(pendingPathIds ? "showing" : "off");
  const [pathFrom, setPathFrom] = createSignal<string | null>(null);
  const [pathInfo, setPathInfo] = createSignal<{ ids: string[]; cost: number } | null>(
    pendingPathIds ? { ids: pendingPathIds, cost: 0 } : null,
  );
  // Mobile node card: tapping a node shows this card (title + actions) instead
  // of immediately opening the note — nodes are tiny on a phone and an
  // accidental tap shouldn't yank you out of the graph. Desktop keeps tap=open.
  const [selectedNode, setSelectedNode] = createSignal<
    { id: string; title: string; path: string } | null
  >(null);
  // Double-tap-to-open detection (mobile). Transient gesture bookkeeping, not
  // reactive UI state.
  const doubleTap = createDoubleTapDetector(300);
  let graphRef: HTMLDivElement | undefined;
  let graphHandle: ForceGraphHandle | undefined;

  // Derive active note ID from path. Must match Rust's `note_id_from_abs_path`
  // (roost-relative, forward-slash, lowercased, no extension) — basename
  // alone collides with sibling daily notes in different week folders and
  // the backend graph would land on the wrong (or no) node. `noteIdFor`
  // is defined further down; this getter is a thin pass-through.
  const activeNoteId = () => {
    const p = props.activeNotePath;
    if (!p) return undefined;
    return noteIdFor(p);
  };

  // Render caps. "Full" mode returns the whole roost bounded to
  // FULL_MAX_NODES so a 50K-note roost doesn't stall the D3 force
  // simulation. "Local" mode keeps the classic 2-hop neighborhood.
  const FULL_MAX_NODES = 1000;
  const LOCAL_MAX_NODES = 200;
  // Search subgraph: same node-cap as local, depth user-picked (1/2/3).
  const SEARCH_MAX_NODES = 200;

  // Search-by-filename state. When a result is picked, the graph re-loads
  // centred on that note's id at the chosen depth, replacing whatever
  // Full / Local were showing. Esc / clear button returns to the toggle.
  const [searchOpen, setSearchOpen] = createSignal(false);
  const [searchQuery, setSearchQuery] = createSignal("");
  const [searchFocusId, setSearchFocusId] = createSignal<string | undefined>(undefined);
  const [searchFocusLabel, setSearchFocusLabel] = createSignal<string | null>(null);
  const [searchDepth, setSearchDepth] = createSignal<1 | 2 | 3>(1);
  // Pulls the autocomplete list from the same roost file tree the editor's
  // link-autocomplete uses, so we get every .md the user can possibly link
  // to — including ones nested in folders.
  const allNotesMemo = createAllNotes();
  let searchInputRef: HTMLInputElement | undefined;

  // Filter notes by substring match against the basename (filename without
  // .md). Capped at 8 so the dropdown stays scannable on mobile.
  const searchResults = createMemo(() => {
    const q = searchQuery().toLowerCase().trim();
    if (!q) return [] as Array<{ name: string; path: string; display: string }>;
    const hits: Array<{ name: string; path: string; display: string }> = [];
    for (const note of allNotesMemo()) {
      if (!/\.(?:md|markdown)$/i.test(note.path)) continue;
      const display = note.name.replace(/\.(?:md|markdown)$/i, "");
      if (display.toLowerCase().includes(q)) {
        hits.push({ name: note.name, path: note.path, display });
        if (hits.length >= 8) break;
      }
    }
    return hits;
  });

  // Mirror Rust's `note_id_from_abs_path` — see graph-style.noteIdFor for
  // the full rationale. Thin wrapper that injects the current roost path.
  function noteIdFor(absPath: string): string {
    return noteIdForPure(absPath, roostStore.roostPath());
  }

  // id → title index for the current render, rebuilt each loadGraph. Lets
  // the find-path banner show human titles instead of the lowercased
  // internal ids (which the code elsewhere is explicit must never be
  // shown). Path nodes that route OUTSIDE the rendered window won't be in
  // here; those fall back to the filename stem via labelForId.
  const [nodeTitles, setNodeTitles] = createSignal<Map<string, string>>(new Map());
  function titleForId(id: string): string {
    return nodeTitles().get(id) ?? labelForId(id);
  }

  function pickSearchResult(note: { name: string; path: string; display: string }) {
    setSearchFocusId(noteIdFor(note.path));
    setSearchFocusLabel(note.display);
    setSearchQuery(note.display);
    setSearchOpen(false);
  }

  function clearSearch() {
    setSearchFocusId(undefined);
    setSearchFocusLabel(null);
    setSearchQuery("");
    setSearchOpen(false);
  }

  // A-10: monotonic load counter. Concurrent triggers (5s stats timer +
  // activeNoteId effect + mode toggle) used to race against each other
  // and clobber in-flight results. Each load captures the counter at
  // entry and bails before applying if a newer one started.
  let loadGen = 0;

  // A-09: PageRank over 10K nodes + Louvain communities aren't free
  // (~80 ms cold on a 200-note roost, more at scale). They only need to
  // recompute when the graph TOPOLOGY changes — which is what
  // `graphStore.stats()` tracks (notes/links/tags counts). Memoise by
  // stats snapshot so mode flips / activeNoteId reloads / search-focus
  // changes reuse the previous result.
  // CO-3: keyed on the graph revision, not on note/link/tag counts. Counts
  // miss count-neutral edits (retargeting a link, renaming a tag), so a
  // count-keyed cache served stale PageRank sizes / cluster colors after such
  // an edit. The revision bumps on every graph mutation.
  let overlayCache: {
    revision: number;
    communities: CommunityEntry[];
    pageranks: PageRankEntry[];
  } | null = null;

  // Persist where nodes settled + the current zoom so the next render
  // (this component or a remount) can resume instead of re-laying-out.
  function saveCurrentLayout() {
    if (graphHandle) {
      graphStore.saveGraphLayout(graphHandle.getPositions(), graphHandle.getTransform());
    }
  }

  async function loadGraph() {
    if (!graphRef) return;
    const gen = ++loadGen;
    setLoading(true);
    setError(null);
    // The handle is about to be destroyed/recreated — a card pointing at the
    // old graph would be stale.
    setSelectedNode(null);
    setHoverCard(null);
    try {
      // Lazily initialise the graph DB if not already ready. On warm start
      // (DB persisted from previous session) this returns in <100ms. On
      // first-ever launch it triggers the one-time roost scan.
      await graphStore.ensureGraphReady();
      if (gen !== loadGen) return;

      if (!graphStore.isReady()) {
        const initErr = graphStore.initError();
        setError(initErr ? `Graph init failed: ${initErr}` : "Waiting for Roost to initialize...");
        return;
      }

      // A-09 cache check, CO-3: key on the monotonic graph revision so a
      // count-neutral edit (link retarget, tag rename) still invalidates the
      // PageRank/community overlays.
      const revNow = graphStore.graphRevision();
      const cacheHit = overlayCache !== null && overlayCache.revision === revNow;

      let communities: CommunityEntry[];
      let pageranks: PageRankEntry[];
      if (cacheHit) {
        communities = overlayCache!.communities;
        pageranks = overlayCache!.pageranks;
      } else {
        // Both calls are non-fatal: the graph still renders without
        // cluster coloring or pagerank sizing.
        [communities, pageranks] = await Promise.all([
          unwrap(commands.graphCommunities(FULL_MAX_NODES)).catch(() => [] as CommunityEntry[]),
          unwrap(commands.graphPageRank(FULL_MAX_NODES)).catch(() => [] as PageRankEntry[]),
        ]);
        if (gen !== loadGen) return;
        overlayCache = { revision: revNow, communities, pageranks };
      }

      // Choose the data source by mode.
      //   Search → windowed BFS around the user-picked note (overrides all)
      //   Local  → windowed BFS around the currently-open note
      //   Full   → the WHOLE roost, every note including orphans
      //
      // Full mode must NOT be a BFS window. A window anchored on any single
      // node (active note, PageRank hub, anything) can only ever reach that
      // node's connected component — so disconnected notes and separate
      // islands silently vanish. On a young roost whose notes aren't linked
      // yet that collapses "whole roost" to a single node. `get_full_graph`
      // reads every note straight from the DB (capped at FULL_MAX_NODES).
      const searchId = searchFocusId();
      const activeId = activeNoteId();
      const isLocalMode = mode() === "local";

      let data: FullGraphResult;
      if (!searchId && !isLocalMode) {
        data = await unwrap(commands.getFullGraph(FULL_MAX_NODES));
      } else {
        // Local / search: a search pick wins over Local for both depth and
        // node cap — the user is explicitly asking for "show me this note's
        // 1/2/3-hop neighbourhood", not "everything in the roost".
        const focusId = searchId ?? activeId; // Local, no open note → empty
        if (!focusId) {
          // Nothing to anchor on. Render empty and let the user know.
          saveCurrentLayout();
          graphHandle?.destroy();
          graphHandle = createForceGraph(graphRef, [], [], () => {}, undefined);
          setNodeCount(0);
          setNoteNodeCount(0);
          setEdgeCount(0);
          setNodeTitles(new Map());
          return;
        }
        const depth = searchId ? searchDepth() : localDepth();
        const maxNodes = searchId ? SEARCH_MAX_NODES : LOCAL_MAX_NODES;
        data = await unwrap(commands.getGraphWindow(focusId, depth, maxNodes));
      }
      if (gen !== loadGen) return;

      const communityById = new Map(communities.map((c) => [c.note_id, c.community]));
      const pagerankById = new Map(pageranks.map((p) => [p.note_id, p.rank ?? undefined]));
      // Stable size-rank color slots (largest community → 0) so cluster
      // colors don't shuffle when Louvain ids churn between recomputes.
      const communityRanks = computeCommunityRanks(communities);

      const s = settings();
      const nodes: GraphNode[] = data.nodes
        .map((n) => {
          const community = communityById.get(n.id);
          const communityRank =
            typeof community === "number" && community >= 0
              ? communityRanks.get(community)
              : undefined;
          return {
            id: n.id,
            title: n.title,
            path: n.path,
            kind: (n.kind === "tag" ? "tag" : "note") as "note" | "tag",
            linkCount: n.link_count,
            community,
            communityRank,
            pagerank: pagerankById.get(n.id),
          };
        })
        // Visibility toggles: hide tag nodes / orphan notes when the user
        // turned them off. Tags are unaffected by the orphan toggle.
        .filter((n) => {
          if (!s.showTags && n.kind === "tag") return false;
          if (!s.showOrphans && n.kind === "note" && n.linkCount === 0) return false;
          return true;
        });

      const nodeIdSet = new Set(nodes.map((n) => n.id));
      // Drop edges to nodes we filtered out, AND dedup exact-duplicate
      // directed edges — a note linking another twice would otherwise add
      // a second spring to the link force, distorting layout. Reciprocal
      // A→B / B→A pairs are KEPT (they're distinct directed edges that the
      // arrow rendering relies on).
      const edges: GraphEdge[] = dedupeEdges(data.edges, nodeIdSet);

      setNodeCount(nodes.length);
      setNoteNodeCount(nodes.filter((n) => n.kind === "note").length);
      setEdgeCount(edges.length);
      // Title index for the find-path banner (id → human title).
      setNodeTitles(new Map(nodes.map((nd) => [nd.id, nd.title])));

      // Snapshot the outgoing layout, then destroy + recreate. Seeding the
      // new render from the saved positions + transform is what stops the
      // graph re-randomizing and re-settling on every reload (tab switch,
      // stats tick, mode flip).
      saveCurrentLayout();
      graphHandle?.destroy();
      const layout = graphStore.getGraphLayout();
      graphHandle = createForceGraph(
        graphRef,
        nodes,
        edges,
        (node) => handleNodeClick(node),
        // Ring + breathing ripple follow the search focus when one is set,
        // otherwise fall back to the currently-open note. Without this the
        // ring is invisible whenever no file is open (common on mobile).
        searchId ?? activeNoteId(),
        // Mobile: a tap on empty canvas dismisses the node card.
        () => {
          doubleTap.reset();
          setSelectedNode(null);
        },
        { positions: layout.positions, transform: layout.transform },
        settings(),
        // Rich hover preview is desktop-only (mobile uses the tap card).
        mobile ? undefined : onGraphHover,
      );

      // Re-apply path highlight if a path was previously found and survives
      // a graph reload (e.g. mode toggle while showing a path).
      const path = pathInfo();
      if (path && findPath() === "showing") {
        graphHandle.highlightPath(path.ids);
      }
    } catch (e) {
      const msg = typeof e === "string" ? e : (e as Error)?.message ?? "Unknown error";
      log.error("graph-view", "failed to load graph", msg);
      setError(msg);
    } finally {
      setLoading(false);
    }
  }

  /**
   * Routes clicks based on the current find-path phase. In "off" mode:
   * desktop opens the note immediately (existing behavior); mobile shows a
   * node card (single tap) and opens only on double-tap. In pickSource →
   * captures source. In pickTarget → kicks off the shortest-path query.
   */
  function handleNodeClick(node: GraphNode) {
    const phase = findPath();
    if (phase === "off") {
      // Mobile: single tap → card, double tap → open. (Tag/concept nodes are
      // routed to highlight inside ForceGraph and never reach here.)
      if (mobile && node.kind === "note") {
        if (doubleTap.isDouble(node.id, Date.now())) {
          setSelectedNode(null);
          props.onNodeClick(node.path);
          return;
        }
        setSelectedNode({ id: node.id, title: node.title, path: node.path });
        return;
      }
      props.onNodeClick(node.path);
      return;
    }
    if (phase === "pickSource") {
      setPathFrom(node.id);
      setFindPath("pickTarget");
      return;
    }
    if (phase === "pickTarget") {
      const from = pathFrom();
      if (!from) {
        // Shouldn't happen, but recover gracefully.
        setFindPath("pickSource");
        return;
      }
      runShortestPath(from, node.id);
      return;
    }
    // While showing or noPath, a node click resets the search and starts
    // a new pick from this node — feels natural for "actually I want
    // path from X instead".
    setPathInfo(null);
    setPathFrom(node.id);
    setFindPath("pickTarget");
    graphHandle?.highlightPath(null);
  }

  // Display-only path: roost-relative, original case, no `.md`. NEVER show
  // the lowercased internal node id (noteIdFor) to the user.
  function prettyPath(absPath: string): string {
    return prettyPathPure(absPath, roostStore.roostPath());
  }

  // Node-card actions — reuse existing machinery, no new IPC.
  function openSelectedNote(node: { path: string }) {
    setSelectedNode(null);
    props.onNodeClick(node.path);
  }
  function focusLocalFromNode(node: { id: string; title: string }) {
    // Same path as picking a search result: re-centres the windowed subgraph
    // on this node (the [searchFocusId, searchDepth] effect reloads).
    setSearchFocusId(node.id);
    setSearchFocusLabel(node.title);
    setSearchQuery(node.title);
    setSearchOpen(false);
    setSelectedNode(null);
  }
  function startPathFromNode(node: { id: string }) {
    // Enter find-path already holding this node as the source.
    setPathInfo(null);
    setPathFrom(node.id);
    setFindPath("pickTarget");
    graphHandle?.highlightPath(null);
    setSelectedNode(null);
  }

  async function runShortestPath(from: string, to: string) {
    try {
      const result = await unwrap(commands.graphShortestPath(from, to));
      if (!result || result.path.length === 0) {
        setPathInfo(null);
        setFindPath("noPath");
        graphHandle?.highlightPath(null);
        return;
      }
      setPathInfo({ ids: result.path, cost: result.cost ?? 0 });
      setFindPath("showing");
      graphHandle?.highlightPath(result.path);
    } catch (e) {
      log.error("graph-view", "shortest_path failed", e);
      setPathInfo(null);
      setFindPath("noPath");
      graphHandle?.highlightPath(null);
    }
  }

  function toggleFindPath() {
    // Entering/leaving path mode: drop any open node card and reset the
    // double-tap state so a stale card from a prior tap can't linger over
    // the pick flow (this is also the entry point for the command-palette /
    // keyboard find-path trigger).
    setSelectedNode(null);
    doubleTap.reset();
    if (findPath() === "off") {
      setFindPath("pickSource");
      setPathFrom(null);
      setPathInfo(null);
    } else {
      setFindPath("off");
      setPathFrom(null);
      setPathInfo(null);
      graphHandle?.highlightPath(null);
    }
  }

  onMount(() => {
    loadGraph();
  });

  // Esc cancels an in-progress find-path (matches the search input's Esc,
  // which clears the search). Only acts while the path machine is active
  // so it never steals Esc from the command palette / other handlers.
  function onKeyDown(e: KeyboardEvent) {
    if (e.key === "Escape" && findPath() !== "off") {
      e.preventDefault();
      e.stopPropagation();
      toggleFindPath();
    }
  }
  onMount(() => document.addEventListener("keydown", onKeyDown, true));
  onCleanup(() => document.removeEventListener("keydown", onKeyDown, true));

  onCleanup(() => {
    // Snapshot layout before teardown so a tab switch / remount resumes
    // where it left off instead of re-settling from scratch.
    saveCurrentLayout();
    graphHandle?.destroy();
  });

  // External trigger from the keyboard shortcut / command palette.
  // Bump the counter → enter pickSource if currently off; if already in
  // any find-path phase, leave the user alone (don't reset their progress).
  createEffect(
    on(
      () => graphStore.findPathRequest(),
      () => {
        if (findPath() === "off") toggleFindPath();
      },
      { defer: true }
    )
  );

  // Reload when mode changes (defer: skip initial run, onMount handles that)
  createEffect(on(mode, () => loadGraph(), { defer: true }));

  // Reload when the user switches files while the graph tab is parked —
  // local mode is anchored to the active note, so without this the
  // graph stays stuck on the first focus and switching tabs does
  // nothing visible. Full mode ignores activeNotePath (it's whole-roost
  // PageRank-focused) so we gate the reload to local-only.
  createEffect(
    on(
      () => activeNoteId(),
      () => {
        if (mode() === "local") loadGraph();
      },
      { defer: true },
    ),
  );

  // Reload when a search subgraph is picked or its depth changes. We track
  // the focus id and the depth signal in a single effect so depth tweaks
  // while a result is active also refire.
  createEffect(
    on(
      () => [searchFocusId(), searchDepth()] as const,
      () => loadGraph(),
      { defer: true },
    ),
  );

  // Reload when graph DB becomes ready (e.g., roost opened after graph view)
  createEffect(on(() => graphStore.isReady(), (ready) => { if (ready) loadGraph(); }, { defer: true }));

  // Reload when graph data changes (new notes indexed) — throttled to avoid
  // excessive reloads from auto-save triggering indexNote every 2s.
  // AUDIT-026: clear the timer on unmount so a graph-tab close inside the
  // 5s window doesn't fire `loadGraph` against a disposed owner.
  // CO-3: track the graph REVISION, not the stat counts. A count-neutral edit
  // (retargeting a link, renaming a tag) leaves notes/links/tags unchanged, so
  // a stats-keyed reload never fired and the open graph kept stale edges.
  let statsReloadTimer: ReturnType<typeof setTimeout> | undefined;
  createEffect(on(() => graphStore.graphRevision(), () => {
    if (statsReloadTimer) clearTimeout(statsReloadTimer);
    statsReloadTimer = setTimeout(() => loadGraph(), 5000);
  }, { defer: true }));
  onCleanup(() => {
    if (statsReloadTimer) clearTimeout(statsReloadTimer);
  });

  // Live-apply force/visual settings (node size, link distance, repel,
  // label density, arrows) to the running renderer — no reload needed.
  createEffect(on(settings, (s) => graphHandle?.updateSettings(s), { defer: true }));

  // Visibility toggles (show tags / orphans) change which nodes exist, so
  // they need a full reload rather than a live settings tweak.
  createEffect(
    on(
      () => [settings().showTags, settings().showOrphans] as const,
      () => loadGraph(),
      { defer: true },
    ),
  );

  // Local-mode depth change → reload (only when local mode is the active
  // source; a search subgraph uses its own depth picker).
  createEffect(
    on(
      localDepth,
      () => {
        if (mode() === "local" && !searchFocusId()) loadGraph();
      },
      { defer: true },
    ),
  );

  return (
    <div class="flex flex-col h-full bg-bg-primary select-none">
      {/* Header — view-mode toggles (Full/Local, shortest-path) on the
          leading edge, utilities (search, reset, close) on the trailing
          edge. The "GRAPH VIEW" wordmark was redundant with the tab
          title, dropped 2026-05-18. */}
      <div class="h-[3.25rem] sm:h-10 flex items-center justify-between px-3 border-b border-border shrink-0">
        <div class="flex items-center gap-1">
          {/* Mode toggle. Clicking either side ALSO clears any active
              search subgraph — the toggle and the search are two paths
              to picking the focus, so when the user explicitly picks a
              mode they expect that mode, not a search remnant. */}
          <div class="flex items-center bg-bg-secondary rounded border border-border text-xs h-11 sm:h-auto">
            <button
              onClick={() => {
                if (searchFocusId()) clearSearch();
                setMode("full");
              }}
              class={`flex items-center h-full sm:h-auto px-3 sm:px-2 py-0 sm:py-0.5 rounded-l transition-colors ${
                mode() === "full" && !searchFocusId()
                  ? "bg-accent text-bg-primary"
                  : "text-text-muted hover:text-text-secondary"
              }`}
            >
              Full
            </button>
            <button
              onClick={() => {
                if (searchFocusId()) clearSearch();
                setMode("local");
              }}
              class={`flex items-center h-full sm:h-auto px-3 sm:px-2 py-0 sm:py-0.5 rounded-r transition-colors ${
                mode() === "local" && !searchFocusId()
                  ? "bg-accent text-bg-primary"
                  : "text-text-muted hover:text-text-secondary"
              }`}
              title={`${localDepth()}-hop around current note`}
            >
              Local
            </button>
          </div>
          {/* Local-mode depth picker (1/2/3 hops) — same control the search
              subgraph uses. Only shown in local mode without a search focus. */}
          <Show when={mode() === "local" && !searchFocusId()}>
            <div class="flex items-center bg-bg-secondary rounded border border-border text-xs h-11 sm:h-auto ml-1">
              <For each={[1, 2, 3] as const}>
                {(d) => (
                  <button
                    onClick={() => setLocalDepth(d)}
                    class={`w-7 h-full sm:h-auto sm:py-0.5 flex items-center justify-center transition-colors ${
                      localDepth() === d
                        ? "bg-accent text-bg-primary"
                        : "text-text-muted hover:text-text-secondary"
                    } ${d === 1 ? "rounded-l" : ""} ${d === 3 ? "rounded-r" : ""}`}
                    title={`${d}-hop neighbourhood`}
                    aria-pressed={localDepth() === d}
                    aria-label={`Local depth ${d} hops`}
                  >
                    {d}
                  </button>
                )}
              </For>
            </div>
          </Show>
          {/* Find path between two notes — drives the path state machine */}
          <button
            onClick={toggleFindPath}
            class={`w-11 h-11 sm:w-7 sm:h-7 flex items-center justify-center rounded transition-colors ml-1 ${
              findPath() !== "off"
                ? "bg-accent text-bg-primary"
                : "hover:bg-bg-hover text-text-muted hover:text-text-secondary"
            }`}
            title="Find shortest path between two notes"
            aria-label="Find shortest path between two notes"
          >
            <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
              <circle cx="6" cy="19" r="2" />
              <circle cx="18" cy="5" r="2" />
              <path d="M7.5 17.5 L16.5 6.5" />
            </svg>
          </button>
        </div>
        <div class="flex items-center gap-1">
          {/* View settings (forces + visibility) — gear opens a popover. */}
          <button
            onClick={() => setSettingsOpen((v) => !v)}
            class={`w-11 h-11 sm:w-7 sm:h-7 flex items-center justify-center rounded transition-colors ${
              settingsOpen()
                ? "bg-accent text-bg-primary"
                : "hover:bg-bg-hover text-text-muted hover:text-text-secondary"
            }`}
            title="Graph settings"
            aria-label="Graph settings"
            aria-pressed={settingsOpen()}
          >
            <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
              <circle cx="12" cy="12" r="3" />
              <path d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 1 1-2.83 2.83l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 0 1-4 0v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 1 1-2.83-2.83l.06-.06a1.65 1.65 0 0 0 .33-1.82 1.65 1.65 0 0 0-1.51-1H3a2 2 0 0 1 0-4h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 1 1 2.83-2.83l.06.06a1.65 1.65 0 0 0 1.82.33H9a1.65 1.65 0 0 0 1-1.51V3a2 2 0 0 1 4 0v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 1 1 2.83 2.83l-.06.06a1.65 1.65 0 0 0-.33 1.82V9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 0 1 0 4h-.09a1.65 1.65 0 0 0-1.51 1z" />
            </svg>
          </button>
          {/* Search-by-filename toggle. When active, shows the search row
              below. Mobile + iOS use the same path — the search input is
              touch-sized (h-9, 36px tap target) and the dropdown scrolls. */}
          <button
            onClick={() => {
              setSearchOpen(!searchOpen());
              if (!searchOpen()) {
                // We just turned it ON — focus the input on next tick.
                requestAnimationFrame(() => searchInputRef?.focus());
              }
            }}
            class={`w-11 h-11 sm:w-7 sm:h-7 flex items-center justify-center rounded transition-colors ${
              searchOpen() || searchFocusId()
                ? "bg-accent text-bg-primary"
                : "hover:bg-bg-hover text-text-muted hover:text-text-secondary"
            }`}
            title="Search the graph by filename"
            aria-label="Search graph"
            aria-pressed={searchOpen() || !!searchFocusId()}
          >
            <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
              <circle cx="11" cy="11" r="7" />
              <line x1="21" y1="21" x2="16.65" y2="16.65" />
            </svg>
          </button>
          {/* Reset view — returns zoom/pan to default and re-settles layout */}
          <button
            onClick={() => graphHandle?.resetView()}
            class="w-11 h-11 sm:w-7 sm:h-7 flex items-center justify-center rounded hover:bg-bg-hover text-text-muted hover:text-text-secondary transition-colors"
            title="Reset view"
            aria-label="Reset view"
          >
            <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
              <path d="M3 12a9 9 0 1 0 3-6.7" />
              <path d="M3 4v5h5" />
            </svg>
          </button>
          {/* Close */}
          <button
            onClick={props.onClose}
            class="w-11 h-11 sm:w-7 sm:h-7 flex items-center justify-center rounded hover:bg-bg-hover text-text-muted hover:text-text-secondary transition-colors"
            title="Close (Ctrl+G)"
            aria-label="Close graph"
          >
            <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
              <path d="M18 6L6 18M6 6l12 12" />
            </svg>
          </button>
        </div>
      </div>

      {/* Search row — second header row, visible when search is open OR a
          subgraph is currently active (so the user can see what's filtered
          and clear it). The input, dropdown, and depth picker all live
          here. Touch targets are h-9 to satisfy mobile / iOS HIG. */}
      <Show when={searchOpen() || searchFocusId()}>
        <div class="relative shrink-0 border-b border-border bg-bg-secondary">
          <div class="flex items-center gap-2 px-3 py-1.5">
            <svg
              width="14"
              height="14"
              viewBox="0 0 24 24"
              fill="none"
              stroke="currentColor"
              stroke-width="2"
              stroke-linecap="round"
              stroke-linejoin="round"
              class="shrink-0 text-text-muted"
              aria-hidden="true"
            >
              <circle cx="11" cy="11" r="7" />
              <line x1="21" y1="21" x2="16.65" y2="16.65" />
            </svg>

            <input
              ref={searchInputRef}
              type="text"
              inputmode="search"
              autocomplete="off"
              autocapitalize="off"
              autocorrect="off"
              spellcheck={false}
              placeholder="Type a note name…"
              value={searchQuery()}
              onInput={(e) => {
                setSearchQuery(e.currentTarget.value);
                if (!searchOpen()) setSearchOpen(true);
              }}
              onFocus={() => setSearchOpen(true)}
              onKeyDown={(e) => {
                if (e.key === "Escape") {
                  e.preventDefault();
                  clearSearch();
                } else if (e.key === "Enter") {
                  e.preventDefault();
                  const first = searchResults()[0];
                  if (first) pickSearchResult(first);
                }
              }}
              class="flex-1 min-w-0 h-9 px-2 bg-bg-primary border border-border rounded text-text-primary text-sm placeholder:text-text-muted focus:outline-none focus:border-accent"
            />

            {/* Depth picker — 1/2/3 hops. Local mode default is 2, but for
                a user-picked subgraph the most useful default is 1 (just
                the immediate neighbourhood) with the ability to expand. */}
            <div class="flex items-center bg-bg-primary rounded border border-border text-xs shrink-0 h-9">
              <For each={[1, 2, 3] as const}>
                {(d) => (
                  <button
                    onClick={() => setSearchDepth(d)}
                    class={`w-7 h-full flex items-center justify-center transition-colors ${
                      searchDepth() === d
                        ? "bg-accent text-bg-primary"
                        : "text-text-muted hover:text-text-secondary"
                    } ${d === 1 ? "rounded-l" : ""} ${d === 3 ? "rounded-r" : ""}`}
                    title={`${d}-hop neighbourhood`}
                    aria-pressed={searchDepth() === d}
                    aria-label={`Show ${d}-hop neighbourhood`}
                  >
                    {d}
                  </button>
                )}
              </For>
            </div>

            <Show when={searchFocusId() || searchQuery()}>
              <button
                onClick={clearSearch}
                class="w-9 h-9 flex items-center justify-center rounded text-text-muted hover:text-text-secondary hover:bg-bg-hover transition-colors shrink-0"
                title="Clear search"
                aria-label="Clear search"
              >
                <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
                  <path d="M18 6L6 18M6 6l12 12" />
                </svg>
              </button>
            </Show>
          </div>

          {/* Autocomplete dropdown — absolute-positioned so it floats over
              the graph canvas. Max height capped so it scrolls instead of
              shoving the canvas down on mobile. */}
          <Show when={searchOpen() && searchResults().length > 0}>
            <div class="absolute left-3 right-3 top-full mt-1 z-30 bg-bg-primary border border-border rounded-md shadow-lg max-h-64 overflow-y-auto">
              <For each={searchResults()}>
                {(note) => (
                  <button
                    onClick={() => pickSearchResult(note)}
                    class="w-full text-left px-3 py-2 text-sm text-text-primary hover:bg-bg-hover focus:bg-bg-hover focus:outline-none transition-colors flex items-center gap-2"
                  >
                    <svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" class="shrink-0 opacity-60">
                      <path d="M14 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8z" />
                      <polyline points="14 2 14 8 20 8" />
                    </svg>
                    <span class="truncate">{note.display}</span>
                  </button>
                )}
              </For>
            </div>
          </Show>

          <Show when={searchOpen() && searchQuery().trim() && searchResults().length === 0}>
            <div class="absolute left-3 right-3 top-full mt-1 z-30 bg-bg-primary border border-border rounded-md shadow-lg px-3 py-2 text-xs text-text-muted">
              No matching notes.
            </div>
          </Show>
        </div>
      </Show>

      {/* Graph canvas */}
      <div class="flex-1 relative overflow-hidden" style={{ "min-height": "0" }}>
        {/* D3-owned container — SolidJS must not manage children here.
            On mobile we inset by ~16px so nodes don't press into the bezels
            / overlap with the status/tab bars. Desktop stays edge-to-edge. */}
        <div
          ref={graphRef}
          class="absolute"
          style={mobile
            ? { top: "8px", right: "8px", bottom: "8px", left: "8px" }
            : { top: "0", right: "0", bottom: "0", left: "0" }}
        />
        {/* Settings popover — floats over the canvas, top-right. Forces +
            visibility live in the same panel; all values persist. */}
        <Show when={settingsOpen()}>
          <div class="absolute top-2 right-2 z-30 w-60 rounded-lg bg-bg-secondary/95 border border-border shadow-2xl backdrop-blur p-3 text-xs">
            <div class="flex items-center justify-between mb-2.5">
              <span class="text-text-secondary font-medium">Graph settings</span>
              <button
                onClick={() => graphSettingsStore.reset()}
                class="text-text-muted hover:text-text-secondary"
                title="Reset to defaults"
              >
                Reset
              </button>
            </div>
            <div class="flex flex-col gap-2 mb-3">
              <For each={GRAPH_TOGGLES}>
                {(t) => (
                  <label class="flex items-center justify-between cursor-pointer select-none">
                    <span class="text-text-secondary">{t.label}</span>
                    <input
                      type="checkbox"
                      checked={settings()[t.key]}
                      onChange={(e) => graphSettingsStore.set(t.key, e.currentTarget.checked)}
                      class="accent-accent w-4 h-4 cursor-pointer"
                    />
                  </label>
                )}
              </For>
            </div>
            <div class="flex flex-col gap-2.5">
              <For each={GRAPH_SLIDERS}>
                {(s) => (
                  <label class="flex flex-col gap-1">
                    <span class="flex items-center justify-between text-text-muted">
                      <span>{s.label}</span>
                      <span>{settings()[s.key].toFixed(1)}×</span>
                    </span>
                    <input
                      type="range"
                      min={GRAPH_SLIDER_RANGES[s.key][0]}
                      max={GRAPH_SLIDER_RANGES[s.key][1]}
                      step={0.1}
                      value={settings()[s.key]}
                      onInput={(e) => graphSettingsStore.set(s.key, Number(e.currentTarget.value))}
                      class="w-full accent-accent cursor-pointer"
                    />
                  </label>
                )}
              </For>
            </div>
          </div>
        </Show>
        {/* Desktop hover preview card — floats near the cursor, pointer
            transparent so it never eats canvas gestures. Title + path
            always; tags + excerpt once the lazy read resolves for notes. */}
        <Show when={!mobile && hoverCard()}>
          {(card) => (
            <div
              class="absolute z-30 w-[280px] max-w-[280px] pointer-events-none rounded-lg bg-bg-secondary/95 border border-border shadow-2xl backdrop-blur p-3"
              style={{ left: `${card().left}px`, top: `${card().top}px` }}
            >
              <div class="text-text-primary text-sm font-medium truncate">{card().title}</div>
              <div class="text-text-muted text-[11px] truncate">
                {card().kind === "tag"
                  ? `${card().linkCount} connection${card().linkCount === 1 ? "" : "s"}`
                  : prettyPath(card().path)}
              </div>
              <Show when={card().kind === "note" && hoverData()?.id === card().id}>
                <Show when={hoverData()!.tags.length > 0}>
                  <div class="flex flex-wrap gap-1 mt-1.5">
                    <For each={hoverData()!.tags}>
                      {(t) => (
                        <span class="px-1.5 py-0.5 rounded bg-bg-tertiary text-[10px] text-text-secondary">
                          #{t}
                        </span>
                      )}
                    </For>
                  </div>
                </Show>
                <Show when={hoverData()!.excerpt}>
                  <div class="mt-1.5 text-text-secondary text-[11px] leading-snug line-clamp-3">
                    {hoverData()!.excerpt}
                  </div>
                </Show>
              </Show>
            </div>
          )}
        </Show>
        {/* Loading overlay — SolidJS-managed, separate from D3 */}
        <Show when={loading()}>
          <div class="absolute inset-0 flex items-center justify-center pointer-events-none">
            <div class="flex items-center gap-2 text-text-muted text-sm">
              <div class="w-2 h-2 rounded-full bg-accent animate-pulse" />
              Loading graph...
            </div>
          </div>
        </Show>
        {/* Error overlay */}
        <Show when={error()}>
          <div class="absolute inset-0 flex items-center justify-center">
            <div class="text-center text-text-muted text-sm px-4">
              <div class="text-red-400 mb-1">Failed to load graph</div>
              <div class="text-xs opacity-70 mb-3">{error()}</div>
              {/* DB-1: a corrupt graph.db can't be retried into working — it
                  has to be rebuilt from the markdown on disk. Offer that
                  directly instead of a dead "Retry". */}
              <Show
                when={graphStore.initCorrupt()}
                fallback={
                  <button
                    onClick={() => loadGraph()}
                    class="px-3 py-1 text-xs rounded bg-accent/20 text-accent hover:bg-accent/30 transition-colors"
                  >
                    Retry
                  </button>
                }
              >
                <button
                  onClick={async () => {
                    if (await graphStore.repairDatabase()) loadGraph();
                  }}
                  class="px-3 py-1 text-xs rounded bg-accent/20 text-accent hover:bg-accent/30 transition-colors"
                >
                  Repair database
                </button>
                <div class="text-[11px] opacity-60 mt-2">
                  Rebuilds the graph from your notes. Your markdown is untouched.
                </div>
              </Show>
            </div>
          </div>
        </Show>
        {/* Empty overlay — message varies by reason. Local-mode-with-no-
            active-note is the most common case (user opened the graph
            tab without a note open) and gets its own affordance.
            Empty-roost is the genuinely-blank fallback. */}
        <Show when={!loading() && !error() && nodeCount() === 0}>
          <div class="absolute inset-0 flex items-center justify-center pointer-events-none">
            <div class="text-center text-text-muted text-sm px-6 max-w-xs pointer-events-auto">
              <Show
                when={mode() === "local" && !activeNoteId() && !searchFocusId()}
                fallback={
                  <>
                    <div class="text-text-secondary mb-1">No graph yet</div>
                    <div class="text-xs opacity-70">
                      Add a few notes and link them with{" "}
                      <code class="font-mono text-text-secondary">[[wiki-links]]</code>{" "}
                      to see them here.
                    </div>
                  </>
                }
              >
                <div class="text-text-secondary mb-1">No active note</div>
                <div class="text-xs opacity-70 mb-3">
                  Local mode shows the neighbourhood of the note you're
                  reading. Open a note, or switch to{" "}
                  <button
                    onClick={() => setMode("full")}
                    class="text-accent hover:text-accent-hover underline-offset-2 hover:underline"
                  >
                    Full
                  </button>
                  .
                </div>
              </Show>
            </div>
          </div>
        </Show>
        {/* Mobile node card — a single tap selects a node and shows this
            instead of opening it (nodes are tiny on a phone). Sits inside the
            canvas above the stats footer so pan/pinch stay live behind it. */}
        <Show when={mobile ? selectedNode() : null}>
          {(node) => (
            <div
              class="absolute left-2 right-2 z-20 rounded-2xl bg-bg-secondary/95 border border-border shadow-2xl backdrop-blur p-3"
              style={{ bottom: "calc(env(safe-area-inset-bottom, 0px) + 8px)" }}
            >
              <div class="flex items-start gap-2">
                <div class="min-w-0 flex-1">
                  <div class="text-text-primary font-medium text-sm truncate">
                    {node().title}
                  </div>
                  <div class="text-text-muted text-xs truncate">
                    {prettyPath(node().path)}
                  </div>
                </div>
                <button
                  onClick={() => {
                    doubleTap.reset();
                    setSelectedNode(null);
                  }}
                  aria-label="Dismiss node card"
                  class="w-9 h-9 -mt-1 -mr-1 flex items-center justify-center rounded text-text-muted hover:text-text-primary shrink-0"
                >
                  <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
                    <path d="M18 6L6 18M6 6l12 12" />
                  </svg>
                </button>
              </div>
              <div class="mt-2.5 flex flex-col gap-1.5">
                <button
                  onClick={() => openSelectedNote(node())}
                  class="h-11 w-full rounded-lg bg-accent text-bg-primary text-sm font-medium active:opacity-90 transition-opacity"
                >
                  Open note
                </button>
                <div class="flex gap-1.5">
                  <button
                    onClick={() => focusLocalFromNode(node())}
                    class="h-11 flex-1 rounded-lg bg-bg-tertiary border border-border text-text-secondary text-xs active:bg-bg-hover transition-colors"
                  >
                    Focus local
                  </button>
                  <button
                    onClick={() => startPathFromNode(node())}
                    class="h-11 flex-1 rounded-lg bg-bg-tertiary border border-border text-text-secondary text-xs active:bg-bg-hover transition-colors"
                  >
                    Start path
                  </button>
                </div>
              </div>
            </div>
          )}
        </Show>
      </div>

      {/* Find-path status banner — only renders when the machine is active */}
      <Show when={findPath() !== "off"}>
        <div class="px-3 py-1.5 border-t border-border bg-bg-secondary text-xs flex items-center gap-2">
          <Show when={findPath() === "pickSource"}>
            <span class="text-accent">Find path —</span>
            <span class="text-text-muted">click the SOURCE note</span>
          </Show>
          <Show when={findPath() === "pickTarget"}>
            <span class="text-accent">Find path —</span>
            <span class="text-text-secondary">
              from <span class="text-accent">{titleForId(pathFrom()!)}</span>
            </span>
            <span class="text-text-muted">· click the TARGET note</span>
          </Show>
          <Show when={findPath() === "showing" && pathInfo()}>
            <span class="text-accent">Path —</span>
            <span class="text-text-secondary truncate flex-1">
              {pathInfo()!.ids.map((id) => titleForId(id)).join(" → ")}
            </span>
            <span class="text-text-muted shrink-0">{pathInfo()!.ids.length - 1} hops</span>
          </Show>
          <Show when={findPath() === "noPath"}>
            <span class="text-red-400">No path —</span>
            <span class="text-text-muted">these notes aren't connected</span>
          </Show>
          <div class="flex-1" />
          <button
            onClick={toggleFindPath}
            class="px-2 py-0.5 rounded text-text-muted hover:text-text-secondary hover:bg-bg-hover transition-colors"
            title="Cancel find path"
          >
            ✕
          </button>
        </div>
      </Show>

      {/* Stats footer. On DESKTOP it's a distinct bg-secondary bar (matches the
          sidebar footer). On MOBILE it blends with bg-primary: the graph lives
          inside #root, which already reserves the bottom safe-area inset as a
          bg-primary strip BELOW this footer — clipped by EditorArea's
          overflow-hidden, a bg-secondary bar can't fill that strip, so it
          looked two-tone with the text floating above a bg-primary gap. Going
          bg-primary makes the footer seam into that strip (one colour, no
          float) and we drop the extra safe-area lift since #root supplies it. */}
      <div
        class={`shrink-0 flex items-center gap-2 px-3 pt-1 border-t border-border text-text-muted text-[10px] ${
          mobile ? "bg-bg-primary" : "bg-bg-secondary"
        }`}
        style={{
          "padding-bottom": mobile
            ? "0.375rem"
            : "calc(0.375rem + env(safe-area-inset-bottom, 0px) * 0.3)",
        }}
      >
        <span>{nodeCount()} nodes</span>
        <span class="opacity-30">·</span>
        <span>{edgeCount()} edges</span>
        {/* Truncation honesty: full mode is capped at FULL_MAX_NODES. When
            the roost has more notes than we drew, say so instead of
            silently pretending "X nodes" is the whole graph. */}
        <Show
          when={
            !searchFocusId() &&
            mode() === "full" &&
            noteNodeCount() >= FULL_MAX_NODES &&
            graphStore.stats().notes > noteNodeCount()
          }
        >
          <span class="opacity-30">·</span>
          <span class="text-amber-400/90 truncate">
            {noteNodeCount().toLocaleString()} of {graphStore.stats().notes.toLocaleString()} notes — search to narrow
          </span>
        </Show>
        <Show when={searchFocusId()}>
          <span class="opacity-30">·</span>
          <span class="text-accent truncate">
            {searchDepth()}-hop around {searchFocusLabel() ?? searchFocusId()}
          </span>
        </Show>
        <Show when={!searchFocusId() && mode() === "local" && activeNoteId()}>
          <span class="opacity-30">·</span>
          <span class="text-accent truncate">{localDepth()}-hop around {titleForId(activeNoteId()!)}</span>
        </Show>
        <div class="flex-1" />
        <Show when={!mobile}>
          <span class="opacity-50 whitespace-nowrap">click to open · shift-click to highlight · drag to move · shift+drop to pin · scroll to zoom</span>
        </Show>
      </div>
    </div>
  );
}
