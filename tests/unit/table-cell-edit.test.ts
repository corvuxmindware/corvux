import {
  splitRow,
  parseAlignments,
  physicalLines,
  cellSpans,
  escapeCell,
  displayRowToLineIndex,
  computeCellChanges,
  blankRow,
  computeAppendRowChange,
  appendRowLineIndex,
  tableAtBoundary,
  pipePositions,
  computeDeleteRowChange,
  computeDeleteColumnChanges,
  computeInsertColumnChanges,
  computeInsertRowChange,
  computeAlignmentChange,
  type RawCellEdit,
} from "@/components/Editor/extensions/table-render";

/**
 * Apply table-relative changes to a source string, mirroring how
 * EditorView.dispatch would, so we can assert on the resulting markdown.
 * Changes are non-overlapping and already sorted ascending by `from`.
 */
function applyChanges(
  source: string,
  changes: { from: number; to: number; insert: string }[],
): string {
  let out = source;
  for (const c of [...changes].sort((a, b) => b.from - a.from)) {
    out = out.slice(0, c.from) + c.insert + out.slice(c.to);
  }
  return out;
}

const TABLE = [
  "| Exercise | Set 1 | Set 2 |",
  "|----------|-------|-------|",
  "| Lever chest press | | |",
  "| DB incline press | | |",
].join("\n");

describe("splitRow", () => {
  it("splits a fully-piped row and trims cells", () => {
    expect(splitRow("| a | b | c |")).toEqual(["a", "b", "c"]);
  });

  it("tolerates a missing leading/trailing pipe", () => {
    expect(splitRow("a | b | c")).toEqual(["a", "b", "c"]);
  });

  it("does not split on an escaped pipe and unescapes it", () => {
    expect(splitRow("| a \\| b | c |")).toEqual(["a | b", "c"]);
  });
});

describe("parseAlignments", () => {
  it("reads left/right/center/none from the delimiter row", () => {
    expect(parseAlignments("|:--|--:|:-:|---|")).toEqual([
      "left",
      "right",
      "center",
      null,
    ]);
  });
});

describe("physicalLines", () => {
  it("tracks each line's offset into the source", () => {
    const lines = physicalLines("ab\ncde\nf");
    expect(lines).toEqual([
      { text: "ab", start: 0 },
      { text: "cde", start: 3 },
      { text: "f", start: 7 },
    ]);
  });

  it("strips trailing CR but keeps it in the offset (CRLF safe)", () => {
    const lines = physicalLines("ab\r\ncd");
    expect(lines[0]).toEqual({ text: "ab", start: 0 });
    // "ab\r" is 3 chars + 1 for "\n" => next line starts at 4
    expect(lines[1]).toEqual({ text: "cd", start: 4 });
  });
});

describe("cellSpans", () => {
  it("returns inter-pipe content spans", () => {
    const line = "| a | bb |";
    const spans = cellSpans(line);
    expect(spans).toEqual([
      { start: 1, end: 4 },
      { start: 5, end: 9 },
    ]);
    expect(line.slice(spans[0].start, spans[0].end)).toBe(" a ");
    expect(line.slice(spans[1].start, spans[1].end)).toBe(" bb ");
  });

  it("ignores escaped pipes when finding boundaries", () => {
    const line = "| a \\| b | c |";
    const spans = cellSpans(line);
    expect(spans).toHaveLength(2);
    expect(line.slice(spans[0].start, spans[0].end)).toBe(" a \\| b ");
  });

  it("returns [] for a non-standard row without enough pipes", () => {
    expect(cellSpans("a | b")).toEqual([]);
  });
});

describe("escapeCell", () => {
  it("escapes pipes and flattens newlines", () => {
    expect(escapeCell("a | b")).toBe("a \\| b");
    expect(escapeCell("line1\nline2")).toBe("line1 line2");
  });
});

describe("displayRowToLineIndex", () => {
  const lines = physicalLines(TABLE);
  it("maps the header to physical line 0", () => {
    expect(displayRowToLineIndex(lines, 0)).toBe(0);
  });
  it("skips the delimiter row for body rows", () => {
    expect(displayRowToLineIndex(lines, 1)).toBe(2);
    expect(displayRowToLineIndex(lines, 2)).toBe(3);
  });
  it("returns -1 past the last body row", () => {
    expect(displayRowToLineIndex(lines, 9)).toBe(-1);
  });
});

