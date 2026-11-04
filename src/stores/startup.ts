import { createSignal, createRoot } from "solid-js";

/**
 * Startup phases run sequentially after the roost opens. Each phase is one
 * unit of post-open work that the user can see progress through. The order
 * is light → heavy so the file tree + editor become usable before the
 * heavier subsystems (sync engine, embed pipeline) start competing for the
 * Rust runtime. Graph + FTS warm lazily on first use, not in this chain.
 */
export type StartupPhase =
  | "idle"
  | "roost"
  | "settings"
  | "history"
  | "sync"
  | "ready";

const PHASE_LABELS: Record<StartupPhase, string> = {
  idle: "",
  roost: "Opening roost…",
  settings: "Loading settings…",
  history: "Loading chat history…",
  sync: "Starting sync…",
  ready: "",
};

function createStartupStore() {
  const [phase, setPhase] = createSignal<StartupPhase>("idle");
  const isWarming = () => {
    const p = phase();
    return p !== "idle" && p !== "ready";
  };
  const label = () => PHASE_LABELS[phase()];
  return {
    phase,
    setPhase,
    isWarming,
    label,
  };
}

export const startupStore = createRoot(createStartupStore);
