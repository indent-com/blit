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
export function measureCell(fontFamily: string, fontSize: number): CellMetrics {
  const canvas = document.createElement("canvas");
  const ctx = canvas.getContext("2d")!;
  ctx.font = `${fontSize}px ${fontFamily}`;
  const metrics = ctx.measureText("M");
  const w = metrics.width;
  // Use font metrics for accurate height: ascent + descent gives the full
  // glyph extent. This matches how real terminals compute cell height from
  // the font's ascender/descender values.
  const h = metrics.fontBoundingBoxAscent + metrics.fontBoundingBoxDescent;

  const dpr = window.devicePixelRatio || 1;
  const pw = Math.round(w * dpr);
  const ph = Math.round(h * dpr);
  return { w: pw / dpr, h: ph / dpr, pw, ph };
}
