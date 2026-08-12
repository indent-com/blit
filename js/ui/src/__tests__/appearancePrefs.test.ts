import { afterEach, describe, expect, it } from "vitest";
import { DEFAULT_TEXT_GAMMA, PALETTES } from "@blit-sh/core";
import {
  FONT_KEY,
  FONT_SIZE_KEY,
  PALETTE_KEY,
  TEXT_GAMMA_KEY,
  preferredFont,
  preferredFontSize,
  preferredPalette,
  preferredTextGamma,
  urlPinnedKeys,
} from "../storage";

/**
 * Appearance comes from three places at once: the URL this document was
 * opened with, the preference synced across the account, and the built-in
 * default. The URL is the most specific of the three — a share or an embed
 * saying how *this* view should look — so it wins, and it has to keep
 * winning after the synced value arrives (Workspace consults
 * `urlPinnedKeys()` before following the config socket).
 */

const at = (search: string) =>
  window.history.replaceState(null, "", `/${search}`);

afterEach(() => {
  localStorage.clear();
  at("");
});

describe("appearance precedence", () => {
  it("prefers the URL over a stored choice", () => {
    localStorage.setItem(FONT_KEY, "Stored Face");
    localStorage.setItem(FONT_SIZE_KEY, "13");
    localStorage.setItem(TEXT_GAMMA_KEY, "1");
    localStorage.setItem(PALETTE_KEY, "catppuccin");
    at("?font=Iosevka&fontSize=19&textGamma=1.4&palette=ayu-mirage");

    expect(preferredFont()).toBe("Iosevka");
    expect(preferredFontSize()).toBe(19);
    expect(preferredTextGamma()).toBe(1.4);
    expect(preferredPalette().id).toBe("ayu-mirage");
  });

  it("falls through to the stored choice when the URL says nothing", () => {
    localStorage.setItem(FONT_SIZE_KEY, "17");
    expect(preferredFontSize()).toBe(17);
    expect(preferredTextGamma()).toBe(DEFAULT_TEXT_GAMMA);
    expect(preferredPalette().id).toBe(PALETTES[0].id);
  });

  it("ignores a URL value it cannot read, rather than the stored one", () => {
    localStorage.setItem(FONT_SIZE_KEY, "17");
    localStorage.setItem(TEXT_GAMMA_KEY, "1.2");
    localStorage.setItem(PALETTE_KEY, "catppuccin");
    at("?fontSize=huge&textGamma=9&palette=no-such-palette&font=%20%20");

    expect(preferredFontSize()).toBe(17);
    expect(preferredTextGamma()).toBe(1.2);
    expect(preferredPalette().id).toBe("catppuccin");
  });
});

describe("urlPinnedKeys", () => {
  it("claims only the keys the URL actually names", () => {
    at("?fontSize=19");
    expect([...urlPinnedKeys()]).toEqual([FONT_SIZE_KEY]);

    at("?font=Iosevka&palette=ayu-mirage&textGamma=1.4");
    expect(urlPinnedKeys()).toEqual(
      new Set([FONT_KEY, PALETTE_KEY, TEXT_GAMMA_KEY]),
    );
  });

  it("claims nothing for a value the setting would reject", () => {
    // Otherwise a typo would strand the setting on its default: pinned
    // against the synced value, but with no URL value to show for it.
    at("?fontSize=0&textGamma=9&palette=no-such-palette&font=%20%20");
    expect(urlPinnedKeys().size).toBe(0);
  });

  it("claims nothing on a bare URL", () => {
    at("");
    expect(urlPinnedKeys().size).toBe(0);
  });
});
