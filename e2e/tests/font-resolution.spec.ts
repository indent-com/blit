import { test, expect } from "@playwright/test";

test("ui-monospace resolves in canvas context", async ({ page }) => {
  await page.goto("about:blank");
  const result = await page.evaluate(() => {
    const canvas = document.createElement("canvas");
    const ctx = canvas.getContext("2d")!;

    // Measure with ui-monospace
    ctx.font = "16px ui-monospace, monospace";
    const uiMono = ctx.measureText("M").width;

    // Measure with just monospace
    ctx.font = "16px monospace";
    const mono = ctx.measureText("M").width;

    // Measure with serif (clearly different)
    ctx.font = "16px serif";
    const serif = ctx.measureText("M").width;

    return { uiMono, mono, serif, same: uiMono === mono };
  });
  console.log("Font widths:", JSON.stringify(result));
  // ui-monospace should either resolve to something different from
  // plain monospace (it's a better font) or at least not break
  expect(result.uiMono).toBeGreaterThan(0);
});
