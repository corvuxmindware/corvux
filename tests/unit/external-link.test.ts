/**
 * Unit tests for the editor's external-link extension.
 *
 * Covers the pure URL helpers plus `getExternalUrlAtPos`, driven against a
 * real markdown-parsed `EditorState` so the syntax-tree walk (Link / autolink
 * / bare URL, and the wikilink + relative-link exclusions) is exercised for
 * real. The click handler and decoration rendering aren't unit-tested here —
 * they depend on CodeMirror layout (`posAtCoords`) which happy-dom can't
 * provide; those are covered by the Playwright suite.
 */

import { describe, it, expect } from "vitest";
import { EditorState } from "@codemirror/state";
import { markdown, markdownLanguage } from "@codemirror/lang-markdown";
import { languages } from "@codemirror/language-data";
import { ensureSyntaxTree } from "@codemirror/language";
import {
  isExternalUrl,
  cleanUrl,
  getExternalUrlAtPos,
} from "../../src/components/Editor/extensions/external-link";

function stateFor(doc: string): EditorState {
  const state = EditorState.create({
    doc,
    extensions: [markdown({ base: markdownLanguage, codeLanguages: languages })],
  });
  // Force a full parse so the syntax tree is complete before we query it.
  ensureSyntaxTree(state, state.doc.length, 5000);
  return state;
}

/** First index of `needle` in `doc`, as a convenience for positioning. */
function at(doc: string, needle: string): number {
  const i = doc.indexOf(needle);
  if (i < 0) throw new Error(`"${needle}" not in doc`);
  return i;
}

describe("isExternalUrl", () => {
  it("accepts http, https and mailto", () => {
    expect(isExternalUrl("https://meetcorvux.com")).toBe(true);
    expect(isExternalUrl("http://example.com")).toBe(true);
    expect(isExternalUrl("mailto:hi@corvux.com")).toBe(true);
    expect(isExternalUrl("  HTTPS://x.com  ")).toBe(true); // trimmed + case-insensitive
  });

  it("rejects relative paths, anchors and other schemes", () => {
    expect(isExternalUrl("note.md")).toBe(false);
    expect(isExternalUrl("#heading")).toBe(false);
    expect(isExternalUrl("./attachments/x.png")).toBe(false);
    expect(isExternalUrl("javascript:alert(1)")).toBe(false);
    expect(isExternalUrl("file:///etc/passwd")).toBe(false);
  });
});

describe("cleanUrl", () => {
  it("strips wrapping angle brackets", () => {
    expect(cleanUrl("<https://x.com>")).toBe("https://x.com");
  });

  it("peels trailing sentence punctuation", () => {
    expect(cleanUrl("https://x.com.")).toBe("https://x.com");
    expect(cleanUrl("https://x.com/path,")).toBe("https://x.com/path");
    expect(cleanUrl("https://x.com!?")).toBe("https://x.com");
  });

  it("leaves a clean url untouched", () => {
    expect(cleanUrl("https://x.com/a/b?q=1")).toBe("https://x.com/a/b?q=1");
  });
});

describe("getExternalUrlAtPos", () => {
  it("resolves an inline markdown link from anywhere in its display text", () => {
    const doc = "See [Corvux](https://meetcorvux.com) for more.";
    const state = stateFor(doc);
    // Position on the visible display text "Corvux".
    expect(getExternalUrlAtPos(state, at(doc, "Corvux"))).toBe(
      "https://meetcorvux.com",
    );
    // Position inside the (rendered-hidden) URL region resolves too.
    expect(getExternalUrlAtPos(state, at(doc, "meetcorvux"))).toBe(
      "https://meetcorvux.com",
    );
  });

  it("resolves a bare / autolinked URL", () => {
    const doc = "ref https://example.com/docs here";
    const state = stateFor(doc);
    expect(getExternalUrlAtPos(state, at(doc, "example"))).toBe(
      "https://example.com/docs",
    );
  });

  it("resolves an angle-bracket autolink without the brackets", () => {
    const doc = "mail <mailto:hi@corvux.com> now";
    const state = stateFor(doc);
    expect(getExternalUrlAtPos(state, at(doc, "corvux.com"))).toBe(
      "mailto:hi@corvux.com",
    );
  });

  it("returns null for a wikilink (handled by wikilink.ts)", () => {
    const doc = "go to [[Some Note]] please";
    const state = stateFor(doc);
    expect(getExternalUrlAtPos(state, at(doc, "Some Note"))).toBeNull();
  });

  it("returns null for a relative/in-doc link", () => {
    const doc = "see [notes](./other.md) and [top](#heading)";
    const state = stateFor(doc);
    expect(getExternalUrlAtPos(state, at(doc, "notes"))).toBeNull();
    expect(getExternalUrlAtPos(state, at(doc, "top"))).toBeNull();
  });

  it("returns null on plain prose", () => {
    const doc = "nothing to click here";
    const state = stateFor(doc);
    expect(getExternalUrlAtPos(state, at(doc, "click"))).toBeNull();
  });
});
