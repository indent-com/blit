/**
 * Wheel input, in the units the rest of the stack wants.
 *
 * A DOM wheel event describes travel, not intent: the same flick arrives as
 * one 120px jump from a notched mouse, a stream of 4px slivers from a
 * trackpad, or three "lines" from Firefox.  Anything downstream that counts
 * discrete steps has to put those back together itself.
 */

export const WHEEL_MODE_LINE = 1;
export const WHEEL_MODE_PAGE = 2;
/** CSS pixels per line when a browser reports a wheel in line mode
 *  (Firefox does, for notched mice). Matches the default line box. */
export const WHEEL_LINE_PX = 16;
/** Lines a wheel notch conventionally travels, so line-mode deltas can be
 *  turned back into detents. */
export const WHEEL_LINES_PER_DETENT = 3;
/** CSS pixels per detent for browsers that report notched wheels in pixel
 *  mode on a whole-detent grid (Chrome and Edge on Windows and Linux).
 *  macOS reports a notch as a fraction of this and lets its own scroll
 *  acceleration vary it, so a wheel there is not recognisable by size. */
export const WHEEL_DETENT_PX = 120;
/**
 * Idle gap that ends a scroll sequence.
 *
 * Long enough to bridge the frame cadence of a macOS momentum tail so one
 * flick stays one gesture: the source is latched for the length of a
 * sequence, and a tail split in two could have its second half reread as
 * a notched wheel. Short enough that the next scroll starts fresh.
 *
 * A touch drag doesn't wait for it — `touchend` ends that sequence at the
 * moment the finger leaves the glass.
 */
export const SCROLL_STOP_MS = 280;

/** Reports one event may produce, so a device with an absurd delta (or a
 *  page-mode wheel on a very tall pane) can't flood the PTY. */
const MAX_DETENTS_PER_EVENT = 32;

/**
 * Whole rows a notched wheel should travel, or 0 for anything that is not a
 * notched wheel and should keep the browser's own scrolling.
 *
 * A notch is 120 CSS px whatever the font is, so left to the browser it lands
 * mid-row.  A terminal can only show whole rows, so the offset that position
 * maps to is rounded, and the render loop writes the rounding back to
 * `scrollTop` once the gesture settles — a jerk of up to half a row, in
 * whichever direction the remainder fell, arriving as late as the next cursor
 * blink.  Every notch leaves a different remainder, so the jerks alternate:
 * at a 19px cell, twelve notches moved 6 or 7 rows apiece and snapped back by
 * +6, -7, -1, +5, -8, -2, +4, -9, -3, +3, +9, -4 px.
 *
 * Rounding the *travel* instead keeps the surface on the row grid, so there is
 * no remainder to write back and every notch moves the same distance.  The
 * distance is still the notch's own 120px worth of rows, so the wheel keeps
 * the speed the browser was giving it.
 *
 * Pixel-precise devices are deliberately left alone: a trackpad means the
 * fraction it reports, and being continuous it settles once per gesture rather
 * than once per notch.  macOS varies a notch's size with its own scroll
 * acceleration, which is why a wheel there is not recognisable by size and
 * falls here too.
 */
export function notchedRows(e: WheelEvent, rowHeightPx: number): number {
  if (!(rowHeightPx > 0)) return 0;
  const dy = e.deltaY;
  if (!dy || !Number.isFinite(dy)) return 0;
  const rowsPerNotch = Math.max(1, Math.round(WHEEL_DETENT_PX / rowHeightPx));
  // Firefox reports a notched wheel in lines of its own line box; Chrome and
  // Edge report it on a whole-detent pixel grid.  Anything else is travel.
  const notches =
    e.deltaMode === WHEEL_MODE_LINE
      ? dy / WHEEL_LINES_PER_DETENT
      : e.deltaMode === 0
        ? dy / WHEEL_DETENT_PX
        : 0;
  if (!Number.isInteger(notches) || notches === 0) return 0;
  return notches * rowsPerNotch;
}

/**
 * Accumulates wheel travel into whole detents.
 *
 * One detent is what an app reading the mouse expects per wheel report —
 * three lines, by the convention every terminal app follows.  Sending one
 * report per DOM event instead makes a trackpad, which emits an event per
 * frame, scroll roughly twenty times too fast; sending none until a whole
 * detent has accumulated makes a notched wheel feel dead.  Carrying the
 * fraction between events of the same gesture does both.
 */
export class WheelDetents {
  private accum = 0;
  private lastAt = 0;

  /**
   * Whole detents completed by this event, sign following `deltaY`
   * (negative = away from the user).  `lineHeightPx` and `pageLines` are
   * the reader's own geometry, which is what "a line" and "a page" mean to
   * the app being scrolled.
   */
  take(
    e: WheelEvent,
    lineHeightPx: number,
    pageLines: number,
    now: number,
  ): number {
    if (now - this.lastAt > SCROLL_STOP_MS) this.accum = 0;
    this.lastAt = now;
    const detents = this.detentsFor(e, lineHeightPx, pageLines);
    if (detents === 0) return 0;
    // A reversal is a new intent, not a continuation of the last one.
    if (this.accum !== 0 && Math.sign(detents) !== Math.sign(this.accum)) {
      this.accum = 0;
    }
    this.accum += detents;
    const whole = Math.trunc(this.accum);
    this.accum -= whole;
    if (whole === 0) return 0; // never -0, which reads badly at call sites
    if (Math.abs(whole) <= MAX_DETENTS_PER_EVENT) return whole;
    this.accum = 0;
    return Math.sign(whole) * MAX_DETENTS_PER_EVENT;
  }

  reset(): void {
    this.accum = 0;
    this.lastAt = 0;
  }

  private detentsFor(
    e: WheelEvent,
    lineHeightPx: number,
    pageLines: number,
  ): number {
    const dy = e.deltaY;
    if (dy === 0 || !Number.isFinite(dy)) return 0;
    if (e.deltaMode === WHEEL_MODE_LINE) return dy / WHEEL_LINES_PER_DETENT;
    if (e.deltaMode === WHEEL_MODE_PAGE) {
      return (dy * Math.max(1, pageLines)) / WHEEL_LINES_PER_DETENT;
    }
    // Pixel mode. A whole-detent delta is a notched wheel reported the way
    // Chrome does it, and taking it as one detent keeps a notch a notch
    // whatever the font size. Anything else is pixel-precise travel, which
    // only means something against the reader's line height.
    if (Math.abs(dy) % WHEEL_DETENT_PX === 0) return dy / WHEEL_DETENT_PX;
    const lineH = lineHeightPx > 0 ? lineHeightPx : WHEEL_LINE_PX;
    return dy / (lineH * WHEEL_LINES_PER_DETENT);
  }
}
