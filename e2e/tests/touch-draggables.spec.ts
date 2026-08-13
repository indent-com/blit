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

/**
 * Swipe horizontally across an element, as a finger does: both event families
 * at once.
 *
 * Pointer events alone would prove only that the drag bridge stays inert.
 * Swipe-to-dismiss is wired to `onTouchStart`/`onTouchMove`/`onTouchEnd` and
 * reads `TouchEvent.touches`, so without real touch events the gesture this
 * must not steal never actually happens, and the test would pass whatever the
 * bridge did to it.
 */
async function swipe(
  page: Page,
  selector: string,
  from: { x: number; y: number },
  dx: number,
) {
  await page.evaluate(
    ([selector, from, dx]) => {
      const el = document.querySelector(selector as string);
      if (!el) throw new Error(`no element for ${selector}`);
      const start = from as { x: number; y: number };
      const at = (x: number) =>
        new Touch({ identifier: 1, target: el, clientX: x, clientY: start.y });
      const touchEvent = (type: string, x: number) => {
        const t = at(x);
        return new TouchEvent(type, {
          touches: type === "touchend" ? [] : [t],
          targetTouches: type === "touchend" ? [] : [t],
          changedTouches: [t],
          bubbles: true,
          cancelable: true,
        });
      };
      const pointerEvent = (type: string, x: number) =>
        new PointerEvent(type, {
          pointerId: 1,
          pointerType: "touch",
          isPrimary: true,
          clientX: x,
          clientY: start.y,
          bubbles: true,
          cancelable: true,
        });

      el.dispatchEvent(pointerEvent("pointerdown", start.x));
      el.dispatchEvent(touchEvent("touchstart", start.x));
      const steps = 6;
      for (let i = 1; i <= steps; i++) {
        const x = start.x + ((dx as number) * i) / steps;
        window.dispatchEvent(pointerEvent("pointermove", x));
        el.dispatchEvent(touchEvent("touchmove", x));
      }
      const end = start.x + (dx as number);
      window.dispatchEvent(pointerEvent("pointerup", end));
      el.dispatchEvent(touchEvent("touchend", end));
    },
    [selector, from, dx] as const,
  );
}

/**
 * Live terminal count, from the status bar's `{count}T`.
 *
 * The distinguishing signal between the two ways a card can leave the dock: a
 * dismiss closes its session, a drag merely displays it somewhere. Asserting
 * only that the card is gone would pass for either, which is how a first
 * version of this test survived making the bridge steal the swipe.
 */
async function terminalCount(page: Page) {
  // The status bar's menu button, not merely the first button on the page —
  // the left dock's gear comes earlier in the DOM.
  const text = await page.locator('button[title="Menu"]').first().innerText();
  const m = /(\d+)T/.exec(text);
  if (!m) throw new Error(`no terminal count in ${JSON.stringify(text)}`);
  return Number(m[1]);
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

  test("a swipe across a dock card still dismisses it", async ({ page }) => {
    await authenticate(page);
    await parkedCard(page);
    const selector = '[data-blit-preview-panel] [draggable="true"]';
    const label = (await page.locator(selector).first().innerText()).trim();
    const cardAt = await centerOf(page, selector);
    const before = await terminalCount(page);

    // Moving straight away, with no hold: the gesture belongs to the card,
    // not to the drag bridge. Past SWIPE_THRESHOLD and horizontal, so it is
    // unambiguously a dismiss.
    await swipe(page, selector, cardAt, 160);
    await page.waitForTimeout(800);

    // Gone from the dock *and* closed. The second half is what makes this a
    // test of the swipe rather than of the drag: a card the bridge stole and
    // dropped somewhere would also leave the dock, but its session would
    // still be running.
    const parked = await page.locator(selector).allInnerTexts();
    expect(parked.map((t) => t.trim())).not.toContain(label);
    await expect.poll(() => terminalCount(page)).toBe(before - 1);
  });
});
