/**
 * E2E — multi-session chat panel.
 *
 * Drives the chat sessions UI in browser-mock mode. Verifies the
 * session strip, the expanded list, the inline search, pinning,
 * and rename + delete flows.
 */

import { type Page } from "@playwright/test";
import { test, expect } from "./fixtures";

async function openChat(page: Page) {
  await page.goto("/");
  // The chat panel is collapsed by default — open it from the icon rail.
  // The button is titled "Corvux Agent" (the old "AI/Chat" label is gone).
  await page.getByTitle("Corvux Agent").click();
  // Panel is open once its session controls render.
  await expect(page.getByTitle("New chat")).toBeVisible();
}

async function useCurrentSession(page: Page, message: string) {
  const composer = page.getByPlaceholder("Ask about your notes... (Enter to send)");
  await expect(composer).toBeEnabled();
  await composer.fill(message);
  await composer.press("Enter");
  await expect(page.getByTitle("Stop generating")).toBeHidden();
}

async function expandSessions(page: Page) {
  // Click the sessions strip — toggles the expanded list.
  await page.getByTitle("Switch session").click();
}

test.describe("chat sessions", () => {
  test.beforeEach(async ({ page }) => {
    // Use the local Ollama path so the composer is enabled without a real
    // cloud credential. The browser mock returns a deterministic response.
    await page.addInitScript(() => {
      localStorage.setItem(
        "corvux-settings",
        JSON.stringify({ llm_provider: "ollama", ollama_model: "playwright" }),
      );
    });
    await openChat(page);
  });

  test("sessions strip shows current chat title + count", async ({ page }) => {
    await expect(page.getByText(/\d+ chat/)).toBeVisible();
  });

  test("clicking + creates a new session and makes it active", async ({ page }) => {
    await useCurrentSession(page, "First session");
    const newButton = page.getByTitle("New chat");
    await newButton.click();
    // New session title defaults to "New chat" and the strip should reflect it.
    await expect(page.getByText("New chat").first()).toBeVisible();
    await expect(page.getByText("2 chats")).toBeVisible();
  });

  test("expanded sessions list shows time-bucketed groups", async ({ page }) => {
    // Ensure at least 2 sessions exist so groups render.
    await useCurrentSession(page, "First session");
    await page.getByTitle("New chat").click();
    await expandSessions(page);
    // Search box visible at top of the panel.
    await expect(page.getByPlaceholder("Search chats…")).toBeVisible();
    // At least one group header is visible.
    await expect(page.getByText(/TODAY|YESTERDAY|PREVIOUS/i).first()).toBeVisible();
  });

  test("session search filters the list by title", async ({ page }) => {
    await expandSessions(page);
    const search = page.getByPlaceholder("Search chats…");
    await search.fill("zzz-no-match");
    await expect(page.getByText(/No chats match/i)).toBeVisible();
    await search.fill("");
  });

  test("pin button toggles pinned state and surfaces a Pinned group", async ({ page }) => {
    await expandSessions(page);
    // Hover the first session row to reveal the action buttons.
    // Both session rows and Sidebar folder rows use `.group.select-none`.
    // Anchor on the always-rendered swipe-layer "Delete chat" button — the
    // hover Rename/Delete buttons vanish once a row enters edit/confirm
    // state, so they can't be a stable filter; the swipe button can't.
    const firstRow = page
      .locator(".group.select-none")
      .filter({ has: page.getByRole("button", { name: "Delete chat" }) })
      .first();
    await firstRow.hover();
    const pinButton = firstRow.getByTitle("Pin").first();
    await expect(pinButton).toBeVisible();
    await pinButton.click();
    await expect(page.getByText(/PINNED/i)).toBeVisible();
  });

  test("rename inline-edits the session title", async ({ page }) => {
    await expandSessions(page);
    // Both session rows and Sidebar folder rows use `.group.select-none`.
    // Anchor on the always-rendered swipe-layer "Delete chat" button — the
    // hover Rename/Delete buttons vanish once a row enters edit/confirm
    // state, so they can't be a stable filter; the swipe button can't.
    const firstRow = page
      .locator(".group.select-none")
      .filter({ has: page.getByRole("button", { name: "Delete chat" }) })
      .first();
    await firstRow.hover();
    await firstRow.getByTitle("Rename").click();
    const input = firstRow.getByRole("textbox");
    await input.fill("Renamed by playwright");
    await input.press("Enter");
    await expect(firstRow.getByText("Renamed by playwright")).toBeVisible();
  });

  test("delete shows inline confirm and removes the session on Delete", async ({ page }) => {
    await expandSessions(page);
    // Both session rows and Sidebar folder rows use `.group.select-none`.
    // Anchor on the always-rendered swipe-layer "Delete chat" button — the
    // hover Rename/Delete buttons vanish once a row enters edit/confirm
    // state, so they can't be a stable filter; the swipe button can't.
    const firstRow = page
      .locator(".group.select-none")
      .filter({ has: page.getByRole("button", { name: "Delete chat" }) })
      .first();
    await firstRow.hover();
    await firstRow.getByTitle("Delete").click();
    // Inline confirm pills appear. Use role+name: the always-rendered
    // swipe-layer button also has the text "Delete" but its accessible
    // name is "Delete chat" (aria-label), so a button named exactly
    // "Delete" uniquely targets the confirm pill.
    const confirmDelete = firstRow.getByRole("button", { name: "Delete", exact: true });
    await expect(confirmDelete).toBeVisible();
    await expect(firstRow.getByRole("button", { name: "Cancel" })).toBeVisible();
    await confirmDelete.click();
    // Deleting the only active session immediately creates a fresh blank one.
    await expect(page.getByText("1 chat")).toBeVisible();
    await expect(page.getByText("New chat").first()).toBeVisible();
  });
});
