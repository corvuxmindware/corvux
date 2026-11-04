/**
 * E2E — search panel (FTS-only).
 *
 * Retrieval is full-text only since the embeddings / hybrid-semantic lane
 * was removed (2026-06-10), so there are no Hybrid/Lexical/Semantic mode
 * pills to assert — this spec verifies the live search-input behavior.
 * RRF / ranking correctness lives in Rust unit tests (commands/search.rs).
 *
 * Boots with a roost open via the shared fixture (see fixtures.ts).
 */
import { test, expect } from "./fixtures";

test.describe("search panel", () => {
  test.beforeEach(async ({ page }) => {
    await page.goto("/");
    await page.getByTitle("Search", { exact: true }).click();
    await expect(page.getByText("SEARCH", { exact: true })).toBeVisible();
  });

  test("opens with the search input ready", async ({ page }) => {
    await expect(page.getByPlaceholder("Search in Roost...")).toBeVisible();
  });

  test("typing a query holds the value", async ({ page }) => {
    const input = page.getByPlaceholder("Search in Roost...");
    await input.fill("test");
    await expect(input).toHaveValue("test");
  });

  test("clearing the input resets it", async ({ page }) => {
    const input = page.getByPlaceholder("Search in Roost...");
    await input.fill("dopamine");
    await expect(input).toHaveValue("dopamine");
    await input.fill("");
    await expect(input).toHaveValue("");
  });
});
