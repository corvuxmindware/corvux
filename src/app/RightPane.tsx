// Desktop-only right sidebar (Backlinks / Outline) + its left-edge
// resize handle. Hidden on mobile — the same views surface via the
// left-overlay activePanel + ⋮ menu. Split off App.tsx to shrink the
// shell.

import { Show, type Setter } from "solid-js";
import RightSidebar from "../components/RightSidebar/RightSidebar";
import type { createRightSidebar } from "./right-sidebar";

interface RightPaneProps {
  rightSidebar: ReturnType<typeof createRightSidebar>;
  visible: () => boolean;
  setGoToLine: Setter<number>;
}

export default function RightPane(props: RightPaneProps) {
  return (
    <>
      <Show when={props.visible()}>
        <div
          class="h-full w-1 cursor-col-resize shrink-0 bg-bg-secondary hover:bg-accent/40 transition-colors"
          onPointerDown={props.rightSidebar.startResize}
          onDblClick={props.rightSidebar.reset}
          title="Drag to resize · double-click to reset"
        />
      </Show>
      <div
        class="h-full bg-bg-secondary border-l border-border flex flex-col overflow-hidden"
        style={{
          width: props.visible() ? `${props.rightSidebar.width()}px` : "0px",
          "min-width": props.visible() ? `${props.rightSidebar.width()}px` : "0px",
        }}
      >
        <Show when={props.visible()}>
          <RightSidebar
            panel={props.rightSidebar.panel()}
            onPanelChange={props.rightSidebar.setPanel}
            onGoToLine={(line) => {
              props.setGoToLine(0);
              requestAnimationFrame(() => props.setGoToLine(line));
            }}
          />
        </Show>
      </div>
    </>
  );
}
