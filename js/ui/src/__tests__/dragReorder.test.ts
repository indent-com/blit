import { describe, expect, it } from "vitest";
import { gapAt, reorderTo, type RowExtent } from "../dragReorder";

/** Three 20px rows stacked from y=100, as the overlays lay them out. */
const ROWS: RowExtent[] = [
  { top: 100, bottom: 120 },
  { top: 120, bottom: 140 },
  { top: 140, bottom: 160 },
];

describe("gapAt", () => {
  it("returns 0 above the first row's midpoint", () => {
    expect(gapAt(0, ROWS)).toBe(0);
    expect(gapAt(100, ROWS)).toBe(0);
    expect(gapAt(109, ROWS)).toBe(0);
  });

  it("moves to the gap below a row once its midpoint is crossed", () => {
    expect(gapAt(110, ROWS)).toBe(1);
    expect(gapAt(129, ROWS)).toBe(1);
    expect(gapAt(130, ROWS)).toBe(2);
    expect(gapAt(149, ROWS)).toBe(2);
    expect(gapAt(150, ROWS)).toBe(3);
  });

  it("clamps past the last row to the trailing gap", () => {
    expect(gapAt(1e6, ROWS)).toBe(3);
  });

  it("is monotonic across unequal heights and inter-row space", () => {
    const uneven: RowExtent[] = [
      { top: 0, bottom: 10 },
      { top: 30, bottom: 90 },
      { top: 100, bottom: 104 },
    ];
    let prev = -1;
    for (let y = -20; y <= 140; y++) {
      const gap = gapAt(y, uneven);
      expect(gap).toBeGreaterThanOrEqual(prev);
      prev = gap;
    }
    expect(prev).toBe(3);
  });

  it("has no gap to pick in an empty list", () => {
    expect(gapAt(42, [])).toBe(0);
  });
});

describe("reorderTo", () => {
  const items = ["a", "b", "c", "d"];

  it("moves an entry down, accounting for its own removal", () => {
    // Gap 3 is "between c and d"; a lands after c, not after d.
    expect(reorderTo(items, 0, 3)).toEqual(["b", "c", "a", "d"]);
    expect(reorderTo(items, 0, 4)).toEqual(["b", "c", "d", "a"]);
  });

  it("moves an entry up", () => {
    expect(reorderTo(items, 3, 0)).toEqual(["d", "a", "b", "c"]);
    expect(reorderTo(items, 2, 1)).toEqual(["a", "c", "b", "d"]);
  });

  it("rejects drops on either side of the source as no-ops", () => {
    expect(reorderTo(items, 1, 1)).toBeNull();
    expect(reorderTo(items, 1, 2)).toBeNull();
    expect(reorderTo(items, 0, 0)).toBeNull();
    expect(reorderTo(items, 3, 4)).toBeNull();
  });

  it("rejects out-of-range indices", () => {
    expect(reorderTo(items, -1, 2)).toBeNull();
    expect(reorderTo(items, 4, 0)).toBeNull();
    expect(reorderTo(items, 0, 5)).toBeNull();
    expect(reorderTo([], 0, 0)).toBeNull();
  });

  it("leaves the input untouched", () => {
    const original = [...items];
    reorderTo(items, 0, 3);
    expect(items).toEqual(original);
  });
});
