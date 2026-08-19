import { describe, it, expect } from "vitest";
import { WheelDetents, WHEEL_DETENT_PX, notchedRows } from "../wheel";

/** A wheel event as a browser would report it; jsdom's WheelEvent drops
 *  deltaMode from the init dict on some versions, so build it by hand. */
function wheel(deltaY: number, deltaMode = 0): WheelEvent {
  return { deltaY, deltaX: 0, deltaMode } as WheelEvent;
}

describe("WheelDetents", () => {
  const CELL_H = 18;
  const ROWS = 40;

  const take = (w: WheelDetents, e: WheelEvent, now: number) =>
    w.take(e, CELL_H, ROWS, now);

  it("reads a Chrome notch as exactly one detent", () => {
    const w = new WheelDetents();
    expect(take(w, wheel(WHEEL_DETENT_PX), 0)).toBe(1);
    expect(take(w, wheel(-WHEEL_DETENT_PX), 10)).toBe(-1);
  });

  it("reads a Firefox line-mode notch as exactly one detent", () => {
    const w = new WheelDetents();
    expect(take(w, wheel(3, 1), 0)).toBe(1);
  });

  it("accumulates trackpad slivers instead of stepping on each one", () => {
    // The bug this replaces: every event, however small, reported a full
    // wheel button press — 60 of them a second on a trackpad.
    const w = new WheelDetents();
    const steps = [];
    for (let i = 0; i < 12; i++) steps.push(take(w, wheel(6), i * 8));
    expect(steps.filter((s) => s !== 0).length).toBeLessThan(steps.length);
    // 72px of travel is 4 lines, so between one and two conventional
    // three-line steps.
    expect(steps.reduce((a, b) => a + b, 0)).toBe(1);
  });

  it("ignores sideways travel", () => {
    const w = new WheelDetents();
    expect(take(w, { deltaY: 0, deltaX: 240, deltaMode: 0 } as WheelEvent, 0));
    expect(
      take(w, { deltaY: 0, deltaX: 240, deltaMode: 0 } as WheelEvent, 10),
    ).toBe(0);
  });

  it("drops carried travel when the gesture reverses", () => {
    const w = new WheelDetents();
    take(w, wheel(30), 0); // half a step down, carried
    // A step up now should need a full step's worth of travel, not be
    // short-changed by the leftover from the other direction.
    expect(take(w, wheel(-30), 10)).toBe(0);
    expect(take(w, wheel(-30), 20)).toBe(-1);
  });

  it("starts fresh after the gesture goes idle", () => {
    const w = new WheelDetents();
    take(w, wheel(30), 0);
    expect(take(w, wheel(30), 5000)).toBe(0);
  });

  it("caps a single absurd event", () => {
    const w = new WheelDetents();
    expect(Math.abs(take(w, wheel(1e9), 0))).toBe(32);
  });

  it("scales a page-mode wheel by the reader's height", () => {
    const w = new WheelDetents();
    // One page of 40 rows is 40 lines, i.e. 13 whole three-line steps.
    expect(take(w, wheel(1, 2), 0)).toBe(13);
  });
});

describe("notchedRows", () => {
  it("moves a whole number of rows, the same for every notch", () => {
    // 120px over a 19px cell is 6.3 rows. Left as pixels the remainder is
    // what the scroll sync jerks back once the gesture settles.
    expect(notchedRows(wheel(WHEEL_DETENT_PX), 19)).toBe(6);
    expect(notchedRows(wheel(-WHEEL_DETENT_PX), 19)).toBe(-6);
  });

  it("keeps the browser's own speed where the cell divides the notch", () => {
    expect(notchedRows(wheel(WHEEL_DETENT_PX), 10)).toBe(12);
    expect(notchedRows(wheel(WHEEL_DETENT_PX), 20)).toBe(6);
  });

  it("gives a coalesced spin the same rows per notch as a single one", () => {
    expect(notchedRows(wheel(WHEEL_DETENT_PX * 3), 19)).toBe(18);
  });

  it("reads a Firefox line-mode notch the same as a Chrome one", () => {
    expect(notchedRows(wheel(3, 1), 19)).toBe(6);
    expect(notchedRows(wheel(-3, 1), 19)).toBe(-6);
  });

  it("leaves a trackpad to the browser", () => {
    // Pixel-precise travel means the fraction it reports, and a continuous
    // gesture settles once rather than once per notch.
    expect(notchedRows(wheel(-4), 19)).toBe(0);
    expect(notchedRows(wheel(-53.5), 19)).toBe(0);
    expect(notchedRows(wheel(-119), 19)).toBe(0);
  });

  it("leaves a sideways swipe and a page-mode wheel alone", () => {
    expect(notchedRows(wheel(0), 19)).toBe(0);
    expect(notchedRows(wheel(1, 2), 19)).toBe(0);
  });

  it("never rounds a notch away, however tall the row", () => {
    expect(notchedRows(wheel(WHEEL_DETENT_PX), 400)).toBe(1);
  });
});
