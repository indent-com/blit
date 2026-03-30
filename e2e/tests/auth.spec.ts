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

  test("saved passphrase auto-connects on reload", async ({ page }) => {
    await page.goto("/");
    const passInput = page.locator('input[type="password"]');
    await expect(passInput).toBeVisible();

    await passInput.fill("test-secret");
    await passInput.press("Enter");

    const newTerminal = page
      .getByRole("button", { name: "New terminal" })
      .first();
    await expect(newTerminal).toBeVisible({ timeout: 10_000 });

    await page.reload();
    await expect(newTerminal).toBeVisible({ timeout: 10_000 });
  });

  test("invalid saved passphrase shows error and input is usable", async ({
    page,
  }) => {
    await page.goto("/");
    await page.evaluate(() => {
      localStorage.setItem("blit.passphrase", "bad-saved-pass");
    });

    await page.reload();

    const passInput = page.locator('input[type="password"]');
    await expect(passInput).toBeVisible({ timeout: 10_000 });
    await expect(passInput).not.toBeDisabled();

    await passInput.focus();
    await page.keyboard.type("test-secret");
    await expect(passInput).toHaveValue("test-secret");

    await passInput.press("Enter");
    await expect(passInput).toBeHidden({ timeout: 10_000 });
    const newTerminal = page
      .getByRole("button", { name: "New terminal" })
      .first();
    await expect(newTerminal).toBeVisible({ timeout: 10_000 });
  });
});
