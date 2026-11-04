import { describe, expect, it } from "vitest";
import {
  getWikiLinkAtPos,
  shouldNavigateWikiLinkClick,
} from "../../src/components/Editor/extensions/wikilink";

function at(doc: string, needle: string): number {
  const i = doc.indexOf(needle);
  if (i < 0) throw new Error(`"${needle}" not in doc`);
  return i;
}

describe("getWikiLinkAtPos", () => {
  it("resolves a simple wiki-link target", () => {
    const doc = "Open [[Project Plan]] today.";
    expect(getWikiLinkAtPos(doc, at(doc, "Project"))).toBe("Project Plan");
  });

  it("returns the target, not the alias", () => {
    const doc = "Open [[folder/Project Plan|plan]] today.";
    expect(getWikiLinkAtPos(doc, at(doc, "plan"))).toBe(
      "folder/Project Plan",
    );
  });

  it("returns null outside a wiki-link", () => {
    const doc = "Open [[Project Plan]] today.";
    expect(getWikiLinkAtPos(doc, at(doc, "today"))).toBeNull();
  });
});

describe("shouldNavigateWikiLinkClick", () => {
  it("allows plain clicks while the link is rendered", () => {
    expect(shouldNavigateWikiLinkClick(false, false)).toBe(true);
  });

  it("requires a modifier on a raw edit line", () => {
    expect(shouldNavigateWikiLinkClick(false, true)).toBe(false);
    expect(shouldNavigateWikiLinkClick(true, true)).toBe(true);
  });
});
