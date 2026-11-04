import { createSignal, createMemo, createRoot } from "solid-js";
import { createStore, produce } from "solid-js/store";
import { commands, events, isTauri, unwrap } from "../lib/invoke";
import { log } from "../lib/log";
import { classifyChatError } from "../lib/chat-error";

export interface ChatMessage {
  role: "user" | "model";
  content: string;
  /** Notes the agent retrieved from the roost to ground this answer.
   *  Populated from `roost_search` / `roost_read` / `note_outline` tool
   *  results that fired during the model's turn. Rendered as clickable
   *  chips under the answer so the user can verify the source rather
   *  than trusting the model's prose. Always set on completed model
   *  turns; absent on user turns. */
  citations?: Citation[];
  /** True for tool-result and cancellation chips ("✓ ...", "✗ Tool
   *  error: ...", "↩ Cancelled"). They share `role: "model"` for
   *  rendering simplicity, but they MUST NOT receive subsequent AI
   *  stream chunks — `appendChunk` checks this flag so the agent's
   *  next prose starts a fresh bubble instead of being concatenated
   *  onto the status row. */
  isStatus?: boolean;
}

export interface Citation {
  /** Roost-relative or absolute path to the source note. */
  path: string;
  /** Human-readable title — falls back to the file stem when the tool
   *  result didn't carry an explicit title. */
  title: string;
  /** Which tool produced this citation. Useful for sorting (`roost_read`
   *  > `roost_search` > `note_outline`) and for badge color. */
  source: "roost_search" | "roost_read" | "note_outline";
}

/** Result of an AI tool call (note creation, etc.) emitted as `ai:tool-result`. */
export interface ToolResult {
  tool: string;
  success: boolean;
  message: string;
  file_path?: string;
  /** Structured tool payload (the JSON the LLM also receives). For
   *  read-tier tools like `roost_search` we mine this to build citation
   *  chips that anchor the model's answer in real notes. Shape varies
   *  per tool — consumers should access it as `unknown` and narrow. */
  data?: unknown;
}

/** A tool call paused awaiting user approval, emitted as `ai:tool-pending`.
 * The frontend renders one `ToolCard` per entry; calling `decide_tool` with
 * the matching `request_id` resolves it. The backend also fires
 * `ai:tool-decided` so the card can collapse once any decision lands
 * (including timeouts where the user never clicked). */
export interface PendingTool {
  request_id: string;
  tool: string;
  tier: "read" | "write" | "destructive";
  args: Record<string, unknown>;
  summary: string;
  file_path?: string;
  /** For write tools (`edit_note`, `roost_write`), the agent loop
   *  pre-computes a unified diff so the user sees what changes before
   *  approving. Absent for tools without a single before/after target. */
  preview?: DiffPreview;
  /** Secret-shaped substrings the backend scanner found in `args`.
   *  Rendered as a red warning banner above the diff so the user
   *  catches injection-driven credential exfiltration before
   *  clicking Approve. Empty array when nothing was flagged.
   *  See `ai/tools/secret_scanner.rs`. */
  warnings: SecretWarning[];
}

export type SecretKind =
  | "aws_access_key_id"
  | "jwt_like"
  | "base64_blob"
  | "url_credentials"
  | "gemini_api_key"
  | "long_hex";

export interface SecretWarning {
  kind: SecretKind;
  /** Dotted path into the args JSON (e.g. `args.body[0].text`).
   *  Tells the user WHERE in the call the suspicious string sits. */
  field: string;
  /** Masked preview of the matched substring (`AKIA…CDEF` style)
   *  so the user can identify the string without it leaking
   *  in plaintext through screenshots or logs. */
  preview: string;
}

export interface DiffPreview {
  kind: "edit" | "create";
  path: string;
  unified_diff: string;
  lines_added: number;
  lines_removed: number;
}

export type ToolDecision = "approve" | "reject" | "always_approve";

/** Metadata for a single chat session. The actual messages live in a
 *  separate per-session file so the index stays small. */
export interface ChatSession {
  id: string;
  title: string;
  created_at: number;
  last_active_at: number;
  message_count: number;
  /** Pinned chats float to the top, ahead of all time-bucketed groups. */
  pinned?: boolean;
  /** First user message of the session, truncated to ~140 chars. Used as
   *  a one-line preview under the title so the user can recognize old
   *  chats without opening them. */
  preview?: string;
  /** Roost-relative path of the note that was open when the session
   *  was created. Lets the UI surface "grounded on [[X]]" context. */
  context_note_path?: string;
}

type ChatStatus = "idle" | "streaming" | "error";

