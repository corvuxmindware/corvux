/**
 * Render GFM markdown tables as real HTML tables, Obsidian-style — and let
 * the user edit cells in place without the table ever reverting to raw pipes.
 *
 * Behaviour:
 *  - The whole `Table` node is replaced by a block widget containing a proper
 *    <table> with header row, alignment, and inline formatting (bold, italic,
 *    code, strikethrough, wiki-links). This render is shown at all times —
 *    there is no caret-aware "reveal the raw pipes" mode for tables anymore.
 *  - Click a cell to edit it: a single-line input overlays the cell, seeded
 *    with that cell's raw markdown. Tab / Shift-Tab / Enter move between cells;
 *    Escape or clicking away commits. Edits are accumulated and written back
 *    to the markdown source as ONE surgical transaction that touches only the
 *    changed cells' inter-pipe spans — surrounding formatting stays verbatim.
 *
 * Why a StateField + block decoration: block decorations affect layout before
 * the viewport is computed, so CodeMirror rejects them when they come from a
 * ViewPlugin. This mirrors the frontmatter-hide extension.
 *
 * Trade-off (v1): because tables never reveal raw pipes, structural edits
 * (add/remove a row or column, change alignment) aren't yet possible from the
 * rendered table — cell *content* editing is. Row/column controls and a
 * "edit source" toggle are the planned fast-follow.
 */

import { ensureSyntaxTree, syntaxTree, syntaxTreeAvailable } from "@codemirror/language";
import {
  Decoration,
  DecorationSet,
  EditorView,
  keymap,
  ViewPlugin,
  type ViewUpdate,
  WidgetType,
} from "@codemirror/view";
import {
  EditorState,
  Prec,
  Range,
  StateEffect,
  StateField,
  type Transaction,
} from "@codemirror/state";

/**
 * Fired when the markdown parser has caught up far enough that we should
 * rescan for `Table` nodes. The StateField below treats this like any
 * other rebuild trigger.
 */
const refreshTables = StateEffect.define<null>();

type InlineNav = (target: string) => void;

/**
 * Split a single `| a | b | c |` row into trimmed cell strings. Tolerates
 * missing leading/trailing pipes and extra whitespace.
 */
export function splitRow(line: string): string[] {
  let body = line.trim();
  if (body.startsWith("|")) body = body.slice(1);
  if (body.endsWith("|")) body = body.slice(0, -1);
  // Split on unescaped pipes only; unescape \| sequences in each cell
  return body.split(/(?<!\\)\|/).map((c) => c.trim().replace(/\\\|/g, "|"));
}

/** Parse the delimiter row (`|---|:--:|---:|`) into a per-column alignment. */
export function parseAlignments(line: string): (string | null)[] {
  return splitRow(line).map((cell) => {
    const left = cell.startsWith(":");
    const right = cell.endsWith(":");
    if (left && right) return "center";
    if (right) return "right";
    if (left) return "left";
    return null;
  });
}

/**
 * Physical lines of a table source, each with its character offset relative
 * to the start of the source. Trailing `\r` is stripped from the text but
 * still accounted for in the offset (so CRLF sources map back correctly).
 */
export function physicalLines(
  source: string,
): { text: string; start: number }[] {
  const out: { text: string; start: number }[] = [];
  let start = 0;
  const parts = source.split("\n");
  for (const raw of parts) {
    out.push({ text: raw.replace(/\r$/, ""), start });
    start += raw.length + 1; // +1 for the consumed "\n"
  }
  return out;
}

/**
 * Character spans of each cell's inter-pipe content within a single row line.
 * A cell span runs from just after one unescaped pipe to just before the
 * next. Requires both a leading and trailing pipe (the well-formed GFM shape
 * our renderer emits); returns `[]` for non-standard rows so callers can skip
 * making them editable rather than corrupt them.
 */
export function pipePositions(line: string): number[] {
  const pipes: number[] = [];
  for (let i = 0; i < line.length; i++) {
    if (line[i] === "|" && (i === 0 || line[i - 1] !== "\\")) pipes.push(i);
  }
  return pipes;
}

export function cellSpans(line: string): { start: number; end: number }[] {
  const pipes = pipePositions(line);
  if (pipes.length < 2) return [];
  const spans: { start: number; end: number }[] = [];
  for (let j = 0; j < pipes.length - 1; j++) {
    spans.push({ start: pipes[j] + 1, end: pipes[j + 1] });
  }
  return spans;
}

/** Make a user-typed cell value safe to drop between pipes. */
export function escapeCell(value: string): string {
  return value
    .replace(/\r?\n/g, " ") // cells are single-line
    .replace(/\|/g, "\\|"); // pipes must be escaped
}

/** Inverse of `escapeCell` for the editable view: show raw `|`, not `\|`. */
function unescapeCell(value: string): string {
  return value.replace(/\\\|/g, "|").trim();
}

/**
 * Map a display row (0 = header, 1.. = body rows in order) to the physical
 * line index in the source, skipping the delimiter row and any blank lines.
 */