describe("computeCellChanges", () => {
  it("writes a value into an empty cell, touching only that span", () => {
    const edits: RawCellEdit[] = [{ lineIndex: 2, cellIndex: 1, value: "10" }];
    const changes = computeCellChanges(TABLE, edits);
    expect(changes).toHaveLength(1);
    const result = applyChanges(TABLE, changes);
    expect(result).toBe(
      [
        "| Exercise | Set 1 | Set 2 |",
        "|----------|-------|-------|",
        "| Lever chest press | 10 | |",
        "| DB incline press | | |",
      ].join("\n"),
    );
  });

  it("edits a header cell without disturbing the body", () => {
    const edits: RawCellEdit[] = [
      { lineIndex: 0, cellIndex: 0, value: "Movement" },
    ];
    const result = applyChanges(TABLE, computeCellChanges(TABLE, edits));
    expect(result.split("\n")[0]).toBe("| Movement | Set 1 | Set 2 |");
    // body lines unchanged, verbatim
    expect(result.split("\n")[2]).toBe("| Lever chest press | | |");
  });

  it("applies multiple cell edits as one non-overlapping, sorted change set", () => {
    const edits: RawCellEdit[] = [
      { lineIndex: 3, cellIndex: 2, value: "12" },
      { lineIndex: 2, cellIndex: 1, value: "10" },
    ];
    const changes = computeCellChanges(TABLE, edits);
    // sorted ascending by `from`
    expect(changes[0].from).toBeLessThan(changes[1].from);
    const result = applyChanges(TABLE, changes);
    expect(result).toBe(
      [
        "| Exercise | Set 1 | Set 2 |",
        "|----------|-------|-------|",
        "| Lever chest press | 10 | |",
        "| DB incline press | | 12 |",
      ].join("\n"),
    );
  });

  it("escapes a typed pipe so the table structure survives", () => {
    const edits: RawCellEdit[] = [
      { lineIndex: 2, cellIndex: 2, value: "a|b" },
    ];
    const result = applyChanges(TABLE, computeCellChanges(TABLE, edits));
    expect(result.split("\n")[2]).toBe("| Lever chest press | | a\\|b |");
    // round-trips: parsing the edited row yields the original cell text
    expect(splitRow(result.split("\n")[2])).toEqual([
      "Lever chest press",
      "",
      "a|b",
    ]);
  });

  it("skips edits that point at a non-existent line or cell", () => {
    const edits: RawCellEdit[] = [
      { lineIndex: 99, cellIndex: 0, value: "x" },
      { lineIndex: 2, cellIndex: 9, value: "y" },
    ];
    expect(computeCellChanges(TABLE, edits)).toEqual([]);
  });
});

describe("blankRow", () => {
  it("builds an empty row with the given column count", () => {
    expect(blankRow(3)).toBe("| | | |");
    expect(splitRow(blankRow(3))).toEqual(["", "", ""]);
    // round-trips through the same cell-span logic the editor uses
    expect(cellSpans(blankRow(3))).toHaveLength(3);
  });
});

describe("computeAppendRowChange", () => {
  it("inserts a new blank row at the end of the table", () => {
    const change = computeAppendRowChange(TABLE, 3);
    expect(change.from).toBe(TABLE.length);
    expect(change.to).toBe(TABLE.length);

    const result = applyChanges(TABLE, [change]);
    const lines = result.split("\n");
    expect(lines).toHaveLength(5);
    expect(lines[4]).toBe("| | | |");

    // the appended row is reachable as the next display row, and its first
    // cell maps to the physical line the editor will focus after the rebuild
    const phys = physicalLines(result);
    const newLineIndex = TABLE.split("\n").length; // what appendRow hands off
    expect(newLineIndex).toBe(4);
    expect(phys[newLineIndex].text).toBe("| | | |");
    expect(displayRowToLineIndex(phys, 3)).toBe(4);
  });

  it("the appended row accepts a surgical cell edit like any other", () => {
    const withRow = applyChanges(TABLE, [computeAppendRowChange(TABLE, 3)]);
    const edited = applyChanges(
      withRow,
      computeCellChanges(withRow, [{ lineIndex: 4, cellIndex: 0, value: "Pulldown" }]),
    );
    expect(edited.split("\n")[4]).toBe("| Pulldown | | |");
  });

  it("inserts after trailing whitespace so the row stays attached to the table", () => {
    // A table node range that includes a trailing newline must not produce a
    // blank line between the table and the appended row (which would detach it).
    const withTrailingNl = TABLE + "\n";
    const change = computeAppendRowChange(withTrailingNl, 3);
    expect(change.from).toBe(TABLE.length); // before the trailing "\n", not after
    const result = applyChanges(withTrailingNl, [change]);
    const lines = result.split("\n");
    expect(lines).toEqual([
      "| Exercise | Set 1 | Set 2 |",
      "|----------|-------|-------|",
      "| Lever chest press | | |",
      "| DB incline press | | |",
      "| | | |", // contiguous — no blank line above it
      "",
    ]);
  });
});

describe("appendRowLineIndex", () => {
  it("points at the appended row's physical line (no trailing newline)", () => {
    expect(appendRowLineIndex(TABLE)).toBe(4);
    const result = applyChanges(TABLE, [computeAppendRowChange(TABLE, 3)]);
    expect(physicalLines(result)[appendRowLineIndex(TABLE)].text).toBe("| | | |");
  });

  it("stays correct when the source has a trailing newline", () => {
    const src = TABLE + "\n";
    const result = applyChanges(src, [computeAppendRowChange(src, 3)]);
    expect(physicalLines(result)[appendRowLineIndex(src)].text).toBe("| | | |");
  });
});

