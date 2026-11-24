/**
 * Which app themes have a light background.
 *
 * One list for everything that must switch to a light variant on those
 * themes: native scrollbars and form controls (`color-scheme`, set in
 * stores/settings.ts), the logo (Brand/CorvuxLogo.tsx) and Mermaid diagrams
 * (editor/extensions/mermaid-render.ts).
 * Mermaid used to be hard-wired to its dark theme, so on Daylight/Parchment
 * diagrams came out as dark boxes with dark text (fixed 2026-11-05).
 *
 * A unit test checks this list against each theme's swatch background, so a
 * new light theme can't be added without updating it.
 */

const LIGHT_BACKGROUND_THEMES: ReadonlySet<string> = new Set(["light", "parchment"]);

export function isLightBackgroundTheme(theme: string | null | undefined): boolean {
  return theme != null && LIGHT_BACKGROUND_THEMES.has(theme);
}

/** The app theme currently applied to `<html>`. Dark is the default and has
 *  no `data-theme` attribute (see `applyTheme` in stores/settings.ts). */
export function appliedTheme(root: HTMLElement = document.documentElement): string {
  return root.getAttribute("data-theme") ?? "dark";
}
