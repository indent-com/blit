import { test, expect } from "@playwright/test";

/**
 * Workspace roots is a Cmd+K entry, not a chord and not status bar chrome.
 *
 * The ⚙ beside the workspace-root selector in the left dock stays: it is
 * contextual to the control it sits next to, not a second global affordance.
 */
test("workspace roots lives in the Cmd+K menu", async ({ page }) => {
  await page.goto("/");
  await page.evaluate(() => localStorage.clear());
  await page.goto("/#psk=test-secret");
  await expect(
    page
      .getByRole("button", { name: "New terminal" })
      .first()
      .or(page.locator("canvas").first()),
  ).toBeVisible({ timeout: 10_000 });
  await page.waitForTimeout(500);

  // The status bar's ⌂ is gone, and nothing advertises the retired chord.
  await expect(page.getByRole("button", { name: "⌂" })).toHaveCount(0);
  await expect(page.locator('[title*="Ctrl+Shift+K"]')).toHaveCount(0);

  // The chord itself no longer opens anything.
  await page.keyboard.press("Control+Shift+K");
  await page.waitForTimeout(400);
  await expect(page.getByText("Workspace roots", { exact: true })).toHaveCount(
    0,
  );

  // It is an entry in the switcher, and it opens the roots overlay.
  await page.keyboard.press("ControlOrMeta+k");
  const entry = page.getByText("Workspace roots", { exact: true }).first();
  await expect(entry).toBeVisible({ timeout: 5_000 });
  await entry.click();
  await expect(
    page.getByText(/add a root|workspace roots/i).first(),
  ).toBeVisible({ timeout: 5_000 });
});
