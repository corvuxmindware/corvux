// Guards the unprotected `parents.pop()` in @codemirror/view's TilePointer.advance.
// Without this, advancing past the last tile throws
// "Cannot destructure property 'tile' of 'parents.pop(...)' as it is undefined"
// and the whole editor viewport update aborts.
// Upstream: @codemirror/view 6.41.0 src/tile.ts `advance()` — reported as dev#1652.

import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const repoRoot = path.resolve(__dirname, "..");

const targets = [
  path.join(repoRoot, "node_modules/@codemirror/view/dist/index.js"),
  path.join(repoRoot, "node_modules/@codemirror/view/dist/index.cjs"),
];

const MARKER = "cv-tilepointer-guard";

// Written against @codemirror/view 6.41.0. There are TWO unguarded `parents.pop()`
// sites in TilePointer.advance — one in the `index == tile.length` branch, another
// at the end-of-composite-tile branch right after `walker.leave(tile)`. Both must
// be patched or the crash just moves to the other path. If the surrounding code
// drifts, replacements silently no-op so installs don't fail — we just log.
const patches = [
  {
    name: "length-branch",
    needle: `                if (index == tile.length) {
                    beforeBreak = !!tile.breakAfter;
                    ({ tile, index } = parents.pop());
                    index++;
                }`,
    replacement: `                if (index == tile.length) {
                    beforeBreak = !!tile.breakAfter;
                    const __p = parents.pop(); /* ${MARKER} */
                    if (!__p) break;
                    tile = __p.tile;
                    index = __p.index;
                    index++;
                }`,
  },
  {
    name: "walker-leave-branch",
    needle: `                if (walker)
                    walker.leave(tile);
                beforeBreak = !!tile.breakAfter;
                ({ tile, index } = parents.pop());
                index++;`,
    replacement: `                if (walker)
                    walker.leave(tile);
                beforeBreak = !!tile.breakAfter;
                const __p2 = parents.pop(); /* ${MARKER}-2 */
                if (!__p2) break;
                tile = __p2.tile;
                index = __p2.index;
                index++;`,
  },
];

let patchedAny = false;
let allSitesCovered = true;

for (const file of targets) {
  if (!fs.existsSync(file)) continue;
  const rel = path.relative(repoRoot, file);
  let content = fs.readFileSync(file, "utf8");
  let changed = false;

  for (const { name, needle, replacement } of patches) {
    // Already-patched sites leave the MARKER in place; skip those silently.
    // Sites that still contain the raw needle get rewritten.
    if (content.includes(needle)) {
      content = content.replace(needle, replacement);
      changed = true;
      console.log(`[patch-cm-view] patched ${rel} (${name})`);
    } else if (!content.includes(`${MARKER}${name === "walker-leave-branch" ? "-2" : ""}`)) {
      // Neither the needle nor our marker — code has drifted.
      console.warn(`[patch-cm-view] ${name} block not found in ${rel} — CodeMirror may have drifted.`);
      allSitesCovered = false;
    }
  }

  if (changed) {
    fs.writeFileSync(file, content);
    patchedAny = true;
  }
}

// Vite pre-bundles deps into .vite/deps/chunk-*.js and caches them. If we patch
// node_modules but leave the cache, dev mode keeps serving the unpatched copy.
// Nuking the cache is cheap — Vite rebuilds it on next start.
const viteDeps = path.join(repoRoot, "node_modules/.vite/deps");
if (patchedAny && fs.existsSync(viteDeps)) {
  fs.rmSync(viteDeps, { recursive: true, force: true });
  console.log("[patch-cm-view] cleared node_modules/.vite/deps");
}

if (!patchedAny && allSitesCovered) {
  console.log("[patch-cm-view] already patched");
}