export function displayRowToLineIndex(
  lines: { text: string }[],
  displayRow: number,
): number {
  if (displayRow === 0) return 0;
  const body: number[] = [];
  for (let i = 2; i < lines.length; i++) {
    if (lines[i].text.trim().length > 0) body.push(i);
  }
  return body[displayRow - 1] ?? -1;
}

export interface RawCellEdit {
  lineIndex: number; // physical line index in the source
  cellIndex: number; // cell index within that line
  value: string; // new (unescaped) cell text
}

/** A blank table row with `colCount` empty cells, e.g. `| | | |`. */
export function blankRow(colCount: number): string {
  return "|" + " |".repeat(colCount);
}

/**
 * Where a row append should insert: right after the last non-whitespace
 * character of the source. Using `source.length` directly would, if the
 * table node's range included a trailing newline, drop the new row after a
 * blank line — and a blank line terminates a GFM table, detaching the row.
 */
function appendInsertOffset(source: string): number {
  return source.replace(/\s*$/, "").length;
}

/**
 * Table-relative change that appends a blank row to the end of the table.
 */
export function computeAppendRowChange(
  source: string,
  colCount: number,
): { from: number; to: number; insert: string } {
  const at = appendInsertOffset(source);
  return { from: at, to: at, insert: "\n" + blankRow(colCount) };
}

/** Physical line index the appended row will occupy in the resulting source. */
export function appendRowLineIndex(source: string): number {
  return source.slice(0, appendInsertOffset(source)).split("\n").length;
}

/**
 * If `pos` sits exactly at a table's start or end boundary, return that
 * table's `from` (its identity for the opener registry); otherwise null.
 * Block-replaced tables only ever place the caret at their boundaries, so
 * a keyboard user arriving from above lands on `from` and from below on `to`.
 */
export function tableAtBoundary(
  ranges: { from: number; to: number }[],
  pos: number,
): number | null {
  for (const r of ranges) {
    if (r.from === pos || r.to === pos) return r.from;
  }
  return null;
}

export type ColumnAlignment = "left" | "center" | "right" | "none";
type ChangeSpec = { from: number; to: number; insert: string };

/** Indices of the physical lines that are real table rows (non-blank). */
function tableRowLineIndices(phys: { text: string }[]): number[] {
  const out: number[] = [];
  for (let i = 0; i < phys.length; i++) {
    if (phys[i].text.trim().length > 0) out.push(i);
  }
  return out;
}

/**
 * Remove an entire row (physical line) plus one adjacent newline, so no blank
 * line is left behind. `lineIndex` must be a body row (the header at line 0 and
 * the delimiter at line 1 are structural and never deleted here).
 */
export function computeDeleteRowChange(
  source: string,
  lineIndex: number,
): ChangeSpec | null {
  const phys = physicalLines(source);
  const line = phys[lineIndex];
  if (!line || lineIndex < 2) return null;
  const next = phys[lineIndex + 1];
  if (next) return { from: line.start, to: next.start, insert: "" };
  // Last line: also consume the newline that precedes it.
  return { from: line.start - 1, to: source.length, insert: "" };
}

/**
 * Remove column `colIndex` from every table row (header, delimiter, body) by
 * deleting the span from its opening pipe up to (not including) the next pipe.
 * Other columns keep their exact text.
 */
export function computeDeleteColumnChanges(
  source: string,
  colIndex: number,
): ChangeSpec[] {
  const phys = physicalLines(source);
  const changes: ChangeSpec[] = [];
  for (const i of tableRowLineIndices(phys)) {
    const pipes = pipePositions(phys[i].text);
    if (pipes.length < colIndex + 2) continue; // row lacks this column
    changes.push({
      from: phys[i].start + pipes[colIndex],
      to: phys[i].start + pipes[colIndex + 1],
      insert: "",
    });
  }
  return changes.sort((a, b) => a.from - b.from);
}

/**
 * Insert a new empty column at `colIndex` (shifting existing columns right) in
 * every table row. The delimiter row gets a `---` cell so it stays valid.
 */
export function computeInsertColumnChanges(
  source: string,
  colIndex: number,
): ChangeSpec[] {
  const phys = physicalLines(source);
  const delimLine = tableRowLineIndices(phys)[1];
  const changes: ChangeSpec[] = [];
  for (const i of tableRowLineIndices(phys)) {
    const pipes = pipePositions(phys[i].text);
    if (pipes.length < 2) continue;
    // Insert just after the pipe that opens the column currently at colIndex
    // (clamped so a request past the last column appends instead).
    const anchor = pipes[Math.min(colIndex, pipes.length - 1)];
    const insert = i === delimLine ? " --- |" : "   |";
    changes.push({ from: phys[i].start + anchor + 1, to: phys[i].start + anchor + 1, insert });
  }
  return changes.sort((a, b) => a.from - b.from);
}

