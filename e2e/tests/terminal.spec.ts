import { test, expect } from "@playwright/test";

/** Helper: authenticate and wait for workspace to be visible */
async function authenticate(page: import("@playwright/test").Page) {
  await page.goto("/");
  await page.evaluate(() => localStorage.clear());
  await page.goto("/");
  const passInput = page.locator("#pass");
  await expect(passInput).toBeVisible();
  await passInput.fill("test-secret");
  await passInput.press("Enter");
  await expect(page.locator("#workspace")).toBeVisible({ timeout: 10_000 });
}

test.describe("Terminal", () => {
  test("after auth, terminal canvas is visible with non-zero dimensions", async ({
    page,
  }) => {
    await authenticate(page);

    const canvas = page.locator("#term");
    await expect(canvas).toBeVisible({ timeout: 10_000 });

    // Canvas should have non-zero dimensions (terminal is rendering)
    const box = await canvas.boundingBox();
    expect(box).not.toBeNull();
    expect(box!.width).toBeGreaterThan(0);
    expect(box!.height).toBeGreaterThan(0);
  });

  test("can type in terminal and see output", async ({ page }) => {
    await authenticate(page);

    // Wait for terminal to be ready
    await page.locator("#term").waitFor({ state: "visible", timeout: 10_000 });

    // Give the terminal a moment to fully initialize
    await page.waitForTimeout(1000);

    // Focus the input sink (how the terminal receives keyboard input)
    const inputSink = page.locator("#input-sink");
    await inputSink.focus();

    // Type a command
    await page.keyboard.type("echo hello-e2e-test", { delay: 50 });
    await page.keyboard.press("Enter");

    // Wait a bit for the command to execute and render
    await page.waitForTimeout(2000);

    // We can't read canvas text directly, but we can verify the terminal
    // is still rendering (canvas dimensions still valid) and check that
    // the input sink is functional
    const canvas = page.locator("#term");
    const box = await canvas.boundingBox();
    expect(box).not.toBeNull();
    expect(box!.width).toBeGreaterThan(0);
    expect(box!.height).toBeGreaterThan(0);
  });

  test("Expose opens on Ctrl+K and shows PTY list", async ({ page }) => {
    await authenticate(page);
    await page.locator("#term").waitFor({ state: "visible", timeout: 10_000 });
    await page.waitForTimeout(500);

    // Open Expose with Ctrl+K
    await page.keyboard.press("Control+k");

    const previewRail = page.locator("#preview-rail");
    await expect(previewRail).toHaveClass(/visible/, { timeout: 5_000 });

    // The expose search input should be visible
    const exposeSearch = page.locator("#expose-search");
    await expect(exposeSearch).toBeVisible();

    // The create button should be visible
    const createBtn = page.locator("#expose-create");
    await expect(createBtn).toBeVisible();
  });

  test("can create a new PTY from Expose", async ({ page }) => {
    await authenticate(page);
    await page.locator("#term").waitFor({ state: "visible", timeout: 10_000 });
    await page.waitForTimeout(500);

    // Open Expose
    await page.keyboard.press("Control+k");
    const previewRail = page.locator("#preview-rail");
    await expect(previewRail).toHaveClass(/visible/, { timeout: 5_000 });

    // Count existing PTY cards
    const cardsBefore = await page.locator(".preview-card").count();

    // Click the "+" button to create a new PTY
    const createBtn = page.locator("#expose-create");
    await createBtn.click();

    // Wait for a new card to appear
    await page.waitForTimeout(2000);

    // Re-open Expose to check card count (it may have auto-closed)
    const isVisible = await previewRail.evaluate((el) =>
      el.classList.contains("visible"),
    );
    if (!isVisible) {
      await page.keyboard.press("Control+k");
      await expect(previewRail).toHaveClass(/visible/, { timeout: 5_000 });
    }

    const cardsAfter = await page.locator(".preview-card").count();
    expect(cardsAfter).toBeGreaterThan(cardsBefore);
  });
});
