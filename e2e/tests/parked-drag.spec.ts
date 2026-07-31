import { test, expect, type Page } from "@playwright/test";

/**
 * Parked cards in the right-side preview panel can be dragged into the live
 * view, and are inert while parked.
 *
 * Two terminals is the cheapest setup that produces a parked card: in non-BSP
 * mode every session except the focused one is off-screen, so creating a
 * second terminal parks the first.
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
  // Let hash encryption and connection setup settle.
  await page.waitForTimeout(500);
}

async function newTerminal(page: Page) {
  const before = await page.locator("canvas").count();
  // The button belongs to an *empty* pane, so it is gone as soon as the first
  // session fills the view. Every session after that comes from the keyboard
  // shortcut the help overlay documents (mod+Enter, `help.newTerminal`).
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

/** The parked cards: draggable roots inside the preview panel. Explorer rows
 *  and commits are draggable tile sources too, so the panel has to scope it. */
function parkedCards(page: Page) {
  return page.locator('[data-blit-preview-panel] [draggable="true"]');
}

test.describe("Parked pane drag", () => {
  test("a parked terminal is inert and drags into the main view", async ({
    page,
  }) => {
    await authenticate(page);
    await newTerminal(page);
    await newTerminal(page);

    // Sessions outlive a page, so what is parked here is "everything the main
    // view is not showing", not a number this test gets to fix.
    const cards = parkedCards(page);
    await expect(cards.first()).toBeVisible({ timeout: 10_000 });
    const parkedBefore = await cards.count();
    const card = cards.first();

    // Non-interactive while parked: the body wrapper is inert, which takes the
    // preview terminal's tabindex=0 input out of the tab order.
    const inertBody = card.locator("[inert]");
    await expect(inertBody).toHaveCount(1);
    expect(await inertBody.evaluate((el) => (el as HTMLElement).inert)).toBe(
      true,
    );
    // The parked terminal really is inside the inert subtree.
    await expect(inertBody.locator("canvas")).toHaveCount(1);

    const parkedLabel = (await card.innerText()).trim();
    expect(parkedLabel.length).toBeGreaterThan(0);

    // Drag it onto the main view (single-pane mode: one destination). The
    // terminal's own scroll surface covers the canvas, so the hit-target check
    // would refuse the drop it is aimed at; the events still land inside the
    // main view and bubble to its drop handler.
    const mainCanvas = page.locator("canvas").first();
    await card.dragTo(mainCanvas, { force: true });
    await page.waitForTimeout(500);

    // A swap: the dropped session took the main view and the one it displaced is
    // parked in its place, so the panel keeps its size and loses that label.
    await expect(parkedCards(page)).toHaveCount(parkedBefore);
    const nowParked = (await parkedCards(page).allInnerTexts()).map((t) =>
      t.trim(),
    );
    expect(nowParked).not.toContain(parkedLabel);
  });

  test("a parked card drags into a specific BSP pane", async ({ page }) => {
    await authenticate(page);
    await newTerminal(page);
    await newTerminal(page);
    await newTerminal(page);

    // Two panes via the hash's `l=` param (loadLayoutFromHash). Assigning
    // location.hash fires a genuine hashchange, which the app re-parses.
    await page.evaluate(() => {
      const h = window.location.hash.replace(/^#/, "");
      window.location.hash = `${h}${h ? "&" : ""}l=line(a,b)`;
    });
    await page.waitForTimeout(800);

    const panes = page.locator("[data-blit-bsp-pane-id]");
    await expect(panes).toHaveCount(2, { timeout: 10_000 });

    // Switching into a layout leaves both panes unassigned, so every session is
    // parked; how many there are is not this test's subject — that one of them
    // lands in the pane it was dropped on is.
    const cards = parkedCards(page);
    await expect(cards.first()).toBeVisible({ timeout: 10_000 });
    const parkedLabel = (await cards.first().innerText()).trim();

    // Drop onto the second pane specifically.
    const target = panes.nth(1);
    const targetId = await target.getAttribute("data-blit-bsp-pane-id");
    await cards.first().dragTo(target, { force: true });
    await page.waitForTimeout(500);

    // That pane now holds the dropped session, and the card is gone from the
    // panel (offScreenSessions is derived from pane assignments).
    await expect(
      page.locator(`[data-blit-bsp-pane-id="${targetId}"] canvas`).first(),
    ).toBeVisible();
    const remaining = await parkedCards(page).allInnerTexts();
    expect(remaining.map((t) => t.trim())).not.toContain(parkedLabel);
  });
});
