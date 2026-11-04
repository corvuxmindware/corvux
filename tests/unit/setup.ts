/**
 * Vitest setup — mocks Tauri's `invoke` so store logic that calls into
 * the backend doesn't error out under happy-dom.
 *
 * The browser-mock invoke layer in `src/lib/invoke.ts` already handles
 * the cases the dev mock-mode supports; for unit tests we override
 * specific commands per-test via `mockInvoke`.
 */

import { beforeEach, vi } from "vitest";

// In-memory file system that read_file / write_file / delete_file work
// against during tests. Cleared before every test so they don't bleed.
export const mockFs = new Map<string, string>();

// Per-command override — if a test sets `mockInvoke.set("name", fn)`,
// invoke() routes to it before falling through to mockFs handling.
export const mockInvoke = new Map<string, (args: any) => any>();

beforeEach(() => {
  mockFs.clear();
  mockInvoke.clear();
});

// Crypto.randomUUID — happy-dom may not have it.
if (!globalThis.crypto?.randomUUID) {
  Object.defineProperty(globalThis, "crypto", {
    value: {
      ...globalThis.crypto,
      randomUUID: () =>
        "test-" + Math.random().toString(16).slice(2, 10) + "-" + Date.now().toString(16),
    },
    configurable: true,
  });
}

