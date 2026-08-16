import { describe, expect, it } from "vitest";

import { CameraRateGovernor, cameraBytesPerSecond } from "../media";

describe("CameraRateGovernor", () => {
  it("starts at full rate", () => {
    expect(new CameraRateGovernor().scale).toBe(1);
  });

  it("backs off while the link is congested", () => {
    const g = new CameraRateGovernor();
    const first = g.observe(true);
    expect(first).toBeLessThan(1);
    expect(g.observe(true)).toBeLessThan(first);
  });

  it("never backs off past the floor, however long the link stays bad", () => {
    const g = new CameraRateGovernor();
    for (let i = 0; i < 200; i++) g.observe(true);
    expect(g.scale).toBe(CameraRateGovernor.MIN_SCALE);
  });

  /** The failure this class is written against: degrade-only. */
  it("recovers once the link stays clear, and reaches full rate again", () => {
    const g = new CameraRateGovernor();
    for (let i = 0; i < 20; i++) g.observe(true);
    const degraded = g.scale;
    expect(degraded).toBe(CameraRateGovernor.MIN_SCALE);

    // Long enough that a governor with a working recovery arm gets all the
    // way home; one that can only hold or degrade never moves at all.
    for (let i = 0; i < 500; i++) g.observe(false);
    expect(g.scale).toBeGreaterThan(degraded);
    expect(g.scale).toBe(CameraRateGovernor.MAX_SCALE);
  });

  it("does not probe upward on the strength of a single clear interval", () => {
    const g = new CameraRateGovernor();
    g.observe(true);
    const degraded = g.scale;
    g.observe(false);
    expect(g.scale).toBe(degraded);
  });

  it("restarts the recovery count when congestion returns", () => {
    const g = new CameraRateGovernor();
    g.observe(true);
    const degraded = g.scale;
    // One short of a probe, then congestion, then one short again: a
    // governor that let the count survive would climb on a link that is
    // still failing every few intervals.
    for (let i = 0; i < CameraRateGovernor.RECOVER_AFTER - 1; i++) {
      g.observe(false);
    }
    g.observe(true);
    for (let i = 0; i < CameraRateGovernor.RECOVER_AFTER - 1; i++) {
      g.observe(false);
    }
    expect(g.scale).toBeLessThan(degraded);
  });

  it("never exceeds full rate no matter how long the link is clear", () => {
    const g = new CameraRateGovernor();
    for (let i = 0; i < 1000; i++) g.observe(false);
    expect(g.scale).toBe(CameraRateGovernor.MAX_SCALE);
  });

  it("resets to full rate for a new lease", () => {
    const g = new CameraRateGovernor();
    for (let i = 0; i < 10; i++) g.observe(true);
    g.reset();
    expect(g.scale).toBe(1);
  });
});

describe("cameraBytesPerSecond", () => {
  /** The server sizes the lease window from the same arithmetic. */
  it("matches the encoder's configured bitrate for 720p30 H.264", () => {
    // 1280*720*30*0.11 bits/s / 8 == 380_160 B/s
    expect(cameraBytesPerSecond(1, 1280, 720, 30)).toBeCloseTo(380_160, 0);
  });

  it("charges Motion JPEG far more, because every frame is a whole one", () => {
    expect(cameraBytesPerSecond(0, 1280, 720, 30)).toBeGreaterThan(
      cameraBytesPerSecond(1, 1280, 720, 30) * 5,
    );
  });

  it("scales with the quality multiplier", () => {
    const full = cameraBytesPerSecond(1, 1280, 720, 30, 1);
    expect(cameraBytesPerSecond(1, 1280, 720, 30, 0.5)).toBeCloseTo(
      full / 2,
      0,
    );
  });

  it("is zero for a lease with no negotiated cadence", () => {
    expect(cameraBytesPerSecond(1, 0, 0, 0)).toBe(0);
  });
});