/** Insert a blank row after `lineIndex`; returns the change + new row's line index. */
export function computeInsertRowChange(
  source: string,
  lineIndex: number,
  colCount: number,
): { change: ChangeSpec; newLineIndex: number } {
  const phys = physicalLines(source);
  const next = phys[lineIndex + 1];
  const at = next ? next.start - 1 : source.length;
  return {
    change: { from: at, to: at, insert: "\n" + blankRow(colCount) },
    newLineIndex: source.slice(0, at).split("\n").length,
  };
}

/** Rewrite the delimiter cell for `colIndex` to the requested alignment marker. */
export function computeAlignmentChange(
  source: string,
  colIndex: number,
  align: ColumnAlignment,
): ChangeSpec | null {
  const phys = physicalLines(source);
  const delimIndex = tableRowLineIndices(phys)[1];
  if (delimIndex === undefined) return null;
  const delim = phys[delimIndex];
  const span = cellSpans(delim.text)[colIndex];
  if (!span) return null;
  const marker =
    align === "left"
      ? ":---"
      : align === "center"
        ? ":--:"
        : align === "right"
          ? "---:"
          : "---";
  return {
    from: delim.start + span.start,
    to: delim.start + span.end,
    insert: ` ${marker} `,
  };
}

/**
 * Compute the surgical text changes for a set of cell edits, as offsets
 * relative to the start of the table source. Only the inter-pipe span of each
 * edited cell is replaced; everything else stays byte-for-byte. Sorted by
 * `from` so the result is a valid CodeMirror change set.
 */
export function computeCellChanges(
  source: string,
  edits: RawCellEdit[],
): { from: number; to: number; insert: string }[] {
  const phys = physicalLines(source);
  const changes: { from: number; to: number; insert: string }[] = [];
  for (const edit of edits) {
    const line = phys[edit.lineIndex];
    if (!line) continue;
    const spans = cellSpans(line.text);
    const span = spans[edit.cellIndex];
    if (!span) continue;
    changes.push({
      from: line.start + span.start,
      to: line.start + span.end,
      insert: ` ${escapeCell(edit.value)} `,
    });
  }
  return changes.sort((a, b) => a.from - b.from);
}

/**
 * Minimal inline-markdown renderer for table cells. Not a full parser —
 * just the patterns that show up inside GFM table cells in practice.
 * Regex alternation is ordered so longer opens (`**`, `~~`) match before
 * single-char ones. Unmatched text falls through as plain strings.
 */
const INLINE_RE =
  /(\*\*[^*\n]+\*\*|~~[^~\n]+~~|\*[^*\n]+\*|_[^_\n]+_|`[^`\n]+`|\[\[[^\]\n]+\]\])/g;

function renderInline(text: string, onNavigate?: InlineNav): (Node | string)[] {
  const out: (Node | string)[] = [];
  let last = 0;
  let m: RegExpExecArray | null;
  INLINE_RE.lastIndex = 0;
  while ((m = INLINE_RE.exec(text)) !== null) {
    if (m.index > last) out.push(text.slice(last, m.index));
    const tok = m[0];
    if (tok.startsWith("**")) {
      const el = document.createElement("strong");
      el.textContent = tok.slice(2, -2);
      out.push(el);
    } else if (tok.startsWith("~~")) {
      const el = document.createElement("del");
      el.textContent = tok.slice(2, -2);
      out.push(el);
    } else if (tok.startsWith("`")) {
      const el = document.createElement("code");
      el.textContent = tok.slice(1, -1);
      el.className = "cm-md-table-code";
      out.push(el);
    } else if (tok.startsWith("[[")) {
      const inner = tok.slice(2, -2);
      const pipe = inner.indexOf("|");
      const target = (pipe >= 0 ? inner.slice(0, pipe) : inner).trim();
      const aliasRaw = pipe >= 0 ? inner.slice(pipe + 1).trim() : null;
      const slash = target.lastIndexOf("/");
      const display =
        aliasRaw ?? (slash >= 0 ? target.slice(slash + 1) : target);
      const a = document.createElement("a");
      a.href = "#";
      a.className = "cm-wikilink-link";
      a.textContent = display;
      a.addEventListener("click", (e) => {
        e.preventDefault();
        e.stopPropagation();
        onNavigate?.(target);
      });
      out.push(a);
    } else {
      // *italic* or _italic_ — both wrap in <em>
      const el = document.createElement("em");
      el.textContent = tok.slice(1, -1);
      out.push(el);
    }
    last = m.index + tok.length;
  }
  if (last < text.length) out.push(text.slice(last));
  return out;
}

/**
 * After a row append (which rebuilds the widget DOM), this tells the freshly
 * rendered widget to re-open a specific cell so editing continues seamlessly.
 * Keyed per-view (so split panes can't steal each other's hand-off) and
 * matched to the table via `from`; consumed once on the next render.
 */
const pendingFocusByView = new WeakMap<
  EditorView,
  { from: number; lineIndex: number; cellIndex: number }
