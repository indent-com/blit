import { describe, expect, it } from "vitest";
import type { CameraCodecProbeOutcome } from "@blit-sh/core";
import { cameraCodecUnavailableReason } from "../cameraCodecStatus";

/** Wire codec 1 is H.264 4:2:0, 3 is H.264 4:4:4; a chip covers both. */
const H264 = (1 << 1) | (1 << 3);
const MJPEG = 1 << 0;
const ALL = 31;

const outcomes = (
  entries: [number, CameraCodecProbeOutcome][],
): ReadonlyMap<number, CameraCodecProbeOutcome> => new Map(entries);

describe("cameraCodecUnavailableReason", () => {
  it("says nothing when the format is available on both sides", () => {
    expect(
      cameraCodecUnavailableReason(H264, ALL, ALL, outcomes([])),
    ).toBeNull();
    expect(
      cameraCodecUnavailableReason(MJPEG, MJPEG, ALL, outcomes([])),
    ).toBeNull();
  });

  it("blames the desktop when only the desktop refuses", () => {
    expect(cameraCodecUnavailableReason(H264, ALL, MJPEG, outcomes([]))).toBe(
      "No connected desktop accepts this format.",
    );
  });

  it("reports what the browser's encoder actually did", () => {
    // The case that hid a whole class of bugs: the desktop accepts H.264, the
    // browser probe failed, and the old tooltip could not say which.
    expect(
      cameraCodecUnavailableReason(
        H264,
        MJPEG,
        ALL,
        outcomes([[1, "config-unsupported"]]),
      ),
    ).toBe("This browser cannot encode this format.");
    expect(
      cameraCodecUnavailableReason(
        H264,
        MJPEG,
        ALL,
        outcomes([[1, "no-keyframe"]]),
      ),
    ).toContain("produced no frame in time");
    expect(
      cameraCodecUnavailableReason(
        H264,
        MJPEG,
        ALL,
        outcomes([[1, "no-webcodecs"]]),
      ),
    ).toBe("This browser has no WebCodecs video encoder.");
  });

  it("prefers the more specific complaint across a chip's two chromas", () => {
    // 4:2:0 merely unsupported, 4:4:4 answered with the wrong chroma: the
    // second is the one worth showing, since it names a real mismatch.
    expect(
      cameraCodecUnavailableReason(
        H264,
        MJPEG,
        ALL,
        outcomes([
          [1, "config-unsupported"],
          [3, "wrong-format"],
        ]),
      ),
    ).toContain("different chroma format");
  });

  it("falls back to the browser when the probe recorded nothing", () => {
    expect(cameraCodecUnavailableReason(H264, MJPEG, ALL, outcomes([]))).toBe(
      "This browser cannot encode this format.",
    );
  });

  it("names both sides when neither supports it", () => {
    const reason = cameraCodecUnavailableReason(
      H264,
      MJPEG,
      MJPEG,
      outcomes([[1, "config-unsupported"]]),
    );
    expect(reason).toContain("This browser cannot encode this format.");
    expect(reason).toContain("No connected desktop accepts it either.");
  });
});
