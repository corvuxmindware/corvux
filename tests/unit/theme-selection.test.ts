/**
 * Guard: text selection must stay visibly stronger than the hover tint,
 * in EVERY theme.
 *
 * The bug this freezes: `.cm-selectionBackground` was styled with
 * `--color-bg-hover` / `--color-bg-active` — the 6%/12% tints meant to
 * whisper under a pointer. At that alpha, selected text was nearly
 * indistinguishable from unselected text, so you could not see what you had
 * grabbed. Selection now has dedicated `--color-selection` tokens at roughly
 * 2.5x the hover alpha.
 *
 * This is a CSS bug, so the regression test is structural rather than
 * behavioural: it parses the theme blocks and asserts the contrast
 * relationship holds. That catches the two ways this regresses — someone adds
 * a 7th theme and forgets the selection tokens (selection silently falls back
 * to `unset` there), or someone "tidies up" by pointing selection back at the
 * hover tint.
 */

import { describe, it, expect } from "vitest";
import { readFileSync } from "node:fs";
import { resolve } from "node:path";

const globalCss = readFileSync(
  resolve(__dirname, "../../src/styles/global.css"),
  "utf-8",
);

const editorTsx = readFileSync(
  resolve(__dirname, "../../src/components/Editor/Editor.tsx"),
  "utf-8",
);

/** A theme's token block: the `@theme { }` default plus each
 *  `[data-theme="x"] { }`. Descendant selectors like
 *  `[data-theme="nord"] .cm-editor { }` are NOT token blocks and are skipped. */
interface ThemeBlock {
  name: string;
  body: string;
}

function parseThemeBlocks(css: string): ThemeBlock[] {
  const lines = css.split("\n");
  const blocks: ThemeBlock[] = [];

  for (let i = 0; i < lines.length; i++) {
    const header = lines[i].match(/^(?:@theme|\[data-theme="([a-z]+)"\])\s*\{$/);
    if (!header) continue;

    const name = header[1] ?? "default";
    // Collect to the matching close brace, counting depth so a nested rule
    // could never truncate the block early.
    let depth = 1;
    const body: string[] = [];
    for (let j = i + 1; j < lines.length && depth > 0; j++) {
      depth += (lines[j].match(/\{/g) ?? []).length;
      depth -= (lines[j].match(/\}/g) ?? []).length;
      if (depth > 0) body.push(lines[j]);
    }
    blocks.push({ name, body: body.join("\n") });
  }
  return blocks;
}

/** Alpha channel of `--<token>: rgba(r, g, b, A)` within a block. */
function alphaOf(body: string, token: string): number | null {
  const m = body.match(
    new RegExp(`--${token}:\\s*rgba\\([^)]*,\\s*([0-9.]+)\\s*\\)`),
  );
  return m ? Number(m[1]) : null;
}

const themes = parseThemeBlocks(globalCss);

describe("theme selection tokens", () => {
  it("finds every theme block, so the checks below are not vacuous", () => {
    const names = themes.map((t) => t.name).sort();
    expect(names).toEqual([
      "corvid",
      "default",
      "light",
      "nord",
      "parchment",
      "synthwave",
    ]);
  });

  it.each(themes.map((t) => t.name))(
    "%s defines both selection tokens",
    (name) => {
      const body = themes.find((t) => t.name === name)!.body;
      expect(alphaOf(body, "color-selection")).not.toBeNull();
      expect(alphaOf(body, "color-selection-inactive")).not.toBeNull();
    },
  );

  it.each(themes.map((t) => t.name))(
    "%s selection is clearly stronger than its hover tint",
    (name) => {
      const body = themes.find((t) => t.name === name)!.body;
      const selection = alphaOf(body, "color-selection")!;
      const active = alphaOf(body, "color-bg-active")!;
      // 2x is the floor, not the target (the themes sit near 2.5x). Below
      // this, selection reads as a hover state rather than a selection.
      expect(selection).toBeGreaterThanOrEqual(active * 2);
    },
  );

  it.each(themes.map((t) => t.name))(
    "%s unfocused selection is dimmer than focused, but still present",
    (name) => {
      const body = themes.find((t) => t.name === name)!.body;
      const selection = alphaOf(body, "color-selection")!;
      const inactive = alphaOf(body, "color-selection-inactive")!;
      const active = alphaOf(body, "color-bg-active")!;
      expect(inactive).toBeLessThan(selection);
      // Still has to be visible — an unfocused selection you cannot see is
      // the same bug in a different state.
      expect(inactive).toBeGreaterThanOrEqual(0.12);
      // And it must not sit BELOW the theme's own hover tint, or an unfocused
      // selection reads as a hover state. Synthwave and Corvid carry heavier
      // hover tints than the rest, which is exactly how this slips through.
      expect(inactive).toBeGreaterThanOrEqual(active);
    },
  );

  it("applies selection outside the editor too", () => {
    // The rendered preview, sidebar and panels are not CodeMirror; without a
    // global rule they fall back to the browser default, which is a muted
    // blue that disappears against the dark themes.
    expect(globalCss).toMatch(
      /::selection\s*\{\s*background-color:\s*var\(--color-selection\)/,
    );
  });
});

describe("editor selection theme", () => {
  it("uses the selection tokens, not the hover tints", () => {
    const [, selectionTheme] = editorTsx.split(".cm-selectionBackground");
    expect(selectionTheme).toBeDefined();
    // Guards the exact regression: pointing selection back at bg-hover /
    // bg-active, which is where this started.
    const block = editorTsx.slice(
      editorTsx.indexOf('".cm-selectionBackground"'),
      editorTsx.indexOf("});", editorTsx.indexOf('".cm-selectionBackground"')),
    );
    expect(block).toContain("var(--color-selection)");
    expect(block).toContain("var(--color-selection-inactive)");
    expect(block).not.toContain("var(--color-bg-hover)");
    expect(block).not.toContain("var(--color-bg-active)");
  });
});