>();

/**
 * Live "open this table's Nth editable cell" handles, so a keymap command can
 * drive a rendered widget without reaching into its closure. Keyed per-view,
 * then by the table's `from`. Each render overwrites its own entry, so the
 * handle always targets the currently-mounted DOM.
 */
const cellOpenersByView = new WeakMap<
  EditorView,
  Map<number, (cellIndex: number) => boolean>
>();

function registerOpener(
  view: EditorView,
  from: number,
  open: (cellIndex: number) => boolean,
): void {
  let map = cellOpenersByView.get(view);
  if (!map) {
    map = new Map();
    cellOpenersByView.set(view, map);
  }
  map.set(from, open);
}

/** Per-cell metadata threaded from render into the editing controller. */
interface CellMeta {
  td: HTMLTableCellElement;
  lineIndex: number; // physical source line
  cellIndex: number; // cell index within the line
  displayRow: number;
  col: number;
  editable: boolean;
}

class TableWidget extends WidgetType {
  constructor(
    private source: string,
    private from: number,
    private onNavigate?: InlineNav,
  ) {
    super();
  }

  eq(other: TableWidget): boolean {
    // `from` disambiguates two identical-source tables so CodeMirror never
    // reuses one's DOM (and stale write-back offset) for the other.
    return other.source === this.source && other.from === this.from;
  }

