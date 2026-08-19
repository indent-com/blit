import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { clampZoom, driveSurfaceResize } from "../surfaceResize";
import type { SurfaceResizeTarget, SurfaceZoom } from "../surfaceResize";

/** Records what a `BlitSurfaceCanvas` would have been told. */
function fakeTarget() {
  const displaySizes: (number | null)[][] = [];
  const resizes: number[][] = [];
  const target: SurfaceResizeTarget = {
    setDisplaySize(width, height, scale120, cssScale120) {
      displaySizes.push([
        width,
        height ?? null,
        scale120 ?? null,
        cssScale120 ?? null,
      ]);
    },
    requestResize(width, height, scale120) {
      resizes.push([width, height, scale120]);
    },
  };
  return { target, displaySizes, resizes };
}

function container(width: number, height: number): HTMLElement {
  const el = document.createElement("div");
  el.getBoundingClientRect = () =>
    ({
      width,
      height,
      left: 0,
      top: 0,
      right: width,
      bottom: height,
    }) as DOMRect;
  return el;
}

describe("clampZoom", () => {
  it("falls back to 1 for anything that is not a usable factor", () => {
    for (const bad of [undefined, NaN, Infinity, 0, -2]) {
      expect(clampZoom(bad as number | undefined)).toBe(1);
    }
  });

  it("clamps to a range both ends of the stack can lay out", () => {
    expect(clampZoom(0.1)).toBe(0.25);
    expect(clampZoom(9)).toBe(4);
    expect(clampZoom(1.25)).toBe(1.25);
  });
});

describe("driveSurfaceResize", () => {
  let callbacks: ResizeObserverCallback[] = [];
  const disconnect = vi.fn();

  /** Deliver a box to every live observer, in CSS pixels only — the shape a
   *  browser without `devicePixelContentBoxSize` reports. */
  function resizeTo(width: number, height: number): void {
    const entry = { contentRect: { width, height } } as ResizeObserverEntry;
    for (const cb of callbacks) cb([entry], null as never);
  }

  beforeEach(() => {
    callbacks = [];
    disconnect.mockClear();
    vi.stubGlobal("devicePixelRatio", 1);
    vi.stubGlobal(
      "ResizeObserver",
      class {
        constructor(cb: ResizeObserverCallback) {
          callbacks.push(cb);
        }
        observe = vi.fn();
        disconnect = disconnect;
      },
    );
    vi.useFakeTimers();
  });

  afterEach(() => {
    vi.useRealTimers();
    vi.unstubAllGlobals();
  });

  it("reports the container's box as a display size on the way up", () => {
    const { target, displaySizes } = fakeTarget();
    driveSurfaceResize(target, container(800, 600));
    // A view that never reports one takes no input at all and is served a
    // thumbnail-grade stream, so this is the whole point of the driver.
    expect(displaySizes).toEqual([[800, 600, 120, 120]]);
  });

  it("rounds each extent down to even", () => {
    const { target, displaySizes } = fakeTarget();
    // The encoder rounds down to even on its own; asking for an odd extent
    // returns a frame a pixel short on that axis and letterboxes the rest.
    driveSurfaceResize(target, container(801, 603));
    expect(displaySizes[0]?.slice(0, 2)).toEqual([800, 602]);
  });

  it("scales by devicePixelRatio and reports the ratio it measured", () => {
    vi.stubGlobal("devicePixelRatio", 2);
    const { target, displaySizes } = fakeTarget();
    driveSurfaceResize(target, container(400, 300));
    expect(displaySizes).toEqual([[800, 600, 240, 240]]);
  });

  it("sends the first size at wire speed and settles on the last", () => {
    const { target, resizes } = fakeTarget();
    driveSurfaceResize(target, container(800, 600));
    // Leading edge: a new interaction must not wait out the debounce.
    expect(resizes).toEqual([[800, 600, 120]]);

    resizeTo(820, 600);
    resizeTo(840, 600);
    // Mid-drag sizes are held back...
    expect(resizes).toHaveLength(1);
    vi.advanceTimersByTime(30);
    // ...and only the last one lands.
    expect(resizes).toEqual([
      [800, 600, 120],
      [840, 600, 120],
    ]);
  });

  it("does not re-ask for a size the server already has", () => {
    const { target, resizes } = fakeTarget();
    driveSurfaceResize(target, container(800, 600));
    resizes.length = 0;
    resizeTo(800, 600);
    vi.advanceTimersByTime(30);
    expect(resizes).toEqual([]);
  });

  it("treats a gap since the last box as the start of a fresh drag", () => {
    const { target, resizes } = fakeTarget();
    driveSurfaceResize(target, container(800, 600));
    vi.advanceTimersByTime(300);
    resizes.length = 0;
    resizeTo(900, 600);
    // No debounce wait: each user-visible drag gets a leading-edge dispatch.
    expect(resizes).toEqual([[900, 600, 120]]);
  });

  it("multiplies the display's DPI in relative mode", () => {
    const { target, displaySizes } = fakeTarget();
    let zoom: SurfaceZoom = { zoom: 1.5, mode: "relative" };
    driveSurfaceResize(target, container(800, 600), () => zoom);
    // The pane still holds 800x600 device pixels; only the scale moves.
    expect(displaySizes).toEqual([[800, 600, 180, 120]]);
  });

  it("names the surface scale directly in exact mode", () => {
    vi.stubGlobal("devicePixelRatio", 2);
    const { target, displaySizes } = fakeTarget();
    const zoom: SurfaceZoom = { zoom: 1.5, mode: "exact" };
    driveSurfaceResize(target, container(400, 300), () => zoom);
    // 120 * 1.5, independent of the 2x display.
    expect(displaySizes).toEqual([[800, 600, 180, 240]]);
  });

  it("re-applies the last box when the zoom changes under it", () => {
    const { target, displaySizes } = fakeTarget();
    let zoom: SurfaceZoom = { zoom: 1, mode: "relative" };
    const driver = driveSurfaceResize(target, container(800, 600), () => zoom);
    displaySizes.length = 0;

    // The box has not moved, so the observer will never fire on its own.
    zoom = { zoom: 2, mode: "relative" };
    driver.reapply();
    expect(displaySizes).toEqual([[800, 600, 240, 120]]);
  });

  it("hands the surface back on dispose and goes quiet", () => {
    const { target, displaySizes, resizes } = fakeTarget();
    const driver = driveSurfaceResize(target, container(800, 600));
    displaySizes.length = 0;
    resizes.length = 0;

    driver.dispose();
    // Null returns the view to frame-tracking mode and withdraws it from the
    // server's size mediation.
    expect(displaySizes).toEqual([[null, null, null, null]]);
    expect(disconnect).toHaveBeenCalledTimes(1);

    // A trailing-edge send must not outlive the mount.
    vi.advanceTimersByTime(1000);
    expect(resizes).toEqual([]);
    // And a late reapply is inert rather than resurrecting the stream.
    driver.reapply();
    expect(displaySizes).toHaveLength(1);
  });

  it("ignores a zero-sized box rather than reporting a degenerate one", () => {
    const { target, displaySizes } = fakeTarget();
    driveSurfaceResize(target, container(0, 0));
    expect(displaySizes).toEqual([]);
    resizeTo(0, 0);
    expect(displaySizes).toEqual([]);
    // A real box still lands once layout runs.
    resizeTo(640, 480);
    expect(displaySizes).toEqual([[640, 480, 120, 120]]);
  });
});
