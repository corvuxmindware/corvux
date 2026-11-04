import { createSignal, createEffect, createMemo, For, Show, onMount, onCleanup, on, type JSX } from "solid-js";
import {
  chatStore,
  type ChatMessage,
  type ChatSession,
  type ToolResult,
} from "../../stores/chat";
import { settingsStore } from "../../stores/settings";
import { tabStore } from "../../stores/tabs";
import { graphStore } from "../../stores/graph";
import { roostStore } from "../../stores/roost";
import CorvuxLogo from "../Brand/CorvuxLogo";
import ToolCard from "./ToolCard";
import SessionRow from "./SessionRow";
import MessageBubble, { stripToolMarkup } from "./MessageBubble";
import { isMobile as isMobilePlatform, isMacOS } from "../../lib/platform";
import TouchIconButton from "../ui/TouchIconButton";
import { isTauri } from "../../lib/invoke";
import { log } from "../../lib/log";
import { WINDOW_CONTROLS_WIDTH } from "../WindowControls/WindowControls";

interface ChatPanelProps {
  noteContent?: string;
  roostPath?: string;
  onClose: () => void;
  /** Opens Settings — wired through to a "Set up your API key" CTA in
   *  the empty state and to the header cog icon. The inline-prompt
   *  flow was removed 2026-05-20: a tiny `Gemini API Key` text box
   *  above the message stream is dead UX, sends users into a flow
   *  with no provider context, and bypasses the proper Settings
   *  surface. The chat panel now refuses to send without a key and
   *  delegates the whole onboarding to Settings → AI Models. */
  onOpenSettings: () => void;
}

