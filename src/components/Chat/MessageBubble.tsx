import { For, Show } from "solid-js";
import type { ChatMessage, Citation } from "../../stores/chat";
import { tabStore } from "../../stores/tabs";
import MarkdownView from "./MarkdownView";

/** Strip the agent-loop wire protocol from a model message before
 *  rendering. Removes:
 *    - completed `<tool_call>...</tool_call>` blocks
 *    - unterminated `<tool_call>...` tails (Gemini stop-sequence cuts
 *      the closing tag mid-stream while the tool is still running)
 *    - leading whitespace left behind after a strip
 *  The actual tool execution status is shown via separate `✓ ...`
 *  messages emitted by the `ai:tool-result` listener. */
export function stripToolMarkup(text: string): string {
  let out = text;
  // Closed blocks first.
  out = out.replace(/<tool_call>[\s\S]*?<\/tool_call>/g, "");
  // Then any unterminated tail — everything from <tool_call> to end of
  // string. This is the streaming case: while the tool is still running,
  // we don't want the user staring at half-emitted JSON.
  const openIdx = out.indexOf("<tool_call>");
  if (openIdx !== -1) out = out.slice(0, openIdx);
  return out.trimStart();
}

export default function MessageBubble(props: { message: ChatMessage; isStreaming: boolean }) {
  const isUser = () => props.message.role === "user";
  const isStatus = () => !!props.message.isStatus;
  const displayContent = () =>
    isUser() ? props.message.content : stripToolMarkup(props.message.content);
  // Hide assistant turns that have no visible content left after stripping
  // tool-call markup — both the streaming case (the "Thinking…" indicator
  // is already showing) AND the completed case (a turn that emitted only
  // `<tool_call>` blocks should leave no bubble behind, since the tool
  // chips already cover that activity).
  const isEmpty = () =>
    !isUser() && !isStatus() && displayContent() === "";

  // Status rows (tool-result / cancellation chips) render compactly —
  // smaller text, no bubble chrome, slight left indent — so a 3-word
  // "↩ Cancelled" doesn't take up the same visual real estate as a
  // paragraph of prose.
  //
  // Empty-while-streaming placeholder bubbles are intentionally NOT
  // rendered here — the panel-level "Thinking…" indicator at the end of
  // the message list is the single source of truth for "agent is busy".
  // A second dots animation inside a bubble (the old design) just made
  // the UI feel cluttered without adding signal.
  return (
    <Show when={!isEmpty()}>
      <Show
        when={!isStatus()}
        fallback={
          <div class="flex justify-start">
            <div class="max-w-[85%] px-2 py-0.5 text-xs text-text-secondary leading-snug break-words">
              {displayContent()}
            </div>
          </div>
        }
      >
        <div class={`flex ${isUser() ? "justify-end" : "justify-start"}`}>
          <div
            class={`max-w-[85%] rounded-lg px-3 py-2 text-sm leading-relaxed ${
              isUser()
                ? "bg-purple/20 text-text-primary"
                : "bg-bg-secondary text-text-primary border border-border"
            }`}
          >
            {/* User messages stay verbatim — they typed it, no surprise
                parsing. Assistant replies render markdown so headings,
                bold, lists, and code blocks come through instead of
                bleeding `##` and `**` into the page. */}
            <Show
              when={!isUser()}
              fallback={
                <div class="whitespace-pre-wrap break-words">{displayContent()}</div>
              }
            >
              <MarkdownView content={displayContent()} />
            </Show>
            <Show when={!isUser() && (props.message.citations?.length ?? 0) > 0}>
              <CitationStrip citations={props.message.citations!} />
            </Show>
          </div>
        </div>
      </Show>
    </Show>
  );
}

/** Source-note chips rendered under the model's answer. Each chip is a
 *  real button that opens the cited note in a tab — that's the trust
 *  affordance: "the model said X; here's the note it read." Without this
 *  the [[wiki-link]]s in the answer body are just text the model wrote;
 *  with this, the user can click straight to the source. */
function CitationStrip(props: { citations: Citation[] }) {
  return (
    <div class="mt-2 pt-2 border-t border-border/50">
      <div class="text-[10px] uppercase tracking-wide text-text-muted mb-1">
        Sources
      </div>
      <div class="flex flex-wrap gap-1.5">
        <For each={props.citations}>
          {(c) => (
            <button
              type="button"
              class="inline-flex items-center gap-1 max-w-[14rem] rounded border border-border bg-bg-primary/40 hover:bg-bg-tertiary hover:border-accent/50 px-1.5 py-0.5 text-[11px] text-text-secondary hover:text-text-primary transition-colors"
              title={c.path}
              onClick={() => tabStore.openTab(c.path)}
            >
              <span class="text-accent/70 shrink-0">{citationIcon(c.source)}</span>
              <span class="truncate">{c.title}</span>
            </button>
          )}
        </For>
      </div>
    </div>
  );
}

function citationIcon(source: Citation["source"]): string {
  switch (source) {
    case "roost_search":
      return "🔍";
    case "roost_read":
      return "📄";
    case "note_outline":
      return "≡";
  }
}
