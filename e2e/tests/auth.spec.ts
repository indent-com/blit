import { test, expect } from "@playwright/test";

test.describe("Auth flow", () => {
  test.beforeEach(async ({ page }) => {
    await page.goto("/");
    await page.evaluate(() => localStorage.clear());
  });

  test("page loads and shows passphrase input", async ({ page }) => {
    await page.goto("/");
    const passInput = page.locator('input[type="password"]');
    await expect(passInput).toBeVisible();
  });

  test('wrong passphrase shows "Authentication failed" and input is re-usable', async ({
    page,
  }) => {
    await page.goto("/");
    const passInput = page.locator('input[type="password"]');
    await expect(passInput).toBeVisible();

    await passInput.fill("wrong-password");
    await passInput.press("Enter");

    const error = page.locator("output");
    await expect(error).toContainText("Authentication failed", {
      timeout: 10_000,
    });

    await expect(passInput).toBeVisible();
    await expect(passInput).not.toBeDisabled();
    await passInput.fill("another-attempt");
    await expect(passInput).toHaveValue("another-attempt");
  });

  test("correct passphrase hides auth form and shows workspace", async ({
    page,
  }) => {
    await page.goto("/");
    const passInput = page.locator('input[type="password"]');
    await expect(passInput).toBeVisible();

    await passInput.fill("test-secret");
    await passInput.press("Enter");

    await expect(passInput).toBeHidden({ timeout: 10_000 });
    const newTerminal = page
      .getByRole("button", { name: "New terminal" })
      .first();
    await expect(newTerminal).toBeVisible({ timeout: 10_000 });
  });

  test("passphrase in hash auto-connects on reload", async ({ page }) => {
    await page.goto("/");
    const passInput = page.locator('input[type="password"]');
    await expect(passInput).toBeVisible();

    await passInput.fill("test-secret");
    await passInput.press("Enter");

    const newTerminal = page
      .getByRole("button", { name: "New terminal" })
      .first();
    await expect(newTerminal).toBeVisible({ timeout: 10_000 });

    // The encrypted passphrase is written to the hash asynchronously
    // after the WebSocket handshake completes — wait for it.
    await page.waitForFunction(() => location.hash.length > 1, {
      timeout: 5_000,
    });
    const url = page.url();
    expect(url).toContain("#");

    await page.reload();
    await expect(newTerminal).toBeVisible({ timeout: 10_000 });
  });

  test("raw passphrase in hash is encrypted and connects", async ({ page }) => {
    await page.goto("/#test-secret");

    const passInput = page.locator('input[type="password"]');
    await expect(passInput).toBeHidden({ timeout: 10_000 });

    const newTerminal = page
      .getByRole("button", { name: "New terminal" })
      .first();
    await expect(newTerminal).toBeVisible({ timeout: 10_000 });

    const url = page.url();
    expect(url).not.toContain("#test-secret");
    expect(url).toContain("#e.");
  });

  test("encrypted hash survives reload", async ({ page }) => {
    await page.goto("/#test-secret");

    const newTerminal = page
      .getByRole("button", { name: "New terminal" })
      .first();
    await expect(newTerminal).toBeVisible({ timeout: 10_000 });

    const urlBefore = page.url();
    expect(urlBefore).toContain("#e.");

    await page.reload();
    await expect(newTerminal).toBeVisible({ timeout: 10_000 });

    const urlAfter = page.url();
    expect(urlAfter).toContain("#e.");
  });
});
