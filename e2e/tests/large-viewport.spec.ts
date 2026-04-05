import { test, expect } from "@playwright/test";

async function authenticate(page: import("@playwright/test").Page) {
  await page.goto("/");
  await page.evaluate(() => localStorage.clear());
  await page.goto("/");
  const passInput = page.locator('input[type="password"]');
  await expect(passInput).toBeVisible();
  await passInput.fill("test-secret");
  await passInput.press("Enter");
  await expect(
    page
      .getByRole("button", { name: "New terminal" })
      .first()
      .or(page.locator("canvas").first()),
  ).toBeVisible({ timeout: 10_000 });
  // Wait for the DOM to stabilise after connection setup.
  await page.waitForTimeout(500);
  const canvas = page.locator("canvas").first();
  if (!(await canvas.isVisible().catch(() => false))) {
    await page.getByRole("button", { name: "New terminal" }).first().click();
  }
  await expect(canvas).toBeVisible({ timeout: 10_000 });
}

const VIEWPORTS = [
  { name: "1080p-8px", width: 1920, height: 1080, dpr: 1, fontSize: 8 },
  { name: "4K-8px", width: 3840, height: 2160, dpr: 1, fontSize: 8 },
  { name: "4K-2x-8px", width: 1920, height: 1080, dpr: 2, fontSize: 8 },
  { name: "4K-2x-13px", width: 1920, height: 1080, dpr: 2, fontSize: 13 },
];

for (const vp of VIEWPORTS) {
  test(`renders at ${vp.name} (${vp.width}x${vp.height} @${vp.dpr}x, ${vp.fontSize}px)`, async ({
    browser,
  }) => {
    const context = await browser.newContext({
      viewport: { width: vp.width, height: vp.height },
      deviceScaleFactor: vp.dpr,
    });
    const page = await context.newPage();

    await authenticate(page);

    // Wait for the canvas to be resized beyond the default 300x150
    // (indicates WASM loaded and terminal rendered).
    await page.waitForFunction(
      () => {
        const c = document.querySelector("canvas");
        return c && c.width > 300 && c.height > 150;
      },
      { timeout: 15_000 },
    );
    await page.waitForTimeout(1000);

    // Measure cell metrics in the browser
    const info = await page.evaluate((fs) => {
      const c = document.querySelector("canvas");
      const canvas2 = document.createElement("canvas");
      const ctx = canvas2.getContext("2d")!;
      ctx.font = `${fs}px ui-monospace, monospace`;
      const m = ctx.measureText("M");
      return {
        canvasW: c?.width ?? 0,
        canvasH: c?.height ?? 0,
        cellW: m.width,
        cellH: m.fontBoundingBoxAscent + m.fontBoundingBoxDescent,
        dpr: window.devicePixelRatio,
        totalCells: c
          ? Math.floor(
              c.width /
                Math.max(1, Math.round(m.width * window.devicePixelRatio)),
            ) *
            Math.floor(
              c.height /
                Math.max(
                  1,
                  Math.round(
                    (m.fontBoundingBoxAscent + m.fontBoundingBoxDescent) *
                      window.devicePixelRatio,
                  ),
                ),
            )
          : 0,
      };
    }, vp.fontSize);
    console.log(
      `${vp.name}: canvas=${info.canvasW}x${info.canvasH} cell=${info.cellW}x${info.cellH} cells=${info.totalCells}`,
    );

    // Check any canvas has non-blank pixels (rendering is working).
    // The display canvas (2D) is reliable; the GL canvas may be cleared
    // by the compositor (preserveDrawingBuffer is false).
    const hasPixels = await page.evaluate(() => {
      for (const c of document.querySelectorAll("canvas")) {
        if (c.width === 0 || c.height === 0) continue;
        const tmp = document.createElement("canvas");
        tmp.width = Math.min(c.width, 200);
        tmp.height = Math.min(c.height, 200);
        const ctx = tmp.getContext("2d");
        if (!ctx) continue;
        ctx.drawImage(c, 0, 0, tmp.width, tmp.height);
        const data = ctx.getImageData(0, 0, tmp.width, tmp.height).data;
        for (let i = 3; i < data.length; i += 4) {
          if (data[i] > 0) return true;
        }
      }
      return false;
    });

    await page.screenshot({
      path: `test-results/viewport-${vp.name}.png`,
      fullPage: false,
    });

    expect(hasPixels).toBe(true);

    await context.close();
  });
}
