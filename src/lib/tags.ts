/**
 * Tag identity — the frontend twin of `src-tauri/src/parser/tags.rs`
 * ("Tag identity" section) and `corvux-mcp/src/frontmatter.rs`.
 *
 * ONE rule, three implementations, one shared fixture
 * (`corvux-mcp/tests/fixtures/tag_identity.json`, asserted by
 * `tests/unit/tags.test.ts` and by a Rust test on each side):
 *
 * - Characters: Unicode letters / numbers / combining marks plus `_`, `-`, `/`.
 *   Whitespace, punctuation (ASCII or CJK) and emoji end a tag. (Telugu and
 *   Hindi vowel signs and viramas are marks — they must not cut a tag short.)
 * - A tag needs at least one non-numeric, non-mark character (`#1`, `#2024`
 *   are list markers / issue refs, not tags).
 * - Identity: Unicode lowercase of the NFC form. The spelling in the note is
 *   never rewritten; identity is applied when comparing / de-duplicating.
 *
 * The backend index is authoritative. These helpers are for DISPLAY (editor
 * highlighting, hover cards, the browser mock) and for de-duplicating tags a
 * user adds in the Properties panel — they never decide what is indexed.
 */

/** Characters allowed inside a tag (mirror of Rust `is_tag_char`). */
const TAG_CHARS = String.raw`[\p{Alphabetic}\p{N}\p{M}_\/-]`;

/**
 * A FRESH global regex matching inline `#tags`: the `#` must open a line or
 * follow whitespace (any Unicode whitespace); capture group 1 is the tag text.
 * Fresh each call because global regexes carry `lastIndex` state. Matches are
 * NOT yet validated — filter with {@link isValidTagText}.
 */
export function makeInlineTagRegex(): RegExp {
  return new RegExp(String.raw`(?:^|(?<=\s))#(${TAG_CHARS}+)`, "gmu");
}

/** At least one char that is neither a number nor a combining mark. */
export function isValidTagText(tag: string): boolean {
  return /[^\p{N}\p{M}]/u.test(tag);
}

/** Tag identity key: Unicode lowercase of the NFC form. */
export function foldTag(tag: string): string {
  return tag.normalize("NFC").toLowerCase().normalize("NFC");
}

/** Are `a` and `b` the same tag? */
export function sameTag(a: string, b: string): boolean {
  return foldTag(a) === foldTag(b);
}

/**
 * Inline `#tags` in `text`, verbatim and in order. Regex-only: unlike the
 * backend it knows nothing about code fences, headings or frontmatter, so
 * callers that care (the editor highlighter) skip those themselves.
 */
export function extractInlineTags(text: string): string[] {
  const out: string[] = [];
  const re = makeInlineTagRegex();
  let m: RegExpExecArray | null;
  while ((m = re.exec(text)) !== null) {
    if (isValidTagText(m[1])) out.push(m[1]);
  }
  return out;
}