  toDOM(view: EditorView): HTMLElement {
    const wrap = document.createElement("div");
    wrap.className = "cm-md-table-wrap";

    const phys = physicalLines(this.source);
    const visibleLines = phys
      .map((l, i) => ({ ...l, i }))
      .filter((l) => l.text.trim().length > 0);
    if (visibleLines.length < 2) {
      wrap.textContent = this.source;
      return wrap;
    }

    const aligns = parseAlignments(phys[1].text);
    const headerCells = splitRow(phys[0].text);
    const colCount = headerCells.length;

    const table = document.createElement("table");
    table.className = "cm-md-table";

    // --- editing controller state (per render) ---
    const edits = new Map<string, RawCellEdit>(); // key `${line}:${cell}`
    const editable: CellMeta[] = []; // editable cells in row-major order
    const input = document.createElement("input");
    input.type = "text";
    input.className = "cm-md-table-cell-input";
    input.setAttribute(
      "aria-label",
      "Edit table cell — press F10 for row and column tools",
    );
    input.style.display = "none";
    let active: CellMeta | null = null;
    let sessionActive = false;

    const originalRaw = (m: CellMeta): string => {
      const span = cellSpans(phys[m.lineIndex].text)[m.cellIndex];
      if (!span) return "";
      return unescapeCell(phys[m.lineIndex].text.slice(span.start, span.end));
    };

    const renderCell = (m: CellMeta, value: string) => {
      m.td.replaceChildren();
      for (const node of renderInline(value, this.onNavigate)) m.td.append(node);
    };

    const commitActive = () => {
      if (!active) return;
      const value = input.value.trim();
      const key = `${active.lineIndex}:${active.cellIndex}`;
      if (value !== originalRaw(active)) {
        edits.set(key, {
          lineIndex: active.lineIndex,
          cellIndex: active.cellIndex,
          value,
        });
      } else {
        edits.delete(key);
      }
      renderCell(active, value);
    };

    const openCell = (m: CellMeta) => {
      if (!m.editable) return;
      commitActive(); // flush the previously-edited cell into the model
      active = m;
      sessionActive = true;
      const wrapRect = wrap.getBoundingClientRect();
      const cellRect = m.td.getBoundingClientRect();
      const relLeft = cellRect.left - wrapRect.left + wrap.scrollLeft;
      const relTop = cellRect.top - wrapRect.top + wrap.scrollTop;
      input.style.left = `${relLeft}px`;
      input.style.top = `${relTop}px`;
      input.style.width = `${cellRect.width}px`;
      input.style.height = `${cellRect.height}px`;
      input.style.textAlign = aligns[m.col] ?? "left";
      input.style.display = "block";

      // Anchor the toolbar just above the active cell — flipping below when
      // there's no room above — so it never covers the cell being edited.
      toolbar.style.display = "flex";
      updateToolbar();
      const above = relTop - toolbar.offsetHeight - 4;
      toolbar.style.top = `${above >= 0 ? above : relTop + cellRect.height + 4}px`;
      const maxLeft = Math.max(0, wrap.scrollWidth - toolbar.offsetWidth);
      toolbar.style.left = `${Math.min(relLeft, maxLeft)}px`;

      const key = `${m.lineIndex}:${m.cellIndex}`;
      input.value = edits.get(key)?.value ?? originalRaw(m);
      input.focus();
      input.select();
    };

    // Accumulated cell edits as absolute (document-space) change specs.
    const buildEditChanges = () =>
      computeCellChanges(this.source, [...edits.values()]).map((c) => ({
        from: c.from + this.from,
        to: c.to + this.from,
        insert: c.insert,
      }));

    const endSession = (refocusEditor: boolean) => {
      if (!sessionActive) return;
      sessionActive = false;
      commitActive();
      active = null;
      input.style.display = "none";
      toolbar.style.display = "none";
      const changes = buildEditChanges();
      if (changes.length > 0) view.dispatch({ changes });
      // Return focus to the editor on an explicit exit (Escape / Tab past the
      // end), so the keyboard keeps working instead of dying on the now-hidden
      // input. A blur-driven exit (clicking elsewhere) passes false and leaves
      // focus where the user put it.
      if (refocusEditor) view.focus();
    };

    // Tab past the last cell grows the table: flush pending cell edits, append
    // a blank row, and hand off focus to its first cell after the rebuild.
    const appendRow = () => {
      commitActive();
      sessionActive = false;
      active = null;
      input.style.display = "none";
      toolbar.style.display = "none";
      const append = computeAppendRowChange(this.source, colCount);
      const changes = buildEditChanges();
      changes.push({
        from: append.from + this.from,
        to: append.to + this.from,
        insert: append.insert,
      });
      changes.sort((a, b) => a.from - b.from);
      pendingFocusByView.set(view, {
        from: this.from,
        lineIndex: appendRowLineIndex(this.source),
        cellIndex: 0,
      });
      view.dispatch({ changes });
    };

    // --- structural editing: rows, columns, alignment ---
    const toolbar = document.createElement("div");
    toolbar.className = "cm-md-table-toolbar";
    toolbar.style.display = "none";
    toolbar.setAttribute("role", "toolbar");
    toolbar.setAttribute("aria-label", "Table structure");

    // Run a structural change: flush the active cell, drop any pending cell
    // edits that the change would invalidate, combine everything into one
    // transaction, and either hand focus to a follow-up cell or the editor.
    const runStructural = (
      structural: ChangeSpec | ChangeSpec[] | null,
      drop: (e: RawCellEdit) => boolean,
      focusAfter: { lineIndex: number; cellIndex: number } | null,
    ) => {
      if (!active || !structural) return;
      const arr = Array.isArray(structural) ? structural : [structural];
      if (arr.length === 0) return;
      commitActive();
      const surviving = [...edits.values()].filter((e) => !drop(e));
      const combined = [...computeCellChanges(this.source, surviving), ...arr]
        .map((c) => ({
          from: c.from + this.from,
          to: c.to + this.from,
          insert: c.insert,
        }))
        // Sort by (from, then to) so a zero-length insert is ordered before a
        // replacement that starts at the same point — CodeMirror allows the
        // two to touch in that order, but rejects them reversed as overlap.
        .sort((a, b) => a.from - b.from || a.to - b.to);
      sessionActive = false;
      active = null;
      input.style.display = "none";
      toolbar.style.display = "none";
      if (focusAfter) {
        pendingFocusByView.set(view, { from: this.from, ...focusAfter });
      }
      view.dispatch({ changes: combined });
      if (!focusAfter) view.focus();
    };

    const makeBtn = (label: string, title: string, onClick: () => void) => {
      const b = document.createElement("button");
      b.type = "button";
      b.className = "cm-md-table-btn";
      b.textContent = label;
      b.title = title;
      b.setAttribute("aria-label", title);
      b.tabIndex = -1; // roving focus: reached via F10, moved with arrows
      // Keep focus on the input so a mouse click doesn't end the session.
      b.addEventListener("mousedown", (e) => e.preventDefault());
      b.addEventListener("click", (e) => {
        e.preventDefault();
        onClick();
      });
      return b;
    };
    const sep = () => {
      const s = document.createElement("span");
      s.className = "cm-md-table-sep";
      return s;
    };

    const btnRowAbove = makeBtn("＋↑", "Insert row above", () => {
      if (!active || active.displayRow === 0) return;
      const r = computeInsertRowChange(this.source, active.lineIndex - 1, colCount);
      runStructural(r.change, () => false, {
        lineIndex: r.newLineIndex,
        cellIndex: active.col,
      });
    });
    const btnRowBelow = makeBtn("＋↓", "Insert row below", () => {
      if (!active) return;
      // A row "below" the header becomes the first body row, after the delimiter.
      const afterLine = active.displayRow === 0 ? 1 : active.lineIndex;
      const r = computeInsertRowChange(this.source, afterLine, colCount);
      runStructural(r.change, () => false, {
        lineIndex: r.newLineIndex,
        cellIndex: active.col,
      });
    });
    const btnRowDel = makeBtn("－row", "Delete row", () => {
      if (!active || active.displayRow === 0) return;
      const line = active.lineIndex;
      runStructural(
        computeDeleteRowChange(this.source, line),
        (e) => e.lineIndex === line,
        null,
      );
    });
    const btnColLeft = makeBtn("＋←", "Insert column left", () => {
      if (!active) return;
      const col = active.col;
      runStructural(computeInsertColumnChanges(this.source, col), () => false, {
        lineIndex: active.lineIndex,
        cellIndex: col,
      });
    });
    const btnColRight = makeBtn("＋→", "Insert column right", () => {
      if (!active) return;
      const col = active.col + 1;
      runStructural(computeInsertColumnChanges(this.source, col), () => false, {
        lineIndex: active.lineIndex,
        cellIndex: col,
      });
    });
    const btnColDel = makeBtn("－col", "Delete column", () => {
      if (!active || colCount <= 1) return;
      const col = active.col;
      runStructural(
        computeDeleteColumnChanges(this.source, col),
        (e) => e.cellIndex === col,
        null,
      );
    });
    const alignBtn = (label: string, title: string, a: ColumnAlignment) =>
      makeBtn(label, title, () => {
        if (!active) return;
        runStructural(
          computeAlignmentChange(this.source, active.col, a),
          () => false,
          { lineIndex: active.lineIndex, cellIndex: active.col },
        );
      });
    const btnAlignLeft = alignBtn("⤆", "Align left", "left");
    const btnAlignCenter = alignBtn("≡", "Align center", "center");
    const btnAlignRight = alignBtn("⤇", "Align right", "right");

    toolbar.append(
      btnRowAbove,
      btnRowBelow,
      btnRowDel,
      sep(),
      btnColLeft,
      btnColRight,
      btnColDel,
      sep(),
      btnAlignLeft,
      btnAlignCenter,
      btnAlignRight,
    );

    const updateToolbar = () => {
      const isHeader = !!active && active.displayRow === 0;
      btnRowAbove.disabled = isHeader; // nothing goes above the header
      btnRowDel.disabled = isHeader; // the header isn't a deletable row
      btnColDel.disabled = colCount <= 1; // never delete the last column
    };

    const enabledButtons = () =>
      Array.from(
        toolbar.querySelectorAll<HTMLButtonElement>("button"),
      ).filter((b) => !b.disabled);

    // F10 (WAI-ARIA's "move focus to toolbar") jumps from the cell editor to
    // the first enabled control.
    const focusToolbar = () => {
      updateToolbar();
      enabledButtons()[0]?.focus();
    };

    // Roving-focus keyboard nav within the toolbar: arrows move, Home/End
    // jump, Escape/Tab return to the cell editor. Enter/Space fall through to
    // the buttons' native click.
    toolbar.addEventListener("keydown", (e) => {
      const btns = enabledButtons();
      if (btns.length === 0) return;
      const idx = btns.indexOf(document.activeElement as HTMLButtonElement);
      if (e.key === "ArrowRight" || e.key === "ArrowDown") {
        e.preventDefault();
        btns[(idx + 1 + btns.length) % btns.length].focus();
      } else if (e.key === "ArrowLeft" || e.key === "ArrowUp") {
        e.preventDefault();
        btns[(idx - 1 + btns.length) % btns.length].focus();
      } else if (e.key === "Home") {
        e.preventDefault();
        btns[0].focus();
      } else if (e.key === "End") {
        e.preventDefault();
        btns[btns.length - 1].focus();
      } else if (e.key === "Escape" || e.key === "Tab") {
        e.preventDefault();
        if (active) input.focus();
        else endSession(true);
      }
    });

    const indexOf = (m: CellMeta) =>
      editable.findIndex(
        (c) => c.lineIndex === m.lineIndex && c.cellIndex === m.cellIndex,
      );

    input.addEventListener("keydown", (e) => {
      if (!active) return;
      // Don't hijack navigation keys while an IME composition is in flight
      // (CJK, accents) — that would abort the composition mid-word.
      if (e.isComposing) return;
      if (e.key === "Escape") {
        e.preventDefault();
        endSession(true);
      } else if (e.key === "Tab") {
        e.preventDefault();
        const idx = indexOf(active);
        if (e.shiftKey) {
          const prev = editable[idx - 1];
          if (prev) openCell(prev);
          else endSession(true);
        } else {
          const next = editable[idx + 1];
          if (next) openCell(next);
          else appendRow(); // grow the table when tabbing past the last cell
        }
      } else if (e.key === "F10") {
        e.preventDefault();
        focusToolbar(); // hand off to the row/column/align controls
      } else if (e.key === "Enter") {
        e.preventDefault();
        const cur = active;
        const down = editable.find(
          (c) => c.col === cur.col && c.displayRow > cur.displayRow,
        );
        if (down) openCell(down);
        else endSession(true);
      }
    });

    // End the session only when focus truly leaves the whole table. Moving
    // between the cell input and the toolbar buttons stays inside `wrap`, so it
    // won't commit; clicking a cell preventDefaults its mousedown so focus
    // never leaves during cell-to-cell navigation.
    wrap.addEventListener("focusout", (e) => {
      if (!wrap.contains(e.relatedTarget as Node | null)) endSession(false);
    });

    const makeCell = (
      tag: "th" | "td",
      raw: string,
      displayRow: number,
      lineIndex: number,
      col: number,
      editableRow: boolean,
    ): HTMLTableCellElement => {
      const cell = document.createElement(tag);
      if (aligns[col]) cell.style.textAlign = aligns[col]!;
      const key = `${lineIndex}:${col}`;
      const value = edits.get(key)?.value ?? raw;
      for (const node of renderInline(value, this.onNavigate)) cell.append(node);
      const meta: CellMeta = {
        td: cell,
        lineIndex,
        cellIndex: col,
        displayRow,
        col,
        editable: editableRow,
      };
      if (editableRow) {
        editable.push(meta);
        cell.classList.add("cm-md-table-editable");
        cell.addEventListener("mousedown", (e) => {
          // Let wiki-link clicks navigate instead of opening the cell editor.
          if ((e.target as HTMLElement).closest("a.cm-wikilink-link")) return;
          e.preventDefault(); // keep focus on our single shared input
          e.stopPropagation();
          openCell(meta);
        });
      }
      return cell;
    };

    // Header row maps to physical line 0; editable when its pipe layout is
    // well-formed (cell count matches the inter-pipe span count).
    const headerEditable = cellSpans(phys[0].text).length === colCount;
    const thead = document.createElement("thead");
    const headerRow = document.createElement("tr");
    headerCells.forEach((raw, col) => {
      headerRow.append(makeCell("th", raw, 0, 0, col, headerEditable));
    });
    thead.append(headerRow);
    table.append(thead);

    // Body rows are the visible lines after header + delimiter.
    const tbody = document.createElement("tbody");
    let displayRow = 1;
    for (let v = 2; v < visibleLines.length; v++) {
      const lineIndex = visibleLines[v].i;
      const rowCells = splitRow(phys[lineIndex].text);
      const rowEditable =
        cellSpans(phys[lineIndex].text).length === rowCells.length;
      const tr = document.createElement("tr");
      rowCells.forEach((raw, col) => {
        tr.append(makeCell("td", raw, displayRow, lineIndex, col, rowEditable));
      });
      tbody.append(tr);
      displayRow++;
    }
    table.append(tbody);

    wrap.append(table);
    wrap.append(input);
    wrap.append(toolbar);

    // Expose an imperative opener so the keyboard-entry keymap can focus a
    // cell in this live DOM. Re-registered on every render, so it always
    // points at the mounted widget.
    registerOpener(view, this.from, (cellIndex) => {
      const target = editable[cellIndex];
      if (!target) return false;
      openCell(target);
      return true;
    });

    // If a row append just rebuilt us, continue editing in the new row's
    // first cell. Deferred to rAF so the wrap is in the document and
    // getBoundingClientRect returns real geometry for positioning the input.
    const pf = pendingFocusByView.get(view);
    if (pf && pf.from === this.from) {
      pendingFocusByView.delete(view);
      const target = editable.find(
        (m) => m.lineIndex === pf.lineIndex && m.cellIndex === pf.cellIndex,
      );
      if (target) requestAnimationFrame(() => openCell(target));
    }

    return wrap;
  }

