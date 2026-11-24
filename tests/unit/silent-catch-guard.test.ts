/**
 * Ratchet guard: no NEW "log-only" catch under `src/`.
 *
 * A "log-only catch" is `catch (e) { log.error(...) }` (or `.catch(e =>
 * log.warn(...))`) whose body does nothing else — no toast, no alert, no
 * rethrow, no state change. When the failed call was started by a user
 * gesture, that is a silent failure: the SAF device matrix (7–8 Nov 2026)
 * found a refused delete, a refused create and a failed unpair that all
 * looked like "nothing happened" on a phone. The fix is
 * `reportActionError` (src/lib/action-error.ts), which logs AND toasts.
 *
 * This is the frontend twin of `src-tauri/tests/sync_invariants.rs`:
 * per-file baselines. It fails on any NEW site, and on an unratcheted
 * improvement (so a fix can't quietly regress back).
 *
 * If you are adding a log-only catch on purpose (a probe, a best-effort
 * cache write, an optional nicety), raise that file's baseline below and say
 * why in the PR. If a user pressed something to get here, use
 * `reportActionError` instead.
 *
 * Regenerate the numbers: `UPDATE_BASELINE=/tmp/counts.json pnpm vitest run
 * tests/unit/silent-catch-guard.test.ts` writes the current per-file counts
 * to that path.
 */

import { describe, it, expect } from "vitest";
import { readdirSync, readFileSync, statSync, writeFileSync } from "node:fs";
import { join, relative, sep } from "node:path";

// vitest runs from the app root (`Corvux/`); under happy-dom `import.meta.url`
// is not a file: URL, so don't derive the path from it.
const SRC = join(process.cwd(), "src");

/**
 * Remaining log-only catches, by file. Each is background/best-effort work
 * with no user gesture behind it, EXCEPT the "persistence" group, which is a
 * KNOWN, DEFERRED gap — see the note under it.
 */
const BASELINE: Record<string, number> = {
  // --- probes / background reads / listeners (no user gesture) -----------
  "App.tsx": 1,
  "app/effects.ts": 10, // startup + backend-event listeners
  "components/Audio/AudioPlayer.tsx": 1, // peak extraction is a nicety
  "components/Onboarding/Onboarding.tsx": 1,
  "components/Settings/SyncSection/QrScanner.tsx": 1, // enumerate/select falls back to the default stream
  "components/Settings/SyncSection/QueuePanel.tsx": 2, // panel stays "unloaded" before sync_init
  "components/Settings/SyncSection/index.tsx": 1, // best-effort per-peer unpair inside "reset sync"
  "lib/audio-recorder.ts": 1,
  "lib/browser-roost.ts": 1, // browser dev-mock only
  "lib/notification-actions.ts": 2,
  "stores/bugReport.ts": 2,
  "stores/graph.ts": 3,
  "stores/roosts.ts": 2,
  "stores/settings.ts": 6, // loads/probes (+ the saved-key status refresh: background, never fails a save)
  "stores/sync.ts": 4, // listeners + status seed
  "stores/tabs.ts": 2,
  "stores/tasks.ts": 1,
  "stores/updater.ts": 2,
  // welcome note, refreshFiles, FTS5 remove, and `deleteNote` (no caller today —
  // revisit the last one if a caller appears).
  "stores/roost.ts": 4,

  // --- PERSISTENCE: DEFERRED, NOT FIXED ----------------------------------
  // A background write that fails is logged and the user is told nothing:
  // a bookmark toggled, a roost setting changed, or a chat turn saved while
  // the provider is unreachable is LOST SILENTLY on the next launch. A toast
  // per failed write would storm during chat streaming, so this needs a
  // coalescing design (one "changes are not being saved" notice), not a
  // one-line swap. Tracked in the Known open items of
  // docs/validation/android-saf-validation-2026-11-07.md.
  "stores/bookmarks.ts": 1,
  "stores/roostSettings.ts": 1,
  "stores/chat.ts": 8,
};

// ---------------------------------------------------------------------------
// Scanner
// ---------------------------------------------------------------------------

function listSourceFiles(dir: string): string[] {
  const out: string[] = [];
  for (const name of readdirSync(dir)) {
    const full = join(dir, name);
    if (statSync(full).isDirectory()) out.push(...listSourceFiles(full));
    else if (/\.(ts|tsx)$/.test(name) && !name.endsWith(".d.ts")) out.push(full);
  }
  return out;
}

