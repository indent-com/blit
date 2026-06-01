export interface CellMetrics {
  /** CSS pixel width. */
  w: number;
  /** CSS pixel height. */
  h: number;
  /** Device pixel width (integer). */
  pw: number;
  /** Device pixel height (integer). */
  ph: number;
}

/**
 * Measure the dimensions of a single monospace cell by rendering 'M'
 * into a hidden span, then snapping to device pixel boundaries.
 */
export const CSS_GENERIC = new Set([
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

/** Quote font families for CSS font shorthand (canvas ctx.font). */
export function cssFontFamily(family: string): string {
  return family
    .split(",")
    .map((f) => {
      f = f.trim();
      if (CSS_GENERIC.has(f.toLowerCase())) return f;
      if (f.startsWith('"') || f.startsWith("'")) return f;
      return `'${f}'`;
    })
    .join(", ");
}

export function measureCell(
  fontFamily: string,
  fontSize: number,
  dpr?: number,
  advanceRatio?: number,
): CellMetrics {
  const canvas = document.createElement("canvas");
  const ctx = canvas.getContext("2d")!;
  ctx.font = `${fontSize}px ${cssFontFamily(fontFamily)}`;

  let w: number;
  if (advanceRatio != null) {
    // Use the font table's advance width, matching how native terminals compute cell width.
    w = advanceRatio * fontSize;
  } else {
    const sample = "MMMMMMMMMM";
    const metrics = ctx.measureText(sample);
    w = metrics.width / sample.length;
  }
  if (!Number.isFinite(w) || w <= 0) {
    // Last-ditch fallback for browsers returning incomplete TextMetrics.
    w = fontSize * 0.6;
  }

  const hMetrics = ctx.measureText("Mg");
  const ascent =
    hMetrics.fontBoundingBoxAscent ??
    hMetrics.actualBoundingBoxAscent ??
    fontSize;
  const descent =
    hMetrics.fontBoundingBoxDescent ??
    hMetrics.actualBoundingBoxDescent ??
    fontSize * 0.2;
  let h = ascent + descent;
  if (!Number.isFinite(h) || h <= 0) {
    // iPadOS/Safari versions without fontBoundingBox* used to produce NaN
    // here, which collapses terminal canvases and leaves them textless.
    h = fontSize * 1.2;
  }

  const d = dpr ?? (window.devicePixelRatio || 1);
  const pw = Math.max(1, Math.round(w * d));
  const ph = Math.max(1, Math.round(h * d));
  return { w: pw / d, h: ph / d, pw, ph };
}
