// AI chat side pane + its left-edge resize handle. On mobile the panel
// fills the viewport (width: 100%); on desktop it sits at the right edge
// with a draggable resize affordance. Split off App.tsx to shrink the
// shell — the chat panel is fully self-contained behind ChatPanel.tsx,
// this wrapper just plumbs visibility + resize state.

import { Show } from "solid-js";
import ChatPanel from "../components/Chat/ChatPanel";
import { tabStore } from "../stores/tabs";
import { roostStore } from "../stores/roost";
import type { createChatResize } from "./chat-resize";

interface ChatPaneProps {
  showChat: () => boolean;
  isMobile: () => boolean;
  chat: ReturnType<typeof createChatResize>;
  onClose: () => void;
  onOpenSettings: () => void;
}

export default function ChatPane(props: ChatPaneProps) {
  return (
    <>
      {/* Resize handle — desktop only. */}
      <Show when={props.showChat() && !props.isMobile()}>
        <div
          class="h-full w-1 cursor-col-resize shrink-0 bg-bg-secondary hover:bg-accent/40 transition-colors"
          onPointerDown={props.chat.startResize}
          onDblClick={props.chat.reset}
          title="Drag to resize chat · double-click to reset"
        />
      </Show>
      <div
        class="h-full bg-bg-secondary border-l border-border transition-[width] duration-200 flex flex-col overflow-hidden"
        style={
          props.isMobile()
            ? {
                width: props.showChat() ? "100%" : "0px",
                "min-width": props.showChat() ? "100%" : "0px",
              }
            : {
                width: props.showChat() ? `${props.chat.width()}px` : "0px",
                "min-width": props.showChat() ? `${props.chat.width()}px` : "0px",
              }
        }
      >
        <Show when={props.showChat()}>
          <ChatPanel
            noteContent={tabStore.activeContent() ?? undefined}
            roostPath={roostStore.roostPath() ?? undefined}
            onClose={props.onClose}
            onOpenSettings={props.onOpenSettings}
          />
        </Show>
      </div>
    </>
  );
}
