import { describe, it, expect } from "vitest";
import { readFileSync, readdirSync, statSync, existsSync } from "node:fs";
import { join, relative, resolve } from "node:path";
import { defaultReadOnly } from "../../src/stores/tabs";

/**
 * Ratchet guard for the "new note opens LOCKED" bug class.
 *
 * `tabStore.openTab` defaults to read mode on desktop. That default is right
 * for opening an EXISTING note and wrong for one the caller just created —
 * the user asked for a new note, so they want a caret in it, not a padlock to
 * hunt for. On mobile it was worse than wrong: the only unlock control is a
 * small icon in the top bar, so the mobile "+ note" flow
 * (`NewNotePrompt.tsx`) dropped people on a locked blank page with no way to
 * type. Non-technical testers reported this as "hard to keep notes".
 *
 * Six call sites had drifted into the bare form. Rather than fix six lines and
 * wait for the seventh, this test fails the build whenever a site creates a
 * note and then opens it WITHOUT stating the mode. It intentionally does not
 * care which mode you pick — Onboarding legitimately wants `readOnly: true`
 * for its read-me primer. It only demands that the choice be deliberate.
 */

// Vitest sets the worker cwd to the project root (where vitest.config.ts
// lives), so `src/` resolves from there. `import.meta.url` is not a file:
// URL under this config, hence the cwd form.
const SRC = resolve(process.cwd(), "src");

/** Every .ts/.tsx file under src/, recursively. */
function sourceFiles(dir: string): string[] {
  const out: string[] = [];
  for (const entry of readdirSync(dir)) {
    const full = join(dir, entry);
    if (statSync(full).isDirectory()) {
      out.push(...sourceFiles(full));
    } else if (/\.tsx?$/.test(full)) {
      out.push(full);
    }
  }
  return out;
}

/** Creation calls whose resulting path is then opened in a tab. */
const CREATE_CALL = /\b(createNewNote|createDailyNote)\s*\(/;
/** `openTab(...)` up to the closing paren of its FIRST argument. */
const OPEN_TAB = /\.openTab\(\s*[^)]*\)/g;

/**
 * Scan forward from a creation call for the `openTab` that consumes it.
 * A window of 12 lines covers every shape in the codebase today: the direct
 * `const path = await createNewNote(); openTab(path)` form, the
 * `.then((path) => openTab(path))` form, and the daily-note form with an
 * `indexPath` + `openRoost` refresh in between.
 */
const WINDOW = 12;

describe("create-then-open sites state the editor mode explicitly", () => {
  it("never opens a freshly created note with a bare openTab()", () => {
    // Fail loudly rather than vacuously pass if the cwd assumption breaks.
    expect(existsSync(SRC), `source dir not found at ${SRC}`).toBe(true);

    const offenders: string[] = [];

    for (const file of sourceFiles(SRC)) {
      const lines = readFileSync(file, "utf8").split("\n");

      lines.forEach((line, i) => {
        if (!CREATE_CALL.test(line)) return;

        const window = lines.slice(i, i + WINDOW).join("\n");
        for (const call of window.match(OPEN_TAB) ?? []) {
          if (call.includes("readOnly")) continue;
          offenders.push(
            `${relative(SRC, file)}:${i + 1} — ${call.trim()} follows ` +
              `${line.trim()}`,
          );
        }
      });
    }

    // Empty array in the message so a failure prints exactly which sites
    // regressed rather than just "expected 3 to be 0".
    expect(offenders).toEqual([]);
  });

  it("the guard can actually see a violation (self-test)", () => {
    // Guard against the guard silently passing because the regexes stopped
    // matching real code. Feed it the exact shape the bug had.
    const buggy = [
      "const path = await roostStore.createNewNote(trimmed);",
      "if (path) tabStore.openTab(path);",
    ].join("\n");

    expect(CREATE_CALL.test(buggy.split("\n")[0])).toBe(true);
    const found = buggy.match(OPEN_TAB) ?? [];
    expect(found).toHaveLength(1);
    expect(found[0].includes("readOnly")).toBe(false);
  });
});

describe("defaultReadOnly", () => {
  it("locks on desktop so the tab-bar toggle stays the read/write control", () => {
    expect(defaultReadOnly(false)).toBe(true);
  });

  it("unlocks on mobile — the first tap must open the keyboard", () => {
    expect(defaultReadOnly(true)).toBe(false);
  });
});
