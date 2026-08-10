import { describe, expect, it } from "vitest";

import { NetSampleRing, RenderSampleRing } from "../createMetrics";

describe("typed metric rings", () => {
  it("keeps render fields aligned after wrapping", () => {
    const samples = new RenderSampleRing(3);
    for (const value of [1, 2, 3, 4, 5]) samples.push(value * 10, value);

    expect(samples.length).toBe(3);
    expect(Array.from({ length: 3 }, (_, i) => samples.time(i))).toEqual([
      30, 40, 50,
    ]);
    expect(Array.from({ length: 3 }, (_, i) => samples.duration(i))).toEqual([
      3, 4, 5,
    ]);
    expect(samples.time(-1)).toBeNaN();
    expect(samples.duration(3)).toBeNaN();
  });

  it("keeps network fields aligned after wrapping", () => {
    const samples = new NetSampleRing(2);
    samples.push(10, 100, true);
    samples.push(20, 200, false);
    samples.push(30, 300, true);

    expect(samples.length).toBe(2);
    expect(samples.time(0)).toBe(20);
    expect(samples.bytes(0)).toBe(200);
    expect(samples.isRx(0)).toBe(false);
    expect(samples.time(1)).toBe(30);
    expect(samples.bytes(1)).toBe(300);
    expect(samples.isRx(1)).toBe(true);
  });
});
