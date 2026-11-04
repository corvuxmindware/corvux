import { createSignal, createRoot } from "solid-js";
import { commands, unwrap } from "../lib/invoke";
import { log } from "../lib/log";
import type { Theme } from "./settings";

// Roost-portable settings — written to `<roost>/.corvux/settings.json` so they
// follow the roost when it's opened on a different device. Mirrors Obsidian's
// `.obsidian/appearance.json` convention but bundles all portable settings
// under one versioned schema instead of one file per feature (one disk write
// per change is cheaper than syncing N files; we can split later if a single
// blob ever becomes a merge hotspot).
//
// What goes here vs. localStorage / app data:
//   Roost file (here):  visual identity (theme, accent), per-roost behavior
//                       (daily-notes folder, default agent profile when added)
//   Per-device:         hardware-dependent prefs (font size, ML backend),
//                       secrets (API keys), session state (open tabs),
//                       sync identity, last-opened roost
//
// Anti-pattern we are NOT replicating: a single shared `workspace.json`. Open
// tabs/pane layout is device-specific — multi-device sync of one file makes
// iCloud spawn hundreds of conflict copies (`workspace 1.json`,
// `workspace 2.json`, ...). When we sync workspace state, namespace by device.

const SETTINGS_VERSION = 1;

export interface RoostAppearance {
  theme: Theme | null;
  accent_color: string | null;
}

/// Configurable target paths for the Tasks pane. Honors Locked Decision
/// #15 (user owns folder structure) — no path may be hardcoded; defaults
/// match the daily-note template shipped in `create_daily_note`.
export interface RoostTasks {
  /// Folder where today's daily note lives. Quick-add writes its
  /// `- [ ]` line into `<daily_note_folder>/{YYYY-MM-DD}.md`.
  daily_note_folder: string;
  /// Roost-relative path for the catch-all inbox file when the user
  /// chooses Inbox over today's daily note. Reserved — slice 4 always
  /// targets the daily note; an inbox toggle ships in slice 4.5.
  inbox_path: string;
}

export interface RoostSettings {
  version: number;
  appearance: RoostAppearance;
  tasks: RoostTasks;
}

function defaultTasksSettings(): RoostTasks {
  return {
    daily_note_folder: "Daily Notes",
    inbox_path: "Inbox.md",
  };
}

function emptySettings(): RoostSettings {
  return {
    version: SETTINGS_VERSION,
    appearance: { theme: null, accent_color: null },
    tasks: defaultTasksSettings(),
  };
}

function settingsPath(roostPath: string): string {
  return `${roostPath}/.corvux/settings.json`;
}

function createRoostSettingsStore() {
  const [settings, setSettings] = createSignal<RoostSettings>(emptySettings());
  const [loaded, setLoaded] = createSignal(false);
  let currentRoostPath: string | null = null;
  let loadGeneration = 0;

  /** Load `.corvux/settings.json` for the given roost. Missing file is not
   *  an error — first-time roosts just start with empty settings. */
  async function load(roostPath: string) {
    const generation = ++loadGeneration;
    currentRoostPath = roostPath;
    setLoaded(false);
    try {
      const raw = await unwrap(commands.readFile(settingsPath(roostPath)));
      if (generation !== loadGeneration || currentRoostPath !== roostPath) return;
      const parsed = JSON.parse(raw) as Partial<RoostSettings>;
      const fallbackTasks = defaultTasksSettings();
      setSettings({
        version: parsed.version ?? SETTINGS_VERSION,
        appearance: {
          theme: (parsed.appearance?.theme as Theme | null) ?? null,
          accent_color: parsed.appearance?.accent_color ?? null,
        },
        tasks: {
          daily_note_folder:
            parsed.tasks?.daily_note_folder ?? fallbackTasks.daily_note_folder,
          inbox_path: parsed.tasks?.inbox_path ?? fallbackTasks.inbox_path,
        },
      });
    } catch {
      if (generation !== loadGeneration || currentRoostPath !== roostPath) return;
      // No settings file yet — fresh roost or pre-migration.
      setSettings(emptySettings());
    }
    setLoaded(true);
  }

  /** Merge updates into the current settings and persist to disk. The
   *  `.corvux/` dir is auto-created by the file IPC layer. */
  async function save(updates: {
    appearance?: Partial<RoostAppearance>;
    tasks?: Partial<RoostTasks>;
  }) {
    if (!currentRoostPath) return;
    const roostPath = currentRoostPath;
    const merged: RoostSettings = {
      version: SETTINGS_VERSION,
      appearance: {
        ...settings().appearance,
        ...(updates.appearance ?? {}),
      },
      tasks: {
        ...settings().tasks,
        ...(updates.tasks ?? {}),
      },
    };
    setSettings(merged);
    try {
      await unwrap(
        commands.writeFile(settingsPath(roostPath), JSON.stringify(merged, null, 2)),
      );
    } catch (e) {
      log.error("roostSettings", "save failed", e);
    }
  }

  function clear() {
    loadGeneration += 1;
    currentRoostPath = null;
    setSettings(emptySettings());
    setLoaded(false);
  }

  return {
    settings,
    loaded,
    load,
    save,
    clear,
    /** Convenience accessor — returns the roost's theme override, or null
     *  if the roost doesn't override the device default. */
    theme: () => settings().appearance.theme,
  };
}

export const roostSettingsStore = createRoot(createRoostSettingsStore);
