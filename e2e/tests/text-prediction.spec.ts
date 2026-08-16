import { test, expect, type Page } from "@playwright/test";
import { mkdtempSync, readFileSync, existsSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

/**
 * Prediction mode stops encoding printable keys at `keydown` and lets them
 * land in the capture textarea instead, so the host can predict against the
 * line; what reaches the pty is the *difference* between the field and what
 * has already been sent.
 *
 * jsdom cannot judge that: it never inserts the character the browser would,
 * so a unit test asserts against a field this code filled in itself.  The
 * question a real browser answers is whether the bytes arrive exactly once —
 * so the oracle is a file the shell writes, not anything drawn on screen.  A
 * doubled character ("heello"), a dropped one, or a stray DEL all produce a
 * different file, and a mangled command produces none.
 */

const OUT = mkdtempSync(join(tmpdir(), "blit-prediction-"));

test.afterAll(() => rmSync(OUT, { recursive: true, force: true }));

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

/** Turn prediction mode on regardless of platform, then reload so the
 *  terminal surface picks it up at mount. */
async function enablePrediction(page: Page) {
  await page.evaluate(() => localStorage.setItem("blit.textPrediction", "on"));
  await page.reload();
  await expect(page.locator("canvas").first()).toBeVisible({ timeout: 15_000 });
  await page.waitForTimeout(1500);
}

function captureTextarea(page: Page) {
  return page.locator('textarea[aria-label="Terminal input"]:not([readonly])');
}

/** Wait for the shell to have written `path`, then read it. */
async function fileEventually(path: string, timeoutMs = 10_000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (existsSync(path)) return readFileSync(path, "utf8");
    await new Promise((r) => setTimeout(r, 200));
  }
  throw new Error(`${path} never appeared — the command did not run`);
}

test.describe("host text prediction", () => {
  test("the session runs a shell that edits its own line", async ({ page }) => {
    // Prediction mode is gated on what the app does to the terminal, so a
    // session running /bin/sh proves nothing about a session running the
    // user's shell: dash has no line editing and leaves the pty cooked,
    // which is the one case the old `icanon` gate accepted.  This suite went
    // green for a day against a server started without SHELL.
    await authenticate(page);

    const out = join(OUT, "shell.txt");
    await captureTextarea(page).focus();
    await page.keyboard.type(`echo $0 > ${out}`, { delay: 40 });
    await page.keyboard.press("Enter");

    const shell = (await fileEventually(out)).trim();
    expect(
      shell,
      `session shell is ${shell}; start the server with SHELL set`,
    ).not.toMatch(/^-?(\/bin\/)?(sh|dash)$/);
  });

  test("typed characters reach the pty exactly once", async ({ page }) => {
    await authenticate(page);
    await enablePrediction(page);

    const out = join(OUT, "typed.txt");
    await captureTextarea(page).focus();
    await page.keyboard.type(`printf 'hello' > ${out}`, { delay: 40 });
    await page.keyboard.press("Enter");

    expect(await fileEventually(out)).toBe("hello");
    // Enter starts a new line: the field must not still be holding the old one.
    await expect(captureTextarea(page)).toHaveValue("");
  });

  test("a Backspace through the field deletes one character, not none or two", async ({
    page,
  }) => {
    await authenticate(page);
    await enablePrediction(page);

    const out = join(OUT, "edited.txt");
    await captureTextarea(page).focus();
    await page.keyboard.type("printf 'helXX", { delay: 40 });
    await page.keyboard.press("Backspace");
    await page.keyboard.press("Backspace");
    await page.keyboard.type(`lo' > ${out}`, { delay: 40 });
    await page.keyboard.press("Enter");

    expect(await fileEventually(out)).toBe("hello");
  });

  test("the field accumulates the line, which is what a predictor reads", async ({
    page,
  }) => {
    await authenticate(page);
    await enablePrediction(page);

    await captureTextarea(page).focus();
    await page.keyboard.type("echo predict", { delay: 40 });
    await page.waitForTimeout(300);

    // The point of the whole exercise: the host has a prefix to complete.
    await expect(captureTextarea(page)).toHaveValue("echo predict");

    // Leave the shell's line clean for whatever runs next.
    for (let i = 0; i < 12; i++) await page.keyboard.press("Backspace");
  });

  test("a proposal shows in a chip on the cursor's line and is not forwarded", async ({
    page,
  }) => {
    await authenticate(page);
    await enablePrediction(page);

    const out = join(OUT, "proposal.txt");
    await captureTextarea(page).focus();
    await page.keyboard.type(`printf 'he' > ${out}`, { delay: 40 });

    // Stand in for the host predictor: put a tail in the field and select it,
    // which is how macOS presents an inline prediction to the page.
    await page.evaluate(() => {
      const ta = document.querySelector(
        'textarea[aria-label="Terminal input"]:not([readonly])',
      ) as HTMLTextAreaElement;
      const committed = ta.value;
      ta.value = committed + " --proposed";
      ta.setSelectionRange(committed.length, ta.value.length);
      ta.dispatchEvent(
        new InputEvent("input", { inputType: "insertText", bubbles: true }),
      );
    });

    const chip = page.locator("[data-blit-suggestion]");
    await expect(chip).toBeVisible();
    await expect(chip).toHaveText(" --proposed");

    // On the caret's own line, starting at the cursor, so it reads as the
    // continuation of the text.  The capture element is parked on the caret,
    // so it is the reference for both axes.
    const chipBox = (await chip.boundingBox())!;
    const caretBox = (await captureTextarea(page).boundingBox())!;
    // Centres, not edges: the chip is sized by its text and straddles the row
    // rather than being clipped to it.
    expect(
      Math.abs(
        chipBox.y + chipBox.height / 2 - (caretBox.y + caretBox.height / 2),
      ),
    ).toBeLessThan(4);
    expect(Math.abs(chipBox.x - caretBox.x)).toBeLessThan(2);

    // The proposal is the host's, not the user's: running the line must not
    // have picked it up.
    await page.keyboard.press("Enter");
    expect(await fileEventually(out)).toBe("he");
  });

  test("with prediction off the field stays empty, as it always was", async ({
    page,
  }) => {
    await authenticate(page);
    await page.evaluate(() =>
      localStorage.setItem("blit.textPrediction", "off"),
    );
    await page.reload();
    await expect(page.locator("canvas").first()).toBeVisible({
      timeout: 15_000,
    });
    await page.waitForTimeout(1500);

    const out = join(OUT, "legacy.txt");
    await captureTextarea(page).focus();
    await page.keyboard.type(`printf 'plain' > ${out}`, { delay: 40 });
    await expect(captureTextarea(page)).toHaveValue("");
    await page.keyboard.press("Enter");

    expect(await fileEventually(out)).toBe("plain");
  });
});
