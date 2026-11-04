import { onCleanup, onMount, For } from "solid-js";

interface ShortcutOverlayProps {
  onClose: () => void;
}

const SECTIONS: Array<{ title: string; shortcuts: Array<[string, string]> }> = [
  {
    title: "Navigation",
    shortcuts: [
      ["Ctrl+O", "Quick switcher"],
      ["Ctrl+P", "Command palette"],
      ["Ctrl+B", "Toggle sidebar"],
      ["Ctrl+Shift+F", "Search in Roost"],
      ["Ctrl+G", "Graph view"],
      ["Ctrl+Shift+P", "Find path between notes"],
      ["Ctrl+Tab", "Next tab"],
      ["Ctrl+Shift+Tab", "Previous tab"],
    ],
  },
  {
    title: "Editing",
    shortcuts: [
      ["Ctrl+N", "New note"],
      ["Ctrl+D", "Daily note"],
      ["Ctrl+S", "Save"],
      ["Ctrl+W", "Close tab"],
      ["Ctrl+V", "Paste (images supported)"],
      ["Ctrl+Shift+R", "Voice memo"],
      ["[[ ", "Link to another note"],
    ],
  },
  {
    title: "Panels",
    shortcuts: [
      ["Ctrl+L", "Corvux Agent"],
      ["Ctrl+T", "Tasks"],
      ["Ctrl+Shift+T", "Toggle theme"],
      ["Ctrl+/", "This cheat sheet"],
    ],
  },
  {
    title: "In Editor",
    shortcuts: [
      ["Ctrl+Click", "Follow [[wiki-link]]"],
      ["Scroll", "Zoom (in graph view)"],
      ["Drag", "Pan (in graph view)"],
      ["Double-click", "Open node (graph)"],
    ],
  },
];

export default function ShortcutOverlay(props: ShortcutOverlayProps) {
  // Capture-phase Escape so when this overlay sits on top of an
  // already-open Settings panel (which also listens for Escape on
  // document), only THIS overlay closes — the user keeps Settings
  // underneath instead of having both dismissed at once.
  function handleKeyDown(e: KeyboardEvent) {
    if (e.key !== "Escape") return;
    e.stopImmediatePropagation();
    props.onClose();
  }
  onMount(() =>
    document.addEventListener("keydown", handleKeyDown, { capture: true }),
  );
  onCleanup(() =>
    document.removeEventListener("keydown", handleKeyDown, { capture: true }),
  );

  return (
    <div
      // z-[60] keeps this overlay above Settings (z-50) when the user
      // opened shortcuts from inside Settings.
      class="fixed inset-0 z-[60] bg-black/60 flex items-center justify-center"
      onClick={(e) => {
        if (e.target === e.currentTarget) props.onClose();
      }}
      role="dialog"
      aria-modal="true"
      aria-label="Keyboard shortcuts"
    >
      <div class="bg-bg-primary border border-border rounded-xl shadow-2xl max-w-2xl w-full mx-4 max-h-[80vh] overflow-y-auto">
        <div class="flex items-center justify-between px-6 py-4 border-b border-border">
          <h2 class="text-lg font-semibold text-text-primary">Keyboard Shortcuts</h2>
          <button
            onClick={props.onClose}
            class="text-text-muted hover:text-text-primary p-1 rounded hover:bg-bg-hover transition-colors"
            aria-label="Close"
          >
            <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
              <path d="M18 6L6 18M6 6l12 12" />
            </svg>
          </button>
        </div>
        <div class="p-6 grid grid-cols-2 gap-6">
          <For each={SECTIONS}>
            {(section) => (
              <div>
                <h3 class="text-sm font-medium text-text-secondary mb-3">
                  {section.title}
                </h3>
                <div class="space-y-1.5">
                  <For each={section.shortcuts}>
                    {([key, desc]) => (
                      <div class="flex items-center justify-between py-1">
                        <span class="text-text-secondary text-sm">{desc}</span>
                        <kbd class="px-2 py-0.5 rounded bg-bg-hover text-text-muted text-xs font-mono border border-border">
                          {key}
                        </kbd>
                      </div>
                    )}
                  </For>
                </div>
              </div>
            )}
          </For>
        </div>
      </div>
    </div>
  );
}
