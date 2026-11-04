/**
 * Settings store — theme cycle + next-theme label.
 *
 * Regression for the bug where the icon-rail / command-palette theme
 * button assumed a binary dark/light world: the tooltip read "Dark
 * Theme" on every non-`dark` theme (Nord, Synthwave, Parchment, …) even
 * though `toggleTheme` actually cycles through all six THEME_OPTIONS.
 *
 * `nextTheme()` is the single source of truth the labels now use — it
 * must always name the theme a click will switch to, wrapping at the end
 * of the list.
 */

import { describe, it, expect } from "vitest";
import { settingsStore, THEME_OPTIONS } from "../../src/stores/settings";

describe("settings store — theme cycle", () => {
  it("nextTheme names the next theme in THEME_OPTIONS order and wraps", () => {
    const order = THEME_OPTIONS.map((o) => o.id);
    for (let i = 0; i < order.length; i++) {
      settingsStore.setTheme(order[i]);
      const expected = THEME_OPTIONS[(i + 1) % order.length];
      expect(settingsStore.nextTheme().id).toBe(expected.id);
      expect(settingsStore.nextTheme().name).toBe(expected.name);
    }
  });

  it("toggleTheme advances to exactly nextTheme()", () => {
    settingsStore.setTheme("dark");
    const target = settingsStore.nextTheme().id;
    settingsStore.toggleTheme();
    expect(settingsStore.theme()).toBe(target);
  });

  it("a full cycle returns to the starting theme", () => {
    settingsStore.setTheme("dark");
    for (let i = 0; i < THEME_OPTIONS.length; i++) settingsStore.toggleTheme();
    expect(settingsStore.theme()).toBe("dark");
  });

  it("never labels a non-dark theme as plain 'Dark' — the old bug", () => {
    // On Nord (a dark-bg theme that is NOT the `dark` id) the next theme
    // is Synthwave, so the label must say "Synthwave", never "Dark".
    settingsStore.setTheme("nord");
    expect(settingsStore.nextTheme().name).not.toBe("Midnight");
    expect(settingsStore.nextTheme().id).toBe("synthwave");
  });
});
