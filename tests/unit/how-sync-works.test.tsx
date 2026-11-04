// @vitest-environment happy-dom
/**
 * Tests for `HowSyncWorks` — the collapsible sync explainer in the Sync
 * settings panel.
 *
 * Two things worth locking:
 *  1. The toggle works: collapsed by default, expands on click. (The box is
 *     shown before sync is enabled, so a broken toggle = a new user can't
 *     read how sync works at the one moment they need to.)
 *  2. The privacy copy stays HONEST. The panel previously claimed "Cloud
 *     relays are never used", which is false for the shipped build
 *     (iroh RelayMode::Default + discovery_n0). This guards against that
 *     regression: the expanded copy must state end-to-end encryption AND the
 *     third-party-connection caveat, and must NOT claim relays are never used.
 */
import { describe, it, expect, afterEach } from "vitest";
import { render } from "solid-js/web";
import HowSyncWorks from "../../src/components/Settings/SyncSection/HowSyncWorks";

let dispose: (() => void) | null = null;
let container: HTMLDivElement | null = null;

afterEach(() => {
  dispose?.();
  dispose = null;
  container?.remove();
  container = null;
});

function mount() {
  container = document.createElement("div");
  document.body.appendChild(container);
  dispose = render(() => <HowSyncWorks />, container);
  return container;
}

describe("HowSyncWorks", () => {
  it("is collapsed by default and reveals the explainer on toggle", () => {
    const el = mount();

    const toggle = el.querySelector("button");
    expect(toggle, "toggle button renders").toBeTruthy();
    expect(toggle?.getAttribute("aria-expanded")).toBe("false");
    // The header is always present; the body copy is not, while collapsed.
    expect(el.textContent).toContain("How sync works");
    expect(el.textContent).not.toContain("end-to-end encrypted");

    toggle!.click();

    expect(toggle?.getAttribute("aria-expanded")).toBe("true");
    expect(el.textContent).toContain("end-to-end encrypted");
    expect(el.textContent).toContain("Settings → Sync → Scan");
  });

  it("states the honest privacy model and never claims relays are unused", () => {
    const el = mount();
    el.querySelector("button")!.click();
    const text = el.textContent ?? "";

    // Honest claims that must be present.
    expect(text).toContain("no server can read them");
    expect(text).toMatch(/third-party discovery service/i);
    expect(text).toMatch(/network addresses/i);

    // The stale, inaccurate claim must never come back.
    expect(text.toLowerCase()).not.toContain("relays are never used");
    expect(text.toLowerCase()).not.toContain("never used");
  });

  it("is honest about current sync limits", () => {
    const el = mount();
    el.querySelector("button")!.click();
    const text = el.textContent ?? "";

    expect(text).toMatch(/image\/audio attachments sync/i);
    expect(text).toMatch(/attachments over 8 MB stay local/i);
    expect(text).toMatch(/online around the same time unless the optional queue is configured/i);

    // The old limitation is stale now that attachment blob sync exists.
    expect(text).not.toMatch(/images and audio don't yet/i);
  });
});
