import { describe, expect, it } from "vitest";
import {
  formatClientAge,
  formatClientBandwidth,
  formatClientSubscription,
  formatSurfaceViewSize,
  formatTerminalViewSize,
} from "../clientDisplay";

describe("client subscription sizes", () => {
  it("formats terminal dimensions as columns by rows", () => {
    expect(formatTerminalViewSize(120, 40)).toBe("120×40");
    expect(formatTerminalViewSize(null, null)).toBe("size not reported");
  });

  it("formats surface dimensions and fractional scale", () => {
    expect(formatSurfaceViewSize(1920, 1080, 120)).toBe("1920×1080 @ 1×");
    expect(formatSurfaceViewSize(1280, 720, 180)).toBe("1280×720 @ 1.5×");
    expect(formatSurfaceViewSize(800, 600, null)).toBe("800×600");
    expect(formatSurfaceViewSize(null, null, null)).toBe("size not reported");
  });

  it("keeps trailing zeros that belong to a scale's integer part", () => {
    // Trimming a trailing "0" after stripping ".00" turned 10× into 1×.
    expect(formatSurfaceViewSize(640, 480, 1200)).toBe("640×480 @ 10×");
    expect(formatSurfaceViewSize(640, 480, 2400)).toBe("640×480 @ 20×");
    // Fractional zeros should still go.
    expect(formatSurfaceViewSize(640, 480, 132)).toBe("640×480 @ 1.1×");
    expect(formatSurfaceViewSize(640, 480, 240)).toBe("640×480 @ 2×");
    // Sub-1× scales round to 2dp rather than disappearing.
    expect(formatSurfaceViewSize(640, 480, 100)).toBe("640×480 @ 0.83×");
  });

  it("formats client age and outbound bandwidth", () => {
    expect(formatClientAge(45)).toBe("45s");
    expect(formatClientAge(125)).toBe("2m 5s");
    expect(formatClientAge(7_380)).toBe("2h 3m");
    expect(formatClientAge(183_600)).toBe("2d 3h");
    expect(formatClientBandwidth(0)).toBe("0 B/s");
    expect(formatClientBandwidth(1_500)).toBe("1.5 kB/s");
    expect(formatClientBandwidth(1_500_000)).toBe("1.5 MB/s");
  });

  it("labels every auxiliary subscription family", () => {
    expect(formatClientSubscription(1, 0)).toBe("Audio");
    expect(formatClientSubscription(2, 3)).toBe("Filesystem #3");
    expect(formatClientSubscription(3, 4)).toBe("Git #4");
    expect(formatClientSubscription(4, 5)).toBe("LSP #5");
    expect(formatClientSubscription(5, 6)).toBe("KV #6");
    expect(formatClientSubscription(6, 7)).toBe("Network #7");
    expect(formatClientSubscription(99, 8)).toBe("Unknown 99 #8");
  });
});
