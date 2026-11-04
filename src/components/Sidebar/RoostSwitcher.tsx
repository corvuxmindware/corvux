import { For, Show, createSignal, onCleanup, onMount } from "solid-js";
import { roostStore } from "../../stores/roost";
import { roostRegistry, type RoostEntry } from "../../stores/roosts";

interface RoostSwitcherProps {
  onOpenManage: () => void;
}

export default function RoostSwitcher(props: RoostSwitcherProps) {
  const [open, setOpen] = createSignal(false);
  let containerRef: HTMLDivElement | undefined;

  function handleDocClick(e: MouseEvent) {
    if (!open()) return;
    if (containerRef && !containerRef.contains(e.target as Node)) {
      setOpen(false);
    }
  }

  onMount(() => {
    document.addEventListener("mousedown", handleDocClick);
    onCleanup(() => document.removeEventListener("mousedown", handleDocClick));
  });

  async function handleSwitch(entry: RoostEntry) {
    setOpen(false);
    if (entry.id === roostRegistry.activeId()) return;
    // Pass the stored bookmark — iOS scoped roosts need it to re-acquire
    // security-scoped access; without it open_roost hits EPERM.
    await roostStore.openRoost(entry.path, entry.displayName, entry.bookmark);
  }

  // Sort: active first, then by lastOpened desc
  function sortedRoosts(): RoostEntry[] {
    const activeId = roostRegistry.activeId();
    const list = [...roostRegistry.roosts()];
    list.sort((a, b) => {
      if (a.id === activeId) return -1;
      if (b.id === activeId) return 1;
      return b.lastOpened - a.lastOpened;
    });
    return list;
  }

  function currentLabel(): string {
    const active = roostRegistry.activeRoost();
    if (active) return active.displayName;
    return roostStore.roostDisplayName() ?? "No Roost";
  }

  return (
    <div ref={containerRef} class="relative">
      <button
        onClick={() => setOpen(!open())}
        class="w-full flex items-center gap-2 px-3 py-2.5 min-h-[44px] hover:bg-bg-hover transition-colors text-left"
        title="Switch Roost"
        aria-label="Switch Roost"
      >
        {/* Corvux nest mark — the roost's logo, on the left of the bar.
            Mirrors the website's roost footer (ProductPreview.astro). */}
        <svg
          width="20"
          height="20"
          viewBox="0 0 24 24"
          fill="none"
          stroke="currentColor"
          stroke-width="1.4"
          stroke-linecap="round"
          stroke-linejoin="round"
          class="shrink-0 text-text-muted"
        >
          <path d="M2.2 12.9c0 2.9 4.4 4.7 9.8 4.7s9.8-1.8 9.8-4.7" />
          <path d="M2.2 12.9c0-1.5 4.4-2.6 9.8-2.6s9.8 1.1 9.8 2.6" />
          <path d="M4.3 14.5c1.9.9 4.6 1.4 7.7 1.4s5.8-.5 7.7-1.4" />
          <path d="M5.4 11.7 3.8 10.5M8.7 11 7.8 9.5M12 10.5V9" />
          <path d="M15.4 10.7c1.9-1 3.2-2.7 3.7-5" />
          <path d="M19.1 5.7c.5-1.3 1.6-2.1 3.2-2.2-.1 1.6-1.1 2.5-3.2 2.2z" />
          <path d="M17.5 8.5c-1.4-.2-2.3-1.1-2.5-2.6 1.5 0 2.4.9 2.5 2.6z" />
        </svg>
        <span class="text-[13px] text-text-primary truncate flex-1">
          {currentLabel()}
        </span>
        {/* Switch chevrons sit at the right edge so the bar reads as
            "switch roost" — same affordance as the preview. */}
        <svg
          width="15"
          height="15"
          viewBox="0 0 24 24"
          fill="none"
          stroke="currentColor"
          stroke-width="1.75"
          stroke-linecap="round"
          stroke-linejoin="round"
          class="shrink-0 text-text-muted"
        >
          <path d="M7 15l5 5 5-5M7 9l5-5 5 5" />
        </svg>
      </button>

      <Show when={open()}>
        <div
          class="absolute left-2 right-2 bottom-full mb-1 bg-bg-secondary border border-border rounded-md shadow-lg py-1 z-50 max-h-[60vh] overflow-y-auto"
        >
          <For each={sortedRoosts()}>
            {(entry) => {
              const isActive = entry.id === roostRegistry.activeId();
              return (
                <button
                  onClick={() => handleSwitch(entry)}
                  class="w-full flex items-center gap-2 px-3 py-2 hover:bg-bg-hover transition-colors text-left"
                >
                  <span class="w-4 text-accent text-[13px] shrink-0">
                    <Show when={isActive}>✓</Show>
                  </span>
                  <div class="flex-1 min-w-0">
                    <div class="text-[13px] text-text-primary truncate">
                      {entry.displayName}
                    </div>
                    <div class="text-[10px] text-text-muted truncate">
                      {entry.type === "saf" ? "Folder (Android)" : entry.path}
                    </div>
                  </div>
                </button>
              );
            }}
          </For>

          <Show when={roostRegistry.roosts().length > 0}>
            <div class="h-px bg-border my-1" />
          </Show>

          <button
            onClick={() => {
              setOpen(false);
              props.onOpenManage();
            }}
            class="w-full flex items-center gap-2 px-3 py-2 hover:bg-bg-hover transition-colors text-left"
          >
            <span class="w-4 shrink-0" />
            <span class="text-[13px] text-text-primary">Manage Roosts…</span>
          </button>
        </div>
      </Show>
    </div>
  );
}
