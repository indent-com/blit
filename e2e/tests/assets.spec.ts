import { test, expect } from "@playwright/test";

test.describe("Asset serving", () => {
  test("GET / returns HTML with correct content-type", async ({ request }) => {
    const resp = await request.get("/");
    expect(resp.status()).toBe(200);
    const ct = resp.headers()["content-type"] ?? "";
    expect(ct).toContain("text/html");
  });

  test("GET /blit_browser.js returns JavaScript", async ({ request }) => {
    const resp = await request.get("/blit_browser.js");
    expect(resp.status()).toBe(200);
    const ct = resp.headers()["content-type"] ?? "";
    expect(ct.includes("javascript") || ct.includes("ecmascript")).toBeTruthy();
  });

  test("GET /blit_browser_bg.wasm returns WASM", async ({ request }) => {
    const resp = await request.get("/blit_browser_bg.wasm");
    expect(resp.status()).toBe(200);
    const ct = resp.headers()["content-type"] ?? "";
    expect(ct).toContain("application/wasm");
  });

  test("GET /vt/blit_browser.js returns JavaScript (reverse proxy prefix)", async ({
    request,
  }) => {
    const resp = await request.get("/vt/blit_browser.js");
    expect(resp.status()).toBe(200);
    const ct = resp.headers()["content-type"] ?? "";
    expect(ct.includes("javascript") || ct.includes("ecmascript")).toBeTruthy();
  });

  test("GET /anyprefix/snippets/blit-browser-anyhash/inline0.js returns JavaScript", async ({
    request,
  }) => {
    const resp = await request.get(
      "/anyprefix/snippets/blit-browser-anyhash/inline0.js",
    );
    expect(resp.status()).toBe(200);
    const ct = resp.headers()["content-type"] ?? "";
    expect(ct.includes("javascript") || ct.includes("ecmascript")).toBeTruthy();
  });

  test("GET /nonexistent returns HTML fallback", async ({ request }) => {
    const resp = await request.get("/nonexistent");
    expect(resp.status()).toBe(200);
    const ct = resp.headers()["content-type"] ?? "";
    expect(ct).toContain("text/html");
  });
});
