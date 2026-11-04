/**
 * Shared E2E fixture — boots the app with a roost already open.
 *
 * The specs in this folder exercise panels (search, AI chat, agent
 * profile) that only render once a roost is open. In a clean Playwright
 * browser context localStorage is empty, so the app's onboarding gate
 * (`installOnboardingGate` in src/app/effects.ts) lands on "No Roost
 * open" and every `beforeEach` that clicks into a panel times out.
 *
 * We seed the persisted-roost breadcrumb before the app script runs.
 * On startup `restoreLastRoost()` (src/stores/roost.ts) reads
 * `corvux:lastVault`, and the browser-mock `open_roost` handler
 * (src/lib/invoke.ts) returns the seeded `/mock-roost` file tree — no
 * IndexedDB / File-System-Access handle required. The app boots straight
 * into the mock roost.
 *
 * Import `{ test, expect }` from this file instead of `@playwright/test`.
 */
import { test as base, expect } from "@playwright/test";

export const test = base.extend({
  page: async ({ page }, use) => {
    await page.addInitScript(() => {
      // Default mock roost path baked into the browser-mock dispatcher.
      localStorage.setItem("corvux:lastVault", "/mock-roost");
      // Skip the first-run onboarding gate.
      localStorage.setItem("corvux:onboarded", "true");
    });
    await use(page);
  },
});

export { expect };
