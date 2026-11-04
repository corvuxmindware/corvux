# Corvux

**Corvux remembers what you forget.**

Corvux is a local-first notes app with an AI agent that reads, searches and connects everything you've written — and asks for your approval before it changes a single file.

Your notes stay plain Markdown files in a folder you choose (a *roost*). Corvux indexes them into a local graph database and gives you wiki-links, backlinks, a live graph view, full-text search, tasks with reminders, and device-to-device sync. It runs on Windows, macOS, Linux, Android and iOS.

Current version: **0.2.4**. Download builds at [meetcorvux.com](https://meetcorvux.com).

## Features

- **Markdown editor** — live preview (CodeMirror 6), tables, task checkboxes, Mermaid diagrams, math, frontmatter properties, daily notes, image paste and voice memos.
- **Knowledge graph** — wiki-links (`[[Note]]`, `[[Note|Alias]]`), backlinks and forward links, and a graph view with PageRank, communities and shortest path (CozoDB).
- **Search** — full-text search across the whole roost (SQLite FTS5), quick switcher and command palette.
- **Tasks** — `- [ ]` checkboxes with a date become tasks, with overdue/today/upcoming views and OS notifications.
- **AI agent** — bring your own key (Gemini, OpenRouter, OpenAI, Anthropic) or run a local model. Writes and deletes wait for your approval (unless you mark a tool as always-approved), and every change is recorded in edit history with one-click revert.
- **MCP server** — `corvux-mcp` exposes a roost to Claude Desktop, Claude Code, Cursor and other MCP clients, confined to the roost folder.
- **Sync** — direct, end-to-end encrypted device-to-device sync (iroh QUIC, Automerge CRDTs), paired by QR code and PIN. No account needed.

## Build from source

### Prerequisites

- [Rust](https://rustup.rs/) (stable)
- [Node.js](https://nodejs.org/) 20+ and [pnpm](https://pnpm.io/) 10 (`corepack enable` picks up the pinned version)
- The [Tauri v2 prerequisites](https://v2.tauri.app/start/prerequisites/) for your OS
- [CMake](https://cmake.org/) and LLVM/Clang (desktop only — used to build the bundled llama.cpp for local AI models)
- Android: Android Studio, SDK and NDK. iOS: a Mac with Xcode.

### Run in development

```bash
git clone https://github.com/corvuxmindware/corvux.git
cd corvux
pnpm install
pnpm sidecar        # builds the corvux-mcp helper once; the app expects it next to the binary
pnpm tauri dev
```

The first Rust build takes a while. Add `--features devtools` to `pnpm tauri dev` to get the WebView inspector.

### Production build

```bash
pnpm tauri build --config src-tauri/tauri.self-build.conf.json
```

Official releases are signed for the auto-updater with a key that is not in this repository. `tauri.self-build.conf.json` turns off updater signing so your own build works without it. Output goes to `src-tauri/target/release/bundle/`.

### Mobile

```bash
pnpm tauri android dev      # or: pnpm tauri android build
pnpm tauri ios dev          # or: pnpm tauri ios build
```

- **Android:** debug builds work as they are. Release signing reads `src-tauri/gen/android/key.properties` — copy `key.properties.example` and fill in your own keystore. Never commit it.
- **iOS:** pick your own team under *Signing & Capabilities* in Xcode, or set `APPLE_DEVELOPMENT_TEAM` before building.

### If you ship your own build

Please use your own name and identity:

- Change `identifier` in `src-tauri/tauri.conf.json` (it is currently `com.corvuxmindware.corvux`).
- Point the updater (`plugins.updater` in the same file) at your own endpoint and public key, or remove it. Otherwise your build will offer to install official Corvux updates.

## Checks

The same checks run in CI on every pull request:

```bash
pnpm exec tsc --noEmit                                     # TypeScript
pnpm test                                                  # Vitest unit tests
pnpm test:e2e                                              # Playwright (browser mock, no Rust needed)
cd src-tauri && cargo fmt --all -- --check && cargo clippy --all-targets -- -D warnings && cargo test --lib
cd corvux-mcp && cargo fmt --all -- --check && cargo clippy --all-targets -- -D warnings && cargo test
```

## Project layout

```
src/            SolidJS frontend (TypeScript, Tailwind)
src-tauri/      Rust backend: commands, CozoDB graph, search, AI agent, sync
  gen/android/  Android project       gen/apple/  iOS project
  plugins/      Local Tauri plugins (Android storage, multicast lock, iOS picker)
  vendor/       Vendored third-party crates, with their licenses
corvux-mcp/     Standalone MCP server
tests/          Vitest unit tests and Playwright end-to-end tests
```

## Contributing

Issues and pull requests are welcome. Please keep each pull request to one change, add or update tests with it, and make sure the checks above pass. By submitting a contribution you agree that it is licensed under the Apache License 2.0, the same as the rest of the project.

## Security

Please don't report security problems in public issues. Email **support@meetcorvux.com** with the details and we will get back to you.

## License

Copyright © Corvux Mindware Private Limited.

The code is licensed under the [Apache License 2.0](LICENSE). Vendored third-party code under `src-tauri/vendor/` keeps its own license files.

The license covers the code, not the Corvux name or logo. If you publish your own build, please give it a different name and icon.