export default function ChatPanel(props: ChatPanelProps) {
  const [input, setInput] = createSignal("");
  const [showSessions, setShowSessions] = createSignal(false);
  // AUDIT-032: gate the input + send button until `setupListeners`
  // resolves. The prior code awaited it inside `onMount` but the user
  // could click an empty-state prompt or paste + press Enter before that
  // — `ai:chunk` events that fire before the listener attaches are
  // dropped, leaving the streaming bubble empty.
  const [ready, setReady] = createSignal(false);
  // Whether a real note is the active tab right now. Drives the
  // empty-state suggestions: with a note open the buttons say
  // "Summarize this note" etc.; without one we swap to roost-level
  // prompts ("What's in my roost?") so we don't tell the agent to
  // summarize a `this note` that doesn't exist — which made the LLM
  // pick a random roost note and lie about having been given content.
  const hasOpenNote = () => !!props.noteContent && props.noteContent.trim().length > 0;
  // Single-slot queue: when the user hits Enter during a streaming turn,
  // their message lands here instead of being dropped. A createEffect
  // below fires it the moment status flips back to "idle". Replace-on-new
  // (not multi-slot) — keeps the UI predictable; users who type two
  // follow-ups in a row almost always want the latest one, not both.
  const [queuedMessage, setQueuedMessage] = createSignal("");

  // Mobile keyboard-aware positioning. When the OS keyboard overlays the
  // WebView, `visualViewport` gives us the real visible rectangle. Pinning
  // the whole chat panel to that rectangle avoids the "composer jumps to
  // the middle with a dead zone below it" failure mode from shrinking the
  // panel by `visualViewport.height` inside an already-resized root.
  const [keyboardInset, setKeyboardInset] = createSignal(0);
  const [visualViewportTop, setVisualViewportTop] = createSignal(0);
  const isMobile = () => isMobilePlatform();
  const syncMobileViewport = () => {
    if (typeof window === "undefined" || !window.visualViewport) return;
    const vv = window.visualViewport;
    const layoutHeight = window.innerHeight;
    const overlap = layoutHeight - (vv.height + vv.offsetTop);
    setKeyboardInset(overlap > 24 ? Math.round(overlap) : 0);
    setVisualViewportTop(Math.max(0, Math.round(vv.offsetTop)));
  };
  const resetMobileDocumentScroll = () => {
    if (typeof window === "undefined" || typeof document === "undefined") return;
    window.scrollTo(0, 0);
    const scrollingElement = document.scrollingElement;
    if (scrollingElement) {
      scrollingElement.scrollTop = 0;
      scrollingElement.scrollLeft = 0;
    }
    document.documentElement.scrollTop = 0;
    document.body.scrollTop = 0;
  };
  const settleMobileInputViewport = () => {
    if (!isMobile()) return;
    const settle = () => {
      resetMobileDocumentScroll();
      syncMobileViewport();
    };
    requestAnimationFrame(settle);
    window.setTimeout(() => requestAnimationFrame(settle), 50);
    window.setTimeout(() => requestAnimationFrame(settle), 150);
    window.setTimeout(() => requestAnimationFrame(settle), 300);
  };
  onMount(() => {
    if (!isMobile() || typeof window === "undefined" || !window.visualViewport) return;
    const vv = window.visualViewport;
    let frame = 0;
    const sync = () => {
      cancelAnimationFrame(frame);
      frame = requestAnimationFrame(syncMobileViewport);
    };
    vv.addEventListener("resize", sync);
    vv.addEventListener("scroll", sync);
    window.addEventListener("orientationchange", sync);
    sync();
    onCleanup(() => {
      cancelAnimationFrame(frame);
      vv.removeEventListener("resize", sync);
      vv.removeEventListener("scroll", sync);
      window.removeEventListener("orientationchange", sync);
    });
  });

  const panelViewportStyle = createMemo<JSX.CSSProperties>(() => {
    if (!isMobile()) {
      return {
        height: "100%",
      };
    }

    return {
      position: "fixed",
      top: `calc(${visualViewportTop()}px + var(--corvux-safe-area-top))`,
      right: "var(--corvux-safe-area-right)",
      bottom:
        keyboardInset() > 0
          ? `${keyboardInset()}px`
          : "var(--corvux-safe-area-bottom)",
      left: "var(--corvux-safe-area-left)",
      height: "auto",
      "max-height": "none",
      transform: "none",
      "z-index": "60",
    };
  });

  // Tail-window virtualization for the message list. Chat messages are
  // variable-height (markdown, code blocks, tool results), which makes
  // scroll-position windowing fragile — pixel offsets shift as content
  // streams in. Instead, we render only the last `windowSize` messages
  // and surface a "Show earlier" pill at the top when older messages
  // are hidden. The user's reading attention is at the bottom of a
  // chat anyway; older messages are scrollback, not active context.
  // Same family as the sidebar virtualization (3.3) but adapted for
  // variable-height content.
  const INITIAL_WINDOW = 50;
  const WINDOW_STEP = 50;
  const [windowSize, setWindowSize] = createSignal(INITIAL_WINDOW);

  const visibleMessages = createMemo(() => {
    const all = chatStore.messages();
    const n = windowSize();
    if (all.length <= n) return all;
    return all.slice(all.length - n);
  });

  const hiddenCount = createMemo(() => {
    const total = chatStore.messages().length;
    const visible = visibleMessages().length;
    return Math.max(0, total - visible);
  });

  // Group consecutive tool-result / cancellation rows ("✓ ...", "✗ Tool
  // error: ...", "↩ Cancelled") into a single collapsible "Used N tools"
  // strip so they don't dominate the scroll when the agent runs 5–10
  // tools per turn. Mirrors ChatGPT / Claude's collapsible "thinking"
  // block. Non-status messages render through MessageBubble unchanged.
  //
  // Bridging: the agent loop emits an empty "model" turn between each
  // tool call (just `<tool_call>...</tool_call>` markup that strips to
  // ""). MessageBubble hides those, but if the grouping treats them as
  // a break the user sees N separate one-tool groups instead of one
  // group of N. Transparent-bridge any model turn that strips to empty
  // so consecutive tool results stay in the same bucket.
  type GroupedItem =
    | { kind: "msg"; msg: ChatMessage; idx: number }
    | { kind: "tools"; msgs: ChatMessage[]; idx: number };
  const groupedItems = createMemo<GroupedItem[]>(() => {
    const out: GroupedItem[] = [];
    let bucket: ChatMessage[] = [];
    const flush = (idx: number) => {
      if (bucket.length === 0) return;
      out.push({ kind: "tools", msgs: bucket, idx });
      bucket = [];
    };
    visibleMessages().forEach((m, i) => {
      if (m.isStatus) {
        // Hide tool-error chips from the user UI entirely. The agent
        // loop on the backend already gets the error result via the
        // tool_protocol channel and self-corrects (calls roost_search,
        // retries, etc.); the verbose red "✗ Tool error: ..." chip in
        // the chat just panics non-technical users who think the app
        // is broken. If the agent gives up after the error, it'll say
        // so in its final model reply — that's the user-facing signal.
        // Successful chips ("✓ 10 results") and cancellations ("↩")
        // still render so the user sees real activity.
        if (m.content.startsWith("✗ Tool error")) {
          return;
        }
        bucket.push(m);
        return;
      }
      // Bridge: a model turn that strips to nothing is wire-protocol
      // noise between tool calls — keep the current bucket open instead
      // of flushing into a tiny one-tool group.
      if (m.role === "model" && stripToolMarkup(m.content) === "") {
        return;
      }
      flush(i);
      out.push({ kind: "msg", msg: m, idx: i });
    });
    flush(visibleMessages().length);
    return out;
  });

  // Reset the window when the active session changes — opening a fresh
  // session shouldn't carry the previous conversation's expanded window.
  createEffect(
    on(
      () => chatStore.activeSessionId(),
      () => setWindowSize(INITIAL_WINDOW),
      { defer: true },
    ),
  );

  let messagesEndRef: HTMLDivElement | undefined;
  let inputRef: HTMLTextAreaElement | undefined;

  /** Title of the active session — shown as the session pill so the user
   *  knows which conversation they're in. Falls back to "New chat" if
   *  the session list hasn't loaded yet. */
  function activeTitle(): string {
    const id = chatStore.activeSessionId();
    if (!id) return "New chat";
    const s = chatStore.sessions().find((x) => x.id === id);
    return s?.title ?? "New chat";
  }

  const [sessionsFilter, setSessionsFilter] = createSignal("");

  async function handleNewSession() {
    // context_note_path is plumbed through chatStore.newSession so future
    // refactors can pass the open-note's roost-relative path here. ChatPanel
    // only receives `noteContent` today, not the path — pass undefined and
    // the metadata row simply hides the "grounded on" anchor.
    await chatStore.newSession();
    setShowSessions(false);
    inputRef?.focus();
  }

  async function handleSwitchSession(id: string) {
    await chatStore.switchSession(id);
    setShowSessions(false);
    inputRef?.focus();
  }

  /** Filter sessions by the inline search box. Matches title + preview
   *  (case-insensitive). Empty filter = all sessions. */
  function visibleSessions(): ChatSession[] {
    const q = sessionsFilter().trim().toLowerCase();
    if (!q) return chatStore.sessions();
    return chatStore.sessions().filter((s) => {
      const hay = `${s.title} ${s.preview ?? ""}`.toLowerCase();
      return hay.includes(q);
    });
  }

  /** Group sessions into time-bucketed sections. Pinned chats are pulled
   *  out into their own section at the top regardless of age. */
  function groupedSessions(): Array<{ label: string; items: ChatSession[] }> {
    const all = visibleSessions();
    const pinned = all.filter((s) => s.pinned);
    const rest = all.filter((s) => !s.pinned);

    const now = new Date();
    const startOfToday = new Date(now.getFullYear(), now.getMonth(), now.getDate()).getTime();
    const startOfYesterday = startOfToday - 86_400_000;
    const sevenDaysAgo = startOfToday - 7 * 86_400_000;
    const thirtyDaysAgo = startOfToday - 30 * 86_400_000;

    const today: ChatSession[] = [];
    const yesterday: ChatSession[] = [];
    const last7: ChatSession[] = [];
    const last30: ChatSession[] = [];
    const older: ChatSession[] = [];

    for (const s of rest) {
      const t = s.last_active_at || s.created_at;
      if (t >= startOfToday) today.push(s);
      else if (t >= startOfYesterday) yesterday.push(s);
      else if (t >= sevenDaysAgo) last7.push(s);
      else if (t >= thirtyDaysAgo) last30.push(s);
      else older.push(s);
    }

    const sortDesc = (xs: ChatSession[]) =>
      xs.sort((a, b) => (b.last_active_at || b.created_at) - (a.last_active_at || a.created_at));

    const groups: Array<{ label: string; items: ChatSession[] }> = [];
    if (pinned.length) groups.push({ label: "Pinned", items: sortDesc(pinned) });
    if (today.length) groups.push({ label: "Today", items: sortDesc(today) });
    if (yesterday.length) groups.push({ label: "Yesterday", items: sortDesc(yesterday) });
    if (last7.length) groups.push({ label: "Previous 7 days", items: sortDesc(last7) });
    if (last30.length) groups.push({ label: "Previous 30 days", items: sortDesc(last30) });
    if (older.length) groups.push({ label: "Older", items: sortDesc(older) });
    return groups;
  }

  async function handleSaveAsNote(id: string) {
    const path = await chatStore.saveSessionAsNote(id);
    if (path) {
      await tabStore.openTab(path);
      setShowSessions(false);
    }
  }

  let unsubTool: (() => void) | null = null;

  onMount(async () => {
    await settingsStore.load();
    // settingsStore.load() also fetches agent profiles, but call here too
    // for the case where settings was loaded earlier via another panel.
    if (settingsStore.agentProfiles().length === 0) {
      settingsStore.loadAgentProfiles();
    }
    await chatStore.setupListeners();
    // Input is safe to enable.
    setReady(true);

    // Warm BOTH indices so the agent has real data to work with:
    //   - Graph DB → populates the `notes` table → roost index lands in
    //     the system prompt → LLM sees actual titles instead of having
    //     to guess them.
    //   - FTS5 → roost_search returns real hits instead of "0 results".
    // Both are idempotent and fast on warm start (DB rows survive).
    graphStore.ensureGraphReady().catch((e) => {
      log.error("chat", "failed to warm graph DB for chat", e);
    });
    graphStore.ensureSearchReady().catch((e) => {
      log.error("chat", "failed to warm search index for chat", e);
    });

    // Auto-open AI-created notes as tabs AND refresh the sidebar so the
    // new file shows up in the tree without forcing the user to close the
    // chat panel. The earlier version skipped the refresh on the theory
    // that "tab open is enough", but `openTab` only touches the editor —
    // the sidebar tree stays stale until something else (panel switch,
    // focus change) happens to re-walk the roost. That left users staring
    // at a "✓ created task at <path>" message with no file visible.
    unsubTool = chatStore.onToolResult(async (result: ToolResult) => {
      if (result.success && result.file_path) {
        await tabStore.openTab(result.file_path);
        // `force: true` bypasses the 10s debounce — the user just SAW the
        // tool succeed, anything slower than instant feels broken.
        await roostStore.refreshFiles({ force: true });
      }
    });

    if (!isMobile()) inputRef?.focus();
  });

  onCleanup(() => {
    chatStore.cleanupListeners();
    // Drop any in-flight tool-approval requests on the backend so the
    // PendingApprovals HashMap doesn't accumulate orphan senders when
    // the user closes the panel mid-approval. Fire-and-forget — the
    // backend treats each drop as a Reject, which is retryable; the
    // agent loop recovers cleanly even if the user reopens the panel.
    chatStore.forgetPendingApprovals();
    unsubTool?.();
  });

  // Auto-scroll to the newest message — but only when content was actually
  // appended (a new message, or a streaming chunk) rather than on any
  // `messages()` change. An in-place update to an existing message while
  // idle (e.g. flipping a revert flag, or an edit) used to yank the view to
  // the bottom even though nothing new arrived to follow.
  let lastMsgCount = 0;
  createEffect(
    on(
      () => chatStore.messages(),
      (msgs) => {
        const grew = msgs.length !== lastMsgCount;
        lastMsgCount = msgs.length;
        if (!grew && chatStore.status() !== "streaming") return;
        requestAnimationFrame(() => {
          messagesEndRef?.scrollIntoView({ behavior: "smooth" });
        });
      }
    )
  );

  function handleSend() {
    const text = input().trim();
    if (!text) return;

    if (!settingsStore.hasApiKey()) {
      props.onOpenSettings();
      return;
    }

    // Streaming? Queue instead of dropping. A createEffect below fires
    // the queued message when status transitions back to "idle".
    if (chatStore.status() === "streaming") {
      setQueuedMessage(text);
      setInput("");
      if (inputRef) inputRef.style.height = "auto";
      return;
    }

    chatStore.sendMessage(text, props.noteContent);
    setInput("");
    if (inputRef) inputRef.style.height = "auto";
  }

  /** Cancel the current streaming turn and immediately send the typed
   *  message. Bound to Ctrl/Cmd+Enter — same UX as Cursor's "send and
   *  interrupt" so users who change their mind mid-turn don't have to
   *  click Stop, wait, then Send. */
  async function handleForceSend() {
    const text = input().trim();
    if (!text) return;

    if (!settingsStore.hasApiKey()) {
      props.onOpenSettings();
      return;
    }

    if (chatStore.status() === "streaming") {
      // Clear the queue BEFORE stopGeneration — once status flips to
      // "idle" the auto-send effect runs synchronously and would
      // otherwise fire the stale queued message between the cancellation
      // and the new sendMessage below.
      setQueuedMessage("");
      await chatStore.stopGeneration();
    }
    chatStore.sendMessage(text, props.noteContent);
    setInput("");
    if (inputRef) inputRef.style.height = "auto";
  }

  // Auto-send the queued message the moment the current turn finishes.
  // Watches only the status signal — a transition from "streaming" to
  // "idle" (or "error", which we treat as cancellation: drop the queue
  // so a failing API key doesn't fire the same request in a loop) is
  // the trigger.
  createEffect(
    on(
      () => chatStore.status(),
      (status, prevStatus) => {
        if (prevStatus !== "streaming") return;
        if (status === "error") {
          setQueuedMessage("");
          return;
        }
        if (status !== "idle") return;
        const q = queuedMessage().trim();
        if (!q) return;
        setQueuedMessage("");
        chatStore.sendMessage(q, props.noteContent);
      },
      { defer: true },
    ),
  );

  function handleKeyDown(e: KeyboardEvent) {
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      // Ctrl/Cmd+Enter = force-send (cancel current, send new).
      // Plain Enter = send-or-queue depending on streaming state.
      if (e.ctrlKey || e.metaKey) {
        handleForceSend();
      } else {
        handleSend();
      }
      return;
    }
    // Esc during streaming = stop generation. Matches the "Esc to stop"
    // hint shown next to the working indicator. Outside of streaming it's
    // a no-op so the textarea doesn't lose focus / clear unexpectedly.
    if (e.key === "Escape" && chatStore.status() === "streaming") {
      e.preventDefault();
      chatStore.stopGeneration();
    }
  }

  function handleInput(e: InputEvent) {
    const target = e.target as HTMLTextAreaElement;
    setInput(target.value);
    // Auto-resize textarea
    target.style.height = "auto";
    target.style.height = Math.min(target.scrollHeight, 120) + "px";
  }

  return (
    <div
      class="flex flex-col h-full overflow-hidden"
      style={panelViewportStyle()}
    >
      {/* Header — slim single row. Wordmark dropped (icon-rail crow
          already identifies the panel); trash dropped (destructive
          op too close to settings, and `New chat` in the session row
          covers the practical use case). aria-labels added for a11y.
          The `WindowControls` overlay (min/max/close trio, fixed top-
          right of the window) sits over this header on Windows +
          Linux when chat is the rightmost pane — same problem
          RightSidebar already solved. Reserve `WINDOW_CONTROLS_WIDTH`
          of right padding in that case so the cog + close X are
          actually clickable and not stuck behind the overlay. */}
      <div
        class="h-[3.25rem] sm:h-10 flex items-center justify-between px-3 border-b border-border shrink-0 select-none"
        style={
          !isMobilePlatform() && isTauri && !isMacOS()
            ? { "padding-right": `${WINDOW_CONTROLS_WIDTH}px` }
            : undefined
        }
      >
        <CorvuxLogo alt="Corvux Agent" width="18" height="18" class="object-contain opacity-80" />
        <div class="flex items-center gap-1">
          <TouchIconButton
            label="Open AI Settings"
            onClick={() => props.onOpenSettings()}
            title="AI Settings — provider, API keys, and more"
          >
            <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
              <path d="M10.325 4.317c.426-1.756 2.924-1.756 3.35 0a1.724 1.724 0 002.573 1.066c1.543-.94 3.31.826 2.37 2.37a1.724 1.724 0 001.066 2.573c1.756.426 1.756 2.924 0 3.35a1.724 1.724 0 00-1.066 2.573c.94 1.543-.826 3.31-2.37 2.37a1.724 1.724 0 00-2.573 1.066c-.426 1.756-2.924 1.756-3.35 0a1.724 1.724 0 00-2.573-1.066c-1.543.94-3.31-.826-2.37-2.37a1.724 1.724 0 00-1.066-2.573c-1.756-.426-1.756-2.924 0-3.35a1.724 1.724 0 001.066-2.573c-.94-1.543.826-3.31 2.37-2.37.996.608 2.296.07 2.572-1.065z" />
              <circle cx="12" cy="12" r="3" />
            </svg>
          </TouchIconButton>
          <TouchIconButton
            label="Close Corvux Agent"
            onClick={() => props.onClose()}
            title="Close panel (Ctrl+L)"
          >
            <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
              <path d="M18 6L6 18M6 6l12 12" />
            </svg>
          </TouchIconButton>
        </div>
      </div>

      {/* Sessions strip — current session pill + history toggle + new chat. */}
      <div class="flex items-center gap-1 px-2 py-1.5 border-b border-border bg-bg-secondary/40 shrink-0 select-none">
        <button
          onClick={() => setShowSessions(!showSessions())}
          class="flex-1 flex items-center gap-1.5 px-2 py-1 rounded text-left hover:bg-bg-hover transition-colors min-w-0"
          title="Switch session"
        >
          <svg
            width="12"
            height="12"
            viewBox="0 0 24 24"
            fill="none"
            stroke="currentColor"
            stroke-width="2"
            class="shrink-0 text-text-muted transition-transform"
            style={{ transform: showSessions() ? "rotate(90deg)" : "rotate(0deg)" }}
          >
            <path d="M9 18l6-6-6-6" />
          </svg>
          <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" class="shrink-0 text-text-muted">
            <path d="M21 15a2 2 0 0 1-2 2H7l-4 4V5a2 2 0 0 1 2-2h14a2 2 0 0 1 2 2z" />
          </svg>
          <span class="text-xs text-text-secondary truncate flex-1">{activeTitle()}</span>
          <span class="text-[10px] text-text-muted shrink-0">
            {chatStore.sessions().length} chat{chatStore.sessions().length === 1 ? "" : "s"}
          </span>
        </button>
        {/* Clear messages in the current session (keeps the session
            row; just empties the transcript). Distinct from New chat
            which creates a fresh session — distinct in commit ccea2b2
            removed this button on the assumption they were redundant,
            which they are not. */}
        <button
          onClick={async () => {
            if (chatStore.messages().length === 0) return;
            // `window.confirm` is blocked by iOS WKWebView (Tauri's
            // dialog allowlist defaults reject it; the log shows
            // `dialog.confirm not allowed. Command not found`). The
            // Tauri dialog plugin's `ask` is the cross-platform
            // replacement and works on macOS, Windows, and iOS. Dynamic
            // import so the plugin code only loads when the user
            // actually hits the button.
            const { ask } = await import("@tauri-apps/plugin-dialog");
            const ok = await ask(
              "Clear all messages in this chat? This cannot be undone.",
              { title: "Clear chat", kind: "warning" },
            );
            if (ok) {
              chatStore.clearMessages(props.roostPath);
            }
          }}
          disabled={chatStore.messages().length === 0}
          class="w-7 h-7 flex items-center justify-center rounded hover:bg-bg-hover text-text-muted hover:text-text-secondary transition-colors shrink-0 disabled:opacity-30 disabled:cursor-not-allowed disabled:hover:bg-transparent"
          title="Clear messages in this chat"
        >
          <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
            <path d="M3 6h18M8 6V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2M19 6l-1 14a2 2 0 0 1-2 2H8a2 2 0 0 1-2-2L5 6" />
          </svg>
        </button>
        <button
          onClick={handleNewSession}
          class="w-7 h-7 flex items-center justify-center rounded hover:bg-bg-hover text-text-muted hover:text-purple transition-colors shrink-0"
          title="New chat"
        >
          <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
            <path d="M12 5v14M5 12h14" />
          </svg>
        </button>
      </div>

      {/* Agent profile picker removed from the chat panel on 2026-05-20
          — telemetry showed 14/14 chats used Default; the four pills
          stole vertical chrome above every chat for a feature nobody
          pulled. The picker now lives in Settings → Corvux Agent as
          a one-time "default profile" dropdown. Per-chat switching is
          gone; restore the <Show>...<For>...</Show> block here if
          per-chat switching becomes a real user need. */}

      {/* Sessions list panel — search + time-bucketed groups + pinned. */}
      <Show when={showSessions()}>
        <div class="border-b border-border bg-bg-secondary/60 max-h-[60vh] overflow-y-auto shrink-0 flex flex-col">
          {/* Inline search across titles + previews. */}
          <Show when={chatStore.sessions().length > 0}>
            <div class="px-2 pt-2 pb-1 sticky top-0 bg-bg-secondary z-10 border-b border-border">
              <div class="flex items-center gap-1.5 px-2 py-1 rounded bg-bg-primary border border-border focus-within:border-accent">
                <svg width="11" height="11" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" class="text-text-muted shrink-0">
                  <circle cx="11" cy="11" r="8" />
                  <path d="M21 21l-4.35-4.35" />
                </svg>
                <input
                  type="text"
                  placeholder="Search chats…"
                  value={sessionsFilter()}
                  onInput={(e) => setSessionsFilter(e.currentTarget.value)}
                  class="flex-1 bg-transparent text-xs text-text-primary outline-none placeholder:text-text-muted min-w-0"
                />
                <Show when={sessionsFilter()}>
                  <button
                    class="text-text-muted hover:text-text-primary p-0.5 shrink-0"
                    onClick={() => setSessionsFilter("")}
                    title="Clear"
                  >
                    <svg width="10" height="10" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5">
                      <path d="M18 6L6 18M6 6l12 12" />
                    </svg>
                  </button>
                </Show>
              </div>
            </div>
          </Show>

          {/* Empty states. */}
          <Show when={chatStore.sessions().length === 0}>
            <div class="px-3 py-6 text-xs text-text-muted text-center">
              <p class="mb-1">No chats yet.</p>
              <p class="opacity-60">Click + to start one.</p>
            </div>
          </Show>
          <Show when={chatStore.sessions().length > 0 && visibleSessions().length === 0}>
            <div class="px-3 py-4 text-xs text-text-muted text-center">
              No chats match "{sessionsFilter()}".
            </div>
          </Show>

          {/* Time-bucketed groups. */}
          <For each={groupedSessions()}>
            {(group) => (
              <div>
                <div class="px-3 pt-2 pb-1 text-[10px] font-semibold uppercase tracking-wide text-text-muted bg-bg-secondary/80 sticky top-[37px] z-[5]">
                  {group.label}
                </div>
                <For each={group.items}>
                  {(session) => (
                    <SessionRow
                      session={session}
                      isActive={session.id === chatStore.activeSessionId()}
                      onSwitch={() => handleSwitchSession(session.id)}
                      onRename={(title) => chatStore.renameSession(session.id, title)}
                      onDelete={() => chatStore.deleteSession(session.id)}
                      onTogglePin={() => chatStore.togglePin(session.id)}
                      onSaveAsNote={() => handleSaveAsNote(session.id)}
                    />
                  )}
                </For>
              </div>
            )}
          </For>
        </div>
      </Show>

      {/* Inline API-key form removed 2026-05-20. Onboarding now lives
          in Settings → AI Models. The empty-state in the message
          body below surfaces a CTA when no key is configured. */}

      {/* Embedding status strip removed — retrieval is FTS-driven now, so
          there is no roost-embedding pass to report on. The agent's
          roost_search tool uses the SQLite FTS5 index, which warms silently
          via ensureSearchReady() on mount. */}

      {/* Messages area */}
      <div class="flex-1 overflow-y-auto px-3 py-2 space-y-3">
        {/* Empty state — sits near the top of the body, NOT vertically
            centered. With multiple chrome rows above (header, sessions,
            agent profile, RAG status), centering the prompts in the
            remaining space left the brand mark floating in a dead zone
            mid-panel. Top-aligned reads as "here's what you can do
            right now" instead of "look, I padded the middle." */}
        <Show when={chatStore.messages().length === 0}>
          <Show
            when={settingsStore.hasApiKey()}
            fallback={
              <div class="flex flex-col items-center px-4 pt-6">
                <div class="text-center space-y-2 mb-4">
                  <CorvuxLogo
                    alt="Corvux"
                    width="32"
                    height="32"
                    class="mx-auto object-contain opacity-55"
                  />
                  <p class="text-text-secondary text-sm font-medium">
                    Add an API key to start chatting
                  </p>
                  <p class="text-text-muted text-xs leading-relaxed">
                    Corvux uses Google Gemini (or another provider via
                    OpenRouter) to answer questions about your notes. Add a
                    key once and you're done — keys stay on this device.
                  </p>
                </div>
                <button
                  type="button"
                  onClick={() => props.onOpenSettings()}
                  class="inline-flex items-center gap-1.5 px-3 py-2 rounded-lg bg-purple text-bg-primary text-xs font-medium hover:opacity-90 transition-opacity"
                >
                  <svg
                    width="13"
                    height="13"
                    viewBox="0 0 24 24"
                    fill="none"
                    stroke="currentColor"
                    stroke-width="2"
                    stroke-linecap="round"
                    stroke-linejoin="round"
                    aria-hidden="true"
                  >
                    <path d="M10.325 4.317c.426-1.756 2.924-1.756 3.35 0a1.724 1.724 0 002.573 1.066c1.543-.94 3.31.826 2.37 2.37a1.724 1.724 0 001.066 2.573c1.756.426 1.756 2.924 0 3.35a1.724 1.724 0 00-1.066 2.573c.94 1.543-.826 3.31-2.37 2.37a1.724 1.724 0 00-2.573 1.066c-.426 1.756-2.924 1.756-3.35 0a1.724 1.724 0 00-2.573-1.066c-1.543.94-3.31-.826-2.37-2.37a1.724 1.724 0 00-1.066-2.573c-1.756-.426-1.756-2.924 0-3.35a1.724 1.724 0 001.066-2.573c-.94-1.543.826-3.31 2.37-2.37.996.608 2.296.07 2.572-1.065z" />
                    <circle cx="12" cy="12" r="3" />
                  </svg>
                  Open AI Settings
                </button>
              </div>
            }
          >
            <div class="flex flex-col items-center px-4 pt-4">
              <div class="text-center space-y-2 mb-4">
                <CorvuxLogo alt="Corvux" width="32" height="32" class="mx-auto object-contain opacity-55" />
                <p class="text-text-secondary text-sm font-medium">
                  <Show when={hasOpenNote()} fallback={<>Ask about your roost</>}>
                    Ask about this note
                  </Show>
                </p>
                <p class="text-text-muted text-xs">
                  <Show
                    when={hasOpenNote()}
                    fallback={<>Open a note for context, or ask about the whole roost</>}
                  >
                    Corvux turns your current context into clearer next steps
                  </Show>
                </p>
              </div>
              {/* Suggestions adapt to whether a note is actually open.
                  Showing "Summarize this note" with an empty editor sent
                  the agent off to roost_search and pick a random note —
                  fixed by gating note-context prompts on hasOpenNote(). */}
              <div class="w-full space-y-1.5">
                <Show
                  when={hasOpenNote()}
                  fallback={
                    <For
                      each={[
                        "What's in my roost?",
                        "Show me my recent notes",
                        "Help me find a note",
                      ]}
                    >
                      {(q) => (
                        <button
                          class="w-full text-left px-3 py-2 rounded-lg border border-border bg-bg-secondary hover:bg-bg-hover text-text-secondary text-xs transition-colors disabled:opacity-40 disabled:cursor-not-allowed"
                          disabled={!ready()}
                          onClick={() =>
                            chatStore.sendMessage(q, props.noteContent)
                          }
                        >
                          {q}
                        </button>
                      )}
                    </For>
                  }
                >
                  <For
                    each={[
                      "Summarize this note",
                      "What are the action items?",
                      "What do I know about this topic?",
                    ]}
                  >
                    {(q) => (
                      <button
                        class="w-full text-left px-3 py-2 rounded-lg border border-border bg-bg-secondary hover:bg-bg-hover text-text-secondary text-xs transition-colors disabled:opacity-40 disabled:cursor-not-allowed"
                        disabled={!ready()}
                        onClick={() =>
                          chatStore.sendMessage(q, props.noteContent)
                        }
                      >
                        {q}
                      </button>
                    )}
                  </For>
                </Show>
              </div>
            </div>
          </Show>
        </Show>

        {/* "Show earlier" pill — only when the tail window is hiding older
            messages. Expanding by WINDOW_STEP keeps each click cheap;
            "Show all" lands when there are fewer than a step left. */}
        <Show when={hiddenCount() > 0}>
          <div class="flex justify-center">
            <button
              class="px-3 py-1 rounded-full bg-bg-secondary border border-border text-text-muted hover:text-text-primary hover:bg-bg-hover text-xs transition-colors"
              onClick={() =>
                setWindowSize((n) =>
                  hiddenCount() <= WINDOW_STEP ? n + hiddenCount() : n + WINDOW_STEP,
                )
              }
            >
              <Show
                when={hiddenCount() > WINDOW_STEP}
                fallback={<>Show all {hiddenCount()} earlier message{hiddenCount() === 1 ? "" : "s"}</>}
              >
                Show {WINDOW_STEP} earlier ({hiddenCount()} more)
              </Show>
            </button>
          </div>
        </Show>

        {/* Message list — windowed to the tail. While the agent is
            streaming, the inline tool chips stay hidden — the live
            ticker below covers "what's happening right now" and the
            per-step chips would just stack a redundant changelog above
            an answer that hasn't landed yet. Once status flips to idle,
            the ToolActivityGroup re-appears as a compact pill so the
            user can still drill in. */}
        <For each={groupedItems()}>
          {(item) => {
            if (item.kind === "msg") {
              return (
                <MessageBubble
                  message={item.msg}
                  isStreaming={chatStore.status() === "streaming"}
                />
              );
            }
            return (
              <Show when={chatStore.status() !== "streaming"}>
                <ToolActivityGroup messages={item.msgs} />
              </Show>
            );
          }}
        </For>

        {/* Working indicator — staggered 3-dot pulse + label. Sits at the
            bottom of the message stream so it's visually adjacent to where
            the next chunk will land. Without this, long gaps between tool
            calls / iterations feel like the app is frozen — Claude Code,
            Cursor, ChatGPT all have an analogous indicator. The label
            switches to "Waiting for approval" when a tool card is pending
            so the user knows the agent is blocked on THEM, not stuck. */}
        <Show when={chatStore.status() === "streaming"}>
          <div class="flex items-center gap-2 px-1 py-1 text-text-muted">
            <div class="flex gap-1">
              <div
                class="w-1.5 h-1.5 rounded-full bg-purple animate-pulse"
                style={{ "animation-delay": "0ms", "animation-duration": "1s" }}
              />
              <div
                class="w-1.5 h-1.5 rounded-full bg-purple animate-pulse"
                style={{ "animation-delay": "200ms", "animation-duration": "1s" }}
              />
              <div
                class="w-1.5 h-1.5 rounded-full bg-purple animate-pulse"
                style={{ "animation-delay": "400ms", "animation-duration": "1s" }}
              />
            </div>
            <span class="text-xs italic">
              <Show
                when={chatStore.pendingTools().length > 0}
                fallback={<>{chatStore.currentActivity()}</>}
              >
                Waiting for your approval…
              </Show>
            </span>
          </div>
        </Show>

        {/* Error message — classified into a friendly card. Rate-limit
            errors get the upgrade CTA; everything else shows the message. */}
        <Show when={chatStore.errorInfo()}>
          {(info) => (
            <div class="px-3 py-2 rounded-lg bg-red/10 border border-red/20 text-red text-xs space-y-1.5">
              <div class="font-medium">{info().title}</div>
              <div class="opacity-90">{info().body}</div>
              <Show when={info().cta}>
                {(cta) => (
                  <button
                    type="button"
                    class="mt-1 px-2.5 py-1 rounded-md bg-red/15 hover:bg-red/25 border border-red/30 font-medium transition-colors"
                    onClick={() => window.open(cta().url, "_blank", "noopener,noreferrer")}
                  >
                    {cta().label}
                  </button>
                )}
              </Show>
            </div>
          )}
        </Show>

        {/* Pending tool-approval cards. Render at the bottom of the
            message stream so the user sees them inline with the agent
            turn that triggered them. Each card resolves itself via
            `chatStore.decideTool` and disappears once decided. */}
        <For each={chatStore.pendingTools()}>
          {(pending) => <ToolCard pending={pending} />}
        </For>

        <div ref={messagesEndRef} />
      </div>

      {/* Input area */}
      <div class="px-3 py-2 border-t border-border shrink-0">
        {/* Queued message chip — shown when the user typed + Entered
            while the agent was streaming. Renders directly above the
            input so the connection between "your message is parked here"
            and the textarea is visually obvious. × cancels the queue. */}
        <Show when={queuedMessage()}>
          <div class="flex items-center gap-2 mb-1.5 px-2 py-1 rounded bg-purple/10 border border-purple/30 text-xs">
            <svg width="11" height="11" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" class="shrink-0 text-purple">
              <circle cx="12" cy="12" r="10" />
              <path d="M12 6v6l4 2" />
            </svg>
            <span class="text-text-secondary shrink-0">Queued:</span>
            <span class="text-text-primary truncate flex-1" title={queuedMessage()}>
              {queuedMessage()}
            </span>
            <button
              type="button"
              onClick={() => setQueuedMessage("")}
              class="shrink-0 w-4 h-4 flex items-center justify-center rounded hover:bg-bg-hover text-text-muted hover:text-text-primary transition-colors"
              title="Cancel queued message"
            >
              <svg width="10" height="10" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5">
                <path d="M18 6L6 18M6 6l12 12" />
              </svg>
            </button>
          </div>
        </Show>
        <div class="flex items-end gap-2">
          <textarea
            ref={inputRef}
            value={input()}
            onInput={handleInput}
            onKeyDown={handleKeyDown}
            onFocus={settleMobileInputViewport}
            onTouchStart={settleMobileInputViewport}
            placeholder={
              !settingsStore.hasApiKey()
                ? isMobile()
                  ? "Set up AI in Settings"
                  : "Set your API key first..."
                : !ready()
                  ? "Initializing chat…"
                  : chatStore.status() === "streaming"
                    ? isMobile()
                      ? "Type a follow-up"
                      : "Type to queue · Ctrl+Enter to send now"
                    : isMobile()
                      ? "Ask about your notes"
                      : "Ask about your notes... (Enter to send)"
            }
            disabled={!settingsStore.hasApiKey() || !ready()}
            rows={1}
            class="flex-1 bg-bg-secondary border border-border rounded-lg px-3 py-2 text-sm text-text-primary placeholder-text-muted resize-none focus:outline-none focus:border-purple disabled:opacity-50 disabled:cursor-not-allowed"
            style={{ "max-height": "120px" }}
          />
          <Show
            when={chatStore.status() !== "streaming"}
            fallback={
              <button
                onClick={() => chatStore.stopGeneration()}
                class="w-11 h-11 sm:w-8 sm:h-8 flex items-center justify-center rounded-lg bg-red/20 text-red hover:bg-red/30 transition-colors shrink-0"
                title="Stop generating"
              >
                <svg width="14" height="14" viewBox="0 0 24 24" fill="currentColor">
                  <rect x="6" y="6" width="12" height="12" rx="1" />
                </svg>
              </button>
            }
          >
            <button
              onClick={handleSend}
              disabled={!input().trim() || !settingsStore.hasApiKey() || !ready()}
              class="w-11 h-11 sm:w-8 sm:h-8 flex items-center justify-center rounded-lg bg-purple text-bg-primary hover:opacity-90 transition-opacity shrink-0 disabled:opacity-30 disabled:cursor-not-allowed"
              title="Send (Enter)"
            >
              <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
                <path d="M22 2L11 13M22 2l-7 20-4-9-9-4 20-7z" />
              </svg>
            </button>
          </Show>
        </div>
        {/* Scroll-resistant working indicator — visible even when the
            user has scrolled up past the in-stream pulse. Same animation
            so both signals read as one consistent "agent is busy" cue. */}
        <Show when={chatStore.status() === "streaming"}>
          <div class="flex items-center gap-1.5 mt-1.5 text-text-muted">
            <div class="flex gap-1">
              <div
                class="w-1.5 h-1.5 rounded-full bg-purple animate-pulse"
                style={{ "animation-delay": "0ms", "animation-duration": "1s" }}
              />
              <div
                class="w-1.5 h-1.5 rounded-full bg-purple animate-pulse"
                style={{ "animation-delay": "200ms", "animation-duration": "1s" }}
              />
              <div
                class="w-1.5 h-1.5 rounded-full bg-purple animate-pulse"
                style={{ "animation-delay": "400ms", "animation-duration": "1s" }}
              />
            </div>
            <span class="text-xs">
              <Show
                when={chatStore.pendingTools().length > 0}
                fallback={<>{chatStore.currentActivity()}</>}
              >
                Waiting for approval…
              </Show>
            </span>
            {/* Desktop-only hint — no Esc key on a phone. */}
            <Show when={!isMobile()}>
              <span class="text-[10px] opacity-60 ml-auto">Esc to stop</span>
            </Show>
          </div>
        </Show>
      </div>
    </div>
  );
}

