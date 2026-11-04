/**
 * Release smoke coverage for the app shell and its lazy-loaded surfaces.
 * Backend correctness remains covered by Rust tests; these checks catch
 * broken UI wiring and missing production chunks in browser-mock mode.
 */
import { test, expect } from "./fixtures";

test.describe("release smoke", () => {
  test.beforeEach(async ({ page }) => {
    await page.goto("/");
  });

  test("settings navigation and shortcut overlay load on demand", async ({ page }) => {
    await page.getByTitle("Settings", { exact: true }).click();
    const settings = page.getByRole("dialog", { name: "Settings" });
    await expect(settings).toBeVisible();

    await settings.getByText("AI Models", { exact: true }).click();
    await expect(settings.locator("h2").getByText("AI Models", { exact: true })).toBeVisible();
    await settings.getByLabel("Back to settings index").click();

    await settings.getByText("Keyboard shortcuts", { exact: true }).click();
    await expect(page.getByText("Keyboard Shortcuts", { exact: true })).toBeVisible();
    await page.keyboard.press("Escape");
    await expect(settings).toBeVisible();

    await settings.getByLabel("Close settings").click();
    await expect(settings).toBeHidden();
  });

  test("quick switcher finds and opens a note", async ({ page }) => {
    await page.getByRole("button", { name: /Go to file/ }).click();
    const switcher = page.getByRole("dialog", { name: "Quick switcher" });
    await expect(switcher).toBeVisible();

    const input = switcher.getByPlaceholder("Type to search notes...");
    await input.fill("Project Ideas");
    await expect(switcher.getByText("Project Ideas", { exact: true })).toBeVisible();
    await input.press("Enter");

    await expect(switcher).toBeHidden();
    await expect(page.getByText("Project Ideas", { exact: true }).first()).toBeVisible();
  });

  test("command palette opens, filters, and closes", async ({ page }) => {
    await page.getByRole("button", { name: /Command palette/ }).click();
    const palette = page.getByRole("dialog", { name: "Command palette" });
    await expect(palette).toBeVisible();

    await palette
      .getByPlaceholder("Type a command or search files... (> for commands only)")
      .fill(">graph");
    await expect(palette.getByText("Toggle Graph View", { exact: true })).toBeVisible();
    await page.keyboard.press("Escape");
    await expect(palette).toBeHidden();
  });

  test("graph view loads mock data and its controls", async ({ page }) => {
    await page.getByTitle("Graph View").click();
    const closeGraph = page.getByRole("button", { name: "Close graph", exact: true });
    await expect(closeGraph).toBeVisible();
    await expect(page.getByLabel("Search graph")).toBeVisible();

    await page.getByLabel("Graph settings").click();
    await expect(page.getByText("Graph settings", { exact: true })).toBeVisible();
    await closeGraph.click();
    await expect(closeGraph).toBeHidden();
  });
});

test.describe("mobile release smoke", () => {
  test.use({
    userAgent:
      "Mozilla/5.0 (Linux; Android 13; Pixel 7) AppleWebKit/537.36 " +
      "(KHTML, like Gecko) Chrome/120.0 Mobile Safari/537.36",
    viewport: { width: 390, height: 844 },
  });

  test("overflow menu lazy-loads on mobile", async ({ page }) => {
    await page.goto("/");
    await page.getByLabel("More actions").click();
    await expect(page.getByText("Settings", { exact: true })).toBeVisible();
    await page.getByRole("button", { name: "Run command…" }).click();
    await expect(page.getByRole("dialog", { name: "Command palette" })).toBeVisible();
  });
});
