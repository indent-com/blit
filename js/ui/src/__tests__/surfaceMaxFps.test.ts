import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const FPS_KEY = "blit.surfaceMaxFps";

async function freshStorage() {
  vi.resetModules();
  return await import("../storage");
}

function stubLocalStorage() {
  const map = new Map<string, string>();
  vi.stubGlobal("localStorage", {
    getItem: (key: string) => map.get(key) ?? null,
    setItem: (key: string, value: string) => void map.set(key, value),
    removeItem: (key: string) => void map.delete(key),
    clear: () => map.clear(),
  });
}

describe("surface frame-rate preference", () => {
  beforeEach(stubLocalStorage);
  afterEach(() => vi.unstubAllGlobals());

  it("is disabled by default", async () => {
    const storage = await freshStorage();
    expect(storage.preferredSurfaceMaxFps()).toBe(0);
  });

  it("loads a valid cap", async () => {
    localStorage.setItem(FPS_KEY, "60");
    const storage = await freshStorage();
    expect(storage.preferredSurfaceMaxFps()).toBe(60);
  });

  it("disables invalid and out-of-range values", async () => {
    for (const value of ["nope", "-1", "1001"]) {
      localStorage.setItem(FPS_KEY, value);
      const storage = await freshStorage();
      expect(storage.preferredSurfaceMaxFps()).toBe(0);
    }
  });
});
