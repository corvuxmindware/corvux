/**
 * Unit tests for the mobile format-bar command spec generators.
 *
 * These test the PURE `compute*` functions — no EditorView, no DOM. We
 * feed a doc string + selection offsets, read back the change spec, and
 * apply it to the source manually to assert the resulting markdown +
 * caret position. Mirrors the pattern in `table-cell-edit.test.ts`.
 */

import { describe, it, expect } from "vitest";
import {
  computeWrap,
  computeHeadingCycle,
  computeLinePrefixToggle,
  computeCheckboxToggle,
  computeInsert,
  type CommandSpec,
} from "@/components/MobileFormatBar/format-commands";

/** Apply a CommandSpec to a source string and return the new doc. */
function applyChanges(source: string, spec: CommandSpec): string {
  let out = source;
  // Sort desc so earlier offsets don't shift later ones mid-apply.
  const sorted = [...spec.changes].sort((a, b) => b.from - a.from);
  for (const c of sorted) {
    out = out.slice(0, c.from) + c.insert + out.slice(c.to);
  }
  return out;
}

describe("computeWrap", () => {
  it("wraps a non-empty selection and keeps the inner text selected", () => {
    const doc = "hello world";
    const spec = computeWrap(doc, 0, 5, "**");
    expect(applyChanges(doc, spec)).toBe("**hello** world");
    expect(spec.selection).toEqual({ anchor: 2, head: 7 });
  });

  it("places the caret between markers when the selection is empty", () => {
    const doc = "hello";
    const spec = computeWrap(doc, 2, 2, "**");
    expect(applyChanges(doc, spec)).toBe("he****llo");
    expect(spec.selection).toEqual({ anchor: 4 });
  });

  it("supports asymmetric open/close markers (wikilink)", () => {
    const doc = "see note here";
    //         pos: 0123456789…
    // "see note here" — "note" occupies 4..8
    const spec = computeWrap(doc, 4, 8, "[[", "]]");
    expect(applyChanges(doc, spec)).toBe("see [[note]] here");
    // "see [[note]] here" — "note" now occupies 6..10
    expect(spec.selection).toEqual({ anchor: 6, head: 10 });
  });

  it("caret lands inside empty wikilink for autocomplete to pick up", () => {
    const doc = "x";
    const spec = computeWrap(doc, 1, 1, "[[", "]]");
    expect(applyChanges(doc, spec)).toBe("x[[]]");
    // Caret between "[[" and "]]": position 1 + 2 = 3.
    expect(spec.selection).toEqual({ anchor: 3 });
  });
});

describe("computeHeadingCycle", () => {
  it("promotes a plain line to H1", () => {
    const doc = "Title";
    const spec = computeHeadingCycle(doc, 0, 0);
    expect(applyChanges(doc, spec)).toBe("# Title");
  });

  it("cycles H1 -> H2 -> H3 -> H4 -> plain", () => {
    let doc = "Title";
    let spec = computeHeadingCycle(doc, 0, 0);
    expect(applyChanges(doc, spec)).toBe("# Title");
    doc = applyChanges(doc, spec);
    spec = computeHeadingCycle(doc, 0, 0);
    expect(applyChanges(doc, spec)).toBe("## Title");
    doc = applyChanges(doc, spec);
    spec = computeHeadingCycle(doc, 0, 0);
    expect(applyChanges(doc, spec)).toBe("### Title");
    doc = applyChanges(doc, spec);
    spec = computeHeadingCycle(doc, 0, 0);
    expect(applyChanges(doc, spec)).toBe("#### Title");
    doc = applyChanges(doc, spec);
    spec = computeHeadingCycle(doc, 0, 0);
    expect(applyChanges(doc, spec)).toBe("Title");
  });

  it("clears H5/H6 (past max) in one tap", () => {
    const doc = "##### Title";
    const spec = computeHeadingCycle(doc, 0, 0);
    expect(applyChanges(doc, spec)).toBe("Title");
  });

  it("applies the next level to every line in a multi-line selection", () => {
    const doc = "one\ntwo\nthree";
    // Select all of it.
    const spec = computeHeadingCycle(doc, 0, doc.length);
    expect(applyChanges(doc, spec)).toBe("# one\n# two\n# three");
  });

  it("strips heading markers from every selected line when cycling back to plain", () => {
    const doc = "## one\n## two";
    const spec = computeHeadingCycle(doc, 0, doc.length);
    expect(applyChanges(doc, spec)).toBe("### one\n### two");
  });
});

