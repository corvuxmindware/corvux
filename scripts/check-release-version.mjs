import { readFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import path from "node:path";

const appRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");

const read = (relativePath) => readFile(path.join(appRoot, relativePath), "utf8");
const capture = (text, pattern, label) => {
  const match = text.match(pattern);
  if (!match) throw new Error(`Could not read ${label}`);
  return match[1];
};

const [packageJson, tauriJson, appCargo, mcpCargo, infoPlist, appleProject] =
  await Promise.all([
    read("package.json"),
    read("src-tauri/tauri.conf.json"),
    read("src-tauri/Cargo.toml"),
    read("corvux-mcp/Cargo.toml"),
    read("src-tauri/gen/apple/corvux_iOS/Info.plist"),
    read("src-tauri/gen/apple/project.yml"),
  ]);

const packageVersion = JSON.parse(packageJson).version;
const tauriConfig = JSON.parse(tauriJson);
const versions = new Map([
  ["package.json", packageVersion],
  ["src-tauri/tauri.conf.json", tauriConfig.version],
  [
    "src-tauri/Cargo.toml",
    capture(
      appCargo,
      /^\[package\][\s\S]*?^version\s*=\s*"([^"]+)"/m,
      "app Cargo package version",
    ),
  ],
  [
    "corvux-mcp/Cargo.toml",
    capture(
      mcpCargo,
      /^\[package\][\s\S]*?^version\s*=\s*"([^"]+)"/m,
      "MCP Cargo package version",
    ),
  ],
  [
    "iOS CFBundleShortVersionString",
    capture(
      infoPlist,
      /<key>CFBundleShortVersionString<\/key>\s*<string>([^<]+)<\/string>/,
      "iOS marketing version",
    ),
  ],
  [
    "iOS project.yml marketing version",
    capture(
      appleProject,
      /^\s*CFBundleShortVersionString:\s*([^\s]+)\s*$/m,
      "iOS project.yml marketing version",
    ),
  ],
]);

if (!/^\d+\.\d+\.\d+(?:[-+][0-9A-Za-z.-]+)?$/.test(packageVersion)) {
  throw new Error(`Release version is not valid semver: ${packageVersion}`);
}

const mismatches = [...versions].filter(([, version]) => version !== packageVersion);
if (mismatches.length > 0) {
  for (const [source, version] of versions) {
    console.error(`${source}: ${version}`);
  }
  throw new Error(`Release version metadata must all match ${packageVersion}`);
}

const configBuild = String(tauriConfig.bundle?.iOS?.bundleVersion ?? "");
const plistBuild = capture(
  infoPlist,
  /<key>CFBundleVersion<\/key>\s*<string>([^<]+)<\/string>/,
  "iOS build number",
);
const projectBuild = capture(
  appleProject,
  /^\s*CFBundleVersion:\s*["']?([^\s"']+)["']?\s*$/m,
  "iOS project.yml build number",
);
if (
  !/^\d+$/.test(configBuild) ||
  configBuild !== plistBuild ||
  configBuild !== projectBuild
) {
  throw new Error(
    "iOS build numbers must be matching positive integers " +
      `(config=${configBuild}, plist=${plistBuild}, project=${projectBuild})`,
  );
}

console.log(`Release metadata is consistent: ${packageVersion} (iOS build ${configBuild})`);