function createChatStore() {
  // AUDIT-036: messages are backed by a `createStore` so streaming
  // chunk-append can mutate the last entry in place via `produce`
  // instead of doing `[...prev]` + new message object per chunk. A
  // 5KB stream of 1-byte chunks used to cost ~5000 array clones +
  // 5000 message-object spreads; with createStore + produce the
  // `<For>` keying stays stable and DOMPurify only sees the latest
  // tail. The `messages()` / `setMessages` accessors keep the
  // existing signature so consumers don't change.
  const [messagesStore, setMessagesStore] = createStore<{ list: ChatMessage[] }>({
    list: [],
  });
  const messages = (): ChatMessage[] => messagesStore.list;
  function setMessages(
    update: ChatMessage[] | ((prev: ChatMessage[]) => ChatMessage[]),
  ) {
    if (typeof update === "function") {
      const next = update(messagesStore.list);
      setMessagesStore("list", next);
    } else {
      setMessagesStore("list", update);
    }
  }
  const [status, setStatus] = createSignal<ChatStatus>("idle");
  const [error, setError] = createSignal<string | null>(null);
  // Friendly, classified view of the raw backend error string. The chat
  // panel renders this (rate-limit → upgrade CTA) instead of the raw blob.
  const errorInfo = createMemo(() => {
    const raw = error();
    return raw ? classifyChatError(raw) : null;
  });
  const [pendingTools, setPendingTools] = createSignal<PendingTool[]>([]);
  const [sessions, setSessions] = createSignal<ChatSession[]>([]);
  const [activeSessionId, setActiveSessionId] = createSignal<string | null>(null);
  // Single-line ticker phrase shown while the agent is running. Updates
  // per tool result so the user sees what the agent is *currently* doing
  // instead of a generic "Thinking…". Reset on send / done / error.
  const [currentActivity, setCurrentActivity] = createSignal<string>("Thinking…");

  let unlistenChunk: (() => void) | null = null;
  let unlistenDone: (() => void) | null = null;
  let unlistenError: (() => void) | null = null;
  let unlistenToolResult: (() => void) | null = null;
  let unlistenToolPending: (() => void) | null = null;
  let unlistenToolDecided: (() => void) | null = null;
  let currentRoostPath: string | null = null;
  // Citations accumulated for the in-flight model turn. Reset on
  // sendMessage, sealed onto the trailing model message in handleDone.
  // Keyed by `${source}:${path}` so the same note pulled by both
  // roost_search and roost_read only shows up once.
  let pendingCitations: Map<string, Citation> = new Map();

  // Listeners fired when AI creates a note (for UI toasts + auto-open)
  const toolResultHandlers = new Set<(result: ToolResult) => void>();

  function onToolResult(handler: (result: ToolResult) => void): () => void {
    toolResultHandlers.add(handler);
    return () => toolResultHandlers.delete(handler);
  }

  function appendChunk(text: string) {
    // AUDIT-036: mutate in place via `produce`. The reactive store
    // only re-emits for the entry that actually changed; the outer
    // array identity stays stable so `<For>` keying doesn't churn.
    setMessagesStore(
      "list",
      produce((arr) => {
        const last = arr[arr.length - 1];
        if (last && last.role === "model" && !last.isStatus) {
          // Normal streaming case — keep filling the active model bubble.
          last.content = last.content + text;
        } else {
          // Last message is the user's prompt or a tool-result/cancellation
          // chip. Either way, start a fresh model bubble so the agent's
          // next prose doesn't get glued onto a status row (or onto the
          // user's own message).
          arr.push({ role: "model", content: text });
        }
      }),
    );
  }

  /// Map a tool name + (optional) result to a short user-facing verb
  /// phrase for the live ticker. Falls back to "Working with N…" when
  /// we don't have a friendlier label. Phrasing stays present-continuous
  /// while running ("Reading…") and shifts to a count once the result
  /// lands ("Read 5 notes").
  function tickerForTool(tool: string, result?: ToolResult): string {
    const data = (result?.data ?? {}) as Record<string, unknown>;
    const count = (key: string): number | undefined => {
      const v = data[key];
      return typeof v === "number" ? v : undefined;
    };
    switch (tool) {
      case "roost_search": {
        const hits = Array.isArray(data.results) ? data.results.length : undefined;
        return result ? `Searched the roost${hits != null ? ` · ${hits} hits` : ""}` : "Searching the roost…";
      }
      case "roost_read": {
        const n = Array.isArray(data.notes) ? data.notes.length : count("count");
        return result ? `Read ${n ?? "notes"}${n === 1 ? " note" : n != null ? " notes" : ""}` : "Reading notes…";
      }
      case "roost_write":
      case "edit_note":
        return result ? "Edited a note" : "Editing a note…";
      case "create_task":
      case "update_task":
      case "complete_task":
      case "delete_task":
      case "reschedule_task":
      case "archive_task":
        return result ? "Updated tasks" : "Updating tasks…";
      case "query_tasks":
        return result ? "Searched tasks" : "Searching tasks…";
      case "note_outline":
        return result ? "Read outline" : "Reading outline…";
      case "backlinks":
        return result ? "Checked backlinks" : "Checking backlinks…";
      case "graph_query":
        return result ? "Queried the graph" : "Querying the graph…";
      case "remember":
      case "forget":
        return result ? "Updated memory" : "Updating memory…";
      default:
        return result ? `Used ${tool}` : `Running ${tool}…`;
    }
  }

  function handleDone() {
    setStatus("idle");
    setCurrentActivity("Thinking…");
    // Seal accumulated citations onto the trailing model message. We
    // walk back from the end because tool-result rows (rendered as
    // synthetic model messages with "✓ ..." content) sit between the
    // streamed answer and the next user turn — the citations belong on
    // the *real* streamed answer, not the status messages.
    if (pendingCitations.size > 0) {
      const cites = Array.from(pendingCitations.values());
      setMessages((prev) => {
        const updated = [...prev];
        for (let i = updated.length - 1; i >= 0; i--) {
          const m = updated[i];
          if (m.role !== "model") continue;
          if (m.content.startsWith("✓ ") || m.content.startsWith("✗ ")) continue;
          updated[i] = { ...m, citations: cites };
          break;
        }
        return updated;
      });
      pendingCitations = new Map();
    }
    // Auto-save the active session's messages after streaming completes.
    if (currentRoostPath) saveActiveSession();
  }

  /// Pull `{ path, title }` rows out of a read-tier tool result so they
  /// can render as citation chips under the answer. Tolerant to absent
  /// fields — the goal is to surface what we can, not block on shape
  /// drift between tool versions.
  function harvestCitations(result: ToolResult) {
    if (!result.success || !result.data || typeof result.data !== "object") return;
    const data = result.data as Record<string, unknown>;

    const push = (
      source: Citation["source"],
      path: string | undefined,
      title: string | undefined,
    ) => {
      if (!path) return;
      const key = `${source}:${path}`;
      if (pendingCitations.has(key)) return;
      const fallback = path.split(/[\\/]/).pop()?.replace(/\.md$/, "") ?? path;
      pendingCitations.set(key, {
        path,
        title: title && title.trim() ? title : fallback,
        source,
      });
    };

    if (result.tool === "roost_search") {
      const rows = Array.isArray(data.results) ? data.results : [];
      for (const r of rows as Record<string, unknown>[]) {
        push("roost_search", r.path as string | undefined, r.title as string | undefined);
      }
    } else if (result.tool === "roost_read") {
      const notes = Array.isArray(data.notes) ? data.notes : [];
      for (const n of notes as Record<string, unknown>[]) {
        push(
          "roost_read",
          n.path as string | undefined,
          (n.title as string | undefined) ?? (n.input as string | undefined),
        );
      }
    } else if (result.tool === "note_outline") {
      push(
        "note_outline",
        data.path as string | undefined,
        data.title as string | undefined,
      );
    }
  }

  function handleError(msg: string) {
    setStatus("error");
    setError(msg);
    setCurrentActivity("Thinking…");
    setMessages((prev) => {
      const updated = [...prev];
      const last = updated[updated.length - 1];
      if (last && last.role === "model" && last.content === "") {
        updated.pop();
      }
      return updated;
    });
  }

  async function setupListeners() {
    // AUDIT-008: tear down ALL listeners (not just chunk/done/error) before
    // re-binding. Missing the three tool listeners caused duplicate
    // tool-result handling under HMR or any second-call path.
    cleanupListeners();

    if (isTauri) {
      unlistenChunk = await events.aiChunk.listen((event) => appendChunk(event.payload));
      unlistenDone = await events.aiDone.listen(() => handleDone());
      unlistenError = await events.aiError.listen((event) => handleError(event.payload));
      unlistenToolResult = await events.aiToolResult.listen((event) => {
        // Same `#[specta(skip)]` workaround as `aiToolPending` above —
        // the generated binding type omits `data` (serde_json::Value
        // would trip BigInt forbid). Wire still delivers it; the
        // hand-written `ToolResult` interface owns the `data?: unknown`
        // shape used by citation harvesting.
        const result = event.payload as unknown as ToolResult;
        // Mine the result for citations BEFORE we render the status row,
        // so handleDone has them ready to seal onto the model answer.
        harvestCitations(result);
        // Ticker: replace the in-flight phrase with the past-tense
        // result phrase so the user briefly sees "Read 5 notes" before
        // the next tool's "Reading…" lands. Errors switch to the error
        // verb so a long red ✕ doesn't masquerade as forward progress.
        setCurrentActivity(
          result.success ? tickerForTool(result.tool, result) : `Hit a snag with ${result.tool}`,
        );
        // Append a system-style message to the chat so the user sees what
        // happened. User rejections get a soft, one-line acknowledgement —
        // the verbose backend message ("User rejected X. Don't retry the
        // same call — adjust args or move on.") is LLM-facing guidance and
        // reads as broken UX when shown verbatim to the user, who already
        // knows they just clicked Reject.
        let content: string;
        if (result.message.startsWith("User rejected ")) {
          content = "↩ Cancelled";
        } else if (result.success) {
          content = `✓ ${result.message}`;
        } else {
          content = `✗ Tool error: ${result.message}`;
        }
        setMessages((prev) => [
          ...prev,
          { role: "model", content, isStatus: true },
        ]);
        // Notify any UI handlers (e.g. toast, auto-open note)
        toolResultHandlers.forEach((h) => h(result));
        // Re-save the active session to include the confirmation
        if (currentRoostPath) saveActiveSession();
      });

      // Approval-gate events. Backend fires `ai:tool-pending` when a Write
      // or Destructive tool is awaiting approval, then `ai:tool-decided`
      // once the user (or a timeout) resolves it. The card UI listens
      // through the `pendingTools()` signal.
      unlistenToolPending = await events.aiToolPending.listen((event) => {
        // Backend types the args field as `serde_json::Value` and
        // `#[specta(skip)]`s it (recurses into BigInt). The wire still
        // delivers args; the hand-written `PendingTool` interface owns
        // the `args: Record<string, unknown>` shape. Cast at the
        // boundary — same pattern as `ChatMessage.tool_calls`. See
        // `docs/decisions/toolcall-specta-skip.md`.
        const pending = event.payload as unknown as PendingTool;
        setPendingTools((prev) => [...prev, pending]);
        // Ticker flips to the in-flight verb the moment a tool is
        // dispatched (write/destructive tools sit here pending approval;
        // read tools blow past this listener so the result handler
        // overwrites it almost immediately).
        setCurrentActivity(tickerForTool(pending.tool));
      });
      unlistenToolDecided = await events.aiToolDecided.listen((event) => {
        setPendingTools((prev) =>
          prev.filter((t) => t.request_id !== event.payload.request_id),
        );
      });
    } else {
      // Mock event listeners for browser dev
      const onChunk = (e: Event) => appendChunk((e as CustomEvent).detail);
      const onDone = () => handleDone();
      const onError = (e: Event) => handleError((e as CustomEvent).detail);

      window.addEventListener("mock:ai:chunk", onChunk);
      window.addEventListener("mock:ai:done", onDone);
      window.addEventListener("mock:ai:error", onError);

      unlistenChunk = () => window.removeEventListener("mock:ai:chunk", onChunk);
      unlistenDone = () => window.removeEventListener("mock:ai:done", onDone);
      unlistenError = () => window.removeEventListener("mock:ai:error", onError);
    }
  }

  async function sendMessage(text: string, noteContent?: string) {
    if (!text.trim()) return;
    setError(null);

    // First message into a "New chat" auto-renames the session and
    // captures a preview line. Both make the sessions panel readable
    // without opening every chat.
    const aid = activeSessionId();
    if (aid) {
      const sess = sessions().find((s) => s.id === aid);
      const isFirstMessage = sess && sess.message_count === 0 && !sess.preview;
      if (isFirstMessage) {
        const previewLine = truncatePreview(text);
        const provisionalTitle =
          sess.title === "New chat" || sess.title === ""
            ? truncateTitle(text)
            : sess.title;
        setSessions((prev) =>
          prev.map((s) =>
            s.id === aid
              ? { ...s, title: provisionalTitle, preview: previewLine }
              : s
          )
        );
      }
    }

    // Add user message
    const userMsg: ChatMessage = { role: "user", content: text.trim() };

    // Reset citation accumulator for this turn — chips from the prior
    // answer should not bleed into the next one.
    pendingCitations = new Map();

    // Capture history BEFORE adding the empty placeholder (so we don't send empty model turn)
    const history = [...messages(), userMsg].map((m) => ({
      role: m.role,
      content: m.content,
    }));

    setMessages((prev) => [...prev, userMsg]);
    // Add empty assistant message (will be filled by streaming)
    setMessages((prev) => [...prev, { role: "model", content: "" }]);
    setCurrentActivity("Thinking…");
    setStatus("streaming");

    try {
      const r = await commands.chatSend({
        messages: history,
        context: noteContent ?? null,
        roost_path: currentRoostPath,
      });
      if (r.status === "error") throw r.error;
    } catch (e: unknown) {
      const errMsg = typeof e === "string" ? e : "Failed to send message";
      setStatus("error");
      setError(errMsg);
      // Remove empty assistant message
      setMessages((prev) => {
        const updated = [...prev];
        const last = updated[updated.length - 1];
        if (last && last.role === "model" && last.content === "") {
          updated.pop();
        }
        return updated;
      });
    }
  }

  async function stopGeneration() {
    try {
      await unwrap(commands.chatStop());
    } catch (e) {
      log.error("chat", "failed to stop generation", e);
    }
    setStatus("idle");
  }

  /** Clear the messages of the active session WITHOUT deleting the session.
   *  Use deleteSession() if you want the session gone entirely. */
  function clearMessages(roostPath?: string) {
    setMessages([]);
    setStatus("idle");
    setError(null);
    if (roostPath) saveActiveSession();
  }

  // ── Session management ─────────────────────────────────────────────

  function truncateTitle(s: string): string {
    const trimmed = s.trim().split("\n")[0];
    return trimmed.length > 50 ? trimmed.slice(0, 50) + "…" : trimmed;
  }

  function truncatePreview(s: string): string {
    const flat = s.replace(/\s+/g, " ").trim();
    return flat.length > 140 ? flat.slice(0, 140) + "…" : flat;
  }

  function newSessionId(): string {
    // crypto.randomUUID() is available in Tauri's WebView2 (Edge). Falls
    // back to a Math.random hex for the unlikely browser-test path.
    if (typeof crypto !== "undefined" && "randomUUID" in crypto) {
      return crypto.randomUUID();
    }
    return `s-${Date.now().toString(16)}-${Math.random().toString(16).slice(2, 10)}`;
  }

  function sessionFile(roost: string, id: string): string {
    return `${roost}/.corvux/chat-sessions/${id}.json`;
  }

  function indexFile(roost: string): string {
    return `${roost}/.corvux/chat-sessions/index.json`;
  }

  function activeFile(roost: string): string {
    return `${roost}/.corvux/active-session.json`;
  }

  let chatLoadGeneration = 0;

  async function persistIndex(
    roostPath = currentRoostPath,
    value = sessions(),
  ) {
    if (!roostPath) return;
    try {
      await unwrap(
        commands.writeFile(indexFile(roostPath), JSON.stringify(value, null, 2)),
      );
    } catch (e) {
      log.error("chat", "failed to persist sessions index", e);
    }
  }

  async function persistActiveId(
    roostPath = currentRoostPath,
    id = activeSessionId(),
  ) {
    if (!roostPath) return;
    if (!id) return;
    try {
      await unwrap(
        commands.writeFile(activeFile(roostPath), JSON.stringify({ id }, null, 2)),
      );
    } catch (e) {
      log.error("chat", "failed to persist active session id", e);
    }
  }

  /** Save the active session's messages + bumped metadata. Called on
   *  every stream completion + tool result so the disk always matches
   *  what the user can see in the UI. */
  async function saveActiveSession() {
    const id = activeSessionId();
    if (!id || !currentRoostPath) return;
    const roostPath = currentRoostPath;
    const generation = chatLoadGeneration;
    const msgs = messages();
    setSessions((prev) =>
      prev.map((s) =>
        s.id === id
          ? {
              ...s,
              message_count: msgs.length,
              last_active_at: Date.now(),
            }
          : s
      )
    );
    try {
      await unwrap(
        commands.writeFile(sessionFile(roostPath, id), JSON.stringify(msgs, null, 2)),
      );
    } catch (e) {
      log.error("chat", "failed to persist session messages", e);
    }
    if (generation !== chatLoadGeneration || currentRoostPath !== roostPath) return;
    await persistIndex(roostPath);
  }

  async function loadSessionMessages(
    id: string,
    roostPath = currentRoostPath,
  ): Promise<ChatMessage[]> {
    if (!roostPath) return [];
    try {
      const data = await unwrap(commands.readFile(sessionFile(roostPath, id)));
      const parsed = JSON.parse(data) as ChatMessage[];
      return Array.isArray(parsed) ? parsed : [];
    } catch {
      return [];
    }
  }

  /** Switch to a different session. Saves the current session first so
   *  no streaming-mid-state is lost. */
  async function switchSession(id: string) {
    if (!currentRoostPath) return;
    const generation = chatLoadGeneration;
    const roostPath = currentRoostPath;
    if (activeSessionId() === id) return;
    const outgoingId = activeSessionId();
    if (outgoingId) {
      await saveActiveSession();
    }
    if (generation !== chatLoadGeneration || currentRoostPath !== roostPath) return;
    setActiveSessionId(id);
    setError(null);
    setStatus("idle");
    const msgs = await loadSessionMessages(id, roostPath);
    if (generation !== chatLoadGeneration || currentRoostPath !== roostPath) return;
    setMessages(msgs);
    await persistActiveId(roostPath, id);
  }

  /** Create a new blank session and make it active. Optionally captures
   *  the currently-open note path so the sessions panel can show
   *  "grounded on [[X]]" later. If the currently-active session is
   *  already an unused "New chat" (no messages, default title), reuse
   *  it instead of stacking another empty entry on the sessions list. */
  async function newSession(contextNotePath?: string): Promise<string> {
    if (!currentRoostPath) return "";
    const roostPath = currentRoostPath;
    const generation = chatLoadGeneration;

    // Reuse-empty: clicking + with no messages yet should be a no-op so
    // users can't accidentally pile up dozens of empty "New chat" rows.
    const currentId = activeSessionId();
    if (currentId) {
      const current = sessions().find((s) => s.id === currentId);
      const isReusable =
        current &&
        current.message_count === 0 &&
        messages().length === 0 &&
        (current.title === "New chat" || current.title === "");
      if (isReusable) {
        // If the caller passed a note path, capture it on the existing
        // session so the "grounded on [[X]]" badge updates.
        if (contextNotePath && current.context_note_path !== contextNotePath) {
          setSessions((prev) =>
            prev.map((s) =>
              s.id === currentId ? { ...s, context_note_path: contextNotePath } : s,
            ),
          );
          await persistIndex(roostPath);
        }
        return currentId;
      }
    }

    // Save the previous session before switching off it.
    const outgoingId = currentId;
    if (outgoingId) {
      await saveActiveSession();
    }
    if (generation !== chatLoadGeneration || currentRoostPath !== roostPath) return "";
    const id = newSessionId();
    const now = Date.now();
    const session: ChatSession = {
      id,
      title: "New chat",
      created_at: now,
      last_active_at: now,
      message_count: 0,
      context_note_path: contextNotePath,
    };
    setSessions((prev) => [session, ...prev]);
    setActiveSessionId(id);
    setMessages([]);
    setError(null);
    setStatus("idle");
    await persistIndex(roostPath);
    await persistActiveId(roostPath, id);
    // Write empty messages file so subsequent reads succeed.
    try {
      await unwrap(commands.writeFile(sessionFile(roostPath, id), "[]"));
    } catch (e) {
      log.error("chat", "failed to seed session file", e);
    }
    return id;
  }

  async function deleteSession(id: string) {
    if (!currentRoostPath) return;
    const roostPath = currentRoostPath;
    const generation = chatLoadGeneration;
    const wasActive = activeSessionId() === id;
    // Drop the active marker BEFORE switchSession can run — otherwise
    // saveActiveSession() inside switchSession would write the deleted
    // session's messages file back to disk, racing the delete_file below.
    if (wasActive) {
      setActiveSessionId(null);
      setMessages([]);
    }
    setSessions((prev) => prev.filter((s) => s.id !== id));
    try {
      await unwrap(commands.deleteFile(sessionFile(roostPath, id), roostPath));
    } catch (e) {
      // File may not exist — that's fine.
      log.warn("chat", "delete_file warning", e);
    }
    if (generation !== chatLoadGeneration || currentRoostPath !== roostPath) return;
    await persistIndex(roostPath);
    if (generation !== chatLoadGeneration || currentRoostPath !== roostPath) return;
    if (wasActive) {
      const remaining = sessions();
      if (remaining.length > 0) {
        await switchSession(remaining[0].id);
      } else {
        await newSession();
      }
    }
  }

  async function renameSession(id: string, title: string) {
    const clean = title.trim();
    if (!clean) return;
    setSessions((prev) =>
      prev.map((s) => (s.id === id ? { ...s, title: clean } : s))
    );
    await persistIndex();
  }

  async function togglePin(id: string) {
    setSessions((prev) =>
      prev.map((s) =>
        s.id === id ? { ...s, pinned: !s.pinned } : s
      )
    );
    await persistIndex();
  }

  /** Promote a chat to a markdown note in the roost. The note becomes a
   *  first-class citizen of the knowledge graph — backlinks, search,
   *  PageRank all see it. The chat session itself stays in the sidebar
   *  unless the user deletes it.
   *
   *  Output path: `{roost}/Chats/{YYYY-MM-DD}-{title-slug}.md`. The slug
   *  strips OS-unsafe chars from the title. Frontmatter records the
   *  source session id so a future "open original chat" link can
   *  re-find it. */
  async function saveSessionAsNote(id: string): Promise<string | null> {
    if (!currentRoostPath) return null;
    const roostPath = currentRoostPath;
    const generation = chatLoadGeneration;
    const session = sessions().find((s) => s.id === id);
    if (!session) return null;
    const msgs = session.id === activeSessionId()
      ? messages()
      : await loadSessionMessages(id, roostPath);
    if (generation !== chatLoadGeneration || currentRoostPath !== roostPath) return null;
    if (msgs.length === 0) return null;

    const date = new Date(session.created_at);
    const yyyy = date.getFullYear();
    const mm = String(date.getMonth() + 1).padStart(2, "0");
    const dd = String(date.getDate()).padStart(2, "0");
    const slug = session.title
      .toLowerCase()
      .replace(/[^a-z0-9\s-]/g, "")
      .trim()
      .replace(/\s+/g, "-")
      .slice(0, 60) || "chat";

    const notePath = `${roostPath}/Chats/${yyyy}-${mm}-${dd}-${slug}.md`;

    let body = "---\n";
    body += `title: ${session.title}\n`;
    body += `created: ${yyyy}-${mm}-${dd}\n`;
    body += `source: corvux-chat\n`;
    body += `chat_session_id: ${session.id}\n`;
    if (session.context_note_path) {
      body += `grounded_on: "${session.context_note_path}"\n`;
    }
    body += `tags:\n  - chat\n  - ai-conversation\n`;
    body += "---\n\n";
    body += `# ${session.title}\n\n`;
    if (session.preview) {
      body += `> ${session.preview}\n\n`;
    }

    for (const m of msgs) {
      const speaker = m.role === "user" ? "**You**" : "**Corvux AI**";
      // Skip empty streaming placeholders.
      if (m.content.trim().length === 0) continue;
      body += `### ${speaker}\n\n${m.content.trim()}\n\n`;
    }

    try {
      await unwrap(commands.writeFile(notePath, body));
      return notePath;
    } catch (e) {
      log.error("chat", "saveSessionAsNote failed", e);
      return null;
    }
  }

  /** Load all sessions for a roost and restore the previously-active one.
   *  Migrates the legacy single-history file (`chat-history.json`) into a
   *  session on first run, so users with existing chats don't lose them. */
  async function loadSessions(roostPath: string) {
    const generation = ++chatLoadGeneration;
    currentRoostPath = roostPath;
    const superseded = () =>
      generation !== chatLoadGeneration || currentRoostPath !== roostPath;

    // Try to read the sessions index.
    let loaded: ChatSession[] = [];
    try {
      const data = await unwrap(commands.readFile(indexFile(roostPath)));
      const parsed = JSON.parse(data) as ChatSession[];
      if (Array.isArray(parsed)) loaded = parsed;
    } catch {
      // No sessions index yet — fresh roost or pre-migration.
    }
    if (superseded()) return;

    // Legacy migration: if no sessions and the old single-history file
    // exists, fold it into a "Migrated chat" session so messages survive.
    if (loaded.length === 0) {
      try {
        const legacy = await unwrap(
          commands.readFile(`${roostPath}/.corvux/chat-history.json`),
        );
        const oldMessages = JSON.parse(legacy) as ChatMessage[];
        if (Array.isArray(oldMessages) && oldMessages.length > 0) {
          const id = newSessionId();
          const now = Date.now();
          const migrated: ChatSession = {
            id,
            title: "Migrated chat",
            created_at: now,
            last_active_at: now,
            message_count: oldMessages.length,
          };
          loaded = [migrated];
          await unwrap(
            commands.writeFile(sessionFile(roostPath, id), JSON.stringify(oldMessages, null, 2)),
          );
        }
      } catch {
        // No legacy file — nothing to migrate.
      }
      if (superseded()) return;
    }

    // Resolve the active-session id BEFORE pruning so we can keep an
    // active-but-empty session around (user might be mid-compose).
    let toActivate: string | null = null;
    try {
      const data = await unwrap(commands.readFile(activeFile(roostPath)));
      const { id } = JSON.parse(data) as { id: string };
      if (typeof id === "string" && loaded.some((s) => s.id === id)) {
        toActivate = id;
      }
    } catch {
      // No active-session marker.
    }
    if (superseded()) return;

    // Prune the litter — empty "New chat" sessions that were created by
    // pre-fix "+" clicks. Keeps only sessions with real content. The
    // currently-active session is preserved even if empty.
    const pruned: ChatSession[] = [];
    const droppedIds: string[] = [];
    for (const s of loaded) {
      const isEmpty =
        s.message_count === 0 && (s.title === "New chat" || s.title === "");
      if (isEmpty && s.id !== toActivate) {
        droppedIds.push(s.id);
      } else {
        pruned.push(s);
      }
    }
    if (droppedIds.length > 0) {
      // Best-effort delete the messages files for the dropped sessions.
      // Don't block on errors — even if a file lingers, the index won't
      // reference it.
      await Promise.allSettled(
        droppedIds.map((id) =>
          commands.deleteFile(sessionFile(roostPath, id), roostPath),
        ),
      );
      if (superseded()) return;
    }
    loaded = pruned;

    if (superseded()) return;
    setSessions(loaded);
    if (loaded.length > 0 || droppedIds.length > 0) {
      // Persist (in case we just migrated OR pruned).
      await persistIndex(roostPath, loaded);
      if (superseded()) return;
    }

    if (!toActivate && loaded.length > 0) {
      const sorted = [...loaded].sort((a, b) => b.last_active_at - a.last_active_at);
      toActivate = sorted[0].id;
    }

    if (toActivate) {
      setActiveSessionId(toActivate);
      const msgs = await loadSessionMessages(toActivate, roostPath);
      if (superseded()) return;
      setMessages(msgs);
      await persistActiveId(roostPath, toActivate);
    } else {
      // No sessions yet — create the first one so the UI always has
      // something to render.
      await newSession();
    }
  }

  function resetForRoostSwitch() {
    // Invalidate the backend loop before clearing the UI. The backend also
    // generation-binds every tool, but cancellation prevents needless model
    // work and makes the switch immediate even before the next tool call.
    void commands.chatStop();
    chatLoadGeneration += 1;
    currentRoostPath = null;
    setSessions([]);
    setActiveSessionId(null);
    setMessages([]);
    setStatus("idle");
    setError(null);
  }

  /** Backwards-compat wrapper: callers that used the single-history API
   *  now go through the session manager. Eventually remove this. */
  async function loadChatHistory(roostPath: string) {
    await loadSessions(roostPath);
  }

  /** Backwards-compat wrapper. Saves the active session. */
  async function saveChatHistory(_roostPath: string) {
    await saveActiveSession();
  }

  function cleanupListeners() {
    unlistenChunk?.();
    unlistenDone?.();
    unlistenError?.();
    unlistenToolResult?.();
    unlistenToolPending?.();
    unlistenToolDecided?.();
    unlistenChunk = null;
    unlistenDone = null;
    unlistenError = null;
    unlistenToolResult = null;
    unlistenToolPending = null;
    unlistenToolDecided = null;
  }

  /// Send a decision back to the backend's approval gate. Optimistically
  /// removes the card from the local list — the backend's `ai:tool-decided`
  /// event will redundantly do the same when it lands. If the IPC fails
  /// (already-decided / timed-out), we leave the optimistic removal in
  /// place; the card's gone either way.
  async function decideTool(requestId: string, decision: ToolDecision): Promise<void> {
    setPendingTools((prev) => prev.filter((t) => t.request_id !== requestId));
    try {
      await unwrap(commands.decideTool(requestId, decision as "approve" | "always_approve" | "reject"));
    } catch (e) {
      log.warn("chat", "decide_tool failed", e);
    }
  }

  /// Drop every currently-pending approval on the backend. Fired by
  /// `ChatPanel` on unmount so an open card doesn't leave a
  /// `oneshot::Sender` sitting in `PendingApprovals` until app restart.
  /// The backend treats each drop as a Reject — retryable, the agent
  /// loop recovers cleanly. Idempotent on the backend so calling with
  /// stale ids is harmless.
  async function forgetPendingApprovals(): Promise<void> {
    const ids = pendingTools().map((t) => t.request_id);
    if (ids.length === 0) return;
    setPendingTools([]);
    await Promise.all(
      ids.map((requestId) =>
        commands.forgetPendingApproval(requestId).catch((e) =>
          log.warn("chat", "forget_pending_approval failed", e),
        ),
      ),
    );
  }

  return {
    messages,
    status,
    error,
    errorInfo,
    sessions,
    activeSessionId,
    pendingTools,
    currentActivity,
    setupListeners,
    cleanupListeners,
    sendMessage,
    stopGeneration,
    clearMessages,
    saveChatHistory,
    loadChatHistory,
    loadSessions,
    resetForRoostSwitch,
    newSession,
    switchSession,
    deleteSession,
    renameSession,
    togglePin,
    saveSessionAsNote,
    onToolResult,
    decideTool,
    forgetPendingApprovals,
  };
}

export const chatStore = createRoot(createChatStore);
