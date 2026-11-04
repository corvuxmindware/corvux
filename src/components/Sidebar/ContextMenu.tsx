import { Show, For, onMount, onCleanup } from "solid-js";

export interface MenuItem {
  label: string;
  icon?: string;
  action: () => void;
  danger?: boolean;
  separator?: boolean;
}

interface ContextMenuProps {
  x: number;
  y: number;
  items: MenuItem[];
  onClose: () => void;
}

export default function ContextMenu(props: ContextMenuProps) {
  let menuRef!: HTMLDivElement;

  onMount(() => {
    // Close on click outside or Escape.
    //
    // Two iOS quirks shape this code:
    // 1. `mousedown` fires as a compatibility event right at the start of
    //    the touch that opened the menu. If we listen on `mousedown`, that
    //    compat event closes the menu the same millisecond it opened.
    //    Listen on `click` instead — the touch gesture handler already
    //    suppresses synthesized clicks for 900ms after a long-press, and
    //    a real later tap (the user dismissing) still fires `click` cleanly.
    // 2. Even with `click`, give a small grace period after mount before
    //    the listener attaches. The `click` event from the gesture that
    //    opened the menu might fire AFTER mount on some WebView versions;
    //    a 150ms delay covers the gap without making dismissal feel laggy.
    const handleClick = (e: MouseEvent) => {
      if (menuRef && !menuRef.contains(e.target as Node)) {
        props.onClose();
      }
    };
    const handleKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") props.onClose();
    };
    const armTimer = window.setTimeout(() => {
      document.addEventListener("click", handleClick);
    }, 150);
    document.addEventListener("keydown", handleKey);
    onCleanup(() => {
      window.clearTimeout(armTimer);
      document.removeEventListener("click", handleClick);
      document.removeEventListener("keydown", handleKey);
    });
  });

  // Keep menu within viewport
  const style = () => {
    const maxX = window.innerWidth - 180;
    const maxY = window.innerHeight - props.items.length * 32 - 16;
    return {
      left: `${Math.min(props.x, maxX)}px`,
      top: `${Math.min(props.y, maxY)}px`,
    };
  };

  return (
    <div
      ref={menuRef}
      class="fixed z-[100] min-w-[160px] bg-bg-secondary border border-border rounded-md shadow-lg py-1 text-sm"
      style={style()}
    >
      <For each={props.items}>
        {(item) => (
          <Show
            when={!item.separator}
            fallback={<div class="h-px bg-border mx-2 my-1" />}
          >
            <button
              class={`w-full text-left px-3 py-1.5 flex items-center gap-2 transition-colors ${
                item.danger
                  ? "text-red hover:bg-red/10"
                  : "text-text-secondary hover:bg-bg-hover hover:text-text-primary"
              }`}
              onClick={() => {
                item.action();
                props.onClose();
              }}
            >
              <span class="truncate">{item.label}</span>
            </button>
          </Show>
        )}
      </For>
    </div>
  );
}
