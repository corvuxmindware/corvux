import { readdir, stat } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import path from "node:path";

const appRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const assetsDir = path.join(appRoot, "dist", "assets");
const budgetBytes = 1_300 * 1_024;

let names;
try {
  names = await readdir(assetsDir);
} catch (error) {
  console.error("Bundle check requires a production build. Run `pnpm build` first.");
  console.error(error instanceof Error ? error.message : String(error));
  process.exit(1);
}

const candidates = names.filter((name) => /^index-[^.]+\.js$/.test(name));
if (candidates.length === 0) {
  console.error(`No main index-*.js chunk found in ${assetsDir}`);
  process.exit(1);
}

const chunks = await Promise.all(
  candidates.map(async (name) => ({
    name,
    bytes: (await stat(path.join(assetsDir, name))).size,
  })),
);
chunks.sort((a, b) => b.bytes - a.bytes);

const main = chunks[0];
const formatter = new Intl.NumberFormat("en-US");
const headroom = budgetBytes - main.bytes;

console.log(
  `Main chunk ${main.name}: ${formatter.format(main.bytes)} bytes ` +
    `(budget ${formatter.format(budgetBytes)}, headroom ${formatter.format(headroom)})`,
);

if (main.bytes > budgetBytes) {
  if (process.env.GITHUB_ACTIONS) {
    console.error(
      `::error::main chunk ${main.bytes} bytes exceeds budget ${budgetBytes}`,
    );
  }
  process.exit(1);
}
