/**
 * Placement for the hidden textarea that terminals and Wayland surfaces use
 * to capture IME composition.
 *
 * The host's IME draws its candidate window against the caret of the focused
 * editable element — which, for a view that paints its own text into a
 * canvas, is a 1px capture textarea that has nothing to do with where the
 * text is going.  Parked in the corner, every composition popup opens in the
 * corner too, far from the words it is composing.  Moving the capture
 * element over the *remote* caret is the whole trick: the IME then places
 * itself as it would in a real text field.
 */

/** A caret, in client coordinates (CSS pixels, as `getBoundingClientRect`). */
export interface CaretRect {
  left: number;
  top: number;
  /** Caret height. The IME opens its candidate window clear of this. */
  height: number;
}

/**
 * The caret of a character grid, in client coordinates.
 *
 * The grid is laid out in CSS pixels (`cell.w`/`cell.h`) but centred inside
 * its canvas by an offset in *device* pixels, which is the one conversion
 * this is here to not get wrong.
 */
export function gridCaretRect(
  canvasOrigin: { left: number; top: number },
  cell: { w: number; h: number; pw: number; ph: number },
  offset: { x: number; y: number },
  col: number,
  row: number,
): CaretRect {
  return {
    left: canvasOrigin.left + (offset.x * cell.w) / cell.pw + col * cell.w,
    top: canvasOrigin.top + (offset.y * cell.h) / cell.ph + row * cell.h,
    height: cell.h,
  };
}

/** The visible part of the layout viewport, in the same client coordinates.
 *  On mobile this shrinks when the software keyboard comes up, which is
 *  exactly the region the capture element has to stay inside. */
function visibleViewport(): {
  left: number;
  top: number;
  width: number;
  height: number;
} {
  const vv = typeof window !== "undefined" ? window.visualViewport : null;
  if (vv) {
    return {
      left: vv.offsetLeft,
      top: vv.offsetTop,
      width: vv.width,
      height: vv.height,
    };
  }
  return {
    left: 0,
    top: 0,
    width: typeof window !== "undefined" ? window.innerWidth : 0,
    height: typeof window !== "undefined" ? window.innerHeight : 0,
  };
}

/**
 * Park `el` over `caret`, or back at the screen's top-left corner when there
 * is no caret to point at (`null`).
 *
 * The corner is the historical resting place and stays the fallback for a
 * reason: an assist target there can never end up under a software keyboard,
 * so iPadOS never pans the page to reveal it.  A caret keeps that property by
 * being clamped into the *visual* viewport, which is the part of the page the
 * keyboard leaves visible.
 *
 * Writes are deduped against the element's own inline style, so the render
 * loop can call this every frame.
 */
export function placeImeTarget(el: HTMLElement, caret: CaretRect | null): void {
  let left = 0;
  let top = 0;
  let height = 1;
  if (caret) {
    const view = visibleViewport();
    height = Math.max(1, Math.min(caret.height, view.height));
    left = Math.min(
      Math.max(caret.left, view.left),
      Math.max(view.left, view.left + view.width - 1),
    );
    top = Math.min(
      Math.max(caret.top, view.top),
      Math.max(view.top, view.top + view.height - height),
    );
  }
  const leftPx = `${Math.round(left)}px`;
  const topPx = `${Math.round(top)}px`;
  const heightPx = `${Math.round(height)}px`;
  if (el.style.left !== leftPx) el.style.left = leftPx;
  if (el.style.top !== topPx) el.style.top = topPx;
  if (el.style.height !== heightPx) el.style.height = heightPx;
}

/** Gap between the caret's line and the chip, in CSS pixels. */
const CHIP_GAP = 4;

/**
 * Park a suggestion chip on the caret's own line, starting at the cursor, so
 * it reads as the continuation of what is being typed.
 *
 * It floats rather than occupying cells: those belong to the app, which may
 * be drawing its own suggestion in them.  When the line has run too close to
 * the right edge for the chip to fit, it drops under the line instead — and
 * under a software keyboard that leaves no room below, above it.
 *
 * Unlike the capture element this one is *seen*, so it is sized by its own
 * content: the caller must have it laid out (non-empty, not `display:none`)
 * before calling, and `size` comes from the element itself.
 */
export function placeChip(
  el: HTMLElement,
  caret: CaretRect,
  size: { width: number; height: number },
): void {
  const view = visibleViewport();
  const w = Math.min(size.width, view.width);
  const h = size.height;
  const viewRight = view.left + view.width;
  const viewBottom = view.top + view.height;

  let left = caret.left;
  // Centred on the caret's line rather than clamped to it: the chip is sized
  // by its own text, and a box forced to the row height clips descenders and
  // full-height CJK — exactly the glyphs a composition is made of.
  let top = caret.top + (caret.height - h) / 2;

  if (left + w > viewRight) {
    // No room on the line: fall back under it, then above it.
    top = caret.top + caret.height + CHIP_GAP;
    if (top + h > viewBottom && caret.top - h - CHIP_GAP >= view.top) {
      top = caret.top - h - CHIP_GAP;
    }
    left = Math.min(
      Math.max(caret.left, view.left),
      Math.max(view.left, viewRight - w),
    );
  }

  top = Math.min(Math.max(top, view.top), Math.max(view.top, viewBottom - h));
  left = Math.min(
    Math.max(left, view.left),
    Math.max(view.left, viewRight - w),
  );

  const leftPx = `${Math.round(left)}px`;
  const topPx = `${Math.round(top)}px`;
  if (el.style.left !== leftPx) el.style.left = leftPx;
  if (el.style.top !== topPx) el.style.top = topPx;
}
