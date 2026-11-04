import { defineConfig } from "vitest/config";
import solid from "vite-plugin-solid";

/**
 * Vitest config — runs pure-logic unit tests on the SolidJS / TS code.
 *
 * Why happy-dom: lighter than jsdom, fast enough for our store + helper
 * tests. We don't render full SolidJS components here; component
 * interaction tests live in the Playwright suite.
 *
 * What's in scope: chat store (sessions, memory hooks, switch), search
 * store (mode, hybrid filter), settings store (profile setter), pure
 * helpers (truncate, fuzzy date, RRF math if exposed).
 *
 * What's NOT in scope: SolidJS component rendering, Tauri IPC behavior
 * — Playwright handles those via the browser-mock invoke layer.
 */
export default defineConfig({
  // `hot: false` disables solid-refresh instrumentation, which crashes
  // under vitest 4's module evaluator (createRequire trips over the
  // virtual `file:///@solid-refresh` URL).
  plugins: [solid({ hot: false })],
  test: {
    globals: true,
    environment: "happy-dom",
    include: ["tests/unit/**/*.test.ts", "tests/unit/**/*.test.tsx"],
    setupFiles: ["tests/unit/setup.ts"],
    // Run vitest serially per file — our stores share createRoot scopes
    // and parallel tests would step on each other's signal state.
    fileParallelism: false,
  },
  resolve: {
    alias: {
      "@": "/src",
    },
  },
});
