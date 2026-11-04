/**
 * Unified invoke layer — uses Tauri IPC when available, falls back to
 * in-memory mock for browser development (pnpm dev → open localhost:1420 in Chrome).
 *
 * The browser-only FSAccess + IDB persistence layer lives in `./browser-roost`.
 * This file owns the mock data + mock IPC switch + the real Tauri bridge.
 */

import type { FileEntry } from "./bindings";
import { errMsg } from "./error";
import { lsGet, lsSet } from "./storage";
import {
  getMockRoostPath,
  getBrowserRoostRoot,
  pathInBrowserRoost,
  resolveBrowserPath,
  walkBrowserRoost,
  tryRestoreBrowserRoost as _tryRestoreBrowserRoost,
} from "./browser-roost";

// Re-export browser-roost primitives so existing callers keep working.
export { setMockRoostPath, setBrowserRoost, isBrowserRoostBound } from "./browser-roost";

const isTauri = !!(window as unknown as Record<string, unknown>).__TAURI_INTERNALS__;

/** Tauri-aware wrapper around tryRestoreBrowserRoost — no-ops in native builds. */
export function tryRestoreBrowserRoost(expectedPath?: string): Promise<string | null> {
  return _tryRestoreBrowserRoost(expectedPath, isTauri);
}

// ── Mock data (browser only) ─────────────────────────────────────

const mockFiles: Map<string, string> = new Map([
  [
    "/mock-roost/Welcome.md",
    `---
tags:
  - getting-started
  - corvux
status: draft
date: 2026-04-09
---

# Welcome to Corvux

This is a test note for browser development.

You can edit this note, add [[Project Ideas]] and test the Properties panel above.

Related: [[Meeting Notes]] | [[Daily Log]]

Some inline tags: #getting-started #corvux #important
`,
  ],
  [
    "/mock-roost/Project Ideas.md",
    `---
tags:
  - ideas
  - brainstorm
priority: high
category: projects
---

# Project Ideas

- Build a graph-based knowledge OS
- Local-first AI assistant
- Voice-powered note taking

See also: [[Welcome]] and [[Meeting Notes]]

Tags for filtering: #ideas #brainstorm #project/corvux
`,
  ],
  [
    "/mock-roost/Meeting Notes.md",
    `---
tags:
  - meetings
  - weekly
date: 2026-04-08
author: Technoo
---

# Meeting Notes — April 8

## Discussion Points
- Reviewed architecture decisions
- Decided on CozoDB for graph storage
- Next steps: build the editor shell

## Action Items
- [ ] Set up Tauri V2 project
- [ ] Integrate CodeMirror 6
- [x] Finalize tech stack
`,
  ],
  [
    "/mock-roost/notes/Daily Log.md",
    `---
tags:
  - daily
  - journal
date: 2026-04-09
---

# Daily Log — April 9

Started building Corvux. Got the editor, file explorer, tabs, and quick switcher working.

Next: wiki-links, CozoDB integration, AI chat.

Key decisions from [[Meeting Notes]]:
- Using CozoDB for the graph layer
- #daily #journal #progress
`,
  ],
  [
    "/mock-roost/notes/No Frontmatter.md",
    `# A Note Without Frontmatter

This note has no YAML frontmatter. The Properties panel should not appear.

Just plain markdown content here.
`,
  ],
]);

// ── Mock attachment store (browser only) ────────────────────────
// Keeps pasted image bytes in memory so the image-embed extension can read
// them back via `read_attachment` during browser dev. Wiped on page reload.
const mockAttachments = new Map<string, string>();

// ── Mock Tasks-pane excluded folders (browser only) ─────────────
// Mirrors the backend's device-local kv setting so the Settings → Tasks
// UI round-trips in browser dev. Reset on page reload.
let mockExcludedFolders: string[] = [];

// ── Mock graph state (browser only) ─────────────────────────────

const mockGraph = {
  notes: new Map<string, { title: string; path: string }>(),
  links: [] as Array<{ from_id: string; to_id: string; link_text: string }>,
  tags: [] as Array<{ note_id: string; tag: string }>,
};

function buildFileTree(roostPath = getMockRoostPath()): FileEntry[] {
  const root: FileEntry[] = [];
  const dirs = new Map<string, FileEntry>();

  for (const path of mockFiles.keys()) {
    if (!path.startsWith(roostPath + "/")) continue;
    const relative = path.slice(roostPath.length + 1);
    const parts = relative.split("/");
    const name = parts[parts.length - 1];

    if (parts.length === 1) {
      root.push({ name, path, is_dir: false, children: [] });
    } else {
      const dirName = parts[0];
      const dirPath = `${roostPath}/${dirName}`;
      if (!dirs.has(dirPath)) {
        dirs.set(dirPath, { name: dirName, path: dirPath, is_dir: true, children: [] });
      }
      dirs.get(dirPath)!.children.push({ name, path, is_dir: false, children: [] });
    }
  }

  const allEntries = [...dirs.values(), ...root];
  return allEntries.sort((a, b) => {
    if (a.is_dir !== b.is_dir) return a.is_dir ? -1 : 1;
    return a.name.localeCompare(b.name);
  });
}

