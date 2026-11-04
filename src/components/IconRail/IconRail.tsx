import { createSignal, Show, type JSX } from "solid-js";
import { settingsStore } from "../../stores/settings";
import { tasksStore } from "../../stores/tasks";
import CorvuxLogo from "../Brand/CorvuxLogo";

interface IconRailProps {
  activePanel: string;
  chatOpen: boolean;
  graphOpen: boolean;
  onPanelChange: (panel: string) => void;
  onChatToggle: () => void;
  onGraphToggle: () => void;
  onSettingsClick: () => void;
  showPanelSwitchers?: boolean;
  // Right-pane views (Backlinks / Outline) live in the RightSidebar's
  // own tab strip and are surfaced via the TopBar
  // toggle — Obsidian convention. They used to be buttons here, which
  // confused users because clicking a left-rail icon would open a panel
  // on the right.
  onTasksToggle?: () => void;
  tasksOpen?: boolean;
  onHistoryToggle?: () => void;
  historyOpen?: boolean;
}

function Tooltip(props: { label: string; shortcut?: string; children: JSX.Element }) {
  const [show, setShow] = createSignal(false);
  let timeout: ReturnType<typeof setTimeout>;

  return (
    <div
      class="relative"
      onMouseEnter={() => { timeout = setTimeout(() => setShow(true), 400); }}
      onMouseLeave={() => { clearTimeout(timeout); setShow(false); }}
    >
      {props.children}
      <Show when={show()}>
        <div class="absolute left-full top-1/2 -translate-y-1/2 ml-2 z-50 pointer-events-none whitespace-nowrap">
          <div class="flex items-center gap-2 px-2.5 py-1.5 rounded-md bg-bg-primary border border-border shadow-lg text-xs">
            <span class="text-text-secondary">{props.label}</span>
            <Show when={props.shortcut}>
              <kbd class="px-1.5 py-0.5 rounded bg-bg-hover text-text-muted text-[10px] font-mono border border-border">
                {props.shortcut}
              </kbd>
            </Show>
          </div>
        </div>
      </Show>
    </div>
  );
}

