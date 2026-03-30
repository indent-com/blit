import { test, expect } from "@playwright/test";

test.describe("Asset serving", () => {
  test("GET / returns HTML with correct content-type", async ({ request }) => {
    const resp = await request.get("/");
    expect(resp.status()).toBe(200);
    const ct = resp.headers()["content-type"] ?? "";
    expect(ct).toContain("text/html");
  });

  test("GET /nonexistent returns HTML fallback", async ({ request }) => {
    const resp = await request.get("/nonexistent");
    expect(resp.status()).toBe(200);
    const ct = resp.headers()["content-type"] ?? "";
    expect(ct).toContain("text/html");
  });

  test("any path returns single-file HTML app", async ({ request }) => {
    for (const path of [
      "/blit_browser.js",
      "/vt/blit_browser.js",
      "/vt/blit_browser_bg.wasm",
    ]) {
      const resp = await request.get(path);
      expect(resp.status()).toBe(200);
      const ct = resp.headers()["content-type"] ?? "";
      expect(ct).toContain("text/html");
    }
  });
});
