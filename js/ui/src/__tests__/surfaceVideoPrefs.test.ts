import { describe, expect, it } from "vitest";
import {
  CUSTOM_WIRE_MAX,
  CUSTOM_WIRE_MIN,
  detailWord,
  effortWord,
  flipWire,
  isCustomWire,
} from "../surfaceVideoPrefs";

// The media panel shows the detail slider running low-to-high, matching the
// preset row above it, while the byte it writes is an AV1 quantizer running
// the other way. That mirroring must never push a value out of the custom
// range: bytes 1-4 are the named presets and 5-9 read as the server default,
// so an off-by-one at either end would silently request the *worst* preset
// from the slider's best-looking position.

describe("flipWire", () => {
  it("keeps every displayable position inside the custom range", () => {
    for (let shown = CUSTOM_WIRE_MIN; shown <= CUSTOM_WIRE_MAX; shown++) {
      const stored = flipWire(shown);
      expect(stored).toBeGreaterThanOrEqual(CUSTOM_WIRE_MIN);
      expect(stored).toBeLessThanOrEqual(CUSTOM_WIRE_MAX);
      expect(isCustomWire(stored)).toBe(true);
    }
  });

  it("is its own inverse", () => {
    for (let shown = CUSTOM_WIRE_MIN; shown <= CUSTOM_WIRE_MAX; shown++) {
      expect(flipWire(flipWire(shown))).toBe(shown);
    }
  });

  it("puts the best quantizer at the high end of the slider", () => {
    // Dragging right must ask for *more* detail, i.e. a lower quantizer.
    expect(flipWire(CUSTOM_WIRE_MAX)).toBe(CUSTOM_WIRE_MIN);
    expect(flipWire(CUSTOM_WIRE_MIN)).toBe(CUSTOM_WIRE_MAX);
    expect(flipWire(200)).toBeLessThan(flipWire(100));
  });
});

describe("isCustomWire", () => {
  it("rejects the preset and reserved bytes", () => {
    // 0 = server default, 1-4 = presets, 5-9 = reserved.
    for (const value of [0, 1, 2, 3, 4, 5, 9]) {
      expect(isCustomWire(value)).toBe(false);
    }
    expect(isCustomWire(10)).toBe(true);
    expect(isCustomWire(255)).toBe(true);
  });
});

describe("detailWord", () => {
  it("agrees with the preset each quantizer belongs to", () => {
    // The server's own preset quantizers: Ultra 1, High 80, Medium 120,
    // Low 180 (crates/server/src/surface_encoder.rs).
    expect(detailWord(1)).toBe("highest");
    expect(detailWord(80)).toBe("high");
    expect(detailWord(120)).toBe("medium");
    expect(detailWord(180)).toBe("low");
    expect(detailWord(255)).toBe("lowest");
  });

  it("never gets better as the quantizer gets worse", () => {
    const rank = ["lowest", "low", "medium", "high", "very high", "highest"];
    let previous = rank.length;
    for (let q = CUSTOM_WIRE_MIN; q <= CUSTOM_WIRE_MAX; q++) {
      const current = rank.indexOf(detailWord(q));
      expect(current).toBeGreaterThanOrEqual(0);
      expect(current).toBeLessThanOrEqual(previous);
      previous = current;
    }
  });
});

describe("effortWord", () => {
  it("runs from most to least as the speed byte rises", () => {
    expect(effortWord(CUSTOM_WIRE_MIN)).toBe("most");
    expect(effortWord(CUSTOM_WIRE_MAX)).toBe("least");
  });

  it("never gains effort as the speed byte rises", () => {
    const rank = ["least", "less", "medium", "more", "most"];
    let previous = rank.length;
    for (let v = CUSTOM_WIRE_MIN; v <= CUSTOM_WIRE_MAX; v++) {
      const current = rank.indexOf(effortWord(v));
      expect(current).toBeGreaterThanOrEqual(0);
      expect(current).toBeLessThanOrEqual(previous);
      previous = current;
    }
  });
});
