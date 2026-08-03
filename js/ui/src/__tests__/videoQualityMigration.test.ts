import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

// `blit.videoQuality` was split into a bandwidth and a speed axis. The old
// value's encoding is exactly the new bandwidth axis, so an upgrade should
// carry it over rather than silently drop the user back to the default.

const LEGACY_KEY = "blit.videoQuality";
const BANDWIDTH_KEY = "blit.videoBandwidth";

/** storage.ts runs the migration at module scope, so each case needs a
 *  freshly imported copy. */
async function freshStorage() {
  vi.resetModules();
  return await import("../storage");
}

/** The sandbox environment has no working `localStorage`, so provide one.
 *  storage.ts only ever uses getItem/setItem/removeItem. */
function stubLocalStorage() {
  const map = new Map<string, string>();
  vi.stubGlobal("localStorage", {
    getItem: (k: string) => map.get(k) ?? null,
    setItem: (k: string, v: string) => void map.set(k, v),
    removeItem: (k: string) => void map.delete(k),
    clear: () => map.clear(),
  });
}

describe("blit.videoQuality migration", () => {
  beforeEach(stubLocalStorage);
  afterEach(() => vi.unstubAllGlobals());

  it("carries a custom quantizer over to the bandwidth key", async () => {
    localStorage.setItem(LEGACY_KEY, "140");
    const storage = await freshStorage();

    expect(storage.preferredVideoBandwidth()).toBe(140);
    expect(localStorage.getItem(BANDWIDTH_KEY)).toBe("140");
    // Read once, then gone — the legacy key is not a permanent read path.
    expect(localStorage.getItem(LEGACY_KEY)).toBeNull();
  });

  it("carries a preset over", async () => {
    localStorage.setItem(LEGACY_KEY, "3");
    const storage = await freshStorage();

    expect(storage.preferredVideoBandwidth()).toBe(3);
  });

  it("leaves the speed axis at the server default", async () => {
    // The old knob implied a speed rather than letting anyone choose one,
    // so there is no user intent to carry over.
    localStorage.setItem(LEGACY_KEY, "140");
    const storage = await freshStorage();

    expect(storage.preferredVideoSpeed()).toBe(0);
  });

  it("does not overwrite a bandwidth chosen after the upgrade", async () => {
    localStorage.setItem(LEGACY_KEY, "140");
    localStorage.setItem(BANDWIDTH_KEY, "2");
    const storage = await freshStorage();

    expect(storage.preferredVideoBandwidth()).toBe(2);
    expect(localStorage.getItem(LEGACY_KEY)).toBeNull();
  });

  it("ignores a legacy value that is out of range", async () => {
    localStorage.setItem(LEGACY_KEY, "999");
    const storage = await freshStorage();

    expect(storage.preferredVideoBandwidth()).toBe(0);
    expect(localStorage.getItem(BANDWIDTH_KEY)).toBeNull();
  });
});