/// Collapsible block for consecutive tool-result chips ("✓ ...", "✗ Tool
/// error: ...", "↩ Cancelled"). Collapsed by default — shows a summary
/// row with a caret. Expanding reveals each underlying chip in order, so
/// the user can still drill in when something failed without the full
/// transcript drowning out the model's actual answer.
function ToolActivityGroup(props: { messages: ChatMessage[] }) {
  const [expanded, setExpanded] = createSignal(false);
  const total = () => props.messages.length;
  const errorCount = () =>
    props.messages.filter((m) => m.content.startsWith("✗")).length;
  const cancelCount = () =>
    props.messages.filter((m) => m.content.startsWith("↩")).length;
  const okCount = () => total() - errorCount() - cancelCount();

  return (
    <div class="flex justify-start">
      <div class="max-w-[85%] w-full">
        <button
          type="button"
          onClick={() => setExpanded(!expanded())}
          aria-expanded={expanded()}
          class="flex items-center gap-1.5 px-1.5 py-1 rounded text-xs text-text-muted hover:text-text-secondary hover:bg-bg-hover transition-colors"
        >
          <svg
            width="12"
            height="12"
            viewBox="0 0 24 24"
            fill="none"
            stroke="currentColor"
            stroke-width="2"
            class="transition-transform shrink-0"
            style={{ transform: expanded() ? "rotate(90deg)" : "rotate(0deg)" }}
          >
            <path d="M9 18l6-6-6-6" />
          </svg>
          <span>
            {total()} tool {total() === 1 ? "call" : "calls"}
            <Show when={okCount() > 0 && (errorCount() > 0 || cancelCount() > 0)}>
              <span class="text-green-400/80"> · {okCount()} ok</span>
            </Show>
            <Show when={errorCount() > 0}>
              <span class="text-red-400/80">
                {" "}
                · {errorCount()} error{errorCount() === 1 ? "" : "s"}
              </span>
            </Show>
            <Show when={cancelCount() > 0}>
              <span class="text-text-muted/80"> · {cancelCount()} cancelled</span>
            </Show>
          </span>
        </button>
        <Show when={expanded()}>
          <div class="ml-3 mt-1 pl-3 border-l border-border space-y-0.5">
            <For each={props.messages}>
              {(m) => (
                <div class="text-xs text-text-secondary py-0.5 leading-snug break-words">
                  {m.content}
                </div>
              )}
            </For>
          </div>
        </Show>
      </div>
    </div>
  );
}
