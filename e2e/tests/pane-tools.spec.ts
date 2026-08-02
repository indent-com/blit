import { test, expect, type Page } from "@playwright/test";

/**
 * PaneTools everywhere: every pane kind in every view gets the corner
 * multitool — grip (drag content out, click to relocate the toolbar) plus ✕ —
 * with no pinned, immovable close button anywhere. The non-BSP main view used
 * to make two exceptions: a bare terminal got a close-only toolbar (no grip,
 * so no way to move it off whatever it covered), and tiles kept a legacy
 * pinned "Close tab" ✕ on top of the multitool.
 */

const GRIP = "Drag to move · click for another corner";

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

/** Hover the main view so the toolbar reveals itself, then return its grip.
 *  `force`: the terminal's own scroll surface covers the canvas, so the
 *  hit-target check would refuse a hover that does land in the main view (the
 *  same reason parked-drag.spec forces its drops). */
async function revealGrip(page: Page) {
  await page.locator("canvas").first().hover({ force: true });
  const grip = page.getByRole("button", { name: GRIP }).first();
  await expect(grip).toBeVisible();
  return grip;
}

test.describe("Pane multitool on a main-view terminal", () => {
  test("a bare terminal has the grip, and clicking it relocates the toolbar", async ({
    page,
  }) => {
    await authenticate(page);
    await newTerminal(page);

    const grip = await revealGrip(page);
    const before = await grip.boundingBox();
    expect(before).not.toBeNull();

    // Click (not drag) sends the toolbar to the next corner.
    await grip.click();
    const after = await grip.boundingBox();
    expect(after).not.toBeNull();
    expect(after!.x !== before!.x || after!.y !== before!.y).toBe(true);
  });

  test("dragging the grip to the dock parks the terminal", async ({ page }) => {
    await authenticate(page);
    await newTerminal(page);

    const grip = await revealGrip(page);
    const dock = page.locator("[data-blit-preview-panel]");

    // One DataTransfer carried across the events by hand, rather than
    // `dragTo`: the grip lives inside a hover-gated `Show`, so the pointer
    // travelling to the dock un-hovers the main view and unmounts the very
    // element a mouse-emulated drag is holding. A real drag is immune (the
    // browser owns it once dragstart fires); Playwright's is not. This still
    // runs the production handlers — startPaneTileDrag writes the payload,
    // the dock's onDrop reads it.
    const dt = await page.evaluateHandle(() => new DataTransfer());
    await grip.dispatchEvent("dragstart", { dataTransfer: dt });
    await dock.dispatchEvent("dragover", { dataTransfer: dt });
    await dock.dispatchEvent("drop", { dataTransfer: dt });
    await page.waitForTimeout(500);

    // Parked: the main view shows the empty pane, and the session is now a
    // card in the dock.
    await expect(
      page.getByRole("button", { name: "New terminal" }).first(),
    ).toBeVisible({ timeout: 10_000 });
    await expect(
      page.locator('[data-blit-preview-panel] [draggable="true"]').first(),
    ).toBeVisible();

    // And it comes back: clicking its card un-parks it.
    await page
      .locator('[data-blit-preview-panel] [draggable="true"]')
      .first()
      .click();
    await expect(page.locator("canvas").first()).toBeVisible({
      timeout: 10_000,
    });
  });

  test("no pinned close button anywhere: every ✕ rides the multitool", async ({
    page,
  }) => {
    await authenticate(page);
    await newTerminal(page);

    // The legacy main-view "Close tab" ✕ is gone for good.
    await expect(page.locator('button[title="Close tab"]')).toHaveCount(0);

    // The multitool's ✕ closes the terminal.
    await page.locator("canvas").first().hover({ force: true });
    const close = page
      .getByRole("button", { name: "Close", exact: true })
      .first();
    await expect(close).toBeVisible();
    const canvasesBefore = await page.locator("canvas").count();
    await close.click();
    await expect
      .poll(() => page.locator("canvas").count(), { timeout: 10_000 })
      .toBeLessThan(canvasesBefore);
  });
});
