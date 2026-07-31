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
  await page.getByRole("button", { name: "New terminal" }).first().click();
  await expect
    .poll(() => page.locator("canvas").count(), { timeout: 10_000 })
    .toBeGreaterThan(before);
  await page.waitForTimeout(300);
}

/** The parked cards: draggable roots inside the preview panel. */
function parkedCards(page: Page) {
  return page.locator('[draggable="true"]');
}

test.describe("Parked pane drag", () => {
  test("a parked terminal is inert and drags into the main view", async ({
    page,
  }) => {
    await authenticate(page);
    await newTerminal(page);
    await newTerminal(page);

    // Exactly one session is parked (the unfocused one).
    const cards = parkedCards(page);
    await expect(cards).toHaveCount(1, { timeout: 10_000 });
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

    // Drag it onto the main view (single-pane mode: one destination).
    const mainCanvas = page.locator("canvas").first();
    await card.dragTo(mainCanvas);
    await page.waitForTimeout(500);

    // The dropped session took the main view, so the one it displaced is now
    // the parked card: still one card, but a different session.
    await expect(parkedCards(page)).toHaveCount(1);
    const nowParked = (await parkedCards(page).first().innerText()).trim();
    expect(nowParked).not.toBe(parkedLabel);
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

    const cards = parkedCards(page);
    await expect(cards).toHaveCount(1, { timeout: 10_000 });
    const parkedLabel = (await cards.first().innerText()).trim();

    // Drop onto the second pane specifically.
    const target = panes.nth(1);
    const targetId = await target.getAttribute("data-blit-bsp-pane-id");
    await cards.first().dragTo(target);
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
