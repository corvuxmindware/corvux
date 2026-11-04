/// Pure task quick-add helpers, extracted from TasksPanel so the 📅/⏰
/// markdown contract (ROOST-CONVENTIONS.md) and the mobile date/time button
/// label are unit-testable without rendering the component.

export interface QuickAddDraft {
  /// Raw text the user typed (may already contain inline 📅/⏰ markers).
  text: string;
  /// "YYYY-MM-DD" from the date picker, or "" when unset.
  date: string;
  /// "HH:MM" from the time picker, or "" when unset.
  time: string;
}

/// Build the final markdown task body. Appends `📅 YYYY-MM-DD` and
/// `⏰ HH:MM` per the roost spec. If the user already typed those emoji
/// markers in the text, their inline metadata wins (no double-stamp) so
/// power-users don't have to reach for the pickers. `⏰` requires a paired
/// `📅`, so a time picked without a date defaults the date to `today`
/// rather than being silently dropped by the indexer. Returns "" for empty
/// text (nothing to add).
export function composeTaskBody(draft: QuickAddDraft, today: string): string {
  const text = draft.text.trim();
  if (!text) return "";
  const parts: string[] = [text];
  let date = draft.date;
  const time = draft.time;
  if (time && !date && !text.includes("📅")) {
    date = today;
  }
  if (date && !text.includes("📅")) {
    parts.push(`📅 ${date}`);
  }
  if (time && !text.includes("⏰")) {
    parts.push(`⏰ ${time}`);
  }
  return parts.join(" ");
}

/// Local-timezone ISO date ("YYYY-MM-DD"). `new Date().toISOString()` would
/// shift to UTC and land on the wrong day near midnight, so we read local
/// fields. Accepts an injected `now` for deterministic tests.
export function todayLocalIso(now: Date = new Date()): string {
  const y = now.getFullYear();
  const m = String(now.getMonth() + 1).padStart(2, "0");
  const day = String(now.getDate()).padStart(2, "0");
  return `${y}-${m}-${day}`;
}