  ignoreEvent(): boolean {
    // We manage all editing inside the widget ourselves; tell CodeMirror to
    // keep its hands off events that originate here (cursor placement, etc.).
    return true;
  }
}

function buildDecorations(
  state: EditorState,
  onNavigate?: InlineNav,
): DecorationSet {
  const doc = state.doc;
  const out: Range<Decoration>[] = [];

  syntaxTree(state).iterate({
    enter: (node) => {
      if (node.type.name !== "Table") return;
      const source = doc.sliceString(node.from, node.to);
      out.push(
        Decoration.replace({
          widget: new TableWidget(source, node.from, onNavigate),
          block: true,
        }).range(node.from, node.to),
      );
    },
  });

  return Decoration.set(out, true);
}

const tableTheme = EditorView.baseTheme({
  // Padding (not margin) — see image-embed.ts for the rationale. Margins
  // on a block widget's wrapper become invisible vertical space CM's
  // height map ignores, drifting click-to-position math below the table.
  ".cm-md-table-wrap": {
    paddingTop: "12px",
    paddingBottom: "12px",
    overflowX: "auto",
    position: "relative", // anchors the absolutely-positioned cell editor
  },
  ".cm-md-table": {
    borderCollapse: "collapse",
    width: "100%",
    fontSize: "0.95em",
    lineHeight: "1.5",
  },
  ".cm-md-table th, .cm-md-table td": {
    border: "1px solid var(--color-border)",
    padding: "6px 10px",
    verticalAlign: "top",
  },
  ".cm-md-table th": {
    background: "var(--color-bg-secondary)",
    color: "var(--color-text-primary)",
    fontWeight: "600",
    textAlign: "left",
  },
  ".cm-md-table tbody tr:nth-child(even) td": {
    background:
      "color-mix(in srgb, var(--color-bg-secondary) 40%, transparent)",
  },
  ".cm-md-table-editable": {
    cursor: "text",
  },
  ".cm-md-table-code": {
    fontFamily: "'JetBrains Mono', 'Fira Code', monospace",
    fontSize: "0.9em",
    background: "var(--color-bg-secondary)",
    padding: "1px 5px",
    borderRadius: "3px",
    color: "var(--color-accent)",
  },
  ".cm-md-table-cell-input": {
    position: "absolute",
    boxSizing: "border-box",
    margin: "0",
    padding: "6px 10px",
    border: "1px solid var(--color-accent)",
    borderRadius: "2px",
    background: "var(--color-bg-primary)",
    color: "var(--color-text-primary)",
    font: "inherit",
    fontSize: "0.95em",
    lineHeight: "1.5",
    outline: "none",
    zIndex: "2",
  },
  ".cm-md-table-toolbar": {
    position: "absolute", // top/left set per-cell in openCell
    display: "none", // toggled to flex while a cell is being edited
    alignItems: "center",
    gap: "2px",
    padding: "3px",
    background: "var(--color-bg-primary)",
    border: "1px solid var(--color-border)",
    borderRadius: "6px",
    boxShadow: "0 2px 8px rgba(0, 0, 0, 0.25)",
    zIndex: "3",
  },
  ".cm-md-table-btn": {
    font: "inherit",
    fontSize: "0.8em",
    lineHeight: "1",
    padding: "4px 6px",
    border: "1px solid transparent",
    borderRadius: "4px",
    background: "transparent",
    color: "var(--color-text-primary)",
    cursor: "pointer",
    whiteSpace: "nowrap",
  },
  ".cm-md-table-btn:hover:not(:disabled)": {
    background: "var(--color-bg-secondary)",
    borderColor: "var(--color-border)",
  },
  ".cm-md-table-btn:disabled": {
    opacity: "0.35",
    cursor: "default",
  },
  ".cm-md-table-sep": {
    width: "1px",
    alignSelf: "stretch",
    margin: "2px 2px",
    background: "var(--color-border)",
  },
});

