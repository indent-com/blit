import { test, expect, type Page } from "@playwright/test";

/**
 * The software keyboard is toggle-only, and the key line tracks the keyboard.
 *
 * Two properties, both directions each:
 *  - Tapping a terminal focuses it but must NOT raise an IME: the textarea
 *    carries inputmode="none" until the status-bar toggle is hit, which
 *    removes it (the browser owns the actual IME decision; the attribute is
 *    the whole contract we can pin from here).
 *  - The extra-keys line appears only while a keyboard actually occludes the
 *    viewport, and vanishes the moment it is reduced — not a settling period
 *    later.  The keyboard is emulated by shrinking the device metrics with
 *    the width held constant, which is exactly the signal the occlusion
 *    tracker reads off visualViewport.
 */
test.use({
  hasTouch: true,
  isMobile: true,
  viewport: { width: 480, height: 800 },
});

async function authenticate(page: Page) {
  await page.goto("/");
  await page.evaluate(() => localStorage.clear());
  await page.goto("/#psk=test-secret");
  await expect(
    page
      .getByRole("button", { name: "New terminal" })
      .first()
      .or(page.locator("canvas").first()),
  ).toBeVisible({ timeout: 10_000 });
  await page.keyboard.press("Escape");
  // Terminal creation goes over the mux; wait for it, not just the UI shell.
  await expect(page.getByRole("status")).toHaveAttribute(
    "aria-label",
    "connected",
    { timeout: 15_000 },
  );
  await page.waitForTimeout(500);
}

async function newTerminal(page: Page) {
  const before = await page.locator("canvas").count();
  const button = page.getByRole("button", { name: "New terminal" }).first();
  if (await button.isVisible()) {
    await button.click();
  } else {
    await page.keyboard.press("ControlOrMeta+Enter");
  }
  await expect
    .poll(() => page.locator("canvas").count(), { timeout: 10_000 })
    .toBeGreaterThan(before);
  await page.waitForTimeout(300);
}

test("keyboard rises only from the toggle and the key line tracks it", async ({
  page,
  context,
}) => {
  await authenticate(page);
  await newTerminal(page);

  const input = page.locator('textarea[aria-label="Terminal input"]').first();
  const keyLine = page.getByRole("button", { name: "Esc" });

  // A terminal on a touch device suppresses the IME from the start.
  await expect(input).toHaveAttribute("inputmode", "none");

  // Tapping the terminal takes focus — for hardware keys and scrollback —
  // but keeps the IME suppressed and raises no key line.
  await page.locator(".blit-scroll-surface").first().tap();
  await expect(input).toBeFocused();
  await expect(input).toHaveAttribute("inputmode", "none");
  await expect(keyLine).toHaveCount(0);

  // The status-bar toggle is the one thing that clears the suppression.
  await page.getByTitle("Show keyboard").tap();
  await expect(input).not.toHaveAttribute("inputmode", "none");
  // Intent alone does not show the key line; the keyboard has not risen.
  await expect(keyLine).toHaveCount(0);

  // The keyboard rising = the visual viewport shrinking under a constant
  // width.  The key line appears with it.
  const cdp = await context.newCDPSession(page);
  await cdp.send("Emulation.setDeviceMetricsOverride", {
    width: 480,
    height: 500,
    deviceScaleFactor: 1,
    mobile: true,
  });
  await expect(keyLine).toBeVisible();

  // Reducing the keyboard removes the key line immediately, expires the
  // toggle's intent, and re-arms the suppression.
  await cdp.send("Emulation.setDeviceMetricsOverride", {
    width: 480,
    height: 800,
    deviceScaleFactor: 1,
    mobile: true,
  });
  await expect(keyLine).toHaveCount(0);
  await expect(page.getByTitle("Show keyboard")).toBeVisible();
  await expect(input).toHaveAttribute("inputmode", "none");
});
