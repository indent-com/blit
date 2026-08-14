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

/** Switch to two side-by-side BSP panes. */
async function twoPanes(page: Page) {
  await page.evaluate(() => {
    const h = window.location.hash.replace(/^#/, "");
    window.location.hash = `${h}${h ? "&" : ""}l=line(a,b)`;
  });
  const panes = page.locator("[data-blit-bsp-pane-id]");
  await expect(panes).toHaveCount(2, { timeout: 10_000 });
  await page.waitForTimeout(500);
  return panes;
}

/** Close every session, leaving the empty pane. Bounded so a stuck close
 *  fails the test rather than spinning. */
async function closeAllTerminals(page: Page) {
  for (let i = 0; i < 12; i++) {
    if ((await page.locator("canvas").count()) === 0) break;
    await page.locator("canvas").first().hover({ force: true });
    const close = page
      .getByRole("button", { name: "Close", exact: true })
      .first();
    if (!(await close.isVisible().catch(() => false))) break;
    await close.click();
    await page.waitForTimeout(400);
  }
  // With nothing left to show, blit offers the Remotes dialog; it is modal and
  // would swallow the clicks that follow.
  await page.keyboard.press("Escape");
  await page.waitForTimeout(300);
  await expect(
    page.getByRole("button", { name: "New terminal" }).first(),
  ).toBeVisible({ timeout: 10_000 });
}

/**
 * Park the main view's content by grip-dragging it to the dock.
 *
 * The events are dispatched by hand with one shared DataTransfer rather than
 * via `dragTo`: the grip lives inside a hover-gated `Show`, so the pointer
 * travelling to the dock un-hovers the main view and unmounts the very element
 * a mouse-emulated drag is holding. A real drag is immune (the browser owns it
 * once dragstart fires); Playwright's is not. The production handlers still
 * run — startPaneTileDrag writes the payload, the dock's onDrop reads it.
 *
 * `dragenter` is not decoration: with nothing parked yet the dock is not in
 * the DOM at all, and it is that window-level event which reveals it as a
 * drop-to-park target.
 */
async function parkViaGrip(page: Page) {
  const grip = await revealGrip(page);
  const dt = await page.evaluateHandle(() => new DataTransfer());
  await grip.dispatchEvent("dragstart", { dataTransfer: dt });
  await page.locator("body").dispatchEvent("dragenter", { dataTransfer: dt });
  const dock = page.locator("[data-blit-preview-panel]");
  await expect(dock).toBeVisible({ timeout: 5_000 });
  await dock.dispatchEvent("dragover", { dataTransfer: dt });
  await dock.dispatchEvent("drop", { dataTransfer: dt });
  await page.waitForTimeout(500);
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
  test("clicking relocates the toolbar and reverses it on the left", async ({
    page,
  }) => {
    await authenticate(page);
    await newTerminal(page);

    const grip = await revealGrip(page);
    const close = page
      .getByRole("button", { name: "Close", exact: true })
      .first();
    const before = await grip.boundingBox();
    const closeBefore = await close.boundingBox();
    expect(before).not.toBeNull();
    expect(closeBefore).not.toBeNull();
    expect(closeBefore!.x).toBeGreaterThan(before!.x);

    // Click (not drag) sends the toolbar to the next corner.
    await grip.click();
    const after = await grip.boundingBox();
    expect(after).not.toBeNull();
    expect(after!.x !== before!.x || after!.y !== before!.y).toBe(true);

    // The next click reaches bottom-left. The whole control reverses so the
    // close remains against the window edge and the grip faces the content.
    await grip.click();
    const leftGrip = await grip.boundingBox();
    const leftClose = await close.boundingBox();
    expect(leftGrip).not.toBeNull();
    expect(leftClose).not.toBeNull();
    expect(leftGrip!.x).toBeGreaterThan(leftClose!.x);
  });

  test("a pane quarter previews and relocates the toolbar", async ({
    page,
  }) => {
    await authenticate(page);
    await newTerminal(page);

    const grip = await revealGrip(page);
    const close = page
      .getByRole("button", { name: "Close", exact: true })
      .first();
    const before = (await grip.boundingBox())!;
    const pane = await grip.evaluate((el) => {
      const rect = (
        el.parentElement!.offsetParent as HTMLElement
      ).getBoundingClientRect();
      return {
        left: rect.left,
        right: rect.right,
        top: rect.top,
        bottom: rect.bottom,
      };
    });
    const target = {
      x: pane.left + (pane.right - pane.left) * 0.25,
      y: pane.top + (pane.bottom - pane.top) * 0.75,
    };
    const dt = await page.evaluateHandle(() => new DataTransfer());
    await grip.dispatchEvent("dragstart", { dataTransfer: dt });
    await page.locator("body").dispatchEvent("dragover", {
      dataTransfer: dt,
      clientX: target.x,
      clientY: target.y,
    });
    // Before release, the tools themselves preview their final corner and
    // the left-side order. This is not a separate quadrant overlay.
    const previewGrip = (await grip.boundingBox())!;
    const previewClose = (await close.boundingBox())!;
    expect(previewGrip.x).toBeLessThan(before.x);
    expect(previewGrip.y).toBeGreaterThan(before.y);
    expect(previewClose.x).toBeLessThan(previewGrip.x);

    await grip.dispatchEvent("dragend", {
      dataTransfer: dt,
      clientX: target.x,
      clientY: target.y,
    });

    const after = (await grip.boundingBox())!;
    const closeAfter = (await close.boundingBox())!;
    expect(after.x).toBe(previewGrip.x);
    expect(after.y).toBe(previewGrip.y);
    expect(closeAfter.x).toBeLessThan(after.x);
  });

  test("the chosen corner follows content to another pane", async ({
    page,
  }) => {
    await authenticate(page);
    await newTerminal(page);
    await newTerminal(page);
    const panes = await twoPanes(page);
    const source = panes.nth(0);
    const target = panes.nth(1);

    await source.hover({ force: true });
    const sourceGrip = source.getByRole("button", { name: GRIP });
    await expect(sourceGrip).toBeVisible();
    // top-right → bottom-right → bottom-left
    await sourceGrip.click();
    await sourceGrip.click();

    const dt = await page.evaluateHandle(() => new DataTransfer());
    await sourceGrip.dispatchEvent("dragstart", { dataTransfer: dt });
    await target.dispatchEvent("dragover", { dataTransfer: dt });
    await target.dispatchEvent("drop", { dataTransfer: dt });
    await page.waitForTimeout(300);

    await target.hover({ force: true });
    const movedGrip = target.getByRole("button", { name: GRIP });
    const movedClose = target.getByRole("button", {
      name: "Close",
      exact: true,
    });
    await expect(movedGrip).toBeVisible();
    const paneBox = (await target.boundingBox())!;
    const gripBox = (await movedGrip.boundingBox())!;
    const closeBox = (await movedClose.boundingBox())!;
    expect(gripBox.y).toBeGreaterThan(paneBox.y + paneBox.height / 2);
    expect(closeBox.x).toBeLessThan(gripBox.x);
  });

  test("dragging the grip to the dock parks the terminal", async ({ page }) => {
    await authenticate(page);
    await newTerminal(page);

    await parkViaGrip(page);

    // Parked: the main view shows the empty pane, and the session is now a
    // card in the dock.
    await expect(
      page.getByRole("button", { name: "New terminal" }).first(),
    ).toBeVisible({ timeout: 10_000 });
    await expect(
      page.locator('input[name^="blit-pane-cmd-"]').first(),
    ).toHaveAttribute("autocapitalize", "off");
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

  test("the background shortcut leaves the standalone view empty", async ({
    page,
  }) => {
    await authenticate(page);
    await newTerminal(page);

    await page.keyboard.press("Control+Shift+Q");

    await expect(
      page.getByRole("button", { name: "New terminal" }).first(),
    ).toBeVisible({ timeout: 10_000 });
    await expect(
      page.locator('[data-blit-preview-panel] [draggable="true"]').first(),
    ).toBeVisible();
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

test.describe("Parked terminal does not resurrect", () => {
  // Reported in review of #138: parking held the session id even after focus
  // moved on, so it only looked un-parked. The core always resolves *some*
  // focus, so closing the session that displaced a parked one handed focus
  // back — and it silently re-parked, with its dock card the only way out.
  test("closing the session that displaced a parked one shows it, not an empty pane", async ({
    page,
  }) => {
    await authenticate(page);
    // Sessions outlive a page and the server is shared across this file, so
    // close whatever earlier tests left running. Without that, `A` is not the
    // session the core's fallback lands on when `B` closes, and the repro
    // simply does not fire — the bug hides rather than the test failing.
    await closeAllTerminals(page);
    await newTerminal(page);

    // Park A.
    await parkViaGrip(page);
    await expect(
      page.getByRole("button", { name: "New terminal" }).first(),
    ).toBeVisible({ timeout: 10_000 });

    // Open B: A un-parks into the dock and B takes the view.
    await newTerminal(page);
    await expect(page.locator("canvas").first()).toBeVisible();

    // Close B. Focus falls back to A, which must be shown — not re-parked.
    await page.locator("canvas").first().hover({ force: true });
    await page
      .getByRole("button", { name: "Close", exact: true })
      .first()
      .click();
    await page.waitForTimeout(1000);
    await expect(page.locator("canvas").first()).toBeVisible({
      timeout: 10_000,
    });
    await expect(
      page.getByRole("button", { name: "New terminal" }),
    ).toHaveCount(0);
  });
});
