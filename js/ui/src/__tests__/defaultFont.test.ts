import { afterEach, describe, expect, it } from "vitest";
import { DEFAULT_FONT } from "@blit-sh/core";
import { defaultFont, preferredFont, setDefaultFont } from "../storage";
import { primaryFontFamily } from "../createFontLoader";

/**
 * The host's font default. An embedder that self-hosts a face (blit.sh ships
 * JetBrains Mono for the whole site) wants the workspace on it; the app
 * served by a blit server ships no webfont and is right to ask the platform
 * for one. The seam is a fallback, so a visitor's own choice has to survive
 * it — that is the part worth pinning.
 */

const HOST = '"Some Host Face", ui-monospace, monospace';

afterEach(() => {
  setDefaultFont(DEFAULT_FONT);
  localStorage.clear();
  window.history.replaceState(null, "", "/");
});

describe("setDefaultFont", () => {
  it("starts at the app's platform stack", () => {
    expect(defaultFont()).toBe(DEFAULT_FONT);
    expect(preferredFont()).toBe(DEFAULT_FONT);
  });

  it("replaces the fallback a host has a better answer for", () => {
    setDefaultFont(HOST);
    expect(defaultFont()).toBe(HOST);
    expect(preferredFont()).toBe(HOST);
  });

  it("loses to a stored choice", () => {
    setDefaultFont(HOST);
    localStorage.setItem("blit.fontFamily", "Iosevka");
    expect(preferredFont()).toBe("Iosevka");
  });

  it("loses to ?font=", () => {
    setDefaultFont(HOST);
    window.history.replaceState(null, "", "/?font=Iosevka");
    expect(preferredFont()).toBe("Iosevka");
  });

  it("ignores an empty host default rather than blanking the stack", () => {
    setDefaultFont("   ");
    expect(defaultFont()).toBe(DEFAULT_FONT);
  });
});

/** How that stack is named back to the user — the status bar's font entry. */
describe("primaryFontFamily", () => {
  it("names the chosen face, not the fallbacks it carries", () => {
    expect(primaryFontFamily(HOST)).toBe("Some Host Face");
    expect(primaryFontFamily('"JetBrains Mono", ui-monospace, monospace')).toBe(
      "JetBrains Mono",
    );
    expect(primaryFontFamily("Iosevka")).toBe("Iosevka");
  });

  it("falls back to the generic when there is no choice behind it", () => {
    expect(primaryFontFamily(DEFAULT_FONT)).toBe("ui-monospace");
    expect(primaryFontFamily("")).toBe("");
  });
});