describe("computeLinePrefixToggle", () => {
  it("adds the prefix to a plain line", () => {
    const doc = "item";
    const spec = computeLinePrefixToggle(doc, 0, 0, "- ");
    expect(applyChanges(doc, spec)).toBe("- item");
  });

  it("strips the prefix when every selected line has it", () => {
    const doc = "- one\n- two";
    const spec = computeLinePrefixToggle(doc, 0, doc.length, "- ");
    expect(applyChanges(doc, spec)).toBe("one\ntwo");
  });

  it("adds to all when not every line has the prefix", () => {
    const doc = "- one\ntwo";
    const spec = computeLinePrefixToggle(doc, 0, doc.length, "- ");
    expect(applyChanges(doc, spec)).toBe("- one\n- two");
  });

  it("toggles quote prefix independently from list prefix", () => {
    const doc = "quoted";
    const spec = computeLinePrefixToggle(doc, 0, 0, "> ");
    expect(applyChanges(doc, spec)).toBe("> quoted");
  });

  it("no-ops on a line that already starts with the prefix when others lack it", () => {
    const doc = "- a\nb\n- c";
    const spec = computeLinePrefixToggle(doc, 0, doc.length, "- ");
    expect(applyChanges(doc, spec)).toBe("- a\n- b\n- c");
  });
});

describe("computeCheckboxToggle", () => {
  it("promotes a plain line to an unchecked checkbox", () => {
    const doc = "task";
    const spec = computeCheckboxToggle(doc, 0, 0);
    expect(applyChanges(doc, spec)).toBe("- [ ] task");
  });

  it("promotes a bare list item to a checkbox", () => {
    const doc = "- task";
    const spec = computeCheckboxToggle(doc, 0, 0);
    expect(applyChanges(doc, spec)).toBe("- [ ] task");
  });

  it("preserves the original list marker (* or +)", () => {
    const doc = "* task";
    const spec = computeCheckboxToggle(doc, 0, 0);
    expect(applyChanges(doc, spec)).toBe("* [ ] task");
  });

  it("checks an unchecked box", () => {
    const doc = "- [ ] task";
    const spec = computeCheckboxToggle(doc, 0, 0);
    expect(applyChanges(doc, spec)).toBe("- [x] task");
  });

  it("unchecks a checked box", () => {
    const doc = "- [x] task";
    const spec = computeCheckboxToggle(doc, 0, 0);
    expect(applyChanges(doc, spec)).toBe("- [ ] task");
  });

  it("handles uppercase [X] checked state", () => {
    const doc = "- [X] task";
    const spec = computeCheckboxToggle(doc, 0, 0);
    expect(applyChanges(doc, spec)).toBe("- [ ] task");
  });

  it("toggles each line in a multi-line selection independently", () => {
    const doc = "- [ ] a\n- [x] b\nplain";
    const spec = computeCheckboxToggle(doc, 0, doc.length);
    expect(applyChanges(doc, spec)).toBe("- [x] a\n- [ ] b\n- [ ] plain");
  });

  it("toggles a checkbox with no trailing space (regression: \\s+ → \\s*)", () => {
    // A line that is exactly "- [ ]" with no trailing content/space
    // must still be recognised as a checkbox. With the old `\s+`
    // anchor it fell through to the bare-list-item branch and got
    // promoted to "- [ ] [ ]".
    const doc = "- [ ]";
    const spec = computeCheckboxToggle(doc, 0, 0);
    expect(applyChanges(doc, spec)).toBe("- [x]");
  });

  it("toggles a checked box with no trailing space", () => {
    const doc = "- [x]";
    const spec = computeCheckboxToggle(doc, 0, 0);
    expect(applyChanges(doc, spec)).toBe("- [ ]");
  });
});

describe("computeInsert", () => {
  it("inserts at cursor with no caret override leaves selection undefined", () => {
    const doc = "abc";
    const spec = computeInsert(doc, 1, 1, "X");
    expect(applyChanges(doc, spec)).toBe("aXbc");
    expect(spec.selection).toBeUndefined();
  });

  it("places the caret at caretOffset inside the inserted text", () => {
    const doc = "abc";
    const spec = computeInsert(doc, 1, 1, "[[]]", 2);
    expect(applyChanges(doc, spec)).toBe("a[[]]bc");
    expect(spec.selection).toEqual({ anchor: 3 }); // 1 + 2 = between [[ and ]]
  });

  it("replaces an existing selection", () => {
    const doc = "hello world";
    const spec = computeInsert(doc, 0, 5, "GOODBYE");
    expect(applyChanges(doc, spec)).toBe("GOODBYE world");
    // caretOffset is ignored when the selection is non-empty.
    expect(spec.selection).toBeUndefined();
  });
});
