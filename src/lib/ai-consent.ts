// Explicit per-provider permission before chat content goes to a third-party
// AI service (App Store guideline 5.1.2(i): name the service, say what is
// sent, and ask BEFORE sending). The Rust side (`commands/ai/consent.rs`)
// enforces it at the one place a chat provider is built; this module is the
// prompt in front of that gate. Dependencies are injected so it is testable
// without Tauri.

import type { AiConsentTarget } from "./bindings";
import { commands, unwrap } from "./invoke";
import { confirmDialog } from "./confirm";

export class AiConsentDeclined extends Error {}

/** Must stay in step with what `chat_send` really includes (system prompt:
 *  roost index of note titles, tag inventory, AGENTS.md, memory.md, the open
 *  note, mentioned notes; plus whatever the agent's tools return — they can
 *  search and read any note, tasks and reminders). Locked by
 *  tests/unit/ai-consent.test.ts. */
export function consentPromptText(t: AiConsentTarget): string {
  return [
    `Send your chats to ${t.provider_name}?`,
    "",
    `To answer you, Corvux sends ${t.provider_name} (${t.host}):`,
    "• your messages and the chat so far",
    "• the note you have open, your note titles and tags",
    "• your AGENTS.md and the assistant's memory notes",
    "• whatever the assistant looks up for you. It can search and read any note in this roost, including your tasks and reminders.",
    ...(t.detail ? ["", t.detail] : []),
    "",
    `${t.provider_name}'s own privacy policy applies to this data. Corvux itself never receives it.`,
    "",
    "Allow? You can withdraw this any time in Settings → Privacy.",
  ].join("\n");
}

export interface ConsentDeps {
  check: () => Promise<AiConsentTarget | null>;
  grant: (id: string) => Promise<void>;
  confirm: (message: string) => Promise<boolean>;
}

const tauriDeps: ConsentDeps = {
  check: () => unwrap(commands.aiConsentCheck()),
  grant: async (id) => {
    await unwrap(commands.aiConsentGrant(id));
  },
  confirm: confirmDialog,
};

/** Ask for permission if the active provider still needs it. Resolves when
 *  sending may proceed; throws `AiConsentDeclined` when the user says no. */
export async function ensureAiConsent(
  deps: ConsentDeps = tauriDeps,
): Promise<"not-needed" | "granted"> {
  const target = await deps.check();
  if (!target) return "not-needed";
  if (!(await deps.confirm(consentPromptText(target)))) {
    throw new AiConsentDeclined(
      `Nothing left your device. Corvux needs your permission before sending anything to ${target.provider_name}. ` +
        `Send again to allow it, or choose another provider in Settings → AI.`,
    );
  }
  await deps.grant(target.id);
  return "granted";
}
