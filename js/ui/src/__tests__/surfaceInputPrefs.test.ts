import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

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

describe("surface input preferences", () => {
  beforeEach(stubLocalStorage);
  afterEach(() => vi.unstubAllGlobals());

  it("defaults to direct touch and honors Wayland keyboard requests", async () => {
    const storage = await freshStorage();
    expect(storage.preferredSurfaceTouchMode()).toBe("direct");
    expect(storage.preferredWaylandKeyboardRequests()).toBe(true);
  });

  it("loads the touch and keyboard opt-outs", async () => {
    localStorage.setItem("blit.surfaceTouchMode", "pointer");
    localStorage.setItem("blit.waylandKeyboardRequests", "0");
    const storage = await freshStorage();
    expect(storage.preferredSurfaceTouchMode()).toBe("pointer");
    expect(storage.preferredWaylandKeyboardRequests()).toBe(false);
  });

  it("falls back to the opt-out defaults for invalid values", async () => {
    localStorage.setItem("blit.surfaceTouchMode", "invalid");
    localStorage.setItem("blit.waylandKeyboardRequests", "invalid");
    const storage = await freshStorage();
    expect(storage.preferredSurfaceTouchMode()).toBe("direct");
    expect(storage.preferredWaylandKeyboardRequests()).toBe(true);
  });
});
