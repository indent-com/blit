import { test, expect, type Page } from "@playwright/test";

/**
 * Every draggable, not just the pane grip, must work with a finger.
 *
 * HTML5 drag-and-drop never fires from touch, so explorer rows, changed
 * files, search hits, problems, commits and dock cards were all mouse-only.
 * They share the grip's bridge but start differently: a list row cannot carry
 * `touch-action: none` without losing its scrolling, so a hold — not a
 * movement — is what begins the drag.
 *
 * The two properties worth pinning are therefore both directions: a hold
 * drags, and a swipe still scrolls.
 */
test.use({
  hasTouch: true,
  isMobile: true,
  viewport: { width: 900, height: 700 },
});

/** Matches `LONG_PRESS_MS` in tileDrag.ts, with room for scheduling. */
const HOLD_MS = 700;

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

async function touch(
  page: Page,
  type: "pointerdown" | "pointermove" | "pointerup",
  at: { x: number; y: number },
) {
  await page.evaluate(
    ([type, at]) => {
      const ev = new PointerEvent(type as string, {
        pointerId: 1,
        pointerType: "touch",
        isPrimary: true,
        clientX: (at as { x: number }).x,
        clientY: (at as { y: number }).y,
        bubbles: true,
        cancelable: true,
      });
      if (type === "pointerdown") {
        const el = document.elementFromPoint(
          (at as { x: number }).x,
          (at as { y: number }).y,
        );
        if (!el) throw new Error("nothing under the finger");
        el.dispatchEvent(ev);
      } else {
        window.dispatchEvent(ev);
      }
    },
    [type, at] as const,
  );
}

async function centerOf(page: Page, selector: string) {
  const box = await page.locator(selector).first().boundingBox();
  if (!box) throw new Error(`no box for ${selector}`);
  return { x: box.x + box.width / 2, y: box.y + box.height / 2 };
}

/** Two terminals, so one is parked and the dock has a card to drag. */
async function parkedCard(page: Page) {
  await newTerminal(page);
  await newTerminal(page);
  const card = page
    .locator('[data-blit-preview-panel] [draggable="true"]')
    .first();
  await expect(card).toBeVisible({ timeout: 10_000 });
  return card;
}

test.describe("Touch drag on list rows", () => {
  test("holding a dock card drags it into the main view", async ({ page }) => {
    await authenticate(page);
    const card = await parkedCard(page);
    const label = (await card.innerText()).trim();
    expect(label.length).toBeGreaterThan(0);

    const cardAt = await centerOf(
      page,
      '[data-blit-preview-panel] [draggable="true"]',
    );
    const mainAt = { x: 400, y: 350 };

    await touch(page, "pointerdown", cardAt);
    // Hold still: movement here would be read as a scroll or a swipe.
    await page.waitForTimeout(HOLD_MS);
    await touch(page, "pointermove", { x: cardAt.x - 60, y: cardAt.y });
    await touch(page, "pointermove", mainAt);
    await touch(page, "pointerup", mainAt);
    await page.waitForTimeout(700);

    // It moved into the main view, so the dock no longer lists that label.
    const parked = await page
      .locator('[data-blit-preview-panel] [draggable="true"]')
      .allInnerTexts();
    expect(parked.map((t) => t.trim())).not.toContain(label);
  });

  test("a swipe across a dock card is not a drag", async ({ page }) => {
    await authenticate(page);
    const card = await parkedCard(page);
    const before = await page
      .locator('[data-blit-preview-panel] [draggable="true"]')
      .count();

    const cardAt = await centerOf(
      page,
      '[data-blit-preview-panel] [draggable="true"]',
    );
    // Moving straight away, with no hold: the gesture belongs to the list,
    // not to the drag bridge.
    await touch(page, "pointerdown", cardAt);
    await touch(page, "pointermove", { x: cardAt.x - 40, y: cardAt.y });
    await touch(page, "pointermove", { x: 400, y: 350 });
    await touch(page, "pointerup", { x: 400, y: 350 });
    await page.waitForTimeout(600);

    // Nothing was dragged anywhere: the same cards are still parked.
    await expect(
      page.locator('[data-blit-preview-panel] [draggable="true"]'),
    ).toHaveCount(before);
    await expect(card).toBeVisible();
  });
});