/** Render GFM tables as editable HTML tables. Pass `onNavigate` for wiki-link clicks. */
export function tableRender(onNavigate?: InlineNav) {
  const field = StateField.define<DecorationSet>({
    create(state) {
      return buildDecorations(state, onNavigate);
    },
    update(value, tr: Transaction) {
      const parseCatchUp = tr.effects.some((e) => e.is(refreshTables));
      // Tables no longer reveal raw source on selection, so a bare selection
      // change can't affect them — only doc edits and parser catch-up do.
      if (!tr.docChanged && !parseCatchUp) return value;
      return buildDecorations(tr.state, onNavigate);
    },
    provide: (f) => EditorView.decorations.from(f),
  });

  // The markdown parser runs incrementally. On a fresh editor mount the
  // StateField's `create` walks the tree before parsing reaches the body,
  // so `Table` nodes don't exist yet and no decorations are emitted.
  // Without this watcher, tables stay as raw pipes until the next
  // doc change rebuilds the field. Poll for parse progress on each
  // ViewUpdate; once the tree covers the whole doc, fire a one-shot
  // refresh effect so the StateField rebuilds with the now-complete tree.
  const parseWatcher = ViewPlugin.fromClass(
    class {
      done = false;
      constructor(view: EditorView) {
        this.check(view);
      }
      update(u: ViewUpdate) {
        if (this.done) return;
        if (u.docChanged) this.done = false;
        this.check(u.view);
      }
      check(view: EditorView) {
        const end = view.state.doc.length;
        if (syntaxTreeAvailable(view.state, end)) {
          this.done = true;
          queueMicrotask(() => {
            view.dispatch({ effects: refreshTables.of(null) });
          });
          return;
        }
        // Nudge the parser; if it still hasn't reached the end, the next
        // ViewUpdate (parser progress dispatches viewport updates) will
        // bring us back here.
        ensureSyntaxTree(view.state, end, 50);
      }
    },
  );

  // Keyboard entry: when the caret sits empty at a table boundary, Enter
  // opens that table's first editable cell instead of inserting a newline.
  // Returns false in every other case so normal Enter behaviour is untouched.
  const enterTable = (view: EditorView): boolean => {
    const sel = view.state.selection.main;
    if (!sel.empty) return false;
    const pos = sel.head;
    const ranges: { from: number; to: number }[] = [];
    syntaxTree(view.state).iterate({
      from: Math.max(0, pos - 1),
      to: Math.min(view.state.doc.length, pos + 1),
      enter: (node) => {
        if (node.type.name === "Table") {
          ranges.push({ from: node.from, to: node.to });
        }
      },
    });
    const from = tableAtBoundary(ranges, pos);
    if (from === null) return false;
    const open = cellOpenersByView.get(view)?.get(from);
    return open ? open(0) : false;
  };

  const keyboardEntry = Prec.high(
    keymap.of([{ key: "Enter", run: enterTable }]),
  );

  return [field, parseWatcher, tableTheme, keyboardEntry];
}
