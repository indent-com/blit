import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { BlitSurfaceCanvas } from "../BlitSurfaceCanvas";
import type { BlitWorkspace } from "../BlitWorkspace";
import type { SurfaceAxisEvent } from "../protocol";
import { AXIS_SOURCE_FINGER, AXIS_SOURCE_WHEEL } from "../types";

/** Minimal workspace stub: no connection, so the canvas never subscribes
 *  and layout can be exercised in isolation. */
function makeWorkspace(): BlitWorkspace {
  return {
    getConnection: () => null,
    subscribe: () => () => {},
  } as unknown as BlitWorkspace;
}

function attachCanvas() {
  const surface = new BlitSurfaceCanvas({
    workspace: makeWorkspace(),
    connectionId: "conn-1" as never,
    surfaceId: 7,
  });
  const container = document.createElement("div");
  surface.attach(container);
  const canvas = surface.canvasElement;
  if (!canvas) throw new Error("Expected surface canvas");
  return { surface, canvas };
}

describe("BlitSurfaceCanvas layout", () => {
  it("fills the container until a display size is known", () => {
    const { surface, canvas } = attachCanvas();
    expect(canvas.style.width).toBe("100%");
    expect(canvas.style.height).toBe("100%");
    expect(canvas.style.objectFit).toBe("contain");
    surface.dispose();
  });

  it("shows a frame smaller than the view at 1:1 device pixels, centered", () => {
    const { surface, canvas } = attachCanvas();
    // Backing buffer is the attach() default 640×480 "frame"; the view is
    // 1280×960 device pixels at scale 2 (scale120 = 240).  The frame must
    // not be upscaled: 640 device px = 320 CSS px, centered.
    surface.setDisplaySize(1280, 960, 240);
    expect(canvas.style.position).toBe("absolute");
    expect(canvas.style.width).toBe("320px");
    expect(canvas.style.height).toBe("240px");
    expect(canvas.style.left).toBe("160px");
    expect(canvas.style.top).toBe("120px");
    surface.dispose();
  });

  it("fills the view exactly when the frame matches it", () => {
    const { surface, canvas } = attachCanvas();
    canvas.width = 1280;
    canvas.height = 960;
    surface.setDisplaySize(1280, 960, 240);
    expect(canvas.style.width).toBe("640px");
    expect(canvas.style.height).toBe("480px");
    expect(canvas.style.left).toBe("0px");
    expect(canvas.style.top).toBe("0px");
    surface.dispose();
  });

  it("scales down (never clips) a frame transiently larger than the view", () => {
    const { surface, canvas } = attachCanvas();
    canvas.width = 2000;
    canvas.height = 480;
    surface.setDisplaySize(1280, 960, 240);
    // fit = 1280/2000 = 0.64 → 1280×307 device px, vertically centered.
    expect(canvas.style.width).toBe("640px");
    expect(canvas.style.height).toBe(`${307 / 2}px`);
    expect(canvas.style.left).toBe("0px");
    expect(canvas.style.top).toBe(`${326 / 2}px`);
    surface.dispose();
  });

  it("reverts to fill-and-contain when the display size is cleared", () => {
    const { surface, canvas } = attachCanvas();
    surface.setDisplaySize(1280, 960, 240);
    surface.setDisplaySize(null);
    expect(canvas.style.position).toBe("");
    expect(canvas.style.width).toBe("100%");
    expect(canvas.style.height).toBe("100%");
    surface.dispose();
  });
});

/** Captures the scroll messages a canvas emits. */
function attachScrolling(
  opts: { frame?: [number, number]; css?: [number, number] } = {},
) {
  const [fw, fh] = opts.frame ?? [800, 600];
  const [cw, ch] = opts.css ?? [800, 600];
  const sent: SurfaceAxisEvent[] = [];
  const conn = {
    sendSurfaceAxis2: (_id: number, ev: SurfaceAxisEvent) => sent.push(ev),
    // Only the surface geometry matters here; everything else the canvas
    // reaches for during attach() answers with an inert unsubscribe, so
    // this stub does not need updating when the store grows a method.
    surfaceStore: new Proxy(
      {
        getSurface: () => ({ width: fw, height: fh }),
        getCanvas: () => null,
        canDecodeVideo: false,
        generation: 0,
      } as Record<string, unknown>,
      {
        get: (target, prop) =>
          prop in target ? target[prop as string] : () => () => {},
      },
    ),
    sendSurfaceSubscribe: () => {},
    sendSurfaceUnsubscribe: () => {},
  };
  const workspace = {
    getConnection: () => conn,
    subscribe: () => () => {},
  } as unknown as BlitWorkspace;
  const surface = new BlitSurfaceCanvas({
    workspace,
    connectionId: "conn-1" as never,
    surfaceId: 7,
  });
  const container = document.createElement("div");
  surface.attach(container);
  const canvas = surface.canvasElement;
  if (!canvas) throw new Error("Expected surface canvas");
  canvas.width = fw;
  canvas.height = fh;
  // A display size is what separates a live view from a thumbnail, and
  // only live views take input.
  surface.setDisplaySize(fw, fh, 120);
  // jsdom lays nothing out, so the drawn region has to be declared.
  canvas.getBoundingClientRect = () =>
    ({ left: 0, top: 0, width: cw, height: ch }) as DOMRect;
  const wheel = (init: Partial<WheelEvent>) => {
    canvas.dispatchEvent(
      new WheelEvent("wheel", { cancelable: true, ...init }),
    );
    // Run out the animation frame the send is batched into, but stay well
    // inside the idle window so the gesture is still open.
    vi.advanceTimersByTime(FRAME_MS);
  };
  return { surface, canvas, sent, wheel };
}

