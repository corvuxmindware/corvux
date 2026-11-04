// Two-action notifications for task-due reminders.
//
// On app startup we call `registerActionTypes` to declare two
// buttons — "Complete" and "Snooze 15m" — under the `task-due`
// action-type id. The Rust scheduler in `commands/task_notify.rs`
// attaches that same id to every notification it fires (the
// `TASK_DUE_ACTION_TYPE` constant must stay in sync with the value
// here — they're grep-pairable).
//
// When the user taps a button, the plugin emits `actionPerformed`
// which we listen to via `onAction`. The notification payload's
// `extra` map carries enough context (note_path + source_line +
// text_anchor) to call the existing `complete_task` /
// `reschedule_task` IPCs without any further lookup.
//
// Bug awareness (2026-05-20):
//   - Issue tauri-apps/plugins-workspace#2968 reports that button
//     labels don't render on Android. iOS support is undocumented but
//     the API has been stable since 2.0 and UNNotificationAction is
//     the canonical iOS surface.
//   - On-device verification is part of the iOS / Android todo.

import {
  onAction,
  registerActionTypes,
} from "@tauri-apps/plugin-notification";
import { appInvoke as invoke, isTauri } from "./invoke";
import { log } from "./log";
import { isMobile } from "./platform";

/** Must match `TASK_DUE_ACTION_TYPE` in `task_notify.rs`. */
export const TASK_DUE_ACTION_TYPE = "task-due";

const ACTION_COMPLETE = "complete";
const ACTION_SNOOZE = "snooze-15";

/** Minutes to push a task forward when the user taps "Snooze 15m". */
const SNOOZE_MINUTES = 15;

interface NotificationExtras {
  note_id?: string;
  note_path?: string;
  // Arrives as a STRING from the Rust scheduler: notification `extra` values
  // must be strings for iOS (decoded as [String: String]). Tolerate a number
  // too, for robustness across plugin/platform variations.
  source_line?: string | number;
  text_anchor?: string;
}

/// Public entry — call once during app startup. Idempotent at the
/// plugin level; calling twice replaces the prior registration.
///
/// Desktop platforms (macOS, Windows, Linux) don't implement the
/// `register_action_types` command — action buttons on notifications
/// are a mobile concept (UNNotificationCategory on iOS, notification
/// channels on Android). Gating here avoids the "Command not found"
/// warning on every desktop boot.
export async function installTaskNotificationActions(): Promise<void> {
  if (!isTauri) return;
  if (!isMobile()) return;
  try {
    await registerActionTypes([
      {
        id: TASK_DUE_ACTION_TYPE,
        actions: [
          {
            id: ACTION_COMPLETE,
            title: "Complete",
            foreground: false,
          },
          {
            id: ACTION_SNOOZE,
            title: "Snooze 15m",
            foreground: false,
          },
        ],
      },
    ]);
  } catch (e) {
    log.warn("notif-actions", "registerActionTypes failed", e);
    return;
  }
  try {
    await onAction((payload) => {
      void dispatchAction(payload);
    });
  } catch (e) {
    log.warn("notif-actions", "onAction listen failed", e);
  }
}

/// Pure dispatcher — extracted so it can be unit-tested without a
/// live Tauri runtime. Returns the resolved tool name + args so tests
/// can assert what we'd call without actually invoking IPC.
export interface PlannedDispatch {
  kind: "complete" | "snooze" | "skip";
  ipc?: string;
  args?: Record<string, unknown>;
  reason?: string;
}

export function planDispatch(
  payload: unknown,
  now: Date = new Date(),
): PlannedDispatch {
  // The plugin payload shape varies a little by version + platform.
  // We accept either `extra: {...}` or the extras spread at the top
  // level; both have been observed in the wild.
  const p = (payload ?? {}) as Record<string, unknown>;
  const actionId =
    typeof p.actionId === "string"
      ? (p.actionId as string)
      : typeof p.action === "string"
        ? (p.action as string)
        : undefined;

  const extras: NotificationExtras = {
    ...((p.extra as NotificationExtras | undefined) ?? {}),
    ...(p as NotificationExtras),
  };

  const notePath = extras.note_path;
  const textAnchor = extras.text_anchor;

  if (typeof notePath !== "string" || !notePath) {
    return { kind: "skip", reason: "missing note_path in payload" };
  }
  // source_line comes through as a STRING (iOS requires string `extra`
  // values — see task_notify.rs). Coerce to a number; reject if absent or
  // non-numeric. Tolerate a real number too. Negative = frontmatter task.
  const rawSourceLine = extras.source_line;
  const sourceLine =
    typeof rawSourceLine === "number"
      ? rawSourceLine
      : typeof rawSourceLine === "string" && rawSourceLine.trim() !== ""
        ? Number(rawSourceLine)
        : NaN;
  if (!Number.isInteger(sourceLine)) {
    return { kind: "skip", reason: "missing/invalid source_line in payload" };
  }

  switch (actionId) {
    case ACTION_COMPLETE:
      return {
        kind: "complete",
        ipc: "complete_task",
        args: {
          notePath,
          sourceLine,
          completed: true,
          contentAnchor: textAnchor ?? "",
        },
      };
    case ACTION_SNOOZE: {
      // The reschedule_task IPC takes a new date + time string. Push
      // the due moment forward `SNOOZE_MINUTES` from "now" so the
      // user always gets the same lead from when they tapped Snooze.
      const next = new Date(now.getTime() + SNOOZE_MINUTES * 60_000);
      const yyyy = next.getFullYear();
      const mm = String(next.getMonth() + 1).padStart(2, "0");
      const dd = String(next.getDate()).padStart(2, "0");
      const hh = String(next.getHours()).padStart(2, "0");
      const min = String(next.getMinutes()).padStart(2, "0");
      return {
        kind: "snooze",
        ipc: "reschedule_task",
        args: {
          notePath,
          textAnchor: textAnchor ?? "",
          newDate: `${yyyy}-${mm}-${dd}`,
          newTime: `${hh}:${min}`,
        },
      };
    }
    default:
      return { kind: "skip", reason: `unknown actionId: ${actionId}` };
  }
}

async function dispatchAction(payload: unknown): Promise<void> {
  const plan = planDispatch(payload);
  if (plan.kind === "skip" || !plan.ipc) {
    console.debug("[notif-actions] skipped:", plan.reason);
    return;
  }
  try {
    await invoke<void>(plan.ipc, plan.args ?? {});
  } catch (e) {
    log.warn("notif-actions", `${plan.ipc} failed`, e);
  }
}
