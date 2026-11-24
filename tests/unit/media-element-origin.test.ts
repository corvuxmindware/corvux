/**
 * Ratchet guard: media elements must not come from Solid JSX templates.
 *
 * Bug class (verified on-device, iPhone 16 Pro / iOS WKWebView, 2026-11-06):
 * Solid compiles a JSX `<video>` into a clone of a `<template>`'s content — an
 * element born in the template's inert document and adopted on insert. WebKit
 * plays a MediaStream in such an element (readyState 4, frames readable via
 * drawImage) but never composites its video layer, so the user sees black.
 * The same stream in a `document.createElement("video")` element renders.
 * This is what made the pairing QR scanner's camera preview black on iOS.
 *
 * Rule: build <video> imperatively (`document.createElement`) and insert the
 * node into JSX as `{el}` — see QrScanner.tsx.
 *
 * `<audio>` has no video layer, so the compositing failure can't apply, but
 * it's the same adoption path and it is NOT verified on iOS. Ratcheted at its
 * current site so no new ones appear unexamined; lower the baseline (don't
 * raise it) when AudioPlayer moves to createElement.
 */
import { readdirSync, readFileSync, statSync } from "node:fs";
import { join, relative, sep } from "node:path";
import { describe, it, expect } from "vitest";

const SRC = join(__dirname, "..", "..", "src");

/** Per-file allowance for JSX `<audio>` (unverified on iOS — see header). */
const AUDIO_BASELINE: Record<string, number> = {
  "components/Audio/AudioPlayer.tsx": 1,
};

function tsxFiles(dir: string): string[] {
  const out: string[] = [];
  for (const name of readdirSync(dir)) {
    const p = join(dir, name);
    if (statSync(p).isDirectory()) out.push(...tsxFiles(p));
    else if (p.endsWith(".tsx")) out.push(p);
  }
  return out;
}

/** Count JSX opening tags for `tag`, ignoring comment lines. */
function countJsxTags(source: string, tag: string): number {
  // `$` too: a multi-line JSX tag ends the line right after the name.
  const re = new RegExp(`(^|[\\s({>])<${tag}(?=[\\s/>]|$)`);
  return source.split("\n").filter((line) => {
    const t = line.trim();
    if (t.startsWith("*") || t.startsWith("//") || t.startsWith("/*") || t.startsWith("{/*")) {
      return false;
    }
    return re.test(line);
  }).length;
}

describe("media elements are not created from JSX templates", () => {
  const files = tsxFiles(SRC).map((p) => ({
    rel: relative(SRC, p).split(sep).join("/"),
    src: readFileSync(p, "utf8"),
  }));

  it("has zero JSX <video> elements (use document.createElement)", () => {
    const offenders = files
      .filter((f) => countJsxTags(f.src, "video") > 0)
      .map((f) => f.rel);
    expect(offenders).toEqual([]);
  });

  it("does not add JSX <audio> elements beyond the ratcheted baseline", () => {
    const actual: Record<string, number> = {};
    for (const f of files) {
      const n = countJsxTags(f.src, "audio");
      if (n > 0) actual[f.rel] = n;
    }
    // Exact match: fails on a NEW site and on an unratcheted improvement
    // (lower AUDIO_BASELINE when a site is removed).
    expect(actual).toEqual(AUDIO_BASELINE);
  });

  it("the tag matcher catches JSX and skips comments", () => {
    expect(countJsxTags("  return <video ref={v} />", "video")).toBe(1);
    expect(countJsxTags("      <video\n        class='x'", "video")).toBe(1);
    expect(countJsxTags(" * renders <video> poorly", "video")).toBe(0);
    expect(countJsxTags("{/* Hidden <audio> drives playback */}", "audio")).toBe(0);
    expect(countJsxTags("const s = '<videos>'", "video")).toBe(0);
  });
});
