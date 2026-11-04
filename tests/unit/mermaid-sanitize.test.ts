/**
 * S-05 regression tests for the mermaid SVG sanitize fallback.
 *
 * Mermaid 11.15 strips most of these payloads in its own internal
 * DOMPurify pass before returning the SVG string, but if a future
 * upstream change drifts (or our cache hits a pre-mermaid-strict
 * generation of an SVG), this regex backstop must still catch
 * `<script>`, inline event handlers, and dangerous `javascript:` /
 * `data:` URL schemes on href / xlink:href.
 */

import { describe, it, expect } from "vitest";
import { sanitizeSvg } from "../../src/components/Editor/extensions/mermaid-render";

describe("S-05 — mermaid SVG sanitize fallback", () => {
  it("strips <script> blocks (single-line)", () => {
    const dirty = `<svg><script>alert(1)</script></svg>`;
    const clean = sanitizeSvg(dirty);
    expect(clean).not.toContain("<script");
    expect(clean).not.toContain("alert(1)");
  });

  it("strips <script> blocks (multi-line with attributes)", () => {
    const dirty = `<svg>\n<script type="text/javascript">\nalert(1);\n</script>\n</svg>`;
    const clean = sanitizeSvg(dirty);
    expect(clean).not.toContain("<script");
    expect(clean).not.toContain("alert(1)");
  });

  it("strips inline onload= handler", () => {
    const dirty = `<svg onload="alert(1)" width="100"></svg>`;
    const clean = sanitizeSvg(dirty);
    expect(clean).not.toMatch(/onload\s*=/i);
    expect(clean).not.toContain("alert(1)");
    // Non-handler attributes survive.
    expect(clean).toContain('width="100"');
  });

  it("strips inline onclick / onerror / onfocus handlers", () => {
    const dirty =
      `<svg><circle onclick="x()" onerror='y()' onfocus=z></circle></svg>`;
    const clean = sanitizeSvg(dirty);
    expect(clean).not.toMatch(/onclick\s*=/i);
    expect(clean).not.toMatch(/onerror\s*=/i);
    expect(clean).not.toMatch(/onfocus\s*=/i);
  });

  it("neutralizes javascript: href", () => {
    const dirty = `<svg><a href="javascript:alert(1)">link</a></svg>`;
    const clean = sanitizeSvg(dirty);
    expect(clean).not.toMatch(/href\s*=\s*["']?javascript:/i);
  });

  it("neutralizes javascript: xlink:href", () => {
    const dirty = `<svg><a xlink:href="javascript:alert(1)">link</a></svg>`;
    const clean = sanitizeSvg(dirty);
    expect(clean).not.toMatch(/xlink:href\s*=\s*["']?javascript:/i);
  });

  it("neutralizes data: href", () => {
    const dirty = `<svg><a href="data:text/html,<script>alert(1)</script>">link</a></svg>`;
    const clean = sanitizeSvg(dirty);
    expect(clean).not.toMatch(/href\s*=\s*["']?data:/i);
  });

  it("preserves legitimate https:// hrefs", () => {
    const dirty = `<svg><a xlink:href="https://example.com">link</a></svg>`;
    const clean = sanitizeSvg(dirty);
    expect(clean).toContain('xlink:href="https://example.com"');
  });

  it("preserves real mermaid label tags inside foreignObject", () => {
    // foreignObject + XHTML labels are the case that broke us under
    // DOMPurify's second pass — confirm the regex backstop leaves them
    // intact.
    const ok = `<svg><foreignObject><div xmlns="http://www.w3.org/1999/xhtml"><span>label</span></div></foreignObject></svg>`;
    expect(sanitizeSvg(ok)).toBe(ok);
  });

  it("idempotent on already-clean SVG", () => {
    const ok = `<svg viewBox="0 0 10 10"><circle cx="5" cy="5" r="3"/></svg>`;
    expect(sanitizeSvg(ok)).toBe(ok);
  });
});