/** Index just past the closing quote of the string starting at `i`. */
function skipQuoted(src: string, i: number): number {
  const q = src[i];
  let j = i + 1;
  while (j < src.length) {
    if (src[j] === "\\") j += 2;
    else if (src[j] === q) return j + 1;
    else if (src[j] === "\n" && q !== "`") return j; // unterminated; bail
    else j += 1;
  }
  return j;
}

/** Index just past the template literal starting at `i` (handles `${…}`). */
function skipTemplate(src: string, i: number): number {
  let j = i + 1;
  while (j < src.length) {
    if (src[j] === "\\") j += 2;
    else if (src[j] === "`") return j + 1;
    else if (src[j] === "$" && src[j + 1] === "{") j = findBlockEnd(src, j + 1) + 1;
    else j += 1;
  }
  return j;
}

/** Index of the `}` matching the `{` at `open`, skipping strings/comments. */
function findBlockEnd(src: string, open: number): number {
  let depth = 0;
  for (let i = open; i < src.length; i += 1) {
    const ch = src[i];
    if (ch === '"' || ch === "'") i = skipQuoted(src, i) - 1;
    else if (ch === "`") i = skipTemplate(src, i) - 1;
    else if (ch === "/" && src[i + 1] === "/") {
      while (i < src.length && src[i] !== "\n") i += 1;
    } else if (ch === "/" && src[i + 1] === "*") {
      const end = src.indexOf("*/", i + 2);
      i = end === -1 ? src.length : end + 1;
    } else if (ch === "{") depth += 1;
    else if (ch === "}") {
      depth -= 1;
      if (depth === 0) return i;
    }
  }
  return src.length;
}

/** Index of the `)` matching the `(` at `open`, skipping strings/comments. */
function findParenEnd(src: string, open: number): number {
  let depth = 0;
  for (let i = open; i < src.length; i += 1) {
    const ch = src[i];
    if (ch === '"' || ch === "'") i = skipQuoted(src, i) - 1;
    else if (ch === "`") i = skipTemplate(src, i) - 1;
    else if (ch === "(") depth += 1;
    else if (ch === ")") {
      depth -= 1;
      if (depth === 0) return i;
    }
  }
  return src.length;
}

/** Drop `//` and `/* *​/` comments, keeping strings intact. */
function stripComments(src: string): string {
  let out = "";
  for (let i = 0; i < src.length; i += 1) {
    const ch = src[i];
    if (ch === '"' || ch === "'") {
      const end = skipQuoted(src, i);
      out += src.slice(i, end);
      i = end - 1;
    } else if (ch === "`") {
      const end = skipTemplate(src, i);
      out += src.slice(i, end);
      i = end - 1;
    } else if (ch === "/" && src[i + 1] === "/") {
      while (i < src.length && src[i] !== "\n") i += 1;
      out += "\n";
    } else if (ch === "/" && src[i + 1] === "*") {
      const end = src.indexOf("*/", i + 2);
      i = end === -1 ? src.length : end + 1;
    } else out += ch;
  }
  return out;
}