// ── Mock invoke handler ──────────────────────────────────────────

// `export` so `./mock-commands.ts` can delegate to the same in-memory
// dispatcher. Marked `Promise<T>` for backwards compat with the legacy
// `appInvoke<T>` API — the typed `commands` wrappers narrow the return
// shape per-method on top.
export async function mockInvoke<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  // Small delay to simulate async
  await new Promise((r) => setTimeout(r, 10));

  switch (cmd) {
    case "open_roost": {
      const vp = (args?.path as string) ?? getMockRoostPath();
      let entries: FileEntry[];
      if (vp === getBrowserRoostRoot()) {
        const walked = await walkBrowserRoost();
        entries = walked ?? buildFileTree(vp);
      } else {
        entries = buildFileTree(vp);
      }
      return { entries, generation: 1, bookmark: null, resolvedPath: null } as T;
    }

    case "close_roost":
      return null as T;

    case "read_file": {
      const path = args?.path as string;
      if (pathInBrowserRoost(path)) {
        const r = await resolveBrowserPath(path);
        if (!r) throw `File not found: ${path}`;
        const fh = await r.parent.getFileHandle(r.name);
        const file = await fh.getFile();
        return (await file.text()) as T;
      }
      const content = mockFiles.get(path);
      if (content === undefined) throw `File not found: ${path}`;
      return content as T;
    }

    case "write_file": {
      const path = args?.path as string;
      const content = args?.content as string;
      if (pathInBrowserRoost(path)) {
        const r = await resolveBrowserPath(path, { create: true });
        if (!r) throw `Cannot resolve path: ${path}`;
        const fh = await r.parent.getFileHandle(r.name, { create: true });
        const w = await fh.createWritable();
        await w.write(content);
        await w.close();
        return undefined as T;
      }
      mockFiles.set(path, content);
      console.log(`[mock] Saved: ${path}`);
      return undefined as T;
    }

    case "create_file": {
      const path = args?.path as string;
      const baseName = path.split("/").pop()?.replace(".md", "") ?? "Untitled";
      const initial = `# ${baseName}\n\n`;
      if (pathInBrowserRoost(path)) {
        const r = await resolveBrowserPath(path, { create: true });
        if (!r) throw `Cannot resolve path: ${path}`;
        const fh = await r.parent.getFileHandle(r.name, { create: true });
        const w = await fh.createWritable();
        await w.write(initial);
        await w.close();
        return undefined as T;
      }
      mockFiles.set(path, initial);
      console.log(`[mock] Created: ${path}`);
      return undefined as T;
    }

    case "delete_file": {
      const path = args?.path as string;
      if (pathInBrowserRoost(path)) {
        const r = await resolveBrowserPath(path);
        if (!r) return undefined as T;
        await r.parent.removeEntry(r.name);
        return undefined as T;
      }
      mockFiles.delete(path);
      console.log(`[mock] Deleted (to trash): ${path}`);
      return undefined as T;
    }

    case "rename_file": {
      const oldPath = args?.old_path as string;
      const newPath = args?.new_path as string;
      if (pathInBrowserRoost(oldPath) && pathInBrowserRoost(newPath)) {
        const src = await resolveBrowserPath(oldPath);
        if (!src) throw `File not found: ${oldPath}`;
        const srcHandle = await src.parent.getFileHandle(src.name);
        const data = await (await srcHandle.getFile()).text();
        const dst = await resolveBrowserPath(newPath, { create: true });
        if (!dst) throw `Cannot resolve path: ${newPath}`;
        const dstHandle = await dst.parent.getFileHandle(dst.name, { create: true });
        const w = await dstHandle.createWritable();
        await w.write(data);
        await w.close();
        await src.parent.removeEntry(src.name);
        return undefined as T;
      }
      const content = mockFiles.get(oldPath);
      if (content !== undefined) {
        mockFiles.delete(oldPath);
        mockFiles.set(newPath, content);
      }
      return undefined as T;
    }

    case "create_daily_note": {
      // AUDIT-037: roostPath argument removed — backend resolves the
      // roost from registered state. Mock uses a synthetic display root.
      const folder = args?.folder as string;
      const today = new Date().toISOString().split("T")[0];
      const path = `mock-roost/${folder}/${today}.md`;
      if (!mockFiles.has(path)) {
        mockFiles.set(path, `---\ndate: ${today}\ntype: daily\ntags:\n  - daily\n---\n\n# ${today}\n\n## Tasks\n\n- [ ] \n\n## Notes\n\n\n`);
      }
      console.log(`[mock] Daily note: ${path}`);
      return path as T;
    }

    case "get_excluded_task_folders": {
      return mockExcludedFolders.slice() as T;
    }

    case "set_excluded_task_folders": {
      // Mirror the backend cleaning: backslashes→/, trim surrounding
      // slashes, drop blanks, de-dupe case-insensitively (first wins).
      const folders = (args?.folders as string[]) ?? [];
      const seen = new Set<string>();
      mockExcludedFolders = folders
        .map((f) => f.replace(/\\/g, "/").trim().replace(/^\/+|\/+$/g, "").trim())
        .filter((f) => f.length > 0)
        .filter((f) => {
          const k = f.toLowerCase();
          if (seen.has(k)) return false;
          seen.add(k);
          return true;
        });
      console.log(`[mock] Excluded task folders: ${mockExcludedFolders.join(", ")}`);
      return undefined as T;
    }

    case "create_folder": {
      const path = args?.path as string;
      if (pathInBrowserRoost(path)) {
        const r = await resolveBrowserPath(path, { create: true });
        if (r) await r.parent.getDirectoryHandle(r.name, { create: true });
        return undefined as T;
      }
      console.log(`[mock] Created folder: ${path}`);
      return undefined as T;
    }

    case "delete_folder": {
      const path = args?.path as string;
      if (pathInBrowserRoost(path)) {
        const r = await resolveBrowserPath(path);
        if (r) await r.parent.removeEntry(r.name, { recursive: true });
        return undefined as T;
      }
      console.log(`[mock] Deleted folder: ${path}`);
      return undefined as T;
    }

    case "move_entry": {
      const source = args?.source as string;
      const destDir = args?.dest_dir as string;
      const fileName = source.split("/").pop() ?? "";
      const newPath = `${destDir}/${fileName}`;
      if (pathInBrowserRoost(source) && pathInBrowserRoost(newPath)) {
        const src = await resolveBrowserPath(source);
        if (!src) throw `File not found: ${source}`;
        const srcHandle = await src.parent.getFileHandle(src.name);
        const data = await (await srcHandle.getFile()).text();
        const dst = await resolveBrowserPath(newPath, { create: true });
        if (!dst) throw `Cannot resolve path: ${newPath}`;
        const dstHandle = await dst.parent.getFileHandle(dst.name, { create: true });
        const w = await dstHandle.createWritable();
        await w.write(data);
        await w.close();
        await src.parent.removeEntry(src.name);
        return newPath as T;
      }
      const content = mockFiles.get(source);
      if (content !== undefined) {
        mockFiles.delete(source);
        mockFiles.set(newPath, content);
      }
      console.log(`[mock] Moved: ${source} → ${newPath}`);
      return newPath as T;
    }

    case "init_graph": {
      // Mock: scan all mock files and build a simple in-memory graph
      mockGraph.notes.clear();
      mockGraph.links.length = 0;
      mockGraph.tags.length = 0;
      const linkRe = /\[\[([^\]\n|]+?)(?:\|[^\]\n]+?)?\]\]/g;
      const tagRe = /(?:^|(?<=\s))#([\w][\w\-_/]*)/gm;
      for (const [path, content] of mockFiles.entries()) {
        // Mirror the Rust indexer: only markdown becomes a graph node.
        // Without this, `.corvux/` internals (index.json, chat sessions)
        // showed up as nodes in the browser-mode graph view.
        if (!path.endsWith(".md")) continue;
        const name = path.split("/").pop()?.replace(".md", "") ?? "";
        const id = name.toLowerCase();
        mockGraph.notes.set(id, { title: name, path });
        let m: RegExpExecArray | null;
        linkRe.lastIndex = 0;
        while ((m = linkRe.exec(content)) !== null) {
          mockGraph.links.push({ from_id: id, to_id: m[1].trim().toLowerCase(), link_text: m[1].trim() });
        }
        tagRe.lastIndex = 0;
        while ((m = tagRe.exec(content)) !== null) {
          mockGraph.tags.push({ note_id: id, tag: m[1] });
        }
      }
      console.log(`[mock] Graph initialized: ${mockGraph.notes.size} notes, ${mockGraph.links.length} links, ${mockGraph.tags.length} tags`);
      return { notes: mockGraph.notes.size, links: mockGraph.links.length, tags: mockGraph.tags.length } as T;
    }

    case "rebuild_database": {
      // AUDIT-051 dev-mode mock: there's no real on-disk DB to nuke,
      // so we just re-run the init_graph mock against the current
      // mock roost. Matches the IPC's prod contract — return the
      // GraphStatsResult of the freshly-walked roost.
      mockGraph.notes.clear();
      mockGraph.links.length = 0;
      mockGraph.tags.length = 0;
      const linkRe = /\[\[([^\]\n|]+?)(?:\|[^\]\n]+?)?\]\]/g;
      const tagRe = /(?:^|(?<=\s))#([\w][\w\-_/]*)/gm;
      for (const [path, content] of mockFiles.entries()) {
        if (!path.endsWith(".md")) continue; // markdown only — see init_graph
        const name = path.split("/").pop()?.replace(".md", "") ?? "";
        const id = name.toLowerCase();
        mockGraph.notes.set(id, { title: name, path });
        let m: RegExpExecArray | null;
        linkRe.lastIndex = 0;
        while ((m = linkRe.exec(content)) !== null) {
          mockGraph.links.push({ from_id: id, to_id: m[1].trim().toLowerCase(), link_text: m[1].trim() });
        }
        tagRe.lastIndex = 0;
        while ((m = tagRe.exec(content)) !== null) {
          mockGraph.tags.push({ note_id: id, tag: m[1] });
        }
      }
      console.log(`[mock] rebuild_database complete: ${mockGraph.notes.size} notes`);
      return { notes: mockGraph.notes.size, links: mockGraph.links.length, tags: mockGraph.tags.length } as T;
    }

    case "save_roost_note": {
      // AUDIT-053 dev-mode mock: write the file + run the same
      // mock-graph re-index that index_note does, then return a
      // SaveOutcome matching the Rust struct shape. Keeps the
      // browser-mock save path single-IPC just like prod.
      const path = args?.path as string;
      const content = args?.content as string;
      mockFiles.set(path, content);
      const name = path.split("/").pop()?.replace(".md", "") ?? "";
      const id = name.toLowerCase();
      mockGraph.notes.set(id, { title: name, path });
      mockGraph.links = mockGraph.links.filter(l => l.from_id !== id);
      mockGraph.tags = mockGraph.tags.filter(t => t.note_id !== id);
      const linkRe = /\[\[([^\]\n|]+?)(?:\|[^\]\n]+?)?\]\]/g;
      const tagRe = /(?:^|(?<=\s))#([\w][\w\-_/]*)/gm;
      let m: RegExpExecArray | null;
      while ((m = linkRe.exec(content)) !== null) {
        mockGraph.links.push({ from_id: id, to_id: m[1].trim().toLowerCase(), link_text: m[1].trim() });
      }
      while ((m = tagRe.exec(content)) !== null) {
        mockGraph.tags.push({ note_id: id, tag: m[1] });
      }
      // Mock backlinks: any link in the graph that targets this note's id.
      const backlinks = mockGraph.links
        .filter(l => l.to_id === id && l.from_id !== id)
        .map(l => {
          const fromNote = mockGraph.notes.get(l.from_id);
          return {
            title: fromNote?.title ?? l.from_id,
            path: fromNote?.path ?? l.from_id,
          };
        });
      return {
        stats: { notes: mockGraph.notes.size, links: mockGraph.links.length, tags: mockGraph.tags.length },
        tasks: [],
        backlinks,
        fts5: { kind: "indexed" },
      } as T;
    }

    case "index_note": {
      const path = args?.path as string;
      const content = args?.content as string;
      const name = path.split("/").pop()?.replace(".md", "") ?? "";
      const id = name.toLowerCase();
      mockGraph.notes.set(id, { title: name, path });
      // Re-parse links and tags for this note
      mockGraph.links = mockGraph.links.filter(l => l.from_id !== id);
      mockGraph.tags = mockGraph.tags.filter(t => t.note_id !== id);
      const linkRe = /\[\[([^\]\n|]+?)(?:\|[^\]\n]+?)?\]\]/g;
      const tagRe = /(?:^|(?<=\s))#([\w][\w\-_/]*)/gm;
      let m: RegExpExecArray | null;
      while ((m = linkRe.exec(content)) !== null) {
        mockGraph.links.push({ from_id: id, to_id: m[1].trim().toLowerCase(), link_text: m[1].trim() });
      }
      while ((m = tagRe.exec(content)) !== null) {
        mockGraph.tags.push({ note_id: id, tag: m[1] });
      }
      return undefined as T;
    }

    case "get_backlinks": {
      const notePath = args?.notePath as string;
      const name = notePath.split("/").pop()?.replace(".md", "") ?? "";
      const noteId = name.toLowerCase();
      const results = mockGraph.links
        .filter(l => l.to_id === noteId)
        .map(l => mockGraph.notes.get(l.from_id))
        .filter(Boolean)
        .map(n => ({ title: n!.title, path: n!.path }));
      return results as T;
    }

    case "get_note_links": {
      const notePath = args?.notePath as string;
      const name = notePath.split("/").pop()?.replace(".md", "") ?? "";
      const noteId = name.toLowerCase();
      const results = mockGraph.links
        .filter(l => l.from_id === noteId)
        .map(l => ({ target: l.to_id, link_text: l.link_text }));
      return results as T;
    }

    case "get_note_tags": {
      const notePath = args?.notePath as string;
      const name = notePath.split("/").pop()?.replace(".md", "") ?? "";
      const noteId = name.toLowerCase();
      const results = mockGraph.tags
        .filter(t => t.note_id === noteId)
        .map(t => t.tag);
      return results as T;
    }

    case "get_graph_stats": {
      return {
        notes: mockGraph.notes.size,
        links: mockGraph.links.length,
        tags: mockGraph.tags.length,
      } as T;
    }

    // Graph data commands. Without these, browser-dev `GraphView` hit
    // `undefined.map(...)` and showed the error state, blocking graph UX
    // testing in the browser. These return the same shapes the Rust backend
    // does so the canvas (and the mobile node card) render against mock data.
    case "get_full_graph": {
      const maxNodes = (args?.maxNodes as number) ?? 1000;
      const linkCount = (id: string) =>
        mockGraph.links.filter((l) => l.from_id === id || l.to_id === id).length;
      const nodes = [...mockGraph.notes.entries()]
        .slice(0, maxNodes)
        .map(([id, n]) => ({ id, title: n.title, path: n.path, kind: "note", link_count: linkCount(id) }));
      const idSet = new Set(nodes.map((n) => n.id));
      const edges = mockGraph.links
        .filter((l) => idSet.has(l.from_id) && idSet.has(l.to_id))
        .map((l) => ({ source: l.from_id, target: l.to_id }));
      return { nodes, edges } as T;
    }

    case "get_graph_window": {
      const focusId = (args?.focusId as string) ?? "";
      const depth = (args?.depth as number) ?? 2;
      const maxNodes = (args?.maxNodes as number) ?? 200;
      const adj = new Map<string, Set<string>>();
      for (const l of mockGraph.links) {
        (adj.get(l.from_id) ?? adj.set(l.from_id, new Set()).get(l.from_id)!).add(l.to_id);
        (adj.get(l.to_id) ?? adj.set(l.to_id, new Set()).get(l.to_id)!).add(l.from_id);
      }
      const visited = new Set<string>(focusId ? [focusId] : []);
      let frontier = [...visited];
      for (let d = 0; d < depth && visited.size < maxNodes; d++) {
        const next: string[] = [];
        for (const id of frontier) {
          for (const nb of adj.get(id) ?? []) {
            if (!visited.has(nb) && visited.size < maxNodes) {
              visited.add(nb);
              next.push(nb);
            }
          }
        }
        frontier = next;
      }
      const linkCount = (id: string) =>
        mockGraph.links.filter((l) => l.from_id === id || l.to_id === id).length;
      const nodes = [...visited]
        .filter((id) => mockGraph.notes.has(id))
        .map((id) => {
          const n = mockGraph.notes.get(id)!;
          return { id, title: n.title, path: n.path, kind: "note", link_count: linkCount(id) };
        });
      const idSet = new Set(nodes.map((n) => n.id));
      const edges = mockGraph.links
        .filter((l) => idSet.has(l.from_id) && idSet.has(l.to_id))
        .map((l) => ({ source: l.from_id, target: l.to_id }));
      return { nodes, edges } as T;
    }

    case "graph_communities": {
      // Single mock community keeps the canvas colouring code happy; the
      // real backend runs Louvain.
      return [...mockGraph.notes.keys()].map((id) => ({ note_id: id, community: 0 })) as T;
    }

    case "graph_page_rank": {
      const limit = (args?.limit as number) ?? 10000;
      return [...mockGraph.notes.keys()].slice(0, limit).map((id) => ({ note_id: id, rank: 1 })) as T;
    }

    case "parse_links": {
      const text = args?.content as string;
      const links: Array<{ target: string; display: string | null; start: number; end: number }> = [];
      const re = /\[\[([^\]\n|]+?)(?:\|([^\]\n]+?))?\]\]/g;
      let m: RegExpExecArray | null;
      while ((m = re.exec(text)) !== null) {
        links.push({
          target: m[1].trim(),
          display: m[2]?.trim() ?? null,
          start: m.index,
          end: m.index + m[0].length,
        });
      }
      return links as T;
    }

    case "parse_tags": {
      const text = args?.content as string;
      const tags: Array<{ tag: string; start: number; end: number }> = [];
      const re = /(?:^|(?<=\s))#([\w][\w\-_/]*)/gm;
      let m: RegExpExecArray | null;
      while ((m = re.exec(text)) !== null) {
        const tagStart = m.index + (m[0].startsWith("#") ? 0 : m[0].indexOf("#"));
        tags.push({
          tag: m[1],
          start: tagStart,
          end: tagStart + 1 + m[1].length,
        });
      }
      return tags as T;
    }

    case "init_search": {
      console.log(`[mock] FTS5 search index initialized for ${args?.roostPath}`);
      return mockFiles.size as T;
    }

    case "index_search": {
      console.log(`[mock] FTS5 indexed: ${args?.path}`);
      return undefined as T;
    }

    case "remove_from_search": {
      console.log(`[mock] FTS5 removed: ${args?.path}`);
      return undefined as T;
    }

    case "list_agent_profiles": {
      // Mirror the Rust descriptor list so the picker renders something
      // useful in browser-mock dev. Real profiles come from Tauri.
      return [
        { id: "default", label: "Default", summary: "Balanced — all tools, normal budget." },
        { id: "quicklookup", label: "Quick Lookup", summary: "Fast — roost + memory only, 3 iterations." },
        { id: "research", label: "Research", summary: "Thorough — all tools, 12 iterations, large budget." },
        { id: "writer", label: "Writer", summary: "Drafting — roost + memory, no graph noise." },
      ] as T;
    }

    case "search_roost": {
      const query = (args?.query as string || "").toLowerCase();
      if (!query) return [] as T;
      const results: Array<{ path: string; name: string; snippet: string; matches: Array<{ line: number; text: string; col: number }>; score: number; match_type: "fts" }> = [];
      for (const [path, content] of mockFiles.entries()) {
        const name = path.split("/").pop() ?? "";
        const matches: Array<{ line: number; text: string; col: number }> = [];
        const lines = content.split("\n");
        for (let i = 0; i < lines.length; i++) {
          const lineLower = lines[i].toLowerCase();
          let searchFrom = 0;
          while (true) {
            const col = lineLower.indexOf(query, searchFrom);
            if (col === -1) break;
            matches.push({ line: i + 1, text: lines[i].trim(), col });
            searchFrom = col + query.length;
          }
        }
        if (matches.length > 0) {
          // Generate a mock snippet from first match context
          const firstMatch = matches[0];
          results.push({
            path,
            name,
            snippet: `...${firstMatch.text}...`,
            matches,
            // Mock hybrid metadata — keeps the new SearchResult shape
            // satisfied so the search panel renders identically in mock
            // and prod. Score is just inverse rank for sorting.
            score: 1 / (results.length + 1),
            match_type: "fts" as const,
          });
        }
      }
      results.sort((a, b) => b.matches.length - a.matches.length);
      return results as T;
    }

    case "get_settings": {
      const stored = lsGet("corvux-settings");
      if (stored) {
        const parsed = JSON.parse(stored);
        // Backfill fields added in later versions so mock stays shape-compatible
        if (parsed.embedding_backend === undefined) parsed.embedding_backend = "auto";
        return parsed as T;
      }
      return {
        // Q-04: keep in sync with `src/stores/settings.ts:DEFAULT_SETTINGS`
        // and Rust `Settings::default()` in
        // `src-tauri/src/commands/ai/types.rs` — see `default_settings_tests`.
        gemini_api_key: null,
        model: "gemini-2.5-flash",
        embedding_backend: "auto",
        llm_backend: "auto",
        agent_profile: "default",
        llm_provider: "gemini",
        openrouter_api_key: null,
        openrouter_model: "anthropic/claude-sonnet-4.6",
        ollama_base_url: "http://localhost:11434",
        ollama_model: "",
        ollama_api_key: null,
        support_endpoint: null,
        agent_autonomy: {
          auto_approve_reads: true,
          auto_approve_writes: false,
          auto_approve_tools: [],
          agent_blocked_paths: [],
        },
        use_native_tools: true,
      } as T;
    }

    case "get_device_profile": {
      // Mock a mid-range laptop — Tier 1 (hybrid, default).
      return {
        total_ram_gb: 8.0,
        cpu_cores: 4,
        free_disk_gb: 120.0,
        tier: "tier1",
        platform: "browser",
      } as T;
    }

    case "save_settings": {
      const settings = args?.settings as Record<string, unknown>;
      lsSet("corvux-settings", JSON.stringify(settings));
      console.log("[mock] Settings saved");
      return undefined as T;
    }

    case "chat_send": {
      const messages = args?.messages as Array<{ role: string; content: string }>;
      const context = args?.context as string | null;

      const stored = lsGet("corvux-settings");
      const settings: Record<string, unknown> = stored ? JSON.parse(stored) : {};
      const provider = (settings.llm_provider as string | undefined) ?? "gemini";
      const apiKey =
        provider === "openrouter"
          ? (settings.openrouter_api_key as string | null)
          : (settings.gemini_api_key as string | null);

      if (!apiKey) {
        const lastMsg = messages?.[messages.length - 1]?.content ?? "";
        const providerLabel = provider === "openrouter" ? "OpenRouter" : "Gemini";
        const fallback = `No ${providerLabel} API key set. Add your key in Settings (gear icon) to use real AI.\n\nYou asked: "${lastMsg.slice(0, 80)}..."`;
        const words = fallback.split(" ");
        let wi = 0;
        const iv = setInterval(() => {
          if (wi < words.length) {
            window.dispatchEvent(new CustomEvent("mock:ai:chunk", { detail: (wi === 0 ? "" : " ") + words[wi++] }));
          } else {
            clearInterval(iv);
            window.dispatchEvent(new CustomEvent("mock:ai:done", { detail: fallback }));
          }
        }, 50);
        return undefined as T;
      }

      // Real Gemini API — fires in browser dev mode when the user has set an API key.
      const model = (settings.model as string | undefined) ?? "gemini-2.5-flash";
      const contents = messages.map((m) => ({
        role: m.role === "model" ? "model" : "user",
        parts: [{ text: m.content }],
      }));
      let systemText =
        "You are Corvux AI, an intelligent knowledge assistant. Help the user understand, connect, and build on their notes. Be concise and insightful.";
      if (context) {
        systemText += `\n\nThe user has this note open:\n\n${context.slice(0, 4000)}`;
      }
      const geminiBody = {
        contents,
        systemInstruction: { parts: [{ text: systemText }] },
        generationConfig: { temperature: 0.7, maxOutputTokens: 2048 },
      };
      const url = `https://generativelanguage.googleapis.com/v1beta/models/${model}:streamGenerateContent?key=${apiKey}&alt=sse`;

      void (async () => {
        try {
          const resp = await fetch(url, {
            method: "POST",
            headers: { "Content-Type": "application/json" },
            body: JSON.stringify(geminiBody),
          });
          if (!resp.ok) {
            const errText = await resp.text();
            window.dispatchEvent(new CustomEvent("mock:ai:error", {
              detail: `Gemini ${resp.status}: ${errText.slice(0, 300)}`,
            }));
            return;
          }
          const reader = resp.body?.getReader();
          if (!reader) {
            window.dispatchEvent(new CustomEvent("mock:ai:error", { detail: "No response body" }));
            return;
          }
          const decoder = new TextDecoder();
          let buf = "";
          while (true) {
            const { done, value } = await reader.read();
            if (done) break;
            buf += decoder.decode(value, { stream: true });
            const lines = buf.split("\n");
            buf = lines.pop() ?? "";
            for (const line of lines) {
              if (!line.startsWith("data: ")) continue;
              const data = line.slice(6).trim();
              if (!data || data === "[DONE]") continue;
              try {
                const parsed = JSON.parse(data) as {
                  candidates?: Array<{ content?: { parts?: Array<{ text?: string }> } }>;
                };
                const text = parsed?.candidates?.[0]?.content?.parts?.[0]?.text;
                if (text) window.dispatchEvent(new CustomEvent("mock:ai:chunk", { detail: text }));
              } catch { /* malformed line */ }
            }
          }
          window.dispatchEvent(new CustomEvent("mock:ai:done"));
        } catch (err) {
          window.dispatchEvent(new CustomEvent("mock:ai:error", {
            detail: errMsg(err),
          }));
        }
      })();

      return undefined as T;
    }

    case "chat_stop": {
      console.log("[mock] Chat stopped");
      return undefined as T;
    }

    case "save_attachment": {
      const ext = args?.extension as string || "png";
      const data = (args?.data as string) ?? "";
      const filename = `paste-${Date.now()}.${ext}`;
      mockAttachments.set(filename, data);
      console.log(`[mock] Saved attachment: ${filename} (${data.length} b64 chars)`);
      return filename as T;
    }

    case "read_attachment": {
      const rel = (args?.relativePath as string) ?? "";
      // Incoming path is like `.attachments/paste-xxx.png` — key by filename only
      const filename = rel.split("/").pop() ?? rel;
      const data = mockAttachments.get(filename);
      if (!data) throw `Attachment not found: ${rel}`;
      return data as T;
    }

    case "get_suggestions": {
      const notePath = args?.notePath as string;
      const name = notePath.split("/").pop()?.replace(".md", "") ?? "";
      const noteId = name.toLowerCase();
      // Build mock suggestions from the mock graph
      const simNotes: Array<{ note_id: string; title: string; path: string; relevance: number; preview: string }> = [];
      for (const [id, n] of mockGraph.notes.entries()) {
        if (id !== noteId) {
          simNotes.push({
            note_id: id,
            title: n.title,
            path: n.path,
            relevance: Math.round((0.5 + Math.random() * 0.4) * 100) / 100,
            preview: `Content from ${n.title}...`,
          });
        }
      }
      simNotes.sort((a, b) => b.relevance - a.relevance);
      // Tag suggestions from neighbor tags
      const currentTags = new Set(mockGraph.tags.filter(t => t.note_id === noteId).map(t => t.tag));
      const tagFreq = new Map<string, number>();
      for (const link of mockGraph.links) {
        const neighbor = link.from_id === noteId ? link.to_id : (link.to_id === noteId ? link.from_id : null);
        if (!neighbor) continue;
        for (const t of mockGraph.tags.filter(t => t.note_id === neighbor)) {
          if (!currentTags.has(t.tag)) {
            tagFreq.set(t.tag, (tagFreq.get(t.tag) ?? 0) + 1);
          }
        }
      }
      const tagSugs = Array.from(tagFreq.entries())
        .map(([tag, count]) => ({ tag, confidence: count / Math.max(1, tagFreq.size), source_count: count }))
        .sort((a, b) => b.confidence - a.confidence)
        .slice(0, 5);
      // Link suggestions: notes not already linked
      const linkedIds = new Set(mockGraph.links.filter(l => l.from_id === noteId).map(l => l.to_id));
      linkedIds.add(noteId);
      const linkSugs = Array.from(mockGraph.notes.entries())
        .filter(([id]) => !linkedIds.has(id))
        .map(([id, n]) => ({
          note_id: id,
          title: n.title,
          path: n.path,
          confidence: Math.round((0.3 + Math.random() * 0.5) * 100) / 100,
          reason: "Similar content",
        }))
        .slice(0, 3);
      return {
        similar_notes: simNotes.slice(0, 5),
        tag_suggestions: tagSugs,
        link_suggestions: linkSugs,
      } as T;
    }

    case "accept_suggestion": {
      console.log(`[mock] Accepted ${args?.suggestionType}: ${args?.target}`);
      return undefined as T;
    }

    case "dismiss_suggestion": {
      console.log(`[mock] Dismissed ${args?.suggestionType}: ${args?.target}`);
      return undefined as T;
    }

    case "embed_roost": {
      console.log("[mock] Roost embedding simulated");
      return 12 as T; // Simulate 12 chunks embedded
    }

    case "embed_note": {
      console.log("[mock] Note embedding simulated");
      return 2 as T;
    }

    case "get_embedding_model_status": {
      // Mock: pretend the model is not installed yet so the download UI shows.
      const installed = lsGet("corvux:mock:model-installed") === "1";
      return {
        name: "nomic-embed-text-v1.5",
        installed,
        size_bytes: installed ? 137_000_000 + 700_000 : 0,
        path: "/mock/corvux/models/nomic-embed-text-v1.5",
      } as T;
    }

    case "get_llm_model_status": {
      // Browser dev never has a local LLM — surface "not installed" so the
      // settings panel shows the cloud-only note instead of crashing.
      return {
        name: "qwen2.5-1.5b-instruct-q4",
        installed: false,
        size_bytes: 0,
        path: "/mock/corvux/models/qwen2.5-1.5b",
      } as T;
    }

    case "download_embedding_model": {
      // Fake a download with synthetic progress events so UI devs can see
      // the progress bar without hitting HuggingFace.
      console.log("[mock] Model download simulated");
      const total = 137_000_000;
      let downloaded = 0;
      const step = total / 50;
      const tick = () => {
        downloaded = Math.min(downloaded + step, total);
        const percent = (downloaded / total) * 100;
        window.dispatchEvent(
          new CustomEvent("mock:ai:model-download-progress", {
            detail: {
              file: "model.onnx",
              bytes_downloaded: Math.round(downloaded),
              total_bytes: total,
              percent,
            },
          })
        );
        if (downloaded < total) {
          setTimeout(tick, 40);
        } else {
          lsSet("corvux:mock:model-installed", "1");
          window.dispatchEvent(new CustomEvent("mock:ai:model-download-done"));
        }
      };
      setTimeout(tick, 50);
      return undefined as T;
    }

    case "claude_desktop_mcp": {
      // Browser dev has no Claude Desktop / sidecar — return a friendly stub.
      const action = (args?.action as string) ?? "status";
      if (action === "status") return "Not connected (browser dev)." as T;
      return `Mock ${action}: works only in the desktop app.` as T;
    }

    case "list_tasks":
      // Browser dev has no task indexer. An explicit empty list keeps the
      // Tasks pane on its real empty state — falling through to the
      // `undefined` default made `applyTasksFromBackend` throw and the
      // pane showed a raw TypeError.
      return [] as T;

    case "is_store_packaged":
      // Browser dev is never an MSIX install.
      return false as T;

    default:
      console.warn(`[mock] Unknown command: ${cmd}`);
      return undefined as T;
  }
}

