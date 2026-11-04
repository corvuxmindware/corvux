// Build the `corvux-mcp` helper and stage it as a Tauri externalBin sidecar.
//
// Tauri's `externalBin: ["binaries/corvux-mcp"]` expects the binary at
// `src-tauri/binaries/corvux-mcp-<target-triple>(.exe)`. This script builds
// the standalone helper crate (release) and copies it into place with the
// correct triple-suffixed name. It's wired into `beforeBuildCommand`, so
// `pnpm tauri build` always bundles a fresh helper — no committed binary.
import { execSync } from "node:child_process";
import { copyFileSync, mkdirSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const mcpDir = join(root, "corvux-mcp");
const binDir = join(root, "src-tauri", "binaries");
const ext = process.platform === "win32" ? ".exe" : "";

const triple = execSync("rustc -vV").toString().match(/host:\s*(\S+)/)?.[1];
if (!triple) {
  console.error("[sidecar] could not determine host target triple from `rustc -vV`");
  process.exit(1);
}

console.log("[sidecar] building corvux-mcp (release)…");
execSync("cargo build --release", { cwd: mcpDir, stdio: "inherit" });

mkdirSync(binDir, { recursive: true });
const src = join(mcpDir, "target", "release", `corvux-mcp${ext}`);
const dst = join(binDir, `corvux-mcp-${triple}${ext}`);
copyFileSync(src, dst);
console.log(`[sidecar] staged ${dst}`);
