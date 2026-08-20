import { describe, it, expect } from "vitest";
import { cardAspectRatio, surfaceCardSignature } from "../surfaceAspect";

function dims(
  width: number,
  height: number,
  logicalWidth = 0,
  logicalHeight = 0,
) {
  return { width, height, logicalWidth, logicalHeight };
}

describe("cardAspectRatio", () => {
  it("follows the window's logical shape", () => {
    expect(cardAspectRatio(dims(3840, 2160, 1280, 720))).toEqual({
      "aspect-ratio": "1280 / 720",
    });
  });

  it("ignores a composite whose even-grid rounding skews the ratio", () => {
    // 1281x721 logical at 1x composites to an even 1280x720: following the
    // composite would put the card a fraction of a pixel out on both axes.
    expect(cardAspectRatio(dims(1280, 720, 1281, 721))).toEqual({
      "aspect-ratio": "1281 / 721",
    });
  });

  it("holds its shape when another viewer's DPI moves the composite", () => {
    // The server mediates at the highest scale any viewer asked for, so a 3x
    // pane joining triples the composite for a window that never changed.
    const at1x = cardAspectRatio(dims(1280, 720, 1280, 720));
    const at3x = cardAspectRatio(dims(3840, 2160, 1280, 720));
    expect(at3x).toEqual(at1x);
  });

  it("falls back to the composite when no logical size is reported", () => {
    // A server too old to send one, which is what this used throughout before.
    expect(cardAspectRatio(dims(800, 600))).toEqual({
      "aspect-ratio": "800 / 600",
    });
  });

  it("emits nothing at all rather than a degenerate ratio", () => {
    // Before the first SURFACE_RESIZED every dimension is 0; the placeholder
    // canvas lays the card out for that one frame.
    expect(cardAspectRatio(dims(0, 0))).toEqual({});
    expect(cardAspectRatio(dims(0, 0, 0, 0))).toEqual({});
    // A half-known size is not a ratio either.
    expect(cardAspectRatio(dims(0, 600, 0, 720))).toEqual({});
  });
});

describe("surfaceCardSignature", () => {
  const base = { ...dims(1280, 720, 1280, 720), title: "vim", appId: "term" };

  it("changes when the logical size does", () => {
    // The whole point: Solid does not track property access on plain objects,
    // so a field missing from the signature is a card that keeps a stale shape
    // forever. This is the field the card actually renders.
    const moved = { ...base, logicalWidth: 1600 };
    expect(surfaceCardSignature(moved)).not.toBe(surfaceCardSignature(base));
  });

  it("changes when the composite size does", () => {
    const rescaled = { ...base, width: 3840, height: 2160 };
    expect(surfaceCardSignature(rescaled)).not.toBe(surfaceCardSignature(base));
  });

  it("changes on title and app id", () => {
    expect(surfaceCardSignature({ ...base, title: "less" })).not.toBe(
      surfaceCardSignature(base),
    );
    expect(surfaceCardSignature({ ...base, appId: "editor" })).not.toBe(
      surfaceCardSignature(base),
    );
  });

  it("changes when the server-stamped origin arrives", () => {
    const stamped = {
      ...base,
      origin: {
        sandboxEngine: "wayland",
        appId: "muster-e74fc019056aae07",
        instanceId: "7",
      },
    };
    expect(surfaceCardSignature(stamped)).not.toBe(surfaceCardSignature(base));
  });

  it("is stable when nothing the card reads moved", () => {
    expect(surfaceCardSignature({ ...base })).toBe(surfaceCardSignature(base));
  });

  it("does not confuse a field boundary for a value", () => {
    // Joined on NUL rather than concatenated, so a title ending where an app id
    // begins cannot collide.
    const a = { ...base, title: "a", appId: "bc" };
    const b = { ...base, title: "ab", appId: "c" };
    expect(surfaceCardSignature(a)).not.toBe(surfaceCardSignature(b));
  });
});
