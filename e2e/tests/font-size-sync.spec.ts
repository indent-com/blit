import { expect, test, type Page } from "@playwright/test";

const PASSPHRASE = process.env.BLIT_PASSPHRASE ?? "test-secret";

async function fontButtonSize(page: Page): Promise<string> {
  return page.locator('button[title="Font"]').evaluate((el) =>
    getComputedStyle(el).fontSize,
  );
}

test("font size stays local while previewing and reaches peers on Apply", async ({
  browser,
}) => {
  const baseURL = test.info().project.use.baseURL as string;
  const firstContext = await browser.newContext();
  const secondContext = await browser.newContext();
  let first: Page | undefined;
  let originalValue: string | undefined;

  try {
    first = await firstContext.newPage();
    const second = await secondContext.newPage();
    await Promise.all([
      first.goto(`${baseURL}/#psk=${encodeURIComponent(PASSPHRASE)}`),
      second.goto(`${baseURL}/#psk=${encodeURIComponent(PASSPHRASE)}`),
    ]);

    const firstFontButton = first.locator('button[title="Font"]');
    const secondFontButton = second.locator('button[title="Font"]');
    await expect(firstFontButton).toBeVisible();
    await expect(secondFontButton).toBeVisible();

    const original = await fontButtonSize(second);
    await firstFontButton.click();
    const sizeInput = first.locator('input[name="blit-font-size"]');
    originalValue = await sizeInput.inputValue();
    const previewValue = originalValue === "22" ? "20" : "22";
    const previewPixels = Number.parseInt(previewValue, 10);
    await sizeInput.fill(previewValue);

    await expect.poll(() => fontButtonSize(first)).toBe(`${previewPixels}px`);
    await second.waitForTimeout(300);
    expect(await fontButtonSize(second)).toBe(original);

    await first.getByRole("button", { name: "Apply", exact: true }).click();
    await expect.poll(() => fontButtonSize(second)).toBe(`${previewPixels}px`);
  } finally {
    // Do not leave the shared developer/e2e config changed after the test.
    if (first && originalValue) {
      try {
        const sizeInput = first.locator('input[name="blit-font-size"]');
        if (!(await sizeInput.isVisible())) {
          await first.locator('button[title="Font"]').click();
        }
        await sizeInput.fill(originalValue);
        await first.getByRole("button", { name: "Apply", exact: true }).click();
      } catch {}
    }
    await firstContext.close();
    await secondContext.close();
  }
});