const LOG_CALL = /\blog\.(?:debug|info|warn|error)\(/;

/** True when `body` is nothing but one or more `log.*(…)` calls. */
function isLogOnly(body: string): boolean {
  let rest = stripComments(body);
  let removed = 0;
  for (;;) {
    const m = LOG_CALL.exec(rest);
    if (!m) break;
    const open = m.index + m[0].length - 1;
    const end = findParenEnd(rest, open);
    rest = rest.slice(0, m.index) + rest.slice(end + 1);
    removed += 1;
  }
  return removed > 0 && rest.replace(/[;\s]/g, "") === "";
}

/** Number of log-only catches in `source`. Exported shape is test-internal. */
export function countLogOnlyCatches(source: string): number {
  const src = stripComments(source);
  let count = 0;

  // `catch (e) { log.x(...) }` / `catch { log.x(...) }`
  const catchRe = /\bcatch\b\s*(?:\(\s*[\w$]+(?:\s*:\s*[\w$.<>[\]| ]+)?\s*\))?\s*\{/g;
  for (let m = catchRe.exec(src); m; m = catchRe.exec(src)) {
    const open = m.index + m[0].length - 1;
    const end = findBlockEnd(src, open);
    if (isLogOnly(src.slice(open + 1, end))) count += 1;
  }

  // `.catch((e) => log.x(...))` / `.catch(e => log.x(...))`
  const promiseRe =
    /\.catch\(\s*\(?\s*[\w$]*(?:\s*:\s*[\w$.<>[\]| ]+)?\s*\)?\s*=>\s*log\.(?:debug|info|warn|error)\(/g;
  count += (src.match(promiseRe) ?? []).length;

  return count;
}

function scanAll(): Record<string, number> {
  const counts: Record<string, number> = {};
  for (const file of listSourceFiles(SRC)) {
    const n = countLogOnlyCatches(readFileSync(file, "utf8"));
    if (n > 0) counts[relative(SRC, file).split(sep).join("/")] = n;
  }
  return counts;
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

describe("countLogOnlyCatches (the scanner itself)", () => {
  it("flags a catch whose body is only a log call", () => {
    expect(countLogOnlyCatches(`try { a(); } catch (e) { log.error("s", "m", e); }`)).toBe(1);
    expect(countLogOnlyCatches(`try { a(); } catch { log.warn("s", "m"); }`)).toBe(1);
  });

  it("flags several log calls with nested parens and template strings", () => {
    const src = "try { a(); } catch (e) { log.warn('s', `x ${f(1)} {y}`, e); log.error('s', 'm'); }";
    expect(countLogOnlyCatches(src)).toBe(1);
  });

  it("does NOT flag a catch that also tells the user", () => {
    expect(countLogOnlyCatches(`try{a()}catch(e){log.error("s","m",e);toastStore.showError("x",e)}`)).toBe(0);
    expect(countLogOnlyCatches(`try{a()}catch(e){log.error("s","m",e);alert("x")}`)).toBe(0);
    expect(countLogOnlyCatches(`try{a()}catch(e){reportActionError("x",e,{code:"C",module:"m"})}`)).toBe(0);
  });

  it("does NOT flag a catch that rethrows or sets state", () => {
    expect(countLogOnlyCatches(`try{a()}catch(e){log.error("s","m",e);throw e}`)).toBe(0);
    expect(countLogOnlyCatches(`try{a()}catch(e){log.error("s","m",e);setError("x")}`)).toBe(0);
  });

  it("ignores catch inside comments and an empty swallow", () => {
    expect(countLogOnlyCatches(`// catch (e) { log.error("s","m",e) }\nconst x = 1;`)).toBe(0);
    expect(countLogOnlyCatches(`try{a()}catch{ /* ignore */ }`)).toBe(0);
  });

  it("flags the promise form but not one that handles the error", () => {
    expect(countLogOnlyCatches(`p.catch((e: unknown) => log.error("s", "m", e));`)).toBe(1);
    expect(countLogOnlyCatches(`p.catch(e => log.warn("s", "m", e));`)).toBe(1);
    expect(countLogOnlyCatches(`p.catch((e) => { toastStore.showError("x", e); });`)).toBe(0);
  });
});

describe("silent-catch ratchet over src/", () => {
  it("has no new log-only catch and no unratcheted improvement", () => {
    const actual = scanAll();

    if (process.env.UPDATE_BASELINE) {
      // Dump mode: write the current counts to the given path and skip the
      // comparison (vitest swallows console output for passing tests).
      writeFileSync(process.env.UPDATE_BASELINE, JSON.stringify(actual, null, 2));
      return;
    }

    const files = new Set([...Object.keys(actual), ...Object.keys(BASELINE)]);
    const worse: string[] = [];
    const better: string[] = [];
    for (const f of [...files].sort()) {
      const a = actual[f] ?? 0;
      const b = BASELINE[f] ?? 0;
      if (a > b) worse.push(`${f}: ${a} log-only catch(es), baseline ${b}`);
      if (a < b) better.push(`${f}: ${a} now, baseline ${b}`);
    }

    expect(
      worse,
      "NEW log-only catch. If a user gesture led here, surface the error with " +
        "reportActionError() (src/lib/action-error.ts). If it is genuinely " +
        "background work, raise the baseline in this file and say why.",
    ).toEqual([]);
    expect(
      better,
      "A log-only catch was fixed — lower its baseline so it cannot regress.",
    ).toEqual([]);
  });

  it("every site fixed by reportActionError stays fixed", () => {
    // The user-gesture surfaces this class was found in. Each must have NO
    // log-only catch left (baseline absent = 0) — asserted by name so
    // that raising one of these in BASELINE is a conscious, visible act.
    const mustBeZero = [
      "components/Sidebar/Sidebar.tsx",
      "components/Settings/AISection/index.tsx",
      "components/Settings/AISection/GeminiProvider.tsx",
      "components/Settings/AILearningPanel.tsx",
      "components/Settings/SkillsPanel.tsx",
      "components/Roost/CreateRoostModal.tsx",
    ];
    for (const f of mustBeZero) {
      expect(BASELINE[f] ?? 0, `${f} must not be baselined`).toBe(0);
      expect(
        countLogOnlyCatches(readFileSync(join(SRC, f), "utf8")),
        `${f} has a log-only catch again`,
      ).toBe(0);
    }
  });
});