/** One animation frame, as the fake clock models requestAnimationFrame. */
const FRAME_MS = 16;

describe("BlitSurfaceCanvas scroll", () => {
  beforeEach(() => {
    // The rAF-batched flush and the idle stop timer both need fake time,
    // and rAF is not faked unless asked for.
    vi.useFakeTimers({
      toFake: [
        "setTimeout",
        "clearTimeout",
        "requestAnimationFrame",
        "cancelAnimationFrame",
      ],
    });
  });
  afterEach(() => {
    vi.useRealTimers();
  });

  it("labels a trackpad's sub-pixel stream as a finger, with no detents", () => {
    const { surface, sent, wheel } = attachScrolling();
    wheel({ deltaY: 12.5, deltaMode: 0 });
    expect(sent).toHaveLength(1);
    expect(sent[0].source).toBe(AXIS_SOURCE_FINGER);
    expect(sent[0].dy).toBeCloseTo(12.5);
    expect(sent[0].v120y).toBe(0);
    surface.dispose();
  });

  it("labels a 120px-per-notch wheel as a wheel, with detents", () => {
    const { surface, sent, wheel } = attachScrolling();
    wheel({ deltaY: 120, deltaMode: 0 });
    expect(sent[0].source).toBe(AXIS_SOURCE_WHEEL);
    expect(sent[0].v120y).toBe(120);
    surface.dispose();
  });

  it("converts a line-mode wheel into pixels and detents", () => {
    const { surface, sent, wheel } = attachScrolling();
    // Firefox reports a notch as 3 lines.
    wheel({ deltaY: 3, deltaMode: 1 });
    expect(sent[0].source).toBe(AXIS_SOURCE_WHEEL);
    expect(sent[0].v120y).toBe(120);
    expect(sent[0].dy).toBeCloseTo(48);
    surface.dispose();
  });

  it("keeps a gesture smooth once it has shown sub-pixel deltas", () => {
    const { surface, sent, wheel } = attachScrolling();
    wheel({ deltaY: 3.5, deltaMode: 0 });
    // A momentum tail can land on a round 120 mid-gesture; that must not
    // reclassify the stream as a notched wheel.
    wheel({ deltaY: 120, deltaMode: 0 });
    expect(sent).toHaveLength(2);
    expect(sent[1].source).toBe(AXIS_SOURCE_FINGER);
    expect(sent[1].v120y).toBe(0);
    surface.dispose();
  });

  it("sends both axes of a diagonal gesture in one event", () => {
    const { surface, sent, wheel } = attachScrolling();
    wheel({ deltaX: 4.5, deltaY: 9.5, deltaMode: 0 });
    expect(sent).toHaveLength(1);
    expect(sent[0].dx).toBeCloseTo(4.5);
    expect(sent[0].dy).toBeCloseTo(9.5);
    surface.dispose();
  });

  it("ignores ctrl+wheel, which is a pinch-zoom rather than a scroll", () => {
    const { surface, sent, wheel } = attachScrolling();
    wheel({ deltaY: 40, deltaMode: 0, ctrlKey: true });
    expect(sent).toHaveLength(0);
    surface.dispose();
  });

  it("scales CSS deltas into frame pixels like pointer positions", () => {
    // 1600px of frame shown in an 800px box: a 10px gesture has to move
    // 20px of content, or scrolling and dragging disagree.
    const { surface, sent, wheel } = attachScrolling({
      frame: [1600, 1200],
      css: [800, 600],
    });
    wheel({ deltaY: 10.5, deltaMode: 0 });
    expect(sent[0].dy).toBeCloseTo(21);
    surface.dispose();
  });

  it("batches a burst of events into one message per frame", () => {
    const { surface, canvas, sent } = attachScrolling();
    for (let i = 0; i < 5; i++) {
      canvas.dispatchEvent(
        new WheelEvent("wheel", { deltaY: 3.5, cancelable: true }),
      );
    }
    vi.advanceTimersByTime(FRAME_MS);
    expect(sent).toHaveLength(1);
    expect(sent[0].dy).toBeCloseTo(17.5);
    surface.dispose();
  });

  it("ends the sequence with a stop once the wheel goes idle", () => {
    const { surface, sent, wheel } = attachScrolling();
    wheel({ deltaY: 8.5, deltaMode: 0 });
    expect(sent.filter((e) => e.stop)).toHaveLength(0);
    vi.advanceTimersByTime(500);
    const stops = sent.filter((e) => e.stop);
    expect(stops).toHaveLength(1);
    expect(stops[0].source).toBe(AXIS_SOURCE_FINGER);
    expect(stops[0].dy).toBe(0);
    surface.dispose();
  });

  /** A thumbnail takes no other input, and must not swallow the page's
   *  scroll to send a gesture to an app the user is only previewing. */
  it("leaves the wheel alone in a view with no display size", () => {
    const { surface, canvas, sent } = attachScrolling();
    surface.setDisplaySize(null);
    const e = new WheelEvent("wheel", { deltaY: 40, cancelable: true });
    canvas.dispatchEvent(e);
    vi.advanceTimersByTime(FRAME_MS);
    expect(sent).toHaveLength(0);
    expect(e.defaultPrevented).toBe(false);
    surface.dispose();
  });

  it("sends one stop per gesture, not one per idle tick", () => {
    const { surface, sent, wheel } = attachScrolling();
    wheel({ deltaY: 8.5, deltaMode: 0 });
    vi.advanceTimersByTime(500);
    vi.advanceTimersByTime(500);
    expect(sent.filter((e) => e.stop)).toHaveLength(1);
    surface.dispose();
  });
});
