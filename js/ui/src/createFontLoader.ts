import { createSignal, createEffect, onCleanup } from "solid-js";
import { basePath } from "./storage";
import { shellCapabilities } from "./shellCapabilities";
import {
  forgetFontCss,
  forgetFontMetrics,
  isStale,
  loadFontCss,
  loadFontMetrics,
  saveFontCss,
  saveFontMetrics,
} from "./fontStore";

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

/** Both cache keys carry the base path: one browser origin can face several
 *  servers under different prefixes, each with its own fonts installed. */
function fontUrl(family: string): string {
  return `${basePath}font/${encodeURIComponent(family)}`;
}

function metricsKey(family: string): string {
  return `${basePath}${family}`;
}

function applyFontCss(id: string, css: string): void {
  const existing = document.getElementById(id);
  if (existing) {
    if (existing.textContent !== css) existing.textContent = css;
    return;
  }
  const style = document.createElement("style");
  style.id = id;
  style.textContent = css;
  document.head.appendChild(style);
}

/** The face CSS, or null when the request failed. `false` for "the server
 *  answered, and it does not have this family". */
async function fetchFontCss(url: string): Promise<string | null> {
  try {
    const response = await fetch(url);
    if (response.ok) return await response.text();
  } catch {}
  return null;
}

/** Ask again for a face we already hold, and keep whatever came back.
 *
 * Off the render path by construction: the stored copy is already applied, so
 * this only matters for the next load — either the face changed on the server
 * (a font upgrade) or the family is gone, and holding a copy of a face nobody
 * can name any more is worse than nothing. */
async function refreshFontCss(
  family: string,
  id: string,
  had: string,
): Promise<void> {
  const url = fontUrl(family);
  let response: Response;
  try {
    response = await fetch(url);
  } catch {
    return; // Offline is not a reason to forget the font.
  }
  if (response.status === 404) {
    void forgetFontCss(url);
    forgetFontMetrics(metricsKey(family));
    return;
  }
  if (!response.ok) return;
  const css = await response.text();
  if (css === had) {
    void saveFontCss(url, css); // Same face, fresh timestamp.
    return;
  }
  applyFontCss(id, css);
  void saveFontCss(url, css);
}

async function fetchFontMetrics(family: string): Promise<number | null> {
  try {
    const response = await fetch(
      `${basePath}font-metrics/${encodeURIComponent(family)}`,
    );
    if (!response.ok) return null;
    const json: unknown = await response.json();
    const ratio = (json as { advanceRatio?: unknown }).advanceRatio;
    if (typeof ratio === "number") return ratio;
  } catch {}
  return null;
}

async function refreshFontMetrics(family: string): Promise<void> {
  const ratio = await fetchFontMetrics(family);
  if (ratio != null) saveFontMetrics(metricsKey(family), ratio);
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
    // Cell width comes from the advance ratio, so a remembered one is worth
    // publishing before anything is awaited: the grid gets sized once instead
    // of laid out on a guess and reflowed when storage answers.
    const remembered = families
      .map((family) => loadFontMetrics(metricsKey(family)))
      .find((m) => m != null);
    if (remembered) setAdvanceRatio(remembered.advanceRatio);

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
          const url = fontUrl(family);
          const stored = await loadFontCss(url);
          if (cancelled || version !== requestVersion) return;
          if (stored) {
            applyFontCss(id, stored.css);
            // A day-old copy still draws this frame; the server gets asked
            // again off the critical path, and next load holds the answer.
            if (isStale(stored)) void refreshFontCss(family, id, stored.css);
          } else {
            const css = await fetchFontCss(url);
            if (cancelled || version !== requestVersion) return;
            if (css != null) {
              applyFontCss(id, css);
              void saveFontCss(url, css);
            }
          }
        }

        if (served && ratio == null) {
          const key = metricsKey(family);
          const remembered = loadFontMetrics(key);
          if (remembered) {
            ratio = remembered.advanceRatio;
            if (remembered.stale) void refreshFontMetrics(family);
          } else {
            const fetched = await fetchFontMetrics(family);
            if (cancelled || version !== requestVersion) return;
            if (fetched != null) {
              ratio = fetched;
              saveFontMetrics(key, fetched);
            }
          }
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
