import { test, expect, type Page } from "@playwright/test";

/**
 * The grip must be draggable with a finger, not only a mouse.
 *
 * HTML5 drag-and-drop never fires from touch, so on Android the grip could be
 * tapped — which cycles the toolbar's corner — but never dragged: every pane
 * move and every park was mouse-only. `startPaneTouchDrag` bridges pointer
 * events to the same DragEvents the drop handlers already listen for.
 *
 * The whole point is touch, so these run on an emulated touch device; the
 * pointer events are dispatched with `pointerType: "touch"` because that is
 * exactly what the bridge keys on (a mouse must keep the native path).
 */
test.use({
  hasTouch: true,
  isMobile: true,
  viewport: { width: 900, height: 700 },
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

/**
 * One touch step at a time, so the test can look at the page mid-drag.
 *
 * `pointerdown` goes to the element under the finger (the bridge listens
 * there); the rest go to `window`, which is where it listens once the drag is
 * under way. The handler's state lives in page listeners, so separate
 * evaluates continue the same gesture.
 */
async function touch(
  page: Page,
  type: "pointerdown" | "pointermove" | "pointerup",
  at: { x: number; y: number },
  pointerType: "touch" | "pen" = "touch",
) {
  await page.evaluate(
    ([type, at, pointerType]) => {
      const ev = new PointerEvent(type as string, {
        pointerId: 1,
        pointerType: pointerType as string,
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
    [type, at, pointerType] as const,
  );
}

async function centerOf(page: Page, selector: string) {
  const box = await page.locator(selector).first().boundingBox();
  if (!box) throw new Error(`no box for ${selector}`);
  return { x: box.x + box.width / 2, y: box.y + box.height / 2 };
}

test.describe("Grip drag with a finger", () => {
  test("dragging the grip to the dock parks the pane", async ({ page }) => {
    await authenticate(page);
    await newTerminal(page);

    // On touch the toolbar is always shown — there is no hover to reveal it.
    const grip = page
      .getByRole("button", { name: "Drag to move · click for another corner" })
      .first();
    await expect(grip).toBeVisible();

    const gripAt = await centerOf(page, "button[title^='Drag to move']");
    await touch(page, "pointerdown", gripAt);
    // Two moves: the first crosses the drag threshold and starts the drag,
    // which is what reveals the dock — it is not in the DOM before that, so
    // it can only be measured now, not aimed at up front.
    await touch(page, "pointermove", { x: gripAt.x - 40, y: gripAt.y + 40 });
    await touch(page, "pointermove", { x: gripAt.x - 80, y: gripAt.y + 80 });
    const dockAt = await centerOf(page, "[data-blit-preview-panel]");
    await touch(page, "pointermove", dockAt);
    await touch(page, "pointerup", dockAt);
    await page.waitForTimeout(600);

    // Parked: the main view fell back to the empty pane.
    await expect(
      page.getByRole("button", { name: "New terminal" }).first(),
    ).toBeVisible({ timeout: 10_000 });
  });

  // Review catch on #141: the guard used to exclude only `mouse`, letting a
  // pen through. A pen drives native drag-and-drop in Chromium, so both paths
  // would run and this one's `dragend` would clear the in-flight count and
  // unmount the dock underneath the native drag.
  test("a pen is left to the native path", async ({ page }) => {
    await authenticate(page);
    await newTerminal(page);

    const gripAt = await centerOf(page, "button[title^='Drag to move']");
    // Aim where the dock lives. Asserted on the outcome rather than on the
    // dock being absent: an earlier test may have left something parked, so
    // the dock can be on screen for reasons of its own. Either way the bridge
    // is what would reveal it and drop on it, so "nothing moved" is the
    // signal that the pen was left alone.
    const rightEdge = { x: 880, y: 300 };
    await touch(page, "pointerdown", gripAt, "pen");
    await touch(
      page,
      "pointermove",
      { x: gripAt.x - 60, y: gripAt.y + 60 },
      "pen",
    );
    await touch(page, "pointermove", rightEdge, "pen");
    await touch(page, "pointerup", rightEdge, "pen");
    await page.waitForTimeout(600);

    // Still displayed: not parked, so the main view never fell back to the
    // empty pane.
    await expect(page.locator("canvas").first()).toBeVisible();
    await expect(
      page.getByRole("button", { name: "New terminal" }),
    ).toHaveCount(0);
  });

  test("a tap still cycles the corner rather than dragging", async ({
    page,
  }) => {
    await authenticate(page);
    await newTerminal(page);

    const grip = page
      .getByRole("button", { name: "Drag to move · click for another corner" })
      .first();
    await expect(grip).toBeVisible();
    const before = (await grip.boundingBox())!;

    await grip.tap();
    await page.waitForTimeout(400);

    const after = (await grip.boundingBox())!;
    expect(after.x !== before.x || after.y !== before.y).toBe(true);
    // And the tap must not have parked anything.
    await expect(page.locator("canvas").first()).toBeVisible();
  });
});