export default function IconRail(props: IconRailProps) {
  // Two active styles so a left-pane panel and a main-area view can't both
  // read as a filled selection at once:
  //   "panel"    — Tasks / Explorer / History / Search: a filled box, the
  //                "this panel is showing in the left pane" affordance.
  //   "launcher" — Graph / Assistant: a quiet tab-style underline, since
  //                these open a main-area view rather than a left panel.
  const railBtn = (
    id: string,
    icon: JSX.Element,
    label: string,
    shortcut?: string,
    onClick?: () => void,
    isActive?: () => boolean,
    activeColor?: string,
    variant: "panel" | "launcher" = "panel",
  ) => {
    const active = () => (isActive ? isActive() : props.activePanel === id);
    return (
      <Tooltip label={label} shortcut={shortcut}>
        <button
          title={label}
          aria-label={label}
          class={`relative w-10 h-10 flex items-center justify-center rounded-lg transition-colors ${
            active()
              ? variant === "launcher"
                ? activeColor || "text-accent"
                : `${activeColor || "text-accent"} bg-bg-hover`
              : "text-text-muted hover:text-text-primary hover:bg-bg-hover"
          }`}
          onClick={onClick || (() => props.onPanelChange(props.activePanel === id ? "" : id))}
        >
          {icon}
          <Show when={variant === "launcher" && active()}>
            {/* currentColor follows the button's active text colour, so the
                underline matches accent (Graph) or purple (Assistant). */}
            <span class="absolute bottom-0.5 left-1/2 -translate-x-1/2 h-0.5 w-5 rounded-full bg-current" />
          </Show>
        </button>
      </Tooltip>
    );
  };

  const icon = (d: string) => (
    <svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
      <path d={d} />
    </svg>
  );

  return (
    <div class="w-12 h-full bg-bg-secondary flex flex-col items-center py-2 gap-1 border-r border-border shrink-0">
      <Show when={props.showPanelSwitchers !== false}>
        {/* Top icons */}
        {railBtn("files", icon("M3 7v10a2 2 0 002 2h14a2 2 0 002-2V9a2 2 0 00-2-2h-6l-2-2H5a2 2 0 00-2 2z"), "Explorer", "Ctrl+B")}
        {railBtn("search", icon("M21 21l-6-6m2-5a7 7 0 11-14 0 7 7 0 0114 0z"), "Search", "Ctrl+Shift+F")}
      </Show>

      {/* Tasks — checkbox-style icon with overdue/today count badge */}
      <Tooltip label="Tasks">
        <button
          title="Tasks"
          aria-label="Tasks"
          class={`relative w-10 h-10 flex items-center justify-center rounded-lg transition-colors ${
            props.tasksOpen
              ? "text-accent bg-accent/15"
              : "text-text-muted hover:text-text-primary hover:bg-bg-hover"
          }`}
          onClick={props.onTasksToggle}
        >
          <svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
            <polyline points="9 11 12 14 22 4" />
            <path d="M21 12v7a2 2 0 01-2 2H5a2 2 0 01-2-2V5a2 2 0 012-2h11" />
          </svg>
          <Show when={!props.tasksOpen && tasksStore.totals().overdue > 0}>
            <span
              class="absolute -top-0.5 -right-0.5 min-w-[16px] h-[16px] px-1 flex items-center justify-center rounded-full text-[10px] font-semibold leading-none tabular-nums"
              style={{ background: "var(--color-red)", color: "var(--color-bg-primary)" }}
              aria-label={`${tasksStore.totals().overdue} overdue`}
            >
              {tasksStore.totals().overdue > 99 ? "99+" : tasksStore.totals().overdue}
            </span>
          </Show>
          <Show when={!props.tasksOpen && tasksStore.totals().overdue === 0 && tasksStore.totals().today > 0}>
            <span class="absolute top-1.5 right-1.5 w-2 h-2 rounded-full" style={{ background: "var(--color-accent)" }} />
          </Show>
        </button>
      </Tooltip>

      {/* Edit History — clock-with-arrow */}
      {railBtn(
        "history",
        (
          <svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
            <path d="M3 12a9 9 0 1 0 3-6.7L3 8" />
            <polyline points="3 3 3 8 8 8" />
            <polyline points="12 7 12 12 15 14" />
          </svg>
        ),
        "Edit History",
        undefined,
        props.onHistoryToggle,
        () => props.historyOpen || false,
      )}

      <div class="w-6 border-t border-border my-1" />

      {/* Graph — desktop keeps the dense 5-node X glyph (more brand
          presence than the simpler mobile-menu variant). */}
      {railBtn("graph",
        <svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
          <circle cx="12" cy="12" r="2.5" fill="currentColor" opacity="0.8" />
          <circle cx="5" cy="5" r="2" />
          <circle cx="19" cy="7" r="2" />
          <circle cx="6" cy="19" r="2" />
          <circle cx="19" cy="17" r="2" />
          <line x1="10" y1="10.5" x2="7" y2="6.5" />
          <line x1="14" y1="11" x2="17" y2="8.5" />
          <line x1="10.5" y1="13.5" x2="8" y2="17.5" />
          <line x1="14" y1="13.5" x2="17.5" y2="15.5" />
        </svg>,
        "Graph View", "Ctrl+G",
        props.onGraphToggle,
        () => props.graphOpen,
        undefined,
        "launcher",
      )}

      {/* AI Chat — Corvux brand mark (desktop only — mobile uses a
          simpler speech-bubble glyph in MobileBottomBar). */}
      {railBtn("chat",
        <CorvuxLogo alt="" width="22" height="22" class="object-contain" />,
        "Corvux Agent", "Ctrl+L",
        props.onChatToggle,
        () => props.chatOpen,
        "text-purple",
        "launcher",
      )}

      <div class="flex-1" />

      {/* Theme toggle — label names the theme the click will switch to,
          since the button cycles through all THEME_OPTIONS (not a binary
          dark/light flip). */}
      <Tooltip label={`Switch to ${settingsStore.nextTheme().name}`} shortcut="Ctrl+Shift+T">
        <button
          title={`Switch to ${settingsStore.nextTheme().name}`}
          aria-label="Cycle theme"
          class="w-10 h-10 flex items-center justify-center rounded-lg text-text-muted hover:text-text-primary hover:bg-bg-hover transition-colors"
          onClick={() => settingsStore.toggleTheme()}
        >
          {settingsStore.theme() === "dark" ? (
            <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
              <circle cx="12" cy="12" r="5" />
              <line x1="12" y1="1" x2="12" y2="3" />
              <line x1="12" y1="21" x2="12" y2="23" />
              <line x1="4.22" y1="4.22" x2="5.64" y2="5.64" />
              <line x1="18.36" y1="18.36" x2="19.78" y2="19.78" />
              <line x1="1" y1="12" x2="3" y2="12" />
              <line x1="21" y1="12" x2="23" y2="12" />
              <line x1="4.22" y1="19.78" x2="5.64" y2="18.36" />
              <line x1="18.36" y1="5.64" x2="19.78" y2="4.22" />
            </svg>
          ) : (
            <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
              <path d="M21 12.79A9 9 0 1111.21 3 7 7 0 0021 12.79z" />
            </svg>
          )}
        </button>
      </Tooltip>

      {/* Settings */}
      <Tooltip label="Settings">
        <button
          title="Settings"
          aria-label="Settings"
          class="w-10 h-10 flex items-center justify-center rounded-lg text-text-muted hover:text-text-primary hover:bg-bg-hover transition-colors"
          onClick={props.onSettingsClick}
        >
          <svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
            <path d="M10.325 4.317c.426-1.756 2.924-1.756 3.35 0a1.724 1.724 0 002.573 1.066c1.543-.94 3.31.826 2.37 2.37a1.724 1.724 0 001.066 2.573c1.756.426 1.756 2.924 0 3.35a1.724 1.724 0 00-1.066 2.573c.94 1.543-.826 3.31-2.37 2.37a1.724 1.724 0 00-2.573 1.066c-.426 1.756-2.924 1.756-3.35 0a1.724 1.724 0 00-2.573-1.066c-1.543.94-3.31-.826-2.37-2.37a1.724 1.724 0 00-1.066-2.573c-1.756-.426-1.756-2.924 0-3.35a1.724 1.724 0 001.066-2.573c-.94-1.543.826-3.31 2.37-2.37.996.608 2.296.07 2.572-1.065z" />
            <circle cx="12" cy="12" r="3" />
          </svg>
        </button>
      </Tooltip>
    </div>
  );
}
