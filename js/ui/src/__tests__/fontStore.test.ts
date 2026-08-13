import { afterEach, describe, expect, it, vi } from "vitest";
import {
  FONT_MAX_AGE_MS,
  FONT_STORE_BUDGET_BYTES,
  isStale,
  loadFontList,
  loadFontMetrics,
  saveFontList,
  saveFontMetrics,
  selectEvictions,
} from "../fontStore";

/**
 * What the frontend remembers about a server's fonts.
 *
 * `font/<family>` inlines the face as base64, which for a real family is tens
 * of megabytes; the point of the store is that a second load spends none of
 * it. So the parts worth pinning are the ones that decide whether we go to
 * the network at all, and the ones that decide what to throw away — a cache
 * that evicts the font currently on screen would be worse than no cache.
 */

afterEach(() => {
  localStorage.clear();
  vi.useRealTimers();
});

describe("isStale", () => {
  const entry = { css: "@font-face{}", savedAt: 1_000_000, usedAt: 1_000_000 };

  it("keeps a fresh copy off the network", () => {
    expect(isStale(entry, entry.savedAt + FONT_MAX_AGE_MS - 1)).toBe(false);
  });

  it("revalidates once the server's own max-age is up", () => {
    expect(isStale(entry, entry.savedAt + FONT_MAX_AGE_MS)).toBe(true);
  });
});

describe("selectEvictions", () => {
  const entry = (key: string, mb: number, usedAt: number) => ({
    key,
    bytes: mb * 1024 * 1024,
    usedAt,
  });

  it("keeps everything while it fits", () => {
    expect(
      selectEvictions(
        [entry("a", 25, 1), entry("b", 25, 2)],
        FONT_STORE_BUDGET_BYTES,
      ),
    ).toEqual([]);
  });

  it("evicts least-recently-used first, and only as far as needed", () => {
    const entries = [
      entry("old", 25, 1),
      entry("older", 25, 0),
      entry("new", 25, 3),
    ];
    expect(selectEvictions(entries, FONT_STORE_BUDGET_BYTES)).toEqual([
      "older",
    ]);
  });

  it("never evicts the family just stored", () => {
    // The one in use is the oldest by `usedAt` — a fresh write has not been
    // read back yet — so LRU alone would throw away the font on screen.
    const entries = [
      entry("in-use", 40, 0),
      entry("a", 20, 5),
      entry("b", 20, 6),
    ];
    const evicted = selectEvictions(entries, FONT_STORE_BUDGET_BYTES, "in-use");
    expect(evicted).not.toContain("in-use");
    expect(evicted).toEqual(["a"]);
  });

  it("keeps a family that alone exceeds the budget", () => {
    const entries = [entry("huge", 200, 0)];
    expect(selectEvictions(entries, FONT_STORE_BUDGET_BYTES, "huge")).toEqual(
      [],
    );
  });
});

describe("metrics", () => {
  it("round-trip a remembered advance ratio", () => {
    saveFontMetrics("/vt/PragmataPro Mono", 0.5);
    expect(loadFontMetrics("/vt/PragmataPro Mono")).toEqual({
      advanceRatio: 0.5,
      stale: false,
    });
  });

  it("are per base path, so two servers do not share a ratio", () => {
    saveFontMetrics("/a/Iosevka", 0.5);
    expect(loadFontMetrics("/b/Iosevka")).toBeNull();
  });

  it("go stale on the same clock as the faces", () => {
    vi.useFakeTimers();
    vi.setSystemTime(0);
    saveFontMetrics("/Iosevka", 0.6);
    vi.setSystemTime(FONT_MAX_AGE_MS);
    expect(loadFontMetrics("/Iosevka")).toEqual({
      advanceRatio: 0.6,
      stale: true,
    });
  });

  it("survive junk in storage", () => {
    localStorage.setItem("blit.font-metrics:/Iosevka", "not json");
    expect(loadFontMetrics("/Iosevka")).toBeNull();
    localStorage.setItem("blit.font-metrics:/Iosevka", '{"value":"wide"}');
    expect(loadFontMetrics("/Iosevka")).toBeNull();
  });
});

describe("the family list", () => {
  it("round-trips", () => {
    saveFontList("/", ["Iosevka", "PragmataPro Mono"]);
    expect(loadFontList("/")).toEqual({
      fonts: ["Iosevka", "PragmataPro Mono"],
      stale: false,
    });
  });

  it("reports an empty listing as nothing remembered", () => {
    // Otherwise a picker that once opened against a server with no fonts
    // installed would never ask again.
    saveFontList("/", []);
    expect(loadFontList("/")).toBeNull();
  });

  it("drops entries that are not family names", () => {
    localStorage.setItem(
      "blit.font-list:/",
      JSON.stringify({ value: ["Iosevka", "", 7, null], savedAt: Date.now() }),
    );
    expect(loadFontList("/")?.fonts).toEqual(["Iosevka"]);
  });
});
