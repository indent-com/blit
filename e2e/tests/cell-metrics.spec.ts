import { test, expect } from "@playwright/test";

const SIZES = [8, 10, 12, 13, 14, 16, 20];
const DPRS = [1, 2];

for (const dpr of DPRS) {
  for (const fs of SIZES) {
    test(`cell metrics at ${fs}px @${dpr}x`, async ({ browser }) => {
      const context = await browser.newContext({
        viewport: { width: 1920, height: 1080 },
        deviceScaleFactor: dpr,
      });
      const page = await context.newPage();
      await page.goto("about:blank");

      const info = await page.evaluate((fontSize) => {
        const canvas = document.createElement("canvas");
        const ctx = canvas.getContext("2d")!;
        const font = `${fontSize}px ui-monospace, monospace`;
        ctx.font = font;
        const m = ctx.measureText("M");
        const dpr = window.devicePixelRatio || 1;
        const w = m.width;
        const h = m.fontBoundingBoxAscent + m.fontBoundingBoxDescent;
        return {
          cssW: w,
          cssH: h,
          pw: Math.round(w * dpr),
          ph: Math.round(h * dpr),
          ascent: m.fontBoundingBoxAscent,
          descent: m.fontBoundingBoxDescent,
          dpr,
          // What the canvas backing store would be at 4K
          canvasW_4k:
            Math.round(w * dpr) *
            Math.floor((3840 * dpr) / Math.max(1, Math.round(w * dpr))),
          canvasH_4k:
            Math.round(h * dpr) *
            Math.floor((2160 * dpr) / Math.max(1, Math.round(h * dpr))),
        };
      }, fs);

      console.log(
        `${fs}px @${dpr}x: css=${info.cssW.toFixed(1)}x${info.cssH.toFixed(1)} dev=${info.pw}x${info.ph} ascent=${info.ascent} descent=${info.descent} 4K_canvas=${info.canvasW_4k}x${info.canvasH_4k}`,
      );
      expect(info.ph).toBeGreaterThan(0);
      expect(info.pw).toBeGreaterThan(0);

      await context.close();
    });
  }
}
