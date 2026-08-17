import { test, expect } from "@playwright/test";

/**
 * Workspace roots is opened by the ⚙ beside the workspace-root selector, and
 * by nothing else: not a chord, not status bar chrome, and not a Cmd+K entry.
 *
 * The ⚙ is contextual to the control it sits next to, which is why it is the
 * one that stays. The switcher deliberately carries only what has no other
 * home, so an entry there would be a second global affordance for a dialog the
 * left dock already opens in one click.
 */
test("workspace roots opens from the dock, and from nowhere else", async ({
  page,
}) => {
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

  // Neither does the switcher, which lists only what the chrome cannot reach.
  await page.keyboard.press("ControlOrMeta+k");
  await page.waitForTimeout(400);
  await expect(page.getByText("Workspace roots", { exact: true })).toHaveCount(
    0,
  );
  await page.keyboard.press("Escape");

  // The ⚙ next to the workspace-root selector is the affordance that remains.
  // Located by title: its accessible name is the glyph, which names nothing.
  await page.getByTitle("Manage workspace roots").click();
  await expect(
    page.getByText(/add a root|workspace roots/i).first(),
  ).toBeVisible({ timeout: 5_000 });
});
