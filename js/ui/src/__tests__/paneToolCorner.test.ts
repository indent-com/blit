import { describe, expect, it } from "vitest";
import { paneToolCornerAtPoint } from "../paneToolCorner";

const RECT = { left: 100, right: 500, top: 50, bottom: 350 };

describe("paneToolCornerAtPoint", () => {
  it.each([
    [112, 62, "top-left"],
    [488, 62, "top-right"],
    [112, 338, "bottom-left"],
    [488, 338, "bottom-right"],
    [250, 125, "top-left"],
    [350, 275, "bottom-right"],
  ] as const)("maps (%i, %i) to %s", (x, y, corner) => {
    expect(paneToolCornerAtPoint(RECT, x, y)).toBe(corner);
  });

  it("ignores points outside the pane", () => {
    expect(paneToolCornerAtPoint(RECT, 90, 60)).toBeNull();
    expect(paneToolCornerAtPoint(RECT, 300, 360)).toBeNull();
  });
});
