/**
 * Browser-mode mock for the typed `commands` surface exported by
 * `bindings.ts`. Each method delegates to the legacy `mockInvoke`
 * dispatcher (which owns the in-memory mock roost state) and wraps the
 * result in the `typedError`-shaped envelope tauri-specta produces, so
 * the return type matches what callers see in native Tauri builds.
 *
 * Only the commands currently present in `bindings.ts` need entries
 * here — TypeScript will fail to compile if drift opens up (the
 * `typeof bindings.commands` annotation is the enforcement).
 *
 * As each slice migrates commands into `collect_commands![]` (Rust),
 * bindings.ts grows, and so does this file.
 */

import type { commands as RealCommands, EntitlementSnapshot } from "./bindings";
import { mockInvoke } from "./invoke";

type OkErr<T, E = string> =
  | { status: "ok"; data: T }
  | { status: "error"; error: E };

async function envelope<T>(p: Promise<T>): Promise<OkErr<T>> {
  try {
    return { status: "ok", data: await p };
  } catch (e) {
    return { status: "error", error: String(e) };
  }
}

export const mockCommands: typeof RealCommands = {
  // ── roost.rs ────────────────────────────────────────────────────
  openRoost: (path, displayName, bookmark) =>
    envelope(mockInvoke("open_roost", { path, displayName, bookmark })),
  closeRoost: () => envelope(mockInvoke("close_roost")),
  initDefaultRoost: () => envelope(mockInvoke("init_default_roost")),
  createNamedRoost: (name) =>
    envelope(mockInvoke("create_named_roost", { name })),
  // MR-B1/B3: no roost manifest or per-roost index in browser-mock mode.
  writeRoostManifest: (_json) => envelope(Promise.resolve(null)),
  readRoostManifest: () => envelope(Promise.resolve(null)),
  deleteRoostIndex: (_roostPath) => envelope(Promise.resolve(null)),
  // ── mcp.rs (Connect to Claude Desktop) ──────────────────────────
  claudeDesktopMcp: (action, roost, mode) =>
    envelope(mockInvoke("claude_desktop_mcp", { action, roost, mode })),
  mcpConfigJson: (roost, mode) =>
    envelope(mockInvoke("mcp_config_json", { roost, mode })),
  // ── files.rs ────────────────────────────────────────────────────
  readFile: (path) => envelope(mockInvoke("read_file", { path })),
  writeFile: (path, content) =>
    envelope(mockInvoke("write_file", { path, content })),
  createFile: (path) => envelope(mockInvoke("create_file", { path })),
  deleteFile: (path, roostPath) =>
    envelope(mockInvoke("delete_file", { path, roostPath })),
  renameFile: (oldPath, newPath) =>
    envelope(mockInvoke("rename_file", { oldPath, newPath })),
  createFolder: (path) => envelope(mockInvoke("create_folder", { path })),
  deleteFolder: (path) => envelope(mockInvoke("delete_folder", { path })),
  moveEntry: (source, destDir) =>
    envelope(mockInvoke("move_entry", { source, destDir })),
  // ── graph.rs ────────────────────────────────────────────────────
  initGraph: (roostPath) => envelope(mockInvoke("init_graph", { roostPath })),
  rebuildDatabase: (roostPath) =>
    envelope(mockInvoke("rebuild_database", { roostPath })),
  indexNote: (path, content) =>
    envelope(mockInvoke("index_note", { path, content })),
  purgeNoteFromGraph: (path) =>
    envelope(mockInvoke("purge_note_from_graph", { path })),
  getBacklinks: (notePath) => envelope(mockInvoke("get_backlinks", { notePath })),
  getNoteLinks: (notePath) =>
    envelope(mockInvoke("get_note_links", { notePath })),
  getNoteTags: (notePath) => envelope(mockInvoke("get_note_tags", { notePath })),
  getGraphStats: () => envelope(mockInvoke("get_graph_stats")),
  getGraphWindow: (focusId, depth, maxNodes) =>
    envelope(mockInvoke("get_graph_window", { focusId, depth, maxNodes })),
  getFullGraph: (maxNodes) =>
    envelope(mockInvoke("get_full_graph", { maxNodes })),
  graphPageRank: (limit) => envelope(mockInvoke("graph_page_rank", { limit })),
  graphCommunities: (limit) => envelope(mockInvoke("graph_communities", { limit })),
  graphBridges: (limit) => envelope(mockInvoke("graph_bridges", { limit })),
  computeBridgesNow: (limit) =>
    envelope(mockInvoke("compute_bridges_now", { limit })),
  graphShortestPath: (fromId, toId) =>
    envelope(mockInvoke("graph_shortest_path", { fromId, toId })),
  graphShortestPathBetween: (fromPath, toPath) =>
    envelope(mockInvoke("graph_shortest_path_between", { fromPath, toPath })),
  // ── search.rs ───────────────────────────────────────────────────
  initSearch: (roostPath) => envelope(mockInvoke("init_search", { roostPath })),
  indexSearch: (path, content) =>
    envelope(mockInvoke("index_search", { path, content })),
  removeFromSearch: (path) =>
    envelope(mockInvoke("remove_from_search", { path })),
  searchRoost: (roostPath, query) =>
    envelope(mockInvoke("search_roost", { roostPath, query })),
  // ── parser/*.rs ─────────────────────────────────────────────────
  // Parsers return raw `Vec<T>` (no `Result`), so tauri-specta emits
  // bare `Promise<T[]>` rather than the typedError envelope. No `envelope()`.
  parseTasks: (content) => mockInvoke("parse_tasks", { content }),
  parseLinks: (content) => mockInvoke("parse_links", { content }),
  parseTags: (content) => mockInvoke("parse_tags", { content }),
  // ── tasks.rs + task_notify.rs ───────────────────────────────────
  listTasks: () => envelope(mockInvoke("list_tasks")),
  completeTask: (notePath, sourceLine, completed, contentAnchor) =>
    envelope(mockInvoke("complete_task", { notePath, sourceLine, completed, contentAnchor })),
  addTaskLine: (notePath, text) =>
    envelope(mockInvoke("add_task_line", { notePath, text })),
  rescheduleTask: (notePath, textAnchor, newDate, newTime) =>
    envelope(mockInvoke("reschedule_task", { notePath, textAnchor, newDate, newTime })),
  debugDumpTasks: () => envelope(mockInvoke("debug_dump_tasks")),
  getExcludedTaskFolders: () =>
    envelope(mockInvoke("get_excluded_task_folders")),
  setExcludedTaskFolders: (folders) =>
    envelope(mockInvoke("set_excluded_task_folders", { folders })),
  sendTestNotification: () => envelope(mockInvoke("send_test_notification")),
  // ── daily_note + attachments + edit_history + save ─────────────
  createDailyNote: (folder) =>
    envelope(mockInvoke("create_daily_note", { folder })),
  saveAttachment: (roostPath, data, extension) =>
    envelope(mockInvoke("save_attachment", { roostPath, data, extension })),
  readAttachment: (roostPath, relativePath) =>
    envelope(mockInvoke("read_attachment", { roostPath, relativePath })),
  // Browser-mock has no roost filesystem to scan → no orphans.
  findOrphanAttachments: () =>
    envelope(
      Promise.resolve({ orphans: [], total_attachments: 0, notes_scanned: 0 }),
    ),
  listEditEvents: (notePath, limit) =>
    envelope(mockInvoke("list_edit_events", { notePath, limit })),
  revertEdit: (notePath, eventId, force) =>
    envelope(mockInvoke("revert_edit", { notePath, eventId, force })),
  // External-write capture has no graph/search DB in browser-mock — report
  // "nothing recorded" so the frontend note_written/note_deleted handlers stay
  // wired without a backend.
  recordExternalEdit: (_notePath, _content) => envelope(Promise.resolve(false)),
  recordExternalDelete: (_notePath) => envelope(Promise.resolve(false)),
  // EH-3: browser-mock has no graph DB — purge is a no-op (0 removed) and
  // export returns an empty audit blob so the Settings controls stay wired.
  purgeNoteEditHistory: (_notePath) => envelope(Promise.resolve(0)),
  purgeRoostEditHistory: () => envelope(Promise.resolve(0)),
  exportEditHistory: (_notePath) =>
    envelope(
      Promise.resolve(
        JSON.stringify(
          {
            corvux_edit_history_export: {
              version: 1,
              roost_path: "",
              exported_at_unix: 0,
              scope: "<entire roost>",
              event_count: 0,
              events: [],
            },
          },
          null,
          2,
        ),
      ),
    ),
  saveRoostNote: (path, content, baseHash) =>
    envelope(mockInvoke("save_roost_note", { path, content, baseHash })),
  // ── diagnostics.rs + perf.rs ────────────────────────────────────
  getDiagnosticReport: (redactPaths) =>
    envelope(mockInvoke("get_diagnostic_report", { redactPaths })),
  getRecentLogs: (limit) =>
    envelope(mockInvoke("get_recent_logs", { limit })),
  clearLogBuffer: () => envelope(mockInvoke("clear_log_buffer")),
  checkPreviousCrash: () => envelope(mockInvoke("check_previous_crash")),
  markUpdateRelaunch: () => envelope(mockInvoke("mark_update_relaunch")),
  isStorePackaged: () => envelope(mockInvoke("is_store_packaged")),
  getPerfSnapshot: () => envelope(mockInvoke("get_perf_snapshot")),
  // ── entitlement.rs ──────────────────────────────────────────────
  // Browser-mode dev has no entitlement token → always Free (matches the
  // Phase A backend stub). Fail-closed-to-Free is the prime invariant.
  getEntitlement: () =>
    envelope(Promise.resolve<EntitlementSnapshot>({ tier: "Free", allowed: [] })),
  // ── ai/llm.rs ───────────────────────────────────────────────────
  getLlmModelStatus: () => mockInvoke("get_llm_model_status"),
  downloadLlmModel: () => envelope(mockInvoke("download_llm_model")),
  loadLlmModel: () => envelope(mockInvoke("load_llm_model")),
  unloadLlmModel: () => envelope(mockInvoke("unload_llm_model")),
  deleteLlmModel: () => envelope(mockInvoke("delete_llm_model")),
  // ── ai/ollama_models.rs ─────────────────────────────────────────
  listOllamaModels: (baseUrl, apiKey) =>
    envelope(mockInvoke("list_ollama_models", { baseUrl, apiKey })),
  // ── ai/connection_test.rs ───────────────────────────────────────
  testGeminiConnection: (apiKey) =>
    envelope(mockInvoke("test_gemini_connection", { apiKey })),
  testOpenrouterConnection: (apiKey) =>
    envelope(mockInvoke("test_openrouter_connection", { apiKey })),
  testOllamaConnection: (baseUrl, apiKey) =>
    envelope(mockInvoke("test_ollama_connection", { baseUrl, apiKey })),
  testOpenaiConnection: (apiKey) =>
    envelope(mockInvoke("test_openai_connection", { apiKey })),
  testAnthropicConnection: (apiKey) =>
    envelope(mockInvoke("test_anthropic_connection", { apiKey })),
  // ── ai/subagents.rs + ai/skills.rs ──────────────────────────────
  subagentList: () => mockInvoke("subagent_list"),
  subagentRefresh: (roostPath) =>
    mockInvoke("subagent_refresh", { roostPath }),
  skillList: () => mockInvoke("skill_list"),
  skillRefresh: (roostPath) =>
    mockInvoke("skill_refresh", { roostPath }),
  skillCreate: (roostPath, name) =>
    envelope(mockInvoke("skill_create", { roostPath, name })),
  // ── ai/settings.rs ──────────────────────────────────────────────
  getSettings: () => envelope(mockInvoke("get_settings")),
  saveSettings: (settings) =>
    envelope(mockInvoke("save_settings", { settings })),
  listAgentProfiles: () => envelope(mockInvoke("list_agent_profiles")),
  getDeviceProfile: () => envelope(mockInvoke("get_device_profile")),
  decideTool: (requestId, decision) =>
    envelope(mockInvoke("decide_tool", { requestId, decision })),
  forgetPendingApproval: (requestId) =>
    envelope(mockInvoke("forget_pending_approval", { requestId })),
  listApprovalLog: (limit) =>
    envelope(mockInvoke("list_approval_log", { limit })),
  toolReliabilityStats: (windowDays) =>
    envelope(mockInvoke("tool_reliability_stats", { windowDays })),
  getAiLearningSummary: (topN) =>
    envelope(mockInvoke("get_ai_learning_summary", { topN })),
  resetAiLearning: () => envelope(mockInvoke("reset_ai_learning")),
  agentsMdStatus: () => envelope(mockInvoke("agents_md_status")),
  createAgentsMd: () => envelope(mockInvoke("create_agents_md")),
  // ── ai/chat.rs ──────────────────────────────────────────────────
  chatSend: (args) => envelope(mockInvoke("chat_send", { args })),
  chatStop: () => envelope(mockInvoke("chat_stop")),
  // ── sync.rs ─────────────────────────────────────────────────────
  syncInit: (roostPath) => envelope(mockInvoke("sync_init", { roostPath })),
  syncTryAutostart: (roostPath) =>
    envelope(mockInvoke("sync_try_autostart", { roostPath })),
  syncStartListening: (port) =>
    envelope(mockInvoke("sync_start_listening", { port })),
  syncListenIroh: (rotate) => envelope(mockInvoke("sync_listen_iroh", { rotate })),
  syncConnect: (addr, pin) =>
    envelope(mockInvoke("sync_connect", { addr, pin })),
  syncConnectIroh: (nodeId, pin) =>
    envelope(mockInvoke("sync_connect_iroh", { nodeId, pin })),
  syncStatus: () => envelope(mockInvoke("sync_status")),
  syncUnpair: (deviceId) =>
    envelope(mockInvoke("sync_unpair", { deviceId })),
  syncRescanRoost: () => envelope(mockInvoke("sync_rescan_roost")),
  syncResetDocstore: () => envelope(mockInvoke("sync_reset_docstore")),
  syncReconnectAll: () => envelope(mockInvoke("sync_reconnect_all")),
  syncStop: () => envelope(mockInvoke("sync_stop")),
  syncGetQueueSettings: () => envelope(mockInvoke("sync_get_queue_settings")),
  syncSetQueueEndpoint: (endpoint) =>
    envelope(mockInvoke("sync_set_queue_endpoint", { endpoint })),
  syncPinCustomQueueHost: (host, hostConfirm) =>
    envelope(mockInvoke("sync_pin_custom_queue_host", { host, hostConfirm })),
  syncSetStrictP2pOnly: (enabled) =>
    envelope(mockInvoke("sync_set_strict_p2p_only", { enabled })),
  syncPullQueueNow: () => envelope(mockInvoke("sync_pull_queue_now")),
  syncOutboxStatus: () => envelope(mockInvoke("sync_outbox_status")),
  syncDrainOutboxNow: () => envelope(mockInvoke("sync_drain_outbox_now")),
};