// Mock the invoke module that stores import. We intercept here instead
// of mocking @tauri-apps/api because the project routes through its
// own appInvoke wrapper (src/lib/invoke.ts).
//
// Q-01: the mock now also exports `unwrap` and `commands` so store
// code that imports them resolves to test-aware behavior. `commands`
// is a Proxy that dispatches each camelCase IPC method through
// `testMockInvoke` with the matching snake_case command name and the
// args object the real binding would build. The mapping table is the
// camelCase → (snake_case + arg names) shape inline below — kept in
// sync with `src/lib/mock-commands.ts` by hand. Drift surfaces as a
// test failure (`Cannot read properties of undefined`).
vi.mock("../../src/lib/invoke", () => {
  const testMockInvoke = async <T>(
    cmd: string,
    args?: Record<string, unknown>,
  ): Promise<T> => {
    // Per-test overrides win.
    const handler = mockInvoke.get(cmd);
    if (handler) return handler(args ?? {}) as T;

    // Default behaviors for fs-style commands so chat session
    // persistence works without ceremony in tests.
    switch (cmd) {
      case "read_file": {
        const path = String(args?.path ?? "");
        const content = mockFs.get(path);
        if (content === undefined) throw new Error(`mock fs: not found ${path}`);
        return content as T;
      }
      case "write_file": {
        mockFs.set(String(args?.path ?? ""), String(args?.content ?? ""));
        return undefined as T;
      }
      case "delete_file": {
        mockFs.delete(String(args?.path ?? ""));
        return undefined as T;
      }
      case "list_agent_profiles": {
        return [
          { id: "default", label: "Default", summary: "Balanced." },
          { id: "quicklookup", label: "Quick Lookup", summary: "Fast." },
          { id: "research", label: "Research", summary: "Thorough." },
          { id: "writer", label: "Writer", summary: "Drafting." },
        ] as T;
      }
      case "get_settings": {
        return {
          gemini_api_key: null,
          model: "gemini-2.5-flash",
          llm_backend: "auto",
          agent_profile: "default",
        } as T;
      }
      case "save_settings": {
        return undefined as T;
      }
      case "get_device_profile": {
        return {
          total_ram_gb: 16,
          cpu_cores: 8,
          free_disk_gb: 100,
          tier: "tier2",
          platform: "test",
        } as T;
      }
      case "get_embedding_model_status":
      case "get_llm_model_status": {
        return {
          name: "test",
          installed: false,
          size_bytes: 0,
          path: "",
        } as T;
      }
      default:
        // Unhandled commands are non-fatal — tests that need them
        // should set a mockInvoke override.
        return undefined as T;
    }
  };

  // Mapping from camelCase IPC method → (snake_case command, ordered
  // arg names). Mirrors `src/lib/bindings.ts` and `src/lib/mock-commands.ts`.
  // Most commands take 0 or 1 arg; multi-arg cases are listed
  // explicitly. Commands not in the table fall through to a generic
  // "pass first positional arg as `args`" wrapper, which is enough
  // for the parser- and embedding-shaped commands the tests don't
  // need to exercise precisely.
  type ArgMap = Record<string, string[]>;
  const ARG_NAMES: ArgMap = {
    openRoost: ["path", "displayName", "bookmark"],
    createNamedRoost: ["name"],
    readFile: ["path"],
    writeFile: ["path", "content"],
    createFile: ["path"],
    deleteFile: ["path", "roostPath"],
    renameFile: ["oldPath", "newPath"],
    createFolder: ["path"],
    deleteFolder: ["path"],
    moveEntry: ["source", "destDir"],
    initGraph: ["roostPath"],
    rebuildDatabase: ["roostPath"],
    indexNote: ["path", "content"],
    getBacklinks: ["notePath"],
    getNoteLinks: ["notePath"],
    getNoteTags: ["notePath"],
    initSearch: ["roostPath"],
    indexSearch: ["path", "content"],
    removeFromSearch: ["path"],
    searchRoost: ["roostPath", "query"],
    parseTasks: ["content"],
    parseLinks: ["content"],
    parseTags: ["content"],
    completeTask: ["notePath", "sourceLine", "completed", "contentAnchor"],
    addTaskLine: ["notePath", "text"],
    rescheduleTask: ["notePath", "textAnchor", "newDate", "newTime"],
    createDailyNote: ["folder"],
    saveAttachment: ["roostPath", "data", "extension"],
    readAttachment: ["roostPath", "relativePath"],
    listEditEvents: ["notePath", "limit"],
    revertEdit: ["notePath", "eventId", "force"],
    saveRoostNote: ["path", "content"],
    getDiagnosticReport: ["redactPaths"],
    getRecentLogs: ["limit"],
    saveSettings: ["settings"],
    decideTool: ["requestId", "decision"],
    forgetPendingApproval: ["requestId"],
    listApprovalLog: ["limit"],
    toolReliabilityStats: ["windowDays"],
    getAiLearningSummary: ["topN"],
    chatSend: ["args"],
    syncInit: ["roostPath"],
    syncSetQueueEndpoint: ["endpoint"],
    syncPinCustomQueueHost: ["host", "hostConfirm"],
    syncSetStrictP2pOnly: ["enabled"],
    listOllamaModels: ["baseUrl", "apiKey"],
    testGeminiConnection: ["apiKey"],
    testOpenrouterConnection: ["apiKey"],
    testOllamaConnection: ["baseUrl", "apiKey"],
    graphPageRank: ["limit"],
    graphCommunities: ["limit"],
    graphBridges: ["limit"],
    computeBridgesNow: ["limit"],
    graphShortestPath: ["fromId", "toId"],
    graphShortestPathBetween: ["fromPath", "toPath"],
    getGraphWindow: ["focusId", "depth", "maxNodes"],
  };
  const camelToSnake = (s: string) =>
    s.replace(/[A-Z]/g, (m) => `_${m.toLowerCase()}`);
  const envelope = async <T>(p: Promise<T>) => {
    try {
      return { status: "ok" as const, data: await p };
    } catch (e) {
      return { status: "error" as const, error: String(e) };
    }
  };
  const commands = new Proxy(
    {},
    {
      get(_target, prop: string | symbol) {
        if (typeof prop !== "string") return undefined;
        const snake = camelToSnake(prop);
        const names = ARG_NAMES[prop];
        return (...positional: unknown[]) => {
          let args: Record<string, unknown> | undefined;
          if (names && names.length > 0) {
            args = {};
            names.forEach((n, i) => {
              args![n] = positional[i];
            });
          } else if (positional.length > 0) {
            args = { args: positional[0] };
          }
          return envelope(testMockInvoke(snake, args));
        };
      },
    },
  );

  return {
    isTauri: false,
    appInvoke: testMockInvoke,
    mockInvoke: testMockInvoke,
    commands,
    unwrap: async <T, E>(
      p: Promise<{ status: "ok"; data: T } | { status: "error"; error: E }>,
    ): Promise<T> => {
      const r = await p;
      if (r.status === "error") throw r.error;
      return r.data;
    },
    // Tests that subscribe to events use their own per-test mocks
    // (see chat-listeners.test.ts). Provide a no-op proxy so naked
    // `events.xxx.listen(...)` calls don't throw — listeners are
    // never invoked because we never `emit`.
    events: new Proxy(
      {},
      {
        get: () => ({
          listen: async () => () => {},
          emit: async () => {},
        }),
      },
    ),
  };
});
