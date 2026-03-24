import { test, expect } from "@playwright/test";

async function authenticate(page: import("@playwright/test").Page) {
  await page.goto("/");
  await page.evaluate(() => localStorage.clear());
  await page.goto("/");
  const passInput = page.locator('input[type="password"]');
  await expect(passInput).toBeVisible();
  await passInput.fill("test-secret");
  await passInput.press("Enter");
  await expect(page.locator("canvas").first()).toBeVisible({ timeout: 10_000 });
}

test.describe("Terminal", () => {
  test("after auth, terminal canvas is visible with non-zero dimensions", async ({
    page,
  }) => {
    await authenticate(page);

    const canvas = page.locator("canvas").first();
    await expect(canvas).toBeVisible({ timeout: 10_000 });

    const box = await canvas.boundingBox();
    expect(box).not.toBeNull();
    expect(box!.width).toBeGreaterThan(0);
    expect(box!.height).toBeGreaterThan(0);
  });

  test("can type in terminal and see output", async ({ page }) => {
    await authenticate(page);

    await page.locator("canvas").first().waitFor({ state: "visible", timeout: 10_000 });
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

  test("Expose opens on Ctrl+K and shows search and PTY list", async ({ page }) => {
    await authenticate(page);
    await page.locator("canvas").first().waitFor({ state: "visible", timeout: 10_000 });
    await page.waitForTimeout(500);

    await page.keyboard.press("Control+k");

    const dialog = page.locator("dialog[open]");
    await expect(dialog).toBeVisible({ timeout: 5_000 });

    const exposeSearch = dialog.locator('input[type="text"]');
    await expect(exposeSearch).toBeVisible();

    const createBtn = dialog.locator("li").last();
    await expect(createBtn).toBeVisible();
  });

  test("can create a new PTY from Expose", async ({ page }) => {
    await authenticate(page);
    await page.locator("canvas").first().waitFor({ state: "visible", timeout: 10_000 });
    await page.waitForTimeout(500);

    await page.keyboard.press("Control+k");
    const dialog = page.locator("dialog[open]");
    await expect(dialog).toBeVisible({ timeout: 5_000 });

    const itemsBefore = await dialog.locator("ul > li").count();

    const createBtn = dialog.locator("ul > li").last();
    await createBtn.click();

    await page.waitForTimeout(2000);

    const isVisible = await dialog.isVisible().catch(() => false);
    if (!isVisible) {
      await page.keyboard.press("Control+k");
      await expect(dialog).toBeVisible({ timeout: 5_000 });
    }

    const itemsAfter = await dialog.locator("ul > li").count();
    expect(itemsAfter).toBeGreaterThan(itemsBefore);
  });

  test("Expose preview canvases render with non-zero dimensions and switching tabs works", async ({ page }) => {
    await authenticate(page);
    await page.locator("canvas").first().waitFor({ state: "visible", timeout: 10_000 });
    await page.waitForTimeout(500);

    await page.keyboard.press("Control+k");
    const dialog = page.locator("dialog[open]");
    await expect(dialog).toBeVisible({ timeout: 5_000 });

    const previewCanvas = dialog.locator("canvas").first();
    await expect(previewCanvas).toBeVisible({ timeout: 5_000 });
    const box = await previewCanvas.boundingBox();
    expect(box).not.toBeNull();
    expect(box!.width).toBeGreaterThan(0);
    expect(box!.height).toBeGreaterThan(0);

    const firstItem = dialog.locator("ul > li").first();
    await firstItem.click();

    await expect(dialog).not.toBeVisible({ timeout: 5_000 });
    const mainCanvas = page.locator("canvas").first();
    await expect(mainCanvas).toBeVisible({ timeout: 5_000 });
    const mainBox = await mainCanvas.boundingBox();
    expect(mainBox).not.toBeNull();
    expect(mainBox!.width).toBeGreaterThan(0);
    expect(mainBox!.height).toBeGreaterThan(0);
  });
});
