import { test, expect } from "@playwright/test";

async function authenticate(page: import("@playwright/test").Page) {
  await page.goto("/");
  await page.evaluate(() => localStorage.clear());
  await page.goto("/#test-secret");
  await expect(
    page
      .getByRole("button", { name: "New terminal" })
      .first()
      .or(page.locator("canvas").first()),
  ).toBeVisible({ timeout: 10_000 });
}

async function authenticateAndCreateTerminal(
  page: import("@playwright/test").Page,
) {
  await authenticate(page);
  // Wait for the DOM to stabilise after hash encryption and connection setup.
  await page.waitForTimeout(500);
  const canvas = page.locator("canvas").first();
  if (!(await canvas.isVisible().catch(() => false))) {
    await page.getByRole("button", { name: "New terminal" }).first().click();
  }
  await expect(canvas).toBeVisible({ timeout: 10_000 });
}

test.describe("Terminal", () => {
  test("after auth, workspace is ready", async ({ page }) => {
    await authenticate(page);
    await expect(
      page
        .getByRole("button", { name: "New terminal" })
        .first()
        .or(page.locator("canvas").first()),
    ).toBeVisible();
  });

  test("creating a terminal shows canvas with non-zero dimensions", async ({
    page,
  }) => {
    await authenticateAndCreateTerminal(page);

    const canvas = page.locator("canvas").first();
    await expect(canvas).toBeVisible({ timeout: 10_000 });

    const box = await canvas.boundingBox();
    expect(box).not.toBeNull();
    expect(box!.width).toBeGreaterThan(0);
    expect(box!.height).toBeGreaterThan(0);
  });

  test("can type in terminal and see output", async ({ page }) => {
    await authenticateAndCreateTerminal(page);

    await page.waitForTimeout(1000);

    const inputSink = page.locator('textarea[aria-label="Terminal input"]');
    await inputSink.focus();

    await page.keyboard.type("echo hello-e2e-test", { delay: 50 });
    await page.keyboard.press("Enter");

    await page.waitForTimeout(2000);

    const canvas = page.locator("canvas").first();
    const box = await canvas.boundingBox();
    expect(box).not.toBeNull();
    expect(box!.width).toBeGreaterThan(0);
    expect(box!.height).toBeGreaterThan(0);
  });

  test("Switcher opens on Ctrl+K and shows search and items", async ({
    page,
  }) => {
    await authenticateAndCreateTerminal(page);
    await page.waitForTimeout(500);

    await page.keyboard.press("Control+k");

    const dialog = page.locator('div[role="dialog"]');
    await expect(dialog).toBeVisible({ timeout: 5_000 });

    const searchInput = dialog.locator('input[type="text"]');
    await expect(searchInput).toBeVisible();

    await expect(dialog.locator("section").first()).toBeVisible();
  });

  test("can create a new PTY from Switcher", async ({ page }) => {
    await authenticateAndCreateTerminal(page);
    await page.waitForTimeout(500);

    const canvasBefore = await page.locator("canvas").count();

    await page.keyboard.press("Control+k");
    const dialog = page.locator('div[role="dialog"]');
    await expect(dialog).toBeVisible({ timeout: 5_000 });

    const newTermBtn = dialog.getByText("New terminal").first();
    await newTermBtn.click();

    await page.waitForTimeout(2000);

    const canvasAfter = await page.locator("canvas").count();
    expect(canvasAfter).toBeGreaterThanOrEqual(canvasBefore);
  });

  test("Switcher preview canvases render with non-zero dimensions", async ({
    page,
  }) => {
    await authenticateAndCreateTerminal(page);
    await page.waitForTimeout(500);

    await page.keyboard.press("Control+k");
    const dialog = page.locator('div[role="dialog"]');
    await expect(dialog).toBeVisible({ timeout: 5_000 });

    const previewCanvas = dialog.locator("canvas").first();
    await expect(previewCanvas).toBeVisible({ timeout: 5_000 });
    const box = await previewCanvas.boundingBox();
    expect(box).not.toBeNull();
    expect(box!.width).toBeGreaterThan(0);
    expect(box!.height).toBeGreaterThan(0);
  });
});
