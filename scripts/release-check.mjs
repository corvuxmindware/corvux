import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import path from "node:path";

const appRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const rustRoot = path.join(appRoot, "src-tauri");
const mcpRoot = path.join(appRoot, "corvux-mcp");
const pnpm = process.platform === "win32" ? "pnpm.cmd" : "pnpm";
const startedAt = Date.now();
const args = new Set(process.argv.slice(2));

if (args.has("--help")) {
  console.log(`Usage: pnpm release:check [options]

Runs the repository-controlled release gate locally.

Options:
  --skip-audit   Skip pnpm audit and cargo-audit (offline use only)
  --skip-native  Skip the native Tauri release build
  --help         Show this help`);
  process.exit(0);
}

const knownArgs = new Set(["--skip-audit", "--skip-native"]);
for (const arg of args) {
  if (!knownArgs.has(arg)) throw new Error(`Unknown release-check option: ${arg}`);
}

function run(label, command, commandArgs, cwd = appRoot) {
  console.log(`\n==> ${label}`);
  const result = spawnSync(command, commandArgs, {
    cwd,
    env: { ...process.env, CI: "true" },
    stdio: "inherit",
    shell: false,
  });
  if (result.error) throw result.error;
  if (result.status !== 0) {
    throw new Error(`${label} failed with exit code ${result.status ?? "unknown"}`);
  }
}

run("Release version consistency", process.execPath, ["scripts/check-release-version.mjs"]);
run("TypeScript", pnpm, ["exec", "tsc", "--noEmit"]);
run("Frontend unit tests", pnpm, ["test"]);
run("Production frontend build", pnpm, ["build"]);
run("Startup bundle budget", pnpm, ["bundle:check"]);
run("Browser-mock E2E", pnpm, ["test:e2e"]);

if (!args.has("--skip-audit")) {
  run("Production dependency audit", pnpm, ["audit", "--prod", "--audit-level=high"]);
}

run("Build and stage MCP sidecar", pnpm, ["sidecar"]);
run("Rust formatting", "cargo", ["fmt", "--all", "--", "--check"], rustRoot);
run("Rust check", "cargo", ["check"], rustRoot);
run(
  "Rust clippy",
  "cargo",
  ["clippy", "--all-targets", "--", "-D", "warnings"],
  rustRoot,
);
run("Rust library tests", "cargo", ["test", "--lib"], rustRoot);
run(
  "Rust integration tests",
  "cargo",
  [
    "test",
    "--test",
    "graph_indexing",
    "--test",
    "ios_bundle_config",
    "--test",
    "macos_bundle_config",
    "--test",
    "sync_invariants",
    "--test",
    "windows_bundle_config",
  ],
  rustRoot,
);
run("MCP formatting", "cargo", ["fmt", "--all", "--", "--check"], mcpRoot);
run(
  "MCP clippy",
  "cargo",
  ["clippy", "--all-targets", "--", "-D", "warnings"],
  mcpRoot,
);
run("MCP tests", "cargo", ["test"], mcpRoot);

if (!args.has("--skip-audit")) {
  run(
    "Rust dependency audit",
    "cargo",
    [
      "audit",
      "--ignore",
      "RUSTSEC-2026-0041",
      "--ignore",
      "RUSTSEC-2024-0429",
      "--ignore",
      "RUSTSEC-2026-0186",
    ],
    rustRoot,
  );
}

if (!args.has("--skip-native")) {
  run("Native Tauri release build (no bundle/signing)", pnpm, ["tauri", "build", "--no-bundle"]);
}

const elapsedSeconds = ((Date.now() - startedAt) / 1_000).toFixed(1);
console.log(`\nRelease gate passed in ${elapsedSeconds}s.`);
