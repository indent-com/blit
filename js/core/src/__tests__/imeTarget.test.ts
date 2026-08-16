import { describe, it, expect, afterEach, vi } from "vitest";
import { gridCaretRect, placeChip, placeImeTarget } from "../imeTarget";

/** Pretend the software keyboard has taken the bottom of the screen. */
function visualViewport(width: number, height: number, top = 0): void {
  vi.stubGlobal("window", {
    innerWidth: 1000,
    innerHeight: 1000,
    visualViewport: { offsetLeft: 0, offsetTop: top, width, height },
  });
}

afterEach(() => {
  vi.unstubAllGlobals();
});

describe("placeImeTarget", () => {
  it("parks in the corner when there is no caret to point at", () => {
    visualViewport(1000, 1000);
    const el = document.createElement("textarea");
    placeImeTarget(el, { left: 200, top: 300, height: 18 });
    placeImeTarget(el, null);
    expect([el.style.left, el.style.top, el.style.height]).toEqual([
      "0px",
      "0px",
      "1px",
    ]);
  });

  it("takes the caret's height, so the popup clears the line", () => {
    visualViewport(1000, 1000);
    const el = document.createElement("textarea");
    placeImeTarget(el, { left: 200.4, top: 300.6, height: 18 });
    expect([el.style.left, el.style.top, el.style.height]).toEqual([
      "200px",
      "301px",
      "18px",
    ]);
  });

  it("keeps the target inside the visible viewport", () => {
    // The software keyboard is up: the visual viewport is the top 400px, and
    // a capture element below that would make iOS pan the page to reveal it.
    visualViewport(1000, 400);
    const el = document.createElement("textarea");
    placeImeTarget(el, { left: 1200, top: 900, height: 20 });
    expect([el.style.left, el.style.top]).toEqual(["999px", "380px"]);
  });

  it("does not write styles that are already right", () => {
    visualViewport(1000, 1000);
    const el = document.createElement("textarea");
    placeImeTarget(el, { left: 40, top: 60, height: 12 });
    // The render loop calls this every frame; a repeat must cost nothing.
    const writes: string[] = [];
    const style = new Proxy(el.style, {
      set(target, prop, value) {
        writes.push(String(prop));
        return Reflect.set(target, prop, value);
      },
    });
    Object.defineProperty(el, "style", { value: style });
    placeImeTarget(el, { left: 40, top: 60, height: 12 });
    expect(writes).toEqual([]);
  });
});

describe("gridCaretRect", () => {
  it("converts the device-pixel grid offset into CSS pixels", () => {
    // A 2x display: cells are 8x17 CSS px, 16x34 device px, and the grid is
    // centred by 20x10 *device* px inside the canvas.
    const caret = gridCaretRect(
      { left: 100, top: 50 },
      { w: 8, h: 17, pw: 16, ph: 34 },
      { x: 20, y: 10 },
      3,
      2,
    );
    expect(caret).toEqual({
      left: 100 + 10 + 24,
      top: 50 + 5 + 34,
      height: 17,
    });
  });
});

describe("placeChip", () => {
  const chip = () => document.createElement("div");

  it("continues the line: same row, starting at the cursor", () => {
    visualViewport(1000, 1000);
    const el = chip();
    placeChip(
      el,
      { left: 200, top: 300, height: 18 },
      { width: 120, height: 18 },
    );
    expect([el.style.left, el.style.top]).toEqual(["200px", "300px"]);
  });

  it("centres a chip taller than the row instead of clipping it", () => {
    // The chip is sized by its own text — a composition's glyphs are taller
    // than a latin row — so it straddles the line rather than being cut to
    // fit it.
    visualViewport(1000, 1000);
    const el = chip();
    placeChip(
      el,
      { left: 200, top: 300, height: 18 },
      { width: 120, height: 26 },
    );
    expect(el.style.top).toBe("296px");
  });

  it("drops under the line when it would run off the right edge", () => {
    visualViewport(1000, 1000);
    const el = chip();
    placeChip(
      el,
      { left: 960, top: 300, height: 18 },
      { width: 120, height: 18 },
    );
    // Not 960: on the row it would leave the viewport, so it goes below and
    // slides back inside.
    expect([el.style.left, el.style.top]).toEqual(["880px", "322px"]);
  });

  it("flips above the line when the keyboard has taken the bottom", () => {
    visualViewport(1000, 340);
    const el = chip();
    placeChip(
      el,
      { left: 960, top: 300, height: 18 },
      { width: 120, height: 22 },
    );
    // Off the right edge → below (322), which is past the visible bottom →
    // above the caret instead.
    expect(el.style.top).toBe("274px");
  });

  it("lands inside even when there is room neither below nor above", () => {
    // Degenerate but reachable — a tiny visual viewport mid keyboard
    // animation, with the line too close to the right edge to stay on it.
    // It must not end up at a negative offset.
    visualViewport(1000, 30);
    const el = chip();
    placeChip(
      el,
      { left: 960, top: 0, height: 18 },
      { width: 120, height: 22 },
    );
    expect(el.style.top).toBe("8px");
    expect(el.style.left).toBe("880px");
  });
});
