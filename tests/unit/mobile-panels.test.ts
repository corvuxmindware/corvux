import { mobilePrimaryPanels, mobileOverflowPanels } from "@/app/mobile-panels";

// Guards the regression the codex review flagged: App.tsx and LeftPane.tsx
// disagreed on the drawer's primary panels. The single source of truth must
// stay coherent — exactly three primary tabs (Bookmarks promoted over Tasks),
// no id in both lists, and every panel well-formed.
describe("mobile-panels source of truth", () => {
  it("has exactly the three intended primary tabs in order", () => {
    expect(mobilePrimaryPanels.map((p) => p.id)).toEqual([
      "files",
      "search",
      "bookmarks",
    ]);
  });

  it("demotes Tasks into the overflow (not a primary tab)", () => {
    expect(mobilePrimaryPanels.some((p) => p.id === "tasks")).toBe(false);
    expect(mobileOverflowPanels.some((p) => p.id === "tasks")).toBe(true);
  });

  it("never lists the same panel id in both primary and overflow", () => {
    const primary = new Set(mobilePrimaryPanels.map((p) => p.id));
    const dupes = mobileOverflowPanels.filter((p) => primary.has(p.id));
    expect(dupes).toEqual([]);
  });

  it("has unique ids across the whole panel set", () => {
    const ids = [...mobilePrimaryPanels, ...mobileOverflowPanels].map((p) => p.id);
    expect(new Set(ids).size).toBe(ids.length);
  });

  it("gives every panel a non-empty label and icon", () => {
    for (const p of [...mobilePrimaryPanels, ...mobileOverflowPanels]) {
      expect(p.label.length).toBeGreaterThan(0);
      expect(p.icon.length).toBeGreaterThan(0);
    }
  });
});
