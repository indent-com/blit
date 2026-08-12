import { createSignal, createEffect, onCleanup } from "solid-js";
import { basePath } from "./storage";
import { shellCapabilities } from "./shellCapabilities";

const CSS_GENERIC = new Set([
  "serif",
  "sans-serif",
  "monospace",
  "cursive",
  "fantasy",
  "system-ui",
  "ui-serif",
  "ui-sans-serif",
  "ui-monospace",
  "ui-rounded",
  "math",
  "emoji",
  "fangsong",
]);

function splitFontFamilies(value: string): string[] {
  return value
    .split(",")
    .map((f) => f.trim().replace(/^['"]|['"]$/g, ""))
    .filter(Boolean);
}

/**
 * The one family a font stack is *about*, for naming it to the user.
 *
 * Everything after the first entry is a fallback, and the generics at the tail
 * are there so text renders at all — "JetBrains Mono, ui-monospace, monospace"
 * is the JetBrains Mono choice. A stack of nothing but generics (the default)
 * has no choice behind it, so its own first entry is the honest answer.
 */
export function primaryFontFamily(stack: string): string {
  const families = splitFontFamilies(stack);
  return (
    families.find((family) => !CSS_GENERIC.has(family.toLowerCase())) ??
    families[0] ??
    ""
  );
}

function fontStyleId(family: string): string {
  return `blit-font-${family.replace(/\s+/g, "-").toLowerCase()}`;
}

/**
 * Reactive font loader. Given a font accessor, resolves server-hosted fonts,
 * loads @font-face CSS, measures advance ratio, and waits for font readiness.
 *
 * Returns reactive accessors for the resolved font family, loading state,
 * and advance ratio (if the server provides metrics).
 */
export function createFontLoader(
  font: () => string,
  defaultFont: string,
): {
  resolvedFont: () => string;
  fontLoading: () => boolean;
  advanceRatio: () => number | undefined;
} {
  const [resolvedFont, setResolvedFont] = createSignal(font());
  const [fontLoading, setFontLoading] = createSignal(false);
  const [advanceRatio, setAdvanceRatio] = createSignal<number | undefined>(
    undefined,
  );

  let requestVersion = 0;

  createEffect(() => {
    const requestedFont = font().trim() || defaultFont;
    const families = splitFontFamilies(requestedFont).filter(
      (family) => !CSS_GENERIC.has(family.toLowerCase()),
    );
    const version = ++requestVersion;
    let cancelled = false;

    if (families.length === 0) {
      setResolvedFont(requestedFont);
      setAdvanceRatio(undefined);
      setFontLoading(false);
      onCleanup(() => {
        cancelled = true;
      });
      return;
    }

    setFontLoading(true);

    const load = async () => {
      let ratio: number | undefined;
      // A blit server serves the face and its metrics; a static host does
      // not, and asking produces a pair of 404s per family. Where the font
      // came with the page (an embedder's own @font-face) it is already
      // declared, so `document.fonts.load` below is the whole job.
      const served = shellCapabilities().serverRoutes;
      for (const family of families) {
        if (cancelled || version !== requestVersion) return;

        const loadSpec = `16px "${family}"`;
        const id = fontStyleId(family);
        if (served && !document.getElementById(id)) {
          try {
            const response = await fetch(
              `${basePath}font/${encodeURIComponent(family)}`,
            );
            if (response.ok) {
              const css = await response.text();
              if (cancelled || version !== requestVersion) return;
              if (!document.getElementById(id)) {
                const style = document.createElement("style");
                style.id = id;
                style.textContent = css;
                document.head.appendChild(style);
              }
            }
          } catch {}
        }

        if (served && ratio == null) {
          try {
            const metricsResp = await fetch(
              `${basePath}font-metrics/${encodeURIComponent(family)}`,
            );
            if (metricsResp.ok) {
              const json = await metricsResp.json();
              if (typeof json.advanceRatio === "number")
                ratio = json.advanceRatio;
            }
          } catch {}
        }

        try {
          if (typeof document.fonts?.load === "function") {
            await document.fonts.load(loadSpec, "BESbswy");
          } else if (document.fonts?.ready) {
            await document.fonts.ready;
          }
        } catch {}
      }

      if (cancelled || version !== requestVersion) return;
      setAdvanceRatio(ratio);
      setResolvedFont(requestedFont);
      setFontLoading(false);
    };

    void load();
    onCleanup(() => {
      cancelled = true;
    });
  });

  return { resolvedFont, fontLoading, advanceRatio };
}
