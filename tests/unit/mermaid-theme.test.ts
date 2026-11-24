/**
 * Mermaid diagrams follow the app theme.
 *
 * The bug this freezes: Mermaid was initialised once with `theme: "dark"`,
 * so on the light app themes (Daylight, Parchment) every diagram rendered
 * as dark boxes, and the editor's CSS forced the label text to the theme's
 * (dark) text colour — dark text on dark boxes. Diagrams must now use
 * Mermaid's light ("default") theme on light-background app themes, and
 * re-render when the user switches theme.
 */

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { EditorState } from "@codemirror/state";
import { EditorView } from "@codemirror/view";

// A stand-in for the real Mermaid: records the theme it was initialised with
// and stamps it on the SVG it "renders".
const fake = vi.hoisted(() => ({ theme: "", initialized: [] as string[] }));
vi.mock("mermaid", () => ({
  default: {
    initialize: (cfg: { theme: string }) => {
      fake.theme = cfg.theme;
      fake.initialized.push(cfg.theme);
    },
    // A diagram whose source contains HANG never finishes rendering.
    render: (_id: string, code: string) =>
      code.includes("HANG")
        ? new Promise(() => {})
        : Promise.resolve({
            svg: `<svg data-mermaid-theme="${fake.theme}"><text>${code.length}</text></svg>`,
          }),
  },
}));

import {
  MERMAID_RENDER_TIMEOUT_MS,
  mermaidConfig,
  mermaidRender,
  mermaidThemeFor,
} from "../../src/components/Editor/extensions/mermaid-render";
import { isLightBackgroundTheme } from "../../src/lib/theme-kind";
import { THEME_OPTIONS } from "../../src/stores/settings";

/** Relative luminance (0 = black, 1 = white) of a `#rrggbb` colour. */
function luminance(hex: string): number {
  const [r, g, b] = [1, 3, 5].map((i) => {
    const c = parseInt(hex.slice(i, i + 2), 16) / 255;
    return c <= 0.03928 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4;
  });
  return 0.2126 * r + 0.7152 * g + 0.0722 * b;
}

describe("light-background theme list", () => {
  it("matches every theme's actual background colour", () => {
    for (const option of THEME_OPTIONS) {
      const lightBg = luminance(option.swatch[0]) > 0.5;
      expect(
        isLightBackgroundTheme(option.id),
        `theme "${option.id}" (bg ${option.swatch[0]}) — update LIGHT_BACKGROUND_THEMES in src/lib/theme-kind.ts`,
      ).toBe(lightBg);
    }
  });
});

describe("mermaid theme mapping", () => {
  it("uses Mermaid's light theme on light app themes, dark otherwise", () => {
    expect(mermaidThemeFor("light")).toBe("default");
    expect(mermaidThemeFor("parchment")).toBe("default");
    expect(mermaidThemeFor("dark")).toBe("dark");
    expect(mermaidThemeFor("nord")).toBe("dark");
    expect(mermaidThemeFor(null)).toBe("dark");
  });

  it("keeps the safety settings whatever the theme", () => {
    for (const theme of ["dark", "default"] as const) {
      const cfg = mermaidConfig(theme);
      expect(cfg.theme).toBe(theme);
      expect(cfg.securityLevel).toBe("strict");
      expect(cfg.htmlLabels).toBe(false);
      expect(cfg.suppressErrorRendering).toBe(true);
      expect(cfg.startOnLoad).toBe(false);
    }
  });
});

describe("mermaid widget re-renders on theme change", () => {
  const html = document.documentElement;
  let savedTheme: string | null;
  let view: EditorView | null = null;

  beforeEach(() => {
    savedTheme = html.getAttribute("data-theme");
    html.removeAttribute("data-theme");
  });

  afterEach(() => {
    view?.destroy();
    view = null;
    if (savedTheme === null) html.removeAttribute("data-theme");
    else html.setAttribute("data-theme", savedTheme);
  });

  const renderedTheme = () =>
    view?.dom.querySelector(".cm-mermaid-pan svg")?.getAttribute("data-mermaid-theme");

  it("dark app theme → dark diagram; switching to Daylight → light diagram, and back", async () => {
    const parent = document.createElement("div");
    document.body.appendChild(parent);
    view = new EditorView({
      parent,
      state: EditorState.create({
        doc: "# Note\n\n```mermaid\ngraph TD\n  A-->B\n```\n",
        extensions: [mermaidRender()],
      }),
    });

    await vi.waitFor(() => expect(renderedTheme()).toBe("dark"));

    html.setAttribute("data-theme", "light");
    await vi.waitFor(() => expect(renderedTheme()).toBe("default"));

    html.setAttribute("data-theme", "nord");
    await vi.waitFor(() => expect(renderedTheme()).toBe("dark"));

    parent.remove();
  });

  it("picks up a theme switch made after the state was created but before the view", async () => {
    const state = EditorState.create({
      doc: "```mermaid\ngraph TD\n  P-->Q\n```\n",
      extensions: [mermaidRender()],
    });
    html.setAttribute("data-theme", "parchment");
    const parent = document.createElement("div");
    document.body.appendChild(parent);
    view = new EditorView({ parent, state });
    await vi.waitFor(() => expect(renderedTheme()).toBe("default"));
    parent.remove();
  });
});

describe("a stuck diagram does not block the others", () => {
  afterEach(() => {
    vi.useRealTimers();
  });

  it("times out the stuck render and still renders the next diagram", async () => {
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] });
    const parent = document.createElement("div");
    document.body.appendChild(parent);
    const view = new EditorView({
      parent,
      state: EditorState.create({
        doc:
          "```mermaid\ngraph TD\n  HANG-->X\n```\n\ntext\n\n```mermaid\ngraph TD\n  R-->S\n```\n",
        extensions: [mermaidRender()],
      }),
    });
    const svgs = () => view.dom.querySelectorAll(".cm-mermaid-pan svg").length;
    const timeoutErrors = () =>
      [...view.dom.querySelectorAll(".cm-mermaid-error-msg")].filter((e) =>
        e.textContent?.includes("longer than"),
      ).length;

    // Before the timeout: the second diagram is queued behind the stuck one.
    await vi.advanceTimersByTimeAsync(MERMAID_RENDER_TIMEOUT_MS - 1000);
    expect(svgs()).toBe(0);

    await vi.advanceTimersByTimeAsync(2000);
    await vi.waitFor(() => {
      expect(timeoutErrors()).toBe(1);
      expect(svgs()).toBe(1);
    });

    view.destroy();
    parent.remove();
  });
});
