import { test, expect, type Page } from "@playwright/test";

/**
 * A terminal has nowhere to put an IME's in-progress composition: the pty
 * protocol has no notion of a preedit and the cells belong to the app.  So
 * blit draws it beside the cursor — and until it did, the only thing on
 * screen while composing was the system's candidate window, which shows the
 * candidates rather than the buffer they are being chosen for.
 *
 * `Input.imeSetComposition` is Chromium's own IME entry point, so this needs
 * no IME installed and does not care what the host OS can do — which is the
 * point: unlike the macOS prediction path, this one works in every engine.
 */

async function authenticate(page: Page) {
  await page.goto("/");
  await page.evaluate(() => localStorage.clear());
  await page.goto("/#psk=test-secret");
  await page.reload();
  await expect(
    page
      .getByRole("button", { name: "New terminal" })
      .first()
      .or(page.locator("canvas").first()),
  ).toBeVisible({ timeout: 15_000 });
  await page.waitForTimeout(500);
  const canvas = page.locator("canvas").first();
  if (!(await canvas.isVisible().catch(() => false))) {
    await page.getByRole("button", { name: "New terminal" }).first().click();
  }
  await expect(canvas).toBeVisible({ timeout: 15_000 });
  await page.waitForTimeout(1500);
}

function captureTextarea(page: Page) {
  return page.locator('textarea[aria-label="Terminal input"]:not([readonly])');
}

function chipOf(page: Page) {
  return page.locator("[data-blit-suggestion]");
}

test.describe("composition chip", () => {
  test("shows the buffer being built, and drops it once committed", async ({
    page,
  }) => {
    await authenticate(page);
    await captureTextarea(page).focus();

    const cdp = await page.context().newCDPSession(page);
    const chip = chipOf(page);

    // Romaji resolving towards kana, one update at a time.
    await cdp.send("Input.imeSetComposition", {
      text: "に",
      selectionStart: 1,
      selectionEnd: 1,
    });
    await expect(chip).toBeVisible();
    await expect(chip).toHaveText("に");

    await cdp.send("Input.imeSetComposition", {
      text: "にほn",
      selectionStart: 3,
      selectionEnd: 3,
    });
    await expect(chip).toHaveText("にほn");

    // Committing ends the composition: the text is the app's now, so the
    // chip has nothing left to show.
    await cdp.send("Input.insertText", { text: "日本" });
    await expect(chip).toBeHidden();
  });

  test("shows the whole buffer rather than cutting it off", async ({
    page,
  }) => {
    // The chip is the only place this text is visible; truncating it defeats
    // the purpose.  A long composition wraps instead.
    await authenticate(page);
    await captureTextarea(page).focus();

    const cdp = await page.context().newCDPSession(page);
    const long = "きょうはとてもいいてんきですね".repeat(4);
    await cdp.send("Input.imeSetComposition", {
      text: long,
      selectionStart: long.length,
      selectionEnd: long.length,
    });

    const chip = chipOf(page);
    await expect(chip).toBeVisible();
    await expect(chip).toHaveText(long);

    // Every glyph laid out, none clipped by a box sized to one terminal row.
    const box = (await chip.boundingBox())!;
    const scroll = await chip.evaluate((el) => ({
      w: el.scrollWidth,
      h: el.scrollHeight,
    }));
    expect(scroll.w).toBeLessThanOrEqual(Math.ceil(box.width) + 1);
    expect(scroll.h).toBeLessThanOrEqual(Math.ceil(box.height) + 1);
  });

  test("withdraws the chip when the composition is abandoned", async ({
    page,
  }) => {
    await authenticate(page);
    await captureTextarea(page).focus();

    const cdp = await page.context().newCDPSession(page);
    const chip = chipOf(page);
    await cdp.send("Input.imeSetComposition", {
      text: "にほn",
      selectionStart: 3,
      selectionEnd: 3,
    });
    await expect(chip).toBeVisible();

    // An empty composition is how a cancelled one arrives.
    await cdp.send("Input.imeSetComposition", {
      text: "",
      selectionStart: 0,
      selectionEnd: 0,
    });
    await expect(chip).toBeHidden();
  });
});
