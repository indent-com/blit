import { createSignal, createEffect, onCleanup } from "solid-js";
import { basePath } from "./storage";

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
      for (const family of families) {
        if (cancelled || version !== requestVersion) return;

        const loadSpec = `16px "${family}"`;
        const id = fontStyleId(family);
        if (!document.getElementById(id)) {
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

        if (ratio == null) {
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
