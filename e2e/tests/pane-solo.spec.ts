import { test, expect, type Page } from "@playwright/test";

/**
 * Solo: one BSP pane fills the workspace, its siblings hidden rather than
 * unmounted. Reachable from the multitool's ▣ segment and from Ctrl+Shift+K
 * (the chord workspace roots gave up).
 *
 * Hidden, not unmounted, is the property worth pinning: pane ids are
 * positional paths, so rewriting the tree would renumber them and dispose
 * every sibling's terminal surface. The canvases must survive a solo.
 */

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
  // With no sessions yet (a previous test may have closed them all), blit
  // offers the Remotes dialog; it is modal and would swallow later clicks.
  await page.keyboard.press("Escape");
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

/** A two-pane layout, via the hash's `l=` param. */
async function twoPanes(page: Page) {
  await page.evaluate(() => {
    const h = window.location.hash.replace(/^#/, "");
    window.location.hash = `${h}${h ? "&" : ""}l=line(a,b)`;
  });
  await page.waitForTimeout(800);
  const panes = page.locator("[data-blit-bsp-pane-id]");
  await expect(panes).toHaveCount(2, { timeout: 10_000 });
  return panes;
}

/** Panes whose box actually has area — a soloed sibling is display:none. */
async function visiblePaneCount(page: Page) {
  const boxes = await page
    .locator("[data-blit-bsp-pane-id]")
    .evaluateAll((els) =>
      els.map((el) => (el as HTMLElement).getBoundingClientRect().width),
    );
  return boxes.filter((w) => w > 0).length;
}

test.describe("Pane solo", () => {
  test("the multitool's solo segment fills the workspace and restores", async ({
    page,
  }) => {
    await authenticate(page);
    await newTerminal(page);
    await newTerminal(page);
    const panes = await twoPanes(page);

    const first = panes.nth(0);
    const widthBefore = (await first.boundingBox())!.width;
    expect(await visiblePaneCount(page)).toBe(2);
    const canvasesBefore = await page.locator("canvas").count();

    await first.hover({ force: true });
    const solo = page.getByRole("button", { name: /Solo this pane/ }).first();
    await expect(solo).toBeVisible();
    await solo.click();
    await page.waitForTimeout(500);

    // One pane on screen, and it grew.
    expect(await visiblePaneCount(page)).toBe(1);
    expect((await first.boundingBox())!.width).toBeGreaterThan(widthBefore);
    // Nothing was torn down: the hidden pane's canvas is still mounted.
    expect(await page.locator("canvas").count()).toBe(canvasesBefore);

    // The segment now offers the way back.
    await first.hover({ force: true });
    const unsolo = page.getByRole("button", { name: /Show all panes/ }).first();
    await expect(unsolo).toBeVisible();
    await unsolo.click();
    await page.waitForTimeout(500);

    expect(await visiblePaneCount(page)).toBe(2);
    expect((await first.boundingBox())!.width).toBeCloseTo(widthBefore, 0);
  });

  test("Ctrl+Shift+K toggles solo on the focused pane", async ({ page }) => {
    await authenticate(page);
    await newTerminal(page);
    await newTerminal(page);
    await twoPanes(page);

    expect(await visiblePaneCount(page)).toBe(2);
    await page.keyboard.press("Control+Shift+K");
    await page.waitForTimeout(500);
    expect(await visiblePaneCount(page)).toBe(1);
    await page.keyboard.press("Control+Shift+K");
    await page.waitForTimeout(500);
    expect(await visiblePaneCount(page)).toBe(2);
  });

  test("a single-pane layout offers no solo", async ({ page }) => {
    await authenticate(page);
    await newTerminal(page);

    // The non-BSP main view has nothing to solo against.
    await page.locator("canvas").first().hover({ force: true });
    await expect(
      page.getByRole("button", { name: /Solo this pane/ }),
    ).toHaveCount(0);
  });
});