// ── Exported invoke function ─────────────────────────────────────

let realInvoke: typeof mockInvoke | null = null;

export async function appInvoke<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  if (isTauri) {
    if (!realInvoke) {
      const mod = await import("@tauri-apps/api/core");
      realInvoke = mod.invoke as typeof mockInvoke;
    }
    return realInvoke<T>(cmd, args);
  }
  return mockInvoke<T>(cmd, args);
}

/** Whether we're running in Tauri (native) or browser (dev mock) */
export { isTauri };

// ── Typed IPC surface (tauri-specta) ─────────────────────────────
//
// The `commands` object below is the *typed* IPC surface — generated
// from Rust by tauri-specta into `./bindings.ts`. Native builds route
// through the real Tauri `invoke` (via the generated bindings); browser
// dev routes to `mockCommands`, which has the same TypeScript type and
// delegates to the legacy `mockInvoke` dispatcher above.
//
// Migration policy: as each Rust module gains `#[specta::specta]`
// annotations and joins `collect_commands![]`, its commands appear in
// `bindings.commands` AND require a corresponding mock entry in
// `mock-commands.ts`. Until a command lives here, call sites continue
// using the legacy untyped `appInvoke(...)`. C11 removes `appInvoke`.

import * as bindings from "./bindings";
import { mockCommands } from "./mock-commands";

