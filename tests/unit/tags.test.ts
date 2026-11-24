import { describe, it, expect } from "vitest";
import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import {
  extractInlineTags,
  foldTag,
  isValidTagText,
  makeInlineTagRegex,
  sameTag,
} from "../../src/lib/tags";
import { addTagToProperty } from "../../src/lib/frontmatter";
import { noteHoverPreview } from "../../src/components/Graph/graph-style";

// LR-6 / LR-7 — ONE tag-identity rule. The same fixture is asserted by the
// Rust app parser (src-tauri/src/parser/tags.rs) and the standalone MCP parser
// (corvux-mcp/src/frontmatter.rs); this file is the frontend's copy of that
// contract. Change a rule only together with all three.
interface ParseCase {
  name: string;
  line?: boolean;
  content: string;
  tags: string[];
}
interface Fixture {
  parse: ParseCase[];
  fold: [string, string][];
}

const fixture: Fixture = JSON.parse(
  readFileSync(
    resolve(__dirname, "../../corvux-mcp/tests/fixtures/tag_identity.json"),
    "utf8",
  ),
);

describe("shared tag-identity fixture", () => {
  it("is big enough to mean something", () => {
    expect(fixture.parse.length).toBeGreaterThanOrEqual(20);
    expect(fixture.fold.length).toBeGreaterThanOrEqual(8);
  });

  // Only single-line body cases: the frontend extractor is regex-only (the
  // editor highlighter skips code/headings itself; the backend index is
  // authoritative for frontmatter).
  const lineCases = fixture.parse.filter((c) => c.line);
  it("has single-line cases to check", () => {
    expect(lineCases.length).toBeGreaterThanOrEqual(10);
  });
  for (const c of lineCases) {
    it(`extracts the same tags as the backend: ${c.name}`, () => {
      expect(extractInlineTags(c.content)).toEqual(c.tags);
    });
  }

  for (const [input, key] of fixture.fold) {
    it(`folds ${JSON.stringify(input)} to the shared identity key`, () => {
      expect(foldTag(input)).toBe(key);
    });
  }
});

describe("tag characters (LR-7)", () => {
  it("does not truncate at a non-ASCII letter", () => {
    expect(extractInlineTags("Le #café ici")).toEqual(["café"]);
    expect(extractInlineTags("#日本語 と #タグ。")).toEqual(["日本語", "タグ"]);
  });

  it("keeps Telugu / Hindi vowel signs and viramas inside the tag", () => {
    expect(extractInlineTags("#ప్రాజెక్ట్ #తెలుగు #हिंदी")).toEqual([
      "ప్రాజెక్ట్",
      "తెలుగు",
      "हिंदी",
    ]);
  });

  it("rejects purely numeric tags, Unicode digits included", () => {
    expect(isValidTagText("2024")).toBe(false);
    expect(isValidTagText("١٢٣")).toBe(false);
    expect(isValidTagText("1\u{FE0F}\u{20E3}")).toBe(false); // keycap emoji
    expect(isValidTagText("2nd")).toBe(true);
    expect(isValidTagText("日本語")).toBe(true);
    expect(isValidTagText("")).toBe(false);
  });

  it("returns a fresh stateful regex each time", () => {
    const a = makeInlineTagRegex();
    const b = makeInlineTagRegex();
    expect(a).not.toBe(b);
    a.exec("#one");
    expect(b.lastIndex).toBe(0);
  });
});

describe("tag identity (LR-6)", () => {
  it("is case- and normalization-insensitive, and idempotent", () => {
    expect(sameTag("ML", "ml")).toBe(true);
    expect(sameTag("café", "café")).toBe(true);
    expect(sameTag("ml", "ml2")).toBe(false);
    expect(sameTag("a/b", "a-b")).toBe(false);
    for (const s of ["ML", "CAFÉ", "café", "İstanbul", "ΣΑΣ", "日本語"]) {
      expect(foldTag(foldTag(s))).toBe(foldTag(s));
    }
  });

  it("addTagToProperty treats a case/NFD variant as already present", () => {
    const note = "---\ntags: [ml, café]\n---\n\nbody";
    expect(addTagToProperty(note, "tags", "ML")).toBe(note);
    expect(addTagToProperty(note, "tags", "CAFÉ")).toBe(note);
    const added = addTagToProperty(note, "tags", "new");
    expect(added).toContain("new");
  });

  it("addTagToProperty tolerates non-string YAML list items", () => {
    const note = "---\ntags: [2024, ml]\n---\n\nbody";
    expect(() => addTagToProperty(note, "tags", "ML")).not.toThrow();
    expect(addTagToProperty(note, "tags", "ML")).toBe(note);
  });

  it("the graph hover card shows one chip per tag identity, Unicode whole", () => {
    const md = ["---", "tags: [ML, café]", "---", "Body #ml #Café #日本語 and #2024"].join("\n");
    const p = noteHoverPreview(md);
    expect(p.tags).toEqual(["ML", "café", "日本語"]);
  });
});