describe("tableAtBoundary (keyboard entry)", () => {
  const ranges = [
    { from: 10, to: 40 },
    { from: 60, to: 90 },
  ];

  it("matches the start boundary (entering from above)", () => {
    expect(tableAtBoundary(ranges, 10)).toBe(10);
  });

  it("matches the end boundary (entering from below) to the same identity", () => {
    expect(tableAtBoundary(ranges, 40)).toBe(10);
    expect(tableAtBoundary(ranges, 90)).toBe(60);
  });

  it("returns null for a caret inside or away from any table", () => {
    expect(tableAtBoundary(ranges, 25)).toBeNull(); // inside (shouldn't open)
    expect(tableAtBoundary(ranges, 5)).toBeNull(); // before everything
    expect(tableAtBoundary(ranges, 50)).toBeNull(); // between tables
  });
});

describe("pipePositions", () => {
  it("finds unescaped pipe offsets", () => {
    expect(pipePositions("| a | b |")).toEqual([0, 4, 8]);
  });
  it("skips escaped pipes", () => {
    expect(pipePositions("| a \\| b |")).toEqual([0, 9]);
  });
});

describe("computeDeleteRowChange", () => {
  it("removes a body row and its newline, leaving the rest verbatim", () => {
    const result = applyChanges(TABLE, [computeDeleteRowChange(TABLE, 2)!]);
    expect(result).toBe(
      [
        "| Exercise | Set 1 | Set 2 |",
        "|----------|-------|-------|",
        "| DB incline press | | |",
      ].join("\n"),
    );
  });

  it("removes the last row without leaving a trailing blank line", () => {
    const result = applyChanges(TABLE, [computeDeleteRowChange(TABLE, 3)!]);
    expect(result.split("\n")).toHaveLength(3);
    expect(result.endsWith("| Lever chest press | | |")).toBe(true);
  });

  it("refuses to delete the header or delimiter", () => {
    expect(computeDeleteRowChange(TABLE, 0)).toBeNull();
    expect(computeDeleteRowChange(TABLE, 1)).toBeNull();
  });
});

describe("computeDeleteColumnChanges", () => {
  it("drops a column from every row, keeping the table well-formed", () => {
    const result = applyChanges(TABLE, computeDeleteColumnChanges(TABLE, 1));
    const lines = result.split("\n");
    expect(splitRow(lines[0])).toEqual(["Exercise", "Set 2"]);
    expect(splitRow(lines[2])).toEqual(["Lever chest press", ""]);
    // delimiter still has one valid marker per remaining column
    expect(parseAlignments(lines[1])).toHaveLength(2);
  });
});

describe("computeInsertColumnChanges", () => {
  it("inserts an empty column into every row; delimiter stays valid", () => {
    const result = applyChanges(TABLE, computeInsertColumnChanges(TABLE, 1));
    const lines = result.split("\n");
    const header = splitRow(lines[0]);
    expect(header).toHaveLength(4);
    expect(header[1]).toBe(""); // the new empty column
    expect(header).toEqual(["Exercise", "", "Set 1", "Set 2"]);
    // every delimiter cell is a usable alignment marker (contains a dash)
    expect(parseAlignments(lines[1])).toHaveLength(4);
    expect(pipePositions(lines[1])).toHaveLength(5);
  });
});

describe("computeInsertRowChange", () => {
  it("inserts the first body row after the delimiter", () => {
    const { change, newLineIndex } = computeInsertRowChange(TABLE, 1, 3);
    expect(newLineIndex).toBe(2);
    const result = applyChanges(TABLE, [change]);
    expect(result.split("\n")[2]).toBe("| | | |");
  });

  it("inserts after the last row when targeting it", () => {
    const { change, newLineIndex } = computeInsertRowChange(TABLE, 3, 3);
    expect(newLineIndex).toBe(4);
    const result = applyChanges(TABLE, [change]);
    expect(result.split("\n")[4]).toBe("| | | |");
  });
});

describe("computeAlignmentChange", () => {
  it("sets a column's delimiter marker without touching others", () => {
    const result = applyChanges(TABLE, [
      computeAlignmentChange(TABLE, 0, "center")!,
    ]);
    const aligns = parseAlignments(result.split("\n")[1]);
    expect(aligns).toEqual(["center", null, null]);
    // body + header untouched
    expect(result.split("\n")[0]).toBe(TABLE.split("\n")[0]);
  });

  it("supports left/right/none markers", () => {
    const left = applyChanges(TABLE, [computeAlignmentChange(TABLE, 1, "left")!]);
    expect(parseAlignments(left.split("\n")[1])[1]).toBe("left");
    const right = applyChanges(TABLE, [
      computeAlignmentChange(TABLE, 2, "right")!,
    ]);
    expect(parseAlignments(right.split("\n")[1])[2]).toBe("right");
  });
});