export const commands: typeof bindings.commands = isTauri
  ? bindings.commands
  : mockCommands;

/**
 * Typed event surface generated by tauri-specta. Listeners get the
 * payload type inferred from the matching Rust `tauri_specta::Event`
 * struct in `src-tauri/src/events.rs`.
 *
 * Browser-mode (`!isTauri`) callers MUST gate `.listen()` behind an
 * `isTauri` check — the underlying `@tauri-apps/api/event` listener
 * has no in-browser fallback and `__TAURI_EVENT.listen(...)` throws
 * outside the Tauri runtime. The events object is re-exported as-is
 * here (no mock layer) because every existing call site already
 * guards on `isTauri` — adding a no-op stub would be a hammer for a
 * problem the callers already solved.
 */
export const events = bindings.events;

/**
 * Unwrap a tauri-specta result envelope, throwing on `status: "error"`.
 * Matches the legacy `invoke<T>(...)` throw-on-Rust-error semantics so
 * per-slice migrations can be mechanical: `await invoke("foo", {...})`
 * becomes `await unwrap(commands.foo(...))` without restructuring the
 * surrounding try/catch.
 *
 * Use the discriminated-union pattern directly (no `unwrap`) when the
 * caller wants to handle errors inline rather than throw.
 */
export async function unwrap<T, E>(
  p: Promise<{ status: "ok"; data: T } | { status: "error"; error: E }>,
): Promise<T> {
  const r = await p;
  if (r.status === "error") throw r.error;
  return r.data;
}

// Re-export the result-envelope and generated types so call sites have
// one canonical import path.
export type {
  FileEntry,
  OpenRoostResponse,
} from "./bindings";
