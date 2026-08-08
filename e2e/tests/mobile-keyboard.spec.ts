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
  // A hash-only navigation stores the psk but does not reliably (re)connect
  // the config WS — only the first context in a fresh browser gets away
  // without a real load.  Reload so the boot path picks the stored psk up.
  await page.reload();
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

/**
 * Surface panes must not override the icon: a canvas is not editable, so an
 * IME dismisses over it — which used to expire the toggle's intent.  Focus
 * landing on a surface canvas has to reach the surface's hidden IME textarea
 * (which routes keys into the surface) instead, and that textarea carries the
 * same inputmode="none" suppression while the keyboard is not wanted.
 *
 * BlitSurfaceCanvas now performs that handoff itself, on every platform, so
 * that a composition can start at all.  What this test covers is the
 * Workspace-level redirect behind it: a capture-phase net for any canvas in a
 * pane, which is what a synthetic one exercises.  A real surface needs a
 * Wayland client this stack does not run, so the test plants the DOM shape
 * BlitSurfaceCanvas.attach() produces — a tabindex=0 canvas with a labeled
 * textarea beside it — and holds the Workspace policy to it on its own.
 */
test("the icon's keyboard survives focus landing on a surface canvas", async ({
  page,
  context,
}) => {
  await authenticate(page);
  await newTerminal(page);

  await page.evaluate(() => {
    const section = document.querySelector("section");
    if (!section) throw new Error("no pane section");
    const holder = document.createElement("div");
    const ta = document.createElement("textarea");
    ta.setAttribute("aria-label", "Surface input");
    ta.tabIndex = -1;
    const canvas = document.createElement("canvas");
    canvas.tabIndex = 0;
    canvas.dataset.testid = "fake-surface-canvas";
    canvas.style.width = "60px";
    canvas.style.height = "60px";
    holder.append(ta, canvas);
    section.append(holder);
  });
  const surfaceInput = page.locator('textarea[aria-label="Surface input"]');
  const surfaceCanvas = page.getByTestId("fake-surface-canvas");

  // While the keyboard is not wanted, the surface textarea is suppressed
  // exactly like a terminal's (the MutationObserver stamps it on mount),
  // and tapping the surface parks focus on the canvas — hardware keys and
  // pointer input want it there, and nothing must shove an IME up.
  await expect(surfaceInput).toHaveAttribute("inputmode", "none");
  await surfaceCanvas.tap();
  await expect(surfaceCanvas).toBeFocused();

  // Raise the keyboard from the icon and emulate it occluding the viewport.
  await page.getByTitle("Show keyboard").tap();
  await expect(surfaceInput).not.toHaveAttribute("inputmode", "none");
  const cdp = await context.newCDPSession(page);
  await cdp.send("Emulation.setDeviceMetricsOverride", {
    width: 480,
    height: 500,
    deviceScaleFactor: 1,
    mobile: true,
  });
  await expect(page.getByRole("button", { name: "Esc" })).toBeVisible();

  // Tapping the surface focuses its canvas; the redirect must park focus on
  // the IME textarea instead, and the icon's intent must survive.
  await surfaceCanvas.tap();
  await expect(surfaceInput).toBeFocused();
  await expect(page.getByTitle("Hide keyboard")).toBeVisible();
});
