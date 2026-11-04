import { defineConfig, devices } from "@playwright/test";

/**
 * Playwright config — drives the SolidJS frontend in browser-mock mode
 * (NOT through Tauri WebView2). Vite's dev server auto-routes Tauri
 * `invoke` calls through the mock layer in `src/lib/invoke.ts`, so
 * E2E tests don't need a Rust backend running.
 *
 * What this catches:
 *   - SolidJS reactivity regressions (a signal not propagating)
 *   - Wiring breakage between stores and components
 *   - Visible UI bugs (button doesn't toggle the panel, etc.)
 *
 * What this does NOT catch (manual checklist territory):
 *   - Real Gemini API responses
 *   - File-system writes from Rust (memory.md, chat-sessions/)
 *   - FTS5 / HNSW behavior at real vault scale
 *   - Tauri V2 plugin / IPC bugs
 *
 * The `webServer` block boots `pnpm dev` automatically before tests
 * run and tears it down afterward. Set `PWDEBUG=1` to step through a
 * test in headed mode.
 */
export default defineConfig({
  testDir: "./tests/e2e",
  timeout: 30_000,
  expect: { timeout: 5_000 },
  fullyParallel: false, // store singletons share state — keep tests serial
  forbidOnly: !!process.env.CI,
  retries: process.env.CI ? 1 : 0,
  workers: 1,
  reporter: process.env.CI ? "github" : "list",
  use: {
    baseURL: "http://localhost:1420",
    trace: "retain-on-failure",
    screenshot: "only-on-failure",
  },
  projects: [
    {
      name: "chromium",
      use: { ...devices["Desktop Chrome"] },
    },
  ],
  webServer: {
    command: "pnpm dev",
    port: 1420,
    reuseExistingServer: !process.env.CI,
    timeout: 60_000,
  },
});
