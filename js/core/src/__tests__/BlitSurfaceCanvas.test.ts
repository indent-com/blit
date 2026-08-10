import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import {
  BlitSurfaceCanvas,
  demoteCodecSupport,
  detectCodecSupport,
  getCodecSupport,
  restoreCodecSupport,
} from "../BlitSurfaceCanvas";
import type { BlitWorkspace } from "../BlitWorkspace";
import type { BlitSurface } from "../types";
import type { SurfaceAxisEvent } from "../protocol";
import {
  SURFACE_POINTER_DOWN,
  SURFACE_POINTER_MOVE,
  SURFACE_POINTER_UP,
  buildSurfaceDragDropMessage,
} from "../protocol";
import {
  AXIS_SOURCE_CONTINUOUS,
  AXIS_SOURCE_FINGER,
  AXIS_SOURCE_WHEEL,
  CODEC_SUPPORT_AV1,
  CODEC_SUPPORT_AV1_444,
} from "../types";

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

/** The store feeds this in; the stub workspace has no connection to do it. */
function setSurfaceInfo(
  surface: BlitSurfaceCanvas,
  dims: { width: number; height: number; lw: number; lh: number },
) {
  (surface as unknown as { surface: BlitSurface }).surface = {
    connectionId: "conn-1" as never,
    surfaceId: 7,
    parentId: 0,
    title: "t",
    appId: "a",
    width: dims.width,
    height: dims.height,
    logicalWidth: dims.lw,
    logicalHeight: dims.lh,
  };
}

describe("BlitSurfaceCanvas layout", () => {
  it("fills the container until a display size is known", () => {
    const { surface, canvas } = attachCanvas();
    expect(canvas.style.width).toBe("100%");
    expect(canvas.style.height).toBe("100%");
    expect(canvas.style.objectFit).toBe("contain");
    surface.dispose();
  });

  it("fills the view with a smaller frame of the same shape", () => {
    const { surface, canvas } = attachCanvas();
    // Backing buffer is the attach() default 640×480 "frame"; the view is
    // 1280×960 device pixels at scale 2 (scale120 = 240).  Same 4:3 shape,
    // so it fills the pane — 1280 device px = 640 CSS px, no offset.
    surface.setDisplaySize(1280, 960, 240);
    expect(canvas.style.position).toBe("absolute");
    expect(canvas.style.width).toBe("640px");
    expect(canvas.style.height).toBe("480px");
    expect(canvas.style.left).toBe("0px");
    expect(canvas.style.top).toBe("0px");
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

  it("reaches the edges of a pane the encoder had to round off", () => {
    // Even-grid mediation: the pane asked for 1237×843 and the stream came
    // back on the 4:2:0 grid at 1236×842.  A box derived from the stream
    // leaves a pixel of background showing along two edges.
    const { surface, canvas } = attachCanvas();
    canvas.width = 1236;
    canvas.height = 842;
    surface.setDisplaySize(1237, 843, 120);
    expect(canvas.style.width).toBe("1237px");
    expect(canvas.style.height).toBe("843px");
    expect(canvas.style.left).toBe("0px");
    expect(canvas.style.top).toBe("0px");
    surface.dispose();
  });

  it("does not move the picture when the stream size changes", () => {
    // A flip between the native stream and a downscaled target used to
    // resize and re-centre the box under the picture.
    const { surface, canvas } = attachCanvas();
    canvas.width = 1236;
    canvas.height = 842;
    surface.setDisplaySize(1237, 843, 120);
    const before = {
      width: canvas.style.width,
      height: canvas.style.height,
      left: canvas.style.left,
      top: canvas.style.top,
    };
    canvas.width = 618;
    canvas.height = 421;
    (surface as unknown as { applyLayout(): void }).applyLayout();
    expect(canvas.style.width).toBe(before.width);
    expect(canvas.style.height).toBe(before.height);
    expect(canvas.style.left).toBe(before.left);
    expect(canvas.style.top).toBe(before.top);
    surface.dispose();
  });

  it("letterboxes a frame of a genuinely different aspect ratio", () => {
    const { surface, canvas } = attachCanvas();
    canvas.width = 2000;
    canvas.height = 480;
    surface.setDisplaySize(1280, 960, 240);
    // fit = 1280/2000 = 0.64 → 1280×307 device px, vertically centered.
    expect(canvas.style.width).toBe("640px");
    expect(canvas.style.height).toBe(`${307 / 2}px`);
    expect(canvas.style.left).toBe("0px");
    expect(canvas.style.top).toBe(`${327 / 2}px`);
    surface.dispose();
  });

  it("draws a surface a high-DPI viewer sized at 1x, not zoomed", () => {
    // A 400×300 pane at 3x and a 1600×1200 pane at 1x watching one surface:
    // mediation gives it the smaller logical size at the higher scale, so it
    // composites 1200×900.  Filling the 1x pane with that frame would show
    // the same window three times larger than the client that asked for it
    // sees it; 400×300 device px is the size the window actually is.
    const { surface, canvas } = attachCanvas();
    setSurfaceInfo(surface, { width: 1200, height: 900, lw: 400, lh: 300 });
    canvas.width = 1200;
    canvas.height = 900;
    surface.setDisplaySize(1600, 1200, 120);
    expect(canvas.style.width).toBe("400px");
    expect(canvas.style.height).toBe("300px");
    // Centred: the pane keeps the leftover as letterbox on both sides.
    expect(canvas.style.left).toBe("600px");
    expect(canvas.style.top).toBe("450px");
    surface.dispose();
  });

  it("still fills the pane of the viewer that sized the surface", () => {
    const { surface, canvas } = attachCanvas();
    setSurfaceInfo(surface, { width: 1200, height: 900, lw: 400, lh: 300 });
    canvas.width = 1200;
    canvas.height = 900;
    // The 3x viewer: 400 logical × 3 = its whole 1200px pane.
    surface.setDisplaySize(1200, 900, 360);
    expect(canvas.style.width).toBe("400px"); // 1200 device px at 3x
    expect(canvas.style.height).toBe("300px");
    expect(canvas.style.left).toBe("0px");
    expect(canvas.style.top).toBe("0px");
    surface.dispose();
  });

  it("does not letterbox a pane the even grid rounded off the logical size", () => {
    // The viewer setting the size gets a logical size rounded onto the
    // 4:2:0 grid — a pixel or two under its own pane.  That is rounding
    // noise, not a smaller window, and must not open a gap.
    const { surface, canvas } = attachCanvas();
    setSurfaceInfo(surface, { width: 1236, height: 842, lw: 1236, lh: 842 });
    canvas.width = 1236;
    canvas.height = 842;
    surface.setDisplaySize(1237, 843, 120);
    expect(canvas.style.width).toBe("1237px");
    expect(canvas.style.height).toBe("843px");
    expect(canvas.style.left).toBe("0px");
    surface.dispose();
  });

  it("fills the pane while the surface's logical size is unknown", () => {
    // Old server, or no resize reported yet: 0 means unknown, and guessing
    // a 0-wide window would draw nothing.
    const { surface, canvas } = attachCanvas();
    setSurfaceInfo(surface, { width: 1200, height: 900, lw: 0, lh: 0 });
    canvas.width = 1200;
    canvas.height = 900;
    surface.setDisplaySize(1600, 1200, 120);
    expect(canvas.style.width).toBe("1600px");
    expect(canvas.style.height).toBe("1200px");
    surface.dispose();
  });

  it("sizes the CSS box by the pane's DPI, not by a zoomed surface scale", () => {
    // A 1x pane, 800×600 CSS px, at 150% zoom: the surface composites at
    // 1.5x, so it lays out in 533×400 of its own logical pixels, but the
    // pane is still 800×600 device pixels and the picture has to cover it.
    // Dividing the box by the surface scale instead would draw it at 2/3
    // size with a third of the pane left empty.
    const { surface, canvas } = attachCanvas();
    setSurfaceInfo(surface, { width: 800, height: 600, lw: 534, lh: 400 });
    canvas.width = 800;
    canvas.height = 600;
    surface.setDisplaySize(800, 600, 180, 120);
    expect(canvas.style.width).toBe("800px");
    expect(canvas.style.height).toBe("600px");
    expect(canvas.style.left).toBe("0px");
    expect(canvas.style.top).toBe("0px");
    surface.dispose();
  });

  it("fills the pane with a sub-1x surface downscale", () => {
    // An 800×600 pane at exact 0.5x gives the app a 1600×1200 logical
    // window. The compositor renders that at Wayland's 1x floor and the
    // server sends this viewer an 800×600 downscale, which still fills the
    // pane at its own 1x CSS density.
    const { surface, canvas } = attachCanvas();
    setSurfaceInfo(surface, {
      width: 1600,
      height: 1200,
      lw: 1600,
      lh: 1200,
    });
    canvas.width = 800;
    canvas.height = 600;
    surface.setDisplaySize(800, 600, 60, 120);
    expect(canvas.style.width).toBe("800px");
    expect(canvas.style.height).toBe("600px");
    expect(canvas.style.left).toBe("0px");
    expect(canvas.style.top).toBe("0px");
    surface.dispose();
  });

  it("keeps the CSS box on the device grid under zoom on a 2x pane", () => {
    // 640×480 CSS px at 2x with 125% zoom: 1280×960 device pixels, surface
    // scale 300 (2 × 1.25), logical 512×384.
    const { surface, canvas } = attachCanvas();
    setSurfaceInfo(surface, { width: 1280, height: 960, lw: 512, lh: 384 });
    canvas.width = 1280;
    canvas.height = 960;
    surface.setDisplaySize(1280, 960, 300, 240);
    expect(canvas.style.width).toBe("640px");
    expect(canvas.style.height).toBe("480px");
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

describe("codec support demotion", () => {
  /** Enough of WebCodecs for the probe: every configuration is supported,
   *  and no decoder ever emits a frame, so the 4:4:4 checks (which demand a
   *  real decode) come back negative. */
  class ProbeDecoder {
    state = "unconfigured";
    constructor(_init: unknown) {}
    static async isConfigSupported() {
      return { supported: true };
    }
    configure() {
      this.state = "configured";
    }
    decode() {}
    flush() {
      return Promise.resolve();
    }
    close() {
      this.state = "closed";
    }
  }

  it("takes a codec off probation, but never one the probe never found", async () => {
    vi.stubGlobal("VideoDecoder", ProbeDecoder);
    vi.stubGlobal("EncodedVideoChunk", class {});
    const probed = await detectCodecSupport();
    expect(probed & CODEC_SUPPORT_AV1).toBeTruthy();
    expect(probed & CODEC_SUPPORT_AV1_444).toBe(0);

    const av1 = CODEC_SUPPORT_AV1 | CODEC_SUPPORT_AV1_444;
    expect(demoteCodecSupport(av1)).toBe(probed & ~av1);
    expect(getCodecSupport() & CODEC_SUPPORT_AV1).toBe(0);

    // Probation ends and the browser is offered the codec again — this is
    // what keeps a transient decode fault from downgrading the page for as
    // long as it stays open.
    expect(restoreCodecSupport(av1)).toBe(probed);
    expect(restoreCodecSupport(av1)).toBeNull();
    // 4:4:4 was never probed as working, so restoring cannot invent it.
    expect(getCodecSupport() & CODEC_SUPPORT_AV1_444).toBe(0);
    vi.unstubAllGlobals();
  });
});

/** Captures the scroll messages a canvas emits. */
function attachScrolling(
  opts: { frame?: [number, number]; css?: [number, number] } = {},
) {
  const [fw, fh] = opts.frame ?? [800, 600];
  const [cw, ch] = opts.css ?? [800, 600];
  const sent: SurfaceAxisEvent[] = [];
  const keys: { keycode: number; pressed: boolean }[] = [];
  const pointers: { type: number; button: number; x: number; y: number }[] = [];
  const conn = {
    sendSurfaceAxis2: (_id: number, ev: SurfaceAxisEvent) => sent.push(ev),
    sendSurfaceInput: (_id: number, keycode: number, pressed: boolean) =>
      keys.push({ keycode, pressed }),
    sendSurfacePointer: (
      _id: number,
      type: number,
      button: number,
      x: number,
      y: number,
    ) => pointers.push({ type, button, x, y }),
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
  return { surface, canvas, sent, keys, pointers, wheel };
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

  it("labels a trackpad's sub-pixel stream as continuous, with no detents", () => {
    const { surface, sent, wheel } = attachScrolling();
    wheel({ deltaY: 12.5, deltaMode: 0 });
    expect(sent).toHaveLength(1);
    expect(sent[0].source).toBe(AXIS_SOURCE_CONTINUOUS);
    expect(sent[0].dy).toBeCloseTo(12.5);
    expect(sent[0].v120y).toBe(0);
    surface.dispose();
  });

  it("forwards smooth trackpad input without waiting for a display frame", () => {
    const { surface, canvas, sent } = attachScrolling();
    canvas.dispatchEvent(
      new WheelEvent("wheel", {
        deltaY: 3.5,
        cancelable: true,
      }),
    );
    expect(sent).toHaveLength(1);
    expect(sent[0].dy).toBeCloseTo(3.5);
    expect(sent[0].source).toBe(AXIS_SOURCE_CONTINUOUS);
    surface.dispose();
  });

  /**
   * The bug this all exists for. macOS hands the browser a notched wheel
   * as plain pixel deltas — around a third of a 120px detent, varied by
   * its own scroll acceleration — so no arithmetic here can tell it from
   * a trackpad. Calling the ones we cannot prove `finger` used to be the
   * safe-looking guess; it is the opposite, because `finger` is what
   * licenses a toolkit to fling. Every notch of a real wheel glided.
   */
  it("never labels a wheel event a finger, whatever its deltas look like", () => {
    const { surface, sent, wheel } = attachScrolling();
    // One notch, then two, the way macOS acceleration reports a spin.
    for (const deltaY of [40, 40, 80, 120, 40]) wheel({ deltaY, deltaMode: 0 });
    vi.advanceTimersByTime(500);
    expect(sent).not.toHaveLength(0);
    expect(sent.map((e) => e.source)).not.toContain(AXIS_SOURCE_FINGER);
    expect(sent.filter((e) => e.stop)).toHaveLength(0);
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
    expect(sent[1].source).toBe(AXIS_SOURCE_CONTINUOUS);
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

  it("batches a burst of notched-wheel events into one message per frame", () => {
    const { surface, canvas, sent } = attachScrolling();
    for (let i = 0; i < 5; i++) {
      canvas.dispatchEvent(
        new WheelEvent("wheel", { deltaY: 120, cancelable: true }),
      );
    }
    vi.advanceTimersByTime(FRAME_MS);
    expect(sent).toHaveLength(1);
    expect(sent[0].dy).toBeCloseTo(600);
    expect(sent[0].v120y).toBe(600);
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

  /** The protocol leaves a `wheel` sequence unterminated, and a wheel has
   *  no finger-lift to report; a stop only invites invented momentum. */
  it("leaves a notched wheel sequence unterminated", () => {
    const { surface, sent, wheel } = attachScrolling();
    wheel({ deltaY: 120, deltaMode: 0 });
    expect(sent[0].source).toBe(AXIS_SOURCE_WHEEL);
    vi.advanceTimersByTime(500);
    expect(sent.filter((e) => e.stop)).toHaveLength(0);
    surface.dispose();
  });

  /** Going idle closes the sequence, so the next one is classified from
   *  scratch rather than inheriting a tail's verdict. */
  it("classifies the next sequence afresh once the last one went idle", () => {
    const { surface, sent, wheel } = attachScrolling();
    wheel({ deltaY: 3.5, deltaMode: 0 });
    expect(sent[0].source).toBe(AXIS_SOURCE_CONTINUOUS);
    vi.advanceTimersByTime(500);
    wheel({ deltaY: 120, deltaMode: 0 });
    expect(sent[1].source).toBe(AXIS_SOURCE_WHEEL);
    expect(sent[1].v120y).toBe(120);
    surface.dispose();
  });

  it("flushes a pending Alt ahead of a scroll", () => {
    // Alt+scroll is a chord (horizontal scroll, zoom in some apps); an Alt
    // press held back for dead-key detection must beat the axis events
    // onto the wire.
    const { surface, canvas, sent, keys, wheel } = attachScrolling();
    (surface as unknown as { macOptionChars: boolean }).macOptionChars = true;

    canvas.dispatchEvent(
      new KeyboardEvent("keydown", {
        key: "Alt",
        code: "AltLeft",
        altKey: true,
        cancelable: true,
      }),
    );
    wheel({ deltaY: 40, deltaMode: 0 });

    expect(keys).toEqual([{ keycode: 56, pressed: true }]);
    expect(sent).toHaveLength(1);
    surface.dispose();
  });
});

/** jsdom implements neither Touch nor TouchEvent, and the handlers only ever
 *  reach for the identifier, the client point and the two touch lists. */
function touchEvent(
  type: string,
  points: { identifier: number; clientX: number; clientY: number }[],
  opts: { ongoing?: boolean } = {},
): Event {
  const list = {
    length: points.length,
    item: (i: number) => points[i] ?? null,
  } as unknown as TouchList;
  const empty = { length: 0, item: () => null } as unknown as TouchList;
  const ev = new Event(type, { bubbles: true, cancelable: true });
  // `touches` is what is still down, `changedTouches` what the event is
  // about — a lift reports the finger only in the latter.
  Object.defineProperty(ev, "touches", {
    value: opts.ongoing === false ? empty : list,
  });
  Object.defineProperty(ev, "changedTouches", { value: list });
  return ev;
}

/** jsdom has no PointerEvent either; a MouseEvent carries the same fields. */
function pointerEvent(type: string, x: number, y: number): Event {
  const ev = new MouseEvent(type, {
    bubbles: true,
    cancelable: true,
    clientX: x,
    clientY: y,
  });
  Object.defineProperty(ev, "pointerId", { value: 1 });
  Object.defineProperty(ev, "pointerType", { value: "touch" });
  return ev;
}

describe("BlitSurfaceCanvas cross-surface mouse grabs", () => {
  it("routes a held move and release to the canvas under the pointer", () => {
    const pointers: {
      id: number;
      type: number;
      button: number;
      x: number;
      y: number;
    }[] = [];
    const conn = {
      sendSurfacePointer: (
        id: number,
        type: number,
        button: number,
        x: number,
        y: number,
      ) => pointers.push({ id, type, button, x, y }),
      sendSurfaceFocus: () => {},
      surfaceStore: new Proxy(
        {
          getSurface: (id: number) => ({
            connectionId: "conn-1",
            surfaceId: id,
            width: 100,
            height: 100,
          }),
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
    const mount = (id: number, left: number) => {
      const surface = new BlitSurfaceCanvas({
        workspace,
        connectionId: "conn-1" as never,
        surfaceId: id,
      });
      const container = document.createElement("div");
      document.body.appendChild(container);
      surface.attach(container);
      surface.setDisplaySize(100, 100, 120);
      const canvas = surface.canvasElement!;
      canvas.width = 100;
      canvas.height = 100;
      canvas.getBoundingClientRect = () =>
        ({ left, top: 0, width: 100, height: 100 }) as DOMRect;
      return { surface, canvas, container };
    };
    const first = mount(1, 0);
    const second = mount(2, 100);

    try {
      first.canvas.dispatchEvent(
        new MouseEvent("mousedown", {
          bubbles: true,
          button: 0,
          buttons: 1,
          clientX: 10,
          clientY: 20,
        }),
      );
      second.canvas.dispatchEvent(
        new MouseEvent("mousemove", {
          bubbles: true,
          button: 0,
          buttons: 1,
          clientX: 130,
          clientY: 40,
        }),
      );
      second.canvas.dispatchEvent(
        new MouseEvent("mouseup", {
          bubbles: true,
          button: 0,
          buttons: 0,
          clientX: 140,
          clientY: 50,
        }),
      );

      expect(pointers).toEqual([
        { id: 1, type: SURFACE_POINTER_DOWN, button: 0, x: 10, y: 20 },
        { id: 2, type: SURFACE_POINTER_MOVE, button: 0, x: 30, y: 40 },
        { id: 2, type: SURFACE_POINTER_UP, button: 0, x: 40, y: 50 },
      ]);
    } finally {
      first.surface.dispose();
      second.surface.dispose();
      first.container.remove();
      second.container.remove();
    }
  });
});

describe("BlitSurfaceCanvas touch", () => {
  beforeEach(() => {
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

  const FINGER = { identifier: 1, clientX: 40, clientY: 40 };

  /**
   * iPadOS dispatches `pointerdown` ahead of `touchstart`, so the pointer
   * path claims the gesture and the touch handlers fall straight through
   * their guards.  They have to cancel the touch on the way out regardless:
   * an uncancelled `touchstart` is what licenses the browser to replay the
   * tap as compatibility mouse events, and those reach the same canvas's
   * mousedown/mouseup listeners as a second press and release — which the
   * app on the far end reads as a double click.
   */
  it("cancels a touch the pointer path has already claimed", () => {
    const { surface, canvas } = attachScrolling();

    canvas.dispatchEvent(pointerEvent("pointerdown", 40, 40));
    const start = touchEvent("touchstart", [FINGER]);
    canvas.dispatchEvent(start);
    expect(start.defaultPrevented).toBe(true);

    canvas.dispatchEvent(pointerEvent("pointerup", 40, 40));
    const end = touchEvent("touchend", [FINGER], { ongoing: false });
    canvas.dispatchEvent(end);
    expect(end.defaultPrevented).toBe(true);

    surface.dispose();
  });

  it("sends one press and one release for a tap", () => {
    const { surface, canvas, pointers } = attachScrolling();

    canvas.dispatchEvent(pointerEvent("pointerdown", 40, 40));
    canvas.dispatchEvent(touchEvent("touchstart", [FINGER]));
    canvas.dispatchEvent(pointerEvent("pointerup", 40, 40));
    canvas.dispatchEvent(touchEvent("touchend", [FINGER], { ongoing: false }));

    expect(pointers.map((p) => p.type)).toEqual([
      SURFACE_POINTER_DOWN,
      SURFACE_POINTER_UP,
    ]);
    surface.dispose();
  });

  /**
   * Axis events go to the surface holding pointer focus, and only motion
   * moves that focus (a tap gets one synthesised from its press point).  A
   * finger drag sends no motion of its own, so a drag that starts without
   * re-seeding the position scrolls wherever the cursor was last left —
   * another window, or nowhere — until a tap places it again.
   */
  it("re-seeds the pointer position when a drag becomes a scroll", () => {
    const { surface, canvas, sent, pointers } = attachScrolling();
    const moved = { identifier: 1, clientX: 40, clientY: 100 };

    canvas.dispatchEvent(touchEvent("touchstart", [FINGER]));
    canvas.dispatchEvent(touchEvent("touchmove", [moved]));
    vi.advanceTimersByTime(FRAME_MS);

    // The move lands where the finger is, ahead of the first axis event.
    expect(pointers).toEqual([
      { type: SURFACE_POINTER_MOVE, button: 0, x: 40, y: 100 },
    ]);
    expect(sent[0].source).toBe(AXIS_SOURCE_FINGER);

    // And it is one move per gesture, not one per frame of the drag.
    canvas.dispatchEvent(
      touchEvent("touchmove", [{ identifier: 1, clientX: 40, clientY: 140 }]),
    );
    vi.advanceTimersByTime(FRAME_MS);
    expect(
      pointers.filter((p) => p.type === SURFACE_POINTER_MOVE),
    ).toHaveLength(1);

    canvas.dispatchEvent(
      touchEvent("touchend", [{ identifier: 1, clientX: 40, clientY: 140 }], {
        ongoing: false,
      }),
    );
    surface.dispose();
  });

  /**
   * The one device that has earned a fling. A finger really does lift, at
   * a moment worth reporting, and a flick on glass that doesn't coast
   * feels broken — so this is the only path that claims `finger` and the
   * only one that sends the `axis_stop` a toolkit flings from.
   */
  it("ends a touch drag with a finger stop, exactly one", () => {
    const { surface, canvas, sent } = attachScrolling();
    const moved = { identifier: 1, clientX: 40, clientY: 100 };

    canvas.dispatchEvent(touchEvent("touchstart", [FINGER]));
    canvas.dispatchEvent(touchEvent("touchmove", [moved]));
    vi.advanceTimersByTime(FRAME_MS);
    canvas.dispatchEvent(touchEvent("touchend", [moved], { ongoing: false }));

    // Dragging the content down scrolls up, and a finger carries no detents.
    expect(sent[0].source).toBe(AXIS_SOURCE_FINGER);
    expect(sent[0].dy).toBeCloseTo(-60);
    expect(sent[0].v120y).toBe(0);
    const stops = sent.filter((e) => e.stop);
    expect(stops).toHaveLength(1);
    expect(stops[0].source).toBe(AXIS_SOURCE_FINGER);
    // The idle timer must not follow up with a second one.
    vi.advanceTimersByTime(1000);
    expect(sent.filter((e) => e.stop)).toHaveLength(1);
    surface.dispose();
  });

  it("still drives a tap when only touch events arrive", () => {
    const { surface, canvas, pointers } = attachScrolling();

    canvas.dispatchEvent(touchEvent("touchstart", [FINGER]));
    canvas.dispatchEvent(touchEvent("touchend", [FINGER], { ongoing: false }));

    expect(pointers.map((p) => p.type)).toEqual([
      SURFACE_POINTER_DOWN,
      SURFACE_POINTER_UP,
    ]);
    surface.dispose();
  });

  /**
   * The touch right-click: a hold that completes and releases without the
   * finger ever travelling. Button 2 is the DOM's right button, mapped to
   * BTN_RIGHT server-side exactly like a mouse's right press.
   */
  it("sends a right-click for a hold released without moving", () => {
    const { surface, canvas, pointers } = attachScrolling();

    canvas.dispatchEvent(pointerEvent("pointerdown", 40, 40));
    canvas.dispatchEvent(touchEvent("touchstart", [FINGER]));
    vi.advanceTimersByTime(400); // past the 350ms hold
    canvas.dispatchEvent(pointerEvent("pointerup", 40, 40));
    canvas.dispatchEvent(touchEvent("touchend", [FINGER], { ongoing: false }));

    expect(pointers).toEqual([
      { type: SURFACE_POINTER_DOWN, button: 2, x: 40, y: 40 },
      { type: SURFACE_POINTER_UP, button: 2, x: 40, y: 40 },
    ]);
    surface.dispose();
  });

  /** The same hold, followed by movement, stays the drag it always was. */
  it("still starts a left drag when the held finger moves", () => {
    const { surface, canvas, pointers } = attachScrolling();

    canvas.dispatchEvent(pointerEvent("pointerdown", 40, 40));
    vi.advanceTimersByTime(400);
    canvas.dispatchEvent(pointerEvent("pointermove", 40, 100));
    canvas.dispatchEvent(pointerEvent("pointermove", 40, 120));
    canvas.dispatchEvent(pointerEvent("pointerup", 40, 120));

    expect(pointers).toEqual([
      { type: SURFACE_POINTER_DOWN, button: 0, x: 40, y: 100 },
      { type: SURFACE_POINTER_MOVE, button: 0, x: 40, y: 100 },
      { type: SURFACE_POINTER_MOVE, button: 0, x: 40, y: 120 },
      { type: SURFACE_POINTER_MOVE, button: 0, x: 40, y: 120 },
      { type: SURFACE_POINTER_UP, button: 0, x: 40, y: 120 },
    ]);
    surface.dispose();
  });

  it("sends a right-click when only touch events arrive", () => {
    const { surface, canvas, pointers } = attachScrolling();

    canvas.dispatchEvent(touchEvent("touchstart", [FINGER]));
    vi.advanceTimersByTime(400);
    canvas.dispatchEvent(touchEvent("touchend", [FINGER], { ongoing: false }));

    expect(pointers).toEqual([
      { type: SURFACE_POINTER_DOWN, button: 2, x: 40, y: 40 },
      { type: SURFACE_POINTER_UP, button: 2, x: 40, y: 40 },
    ]);
    surface.dispose();
  });
});

/**
 * A view that reports a scaled target is excluded from the server's size
 * mediation entirely — a thumbnail asks to be served a downscale of whatever
 * the surface happens to be, so it gets no say in how big that is.  The
 * target is therefore not merely a stream-size hint: registering one, or
 * failing to drop one, decides whether this view can size the surface at all.
 */
function attachTargeting() {
  const targets: ({ width: number; height: number } | null)[] = [];
  let roCallback: ResizeObserverCallback | undefined;
  const prevRO = globalThis.ResizeObserver;
  globalThis.ResizeObserver = class {
    constructor(cb: ResizeObserverCallback) {
      roCallback = cb;
    }
    observe() {}
    unobserve() {}
    disconnect() {}
  } as unknown as typeof ResizeObserver;

  const conn = {
    surfaceStore: new Proxy(
      {
        getSurface: () => ({ width: 1920, height: 1080 }),
        getCanvas: () => null,
        canDecodeVideo: true,
        generation: 0,
      } as Record<string, unknown>,
      {
        get: (target, prop) =>
          prop in target ? target[prop as string] : () => () => {},
      },
    ),
    sendSurfaceSubscribe: (
      _sid: number,
      _viewId: string,
      target: { width: number; height: number } | null,
    ) => targets.push(target),
    setSurfaceViewTarget: (
      _sid: number,
      _viewId: string,
      target: { width: number; height: number } | null,
    ) => targets.push(target),
    sendSurfaceUnsubscribe: () => {},
    offerSurfaceViewSize: () => true,
    withdrawSurfaceViewSize: () => {},
    allocSurfaceViewId: () => "s1",
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
  surface.attach(document.createElement("div"));
  /** Fire the box observer the way the browser does after layout. */
  const layOut = (width: number, height: number) =>
    roCallback?.(
      [{ contentRect: { width, height } } as ResizeObserverEntry],
      null as unknown as ResizeObserver,
    );
  const restore = () => {
    surface.dispose();
    globalThis.ResizeObserver = prevRO;
  };
  return { surface, targets, layOut, restore };
}

describe("BlitSurfaceCanvas size mediation", () => {
  it("drops the scaled target once it is given a display size", () => {
    const { surface, targets, layOut, restore } = attachTargeting();

    // A pane whose box is measured before the binding's own observer gets
    // to it — the container was still 0×0 when the effect ran, so
    // getBoundingClientRect() declined to set a display size and this
    // observer wins the race.  The view registers a thumbnail's target.
    layOut(900, 500);
    expect(targets.at(-1)).toEqual({ width: 1024, height: 512 });

    // Now the binding measures and hands over the pane's real size.  This
    // view is a live pane, not a thumbnail: it must give up the target, or
    // the server keeps skipping it in mediation and the surface never
    // resizes to the pane.
    surface.setDisplaySize(900, 500, 120);
    expect(targets.at(-1)).toBeNull();

    restore();
  });

  it("re-registers the scaled target when the display size goes away", () => {
    const { surface, targets, layOut, restore } = attachTargeting();

    surface.setDisplaySize(900, 500, 120);
    layOut(900, 500);
    expect(targets.at(-1)).toBeNull();

    // The pane became a thumbnail (a BSP leaf hidden behind a solo, a view
    // moved back to the sidebar).  It stops sizing the surface and goes
    // back to asking for a downscale of it.
    surface.setDisplaySize(null);
    expect(targets.at(-1)).toEqual({ width: 1024, height: 512 });

    restore();
  });

  it("re-derives nothing while the box is still unmeasured", () => {
    const { surface, targets, restore } = attachTargeting();

    // Every wire subscribe costs the server an encoder rebuild and this
    // client a keyframe, so a display size arriving before the box has been
    // measured must not manufacture one: there is no box to scale to and
    // the eager subscribe already went out unscaled.
    const before = targets.length;
    surface.setDisplaySize(900, 500, 120);
    expect(targets.slice(before)).toEqual([]);

    restore();
  });
});

describe("BlitSurfaceCanvas visibility", () => {
  it("releases hidden mounts and reclaims the same view on entry", () => {
    let intersectionCallback: IntersectionObserverCallback | undefined;
    let disconnected = false;
    const prevIO = globalThis.IntersectionObserver;
    globalThis.IntersectionObserver = class {
      constructor(cb: IntersectionObserverCallback) {
        intersectionCallback = cb;
      }
      observe() {}
      unobserve() {}
      disconnect() {
        disconnected = true;
      }
    } as unknown as typeof IntersectionObserver;

    let generation = 0;
    let change: (() => void) | undefined;
    const subscribes: { surfaceId: number; viewId: string }[] = [];
    const unsubscribes: { surfaceId: number; viewId: string }[] = [];
    const store = {
      getSurface: () => ({ width: 1920, height: 1080 }),
      getCanvas: () => null,
      getCursor: () => "default",
      canDecodeVideo: true,
      get generation() {
        return generation;
      },
      onChange: (cb: () => void) => {
        change = cb;
        return () => {};
      },
      onCursor: () => () => {},
      onFrame: () => () => {},
    };
    const conn = {
      surfaceStore: store,
      allocSurfaceViewId: () => "s1",
      sendSurfaceSubscribe: (surfaceId: number, viewId: string) =>
        subscribes.push({ surfaceId, viewId }),
      sendSurfaceUnsubscribe: (surfaceId: number, viewId: string) =>
        unsubscribes.push({ surfaceId, viewId }),
      refreshSurfaceSubscribe: () => {},
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

    const intersect = (isIntersecting: boolean) =>
      intersectionCallback?.(
        [{ isIntersecting } as IntersectionObserverEntry],
        null as unknown as IntersectionObserver,
      );

    try {
      surface.attach(document.createElement("div"));
      expect(subscribes).toEqual([{ surfaceId: 7, viewId: "s1" }]);

      intersect(false);
      expect(unsubscribes).toEqual([{ surfaceId: 7, viewId: "s1" }]);

      // Store reconnect/change notifications while hidden must not bring
      // the invisible stream back.
      generation++;
      change?.();
      intersect(false);
      expect(subscribes).toHaveLength(1);

      intersect(true);
      expect(subscribes).toEqual([
        { surfaceId: 7, viewId: "s1" },
        { surfaceId: 7, viewId: "s1" },
      ]);
    } finally {
      surface.dispose();
      globalThis.IntersectionObserver = prevIO;
    }
    expect(disconnected).toBe(true);
  });
});

/** evdev keycode for KeyV, the key the paste chord defers. */
const EVDEV_V = 47;

/** A canvas wired for paste: captures what reaches the Wayland selection
 *  and which keycodes are forwarded, in order. */
function attachPasting(initialWaylandOwner = false) {
  const clipboard: { mime: string; data: Uint8Array }[] = [];
  const keys: { keycode: number; pressed: boolean }[] = [];
  let waylandOwner: boolean | null = initialWaylandOwner;
  const conn = {
    sendClipboard: (mime: string, data: Uint8Array) =>
      clipboard.push({ mime, data }),
    sendSurfaceInput: (_id: number, keycode: number, pressed: boolean) =>
      keys.push({ keycode, pressed }),
    sendSurfaceText: () => {},
    // The container is in the document here, so canvas.focus() really does
    // fire a focus event and the canvas really does claim keyboard focus.
    sendSurfaceFocus: () => {},
    usesWaylandClipboard: () => waylandOwner === true,
    noteBrowserClipboardMayHaveChanged: () => {
      waylandOwner = null;
    },
    surfaceStore: new Proxy(
      {
        getSurface: () => ({ width: 800, height: 600 }),
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
  // In the document, so a paste reaches the document-level capture listener
  // as well as the canvas's own — as it does in a real page.
  document.body.appendChild(container);
  surface.attach(container);
  const canvas = surface.canvasElement;
  if (!canvas) throw new Error("Expected surface canvas");
  // Only a live view takes input.
  surface.setDisplaySize(800, 600, 120);

  const pressCtrlV = () =>
    canvas.dispatchEvent(
      new KeyboardEvent("keydown", {
        key: "v",
        code: "KeyV",
        ctrlKey: true,
        bubbles: true,
        cancelable: true,
      }),
    );

  /** Dispatch a paste carrying any mix of files and plain text. */
  const firePaste = (opts: { files?: File[]; text?: string }) => {
    const items = (opts.files ?? []).map(
      (file) =>
        ({
          kind: "file",
          type: file.type,
          getAsFile: () => file,
        }) as unknown as DataTransferItem,
    );
    const clipboardData = {
      items: items as unknown as DataTransferItemList,
      getData: (mime: string) =>
        mime === "text/plain" ? (opts.text ?? "") : "",
    } as unknown as DataTransfer;
    const ev = new Event("paste", { bubbles: true, cancelable: true });
    Object.defineProperty(ev, "clipboardData", { value: clipboardData });
    canvas.dispatchEvent(ev);
    return ev;
  };

  const dispose = () => {
    surface.dispose();
    container.remove();
  };

  return { surface, canvas, clipboard, keys, pressCtrlV, firePaste, dispose };
}

/** `File.arrayBuffer()` resolves on a microtask chain; drain it. */
async function settle() {
  for (let i = 0; i < 4; i++) await Promise.resolve();
}

describe("BlitSurfaceCanvas paste", () => {
  beforeEach(() => {
    // The paste chord reads the clipboard unconditionally; keep it denied so
    // the `paste` event stays the only source, as it is in Chromium.
    vi.stubGlobal("navigator", {
      ...navigator,
      clipboard: { readText: vi.fn().mockRejectedValue(new Error("denied")) },
    });
  });

  it("preserves an app-owned image selection instead of importing the browser clipboard", () => {
    const readText = vi.fn().mockResolvedValue("stale browser clipboard");
    Object.defineProperty(navigator, "clipboard", {
      configurable: true,
      value: { readText, read: vi.fn() },
    });
    const { clipboard, keys, pressCtrlV, dispose } = attachPasting(true);

    // dispatchEvent returns false when the handler cancelled the browser's
    // native paste.  The V press is sent straight to Wayland, whose current
    // image/png source answers the destination directly.
    expect(pressCtrlV()).toBe(false);
    expect(readText).not.toHaveBeenCalled();
    expect(clipboard).toEqual([]);
    expect(keys).toContainEqual({ keycode: EVDEV_V, pressed: true });

    dispose();
  });
  afterEach(() => {
    vi.unstubAllGlobals();
    vi.restoreAllMocks();
  });

  it("offers a pasted image to the surface, then presses V", async () => {
    const { clipboard, keys, pressCtrlV, firePaste, dispose } = attachPasting();
    const bytes = new Uint8Array([0x89, 0x50, 0x4e, 0x47]); // PNG magic
    pressCtrlV();
    firePaste({
      files: [new File([bytes], "clip.png", { type: "image/png" })],
    });
    await settle();

    expect(clipboard).toHaveLength(1);
    expect(clipboard[0].mime).toBe("image/png");
    expect(Array.from(clipboard[0].data)).toEqual(Array.from(bytes));
    // The selection has to be in place before the app sees the chord.
    expect(keys).toEqual([{ keycode: EVDEV_V, pressed: true }]);
    dispose();
  });

  it("does not request an eager rich read where Ctrl+V fires a paste event", async () => {
    const read = vi.fn();
    Object.defineProperty(navigator, "clipboard", {
      configurable: true,
      value: {
        readText: vi.fn().mockReturnValue(new Promise(() => {})),
        read,
      },
    });
    const { clipboard, pressCtrlV, firePaste, dispose } = attachPasting();
    pressCtrlV();

    // Non-macOS browsers authorize and deliver the normal paste event.  A
    // second async read here would create an unnecessary permission prompt.
    expect(read).not.toHaveBeenCalled();
    firePaste({
      files: [
        new File([new Uint8Array([1])], "clip.png", { type: "image/png" }),
      ],
    });
    await settle();

    expect(clipboard).toHaveLength(1);
    expect(read).not.toHaveBeenCalled();
    dispose();
  });

  it("prefers text when the clipboard carries both", async () => {
    const { clipboard, pressCtrlV, firePaste, dispose } = attachPasting();
    pressCtrlV();
    // What a spreadsheet range puts on the clipboard: the cells as text, and
    // a picture of the same cells.  Pasting is expected to produce the text.
    firePaste({
      text: "a\tb",
      files: [
        new File([new Uint8Array([1])], "cells.png", { type: "image/png" }),
      ],
    });
    await settle();

    expect(clipboard).toHaveLength(1);
    expect(clipboard[0].mime).toBe("text/plain;charset=utf-8");
    expect(new TextDecoder().decode(clipboard[0].data)).toBe("a\tb");
    dispose();
  });

  it("prefers PNG over the other image types on offer", async () => {
    const { clipboard, pressCtrlV, firePaste, dispose } = attachPasting();
    pressCtrlV();
    firePaste({
      files: [
        new File([new Uint8Array([1])], "clip.jpg", { type: "image/jpeg" }),
        new File([new Uint8Array([2])], "clip.png", { type: "image/png" }),
      ],
    });
    await settle();

    expect(clipboard).toHaveLength(1);
    expect(clipboard[0].mime).toBe("image/png");
    dispose();
  });

  it("drops an image too large for one protocol frame", async () => {
    const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
    const { clipboard, keys, pressCtrlV, firePaste, dispose } = attachPasting();
    pressCtrlV();
    firePaste({
      files: [
        new File([new Uint8Array(9 * 1024 * 1024)], "huge.png", {
          type: "image/png",
        }),
      ],
    });
    await settle();

    // Nothing on the wire — an over-length CLIPBOARD_SET is refused by the
    // server, not truncated — and no V either.  Pressing it would paste
    // whatever the selection held before, which is not what was copied.
    expect(clipboard).toHaveLength(0);
    expect(keys).toEqual([]);
    expect(warn).toHaveBeenCalled();
    dispose();
  });

  it("gives up rather than press V when the image cannot be read", async () => {
    const { clipboard, keys, pressCtrlV, firePaste, dispose } = attachPasting();
    const file = new File([new Uint8Array([1])], "clip.png", {
      type: "image/png",
    });
    // A blob the browser can name but not hand over.
    Object.defineProperty(file, "arrayBuffer", {
      value: () => Promise.reject(new Error("unreadable")),
    });
    pressCtrlV();
    firePaste({ files: [file] });
    await settle();

    expect(clipboard).toHaveLength(0);
    expect(keys).toEqual([]);
    dispose();
  });

  it("forwards one paste once, however many listeners see it", async () => {
    const { clipboard, pressCtrlV, firePaste, dispose } = attachPasting();
    pressCtrlV();
    // The canvas listener and the document-level capture listener are both on
    // this event's path.  Forwarding from each would put the image on the
    // wire twice — cheap for text, megabytes for a screenshot.
    firePaste({
      files: [
        new File([new Uint8Array(1024)], "clip.png", { type: "image/png" }),
      ],
    });
    await settle();

    expect(clipboard).toHaveLength(1);
    dispose();
  });

  it("sends a bare paste with no chord in flight straight through", async () => {
    const { clipboard, keys, firePaste, dispose } = attachPasting();
    // A context-menu paste: no Ctrl+V, so no key to defer.
    firePaste({
      files: [
        new File([new Uint8Array([7])], "clip.png", { type: "image/png" }),
      ],
    });
    await settle();

    expect(clipboard).toHaveLength(1);
    expect(clipboard[0].mime).toBe("image/png");
    expect(keys).toEqual([]);
    dispose();
  });

  it("releases a Cmd chord's V with its press — macOS eats the key-up", async () => {
    const { clipboard, keys, canvas, firePaste, dispose } = attachPasting();
    // Chrome on macOS consumes Cmd+V as the Paste menu command: the page
    // sees the keydown and the paste event, but the V key-up never
    // arrives.  Waiting for it would leave V held at the compositor,
    // key-repeating the paste forever.
    const key = (
      type: "keydown" | "keyup",
      k: string,
      code: string,
      meta: boolean,
    ) =>
      canvas.dispatchEvent(
        new KeyboardEvent(type, {
          key: k,
          code,
          metaKey: meta,
          bubbles: true,
          cancelable: true,
        }),
      );
    key("keydown", "Meta", "MetaLeft", true);
    key("keydown", "v", "KeyV", true);
    firePaste({
      files: [
        new File([new Uint8Array([1])], "clip.png", { type: "image/png" }),
      ],
    });
    await settle();
    // A late V key-up, if a browser ever delivers one, must be inert.
    key("keyup", "v", "KeyV", true);
    key("keyup", "Meta", "MetaLeft", false);
    await settle();

    expect(clipboard).toHaveLength(1);
    expect(keys).toEqual([
      { keycode: 125, pressed: true }, // MetaLeft in…
      { keycode: 125, pressed: false }, // …swapped for Ctrl — Wayland apps paste on Ctrl+V
      { keycode: 29, pressed: true },
      { keycode: EVDEV_V, pressed: true },
      { keycode: EVDEV_V, pressed: false }, // sent with the press, not awaited
      { keycode: 29, pressed: false }, // the physical Cmd key-up
    ]);
    dispose();
  });

  it("starts the image read while the Ctrl keydown still has user activation", async () => {
    // macOS Chrome Ctrl+V: no menu command, no paste event — readText
    // resolves "" for an image-only clipboard.  clipboard.read() must start
    // before that promise settles or macOS browsers can reject it after the
    // key event's transient user activation has expired.
    const bytes = new Uint8Array([0x89, 0x50, 0x4e, 0x47]);
    let resolveText: (text: string) => void = () => {};
    const readText = vi.fn(
      () =>
        new Promise<string>((resolve) => {
          resolveText = resolve;
        }),
    );
    const read = vi.fn().mockResolvedValue([
      {
        types: ["image/png"],
        getType: (mime: string) =>
          Promise.resolve(new Blob([bytes], { type: mime })),
      },
    ]);
    vi.stubGlobal("navigator", {
      ...navigator,
      platform: "MacIntel",
      clipboard: {
        readText,
        read,
      },
    });
    const { clipboard, keys, canvas, pressCtrlV, dispose } = attachPasting();
    pressCtrlV();

    // readText is still pending, but the richer read has already captured
    // the keydown's authorization window.
    expect(readText).toHaveBeenCalledOnce();
    expect(read).toHaveBeenCalledOnce();
    expect(clipboard).toHaveLength(0);

    resolveText("");
    await settle();
    await settle();

    expect(clipboard).toHaveLength(1);
    expect(clipboard[0].mime).toBe("image/png");
    expect(Array.from(clipboard[0].data)).toEqual(Array.from(bytes));
    expect(keys).toEqual([{ keycode: EVDEV_V, pressed: true }]);

    // Ctrl+V keeps its key-up: only Cmd chords release with the press.
    canvas.dispatchEvent(
      new KeyboardEvent("keyup", {
        key: "v",
        code: "KeyV",
        bubbles: true,
        cancelable: true,
      }),
    );
    expect(keys).toEqual([
      { keycode: EVDEV_V, pressed: true },
      { keycode: EVDEV_V, pressed: false },
    ]);
    dispose();
  });

  it("never reads the clipboard directly for a Cmd chord", async () => {
    // The macOS paste command always follows Cmd+V with a paste event; it
    // may trail the readText settle by a task, but it owns the chord.
    // Reading directly anyway would race it — and needlessly prompt for
    // the clipboard-read permission.
    const read = vi.fn();
    vi.stubGlobal("navigator", {
      ...navigator,
      clipboard: {
        readText: vi.fn().mockResolvedValue(""),
        read,
      },
    });
    const { clipboard, canvas, firePaste, dispose } = attachPasting();
    canvas.dispatchEvent(
      new KeyboardEvent("keydown", {
        key: "Meta",
        code: "MetaLeft",
        metaKey: true,
        bubbles: true,
        cancelable: true,
      }),
    );
    canvas.dispatchEvent(
      new KeyboardEvent("keydown", {
        key: "v",
        code: "KeyV",
        metaKey: true,
        bubbles: true,
        cancelable: true,
      }),
    );
    firePaste({
      files: [
        new File([new Uint8Array([1])], "clip.png", { type: "image/png" }),
      ],
    });
    await settle();

    expect(clipboard).toHaveLength(1);
    expect(read).not.toHaveBeenCalled();
    dispose();
  });

  it("stands a Ctrl chord down on a clipboard with nothing pastable", async () => {
    // Empty clipboard, no paste event: the chord ends with no V pressed —
    // decided by the reads settling, not by a timer.
    vi.stubGlobal("navigator", {
      ...navigator,
      clipboard: {
        readText: vi.fn().mockResolvedValue(""),
        read: vi.fn().mockResolvedValue([]),
      },
    });
    const { clipboard, keys, canvas, pressCtrlV, dispose } = attachPasting();
    canvas.dispatchEvent(
      new KeyboardEvent("keydown", {
        key: "Control",
        code: "ControlLeft",
        ctrlKey: true,
        bubbles: true,
        cancelable: true,
      }),
    );
    pressCtrlV();
    // Releasing both keys mid-read defers both releases…
    canvas.dispatchEvent(
      new KeyboardEvent("keyup", {
        key: "v",
        code: "KeyV",
        ctrlKey: true,
        bubbles: true,
        cancelable: true,
      }),
    );
    canvas.dispatchEvent(
      new KeyboardEvent("keyup", {
        key: "Control",
        code: "ControlLeft",
        bubbles: true,
        cancelable: true,
      }),
    );
    await settle();
    await settle();

    // …and the stand-down releases the deferred Ctrl without pressing V.
    expect(clipboard).toHaveLength(0);
    expect(keys).toEqual([
      { keycode: 29, pressed: true }, // the physical Ctrl keydown
      { keycode: 29, pressed: false }, // released by the stand-down
    ]);
    dispose();
  });

  it("stands the chord down when focus leaves mid-read", async () => {
    // A readText that never settles stands in for a permission prompt;
    // the user clicking away is the event that ends the chord.
    vi.stubGlobal("navigator", {
      ...navigator,
      clipboard: { readText: vi.fn().mockReturnValue(new Promise(() => {})) },
    });
    const { clipboard, keys, canvas, pressCtrlV, dispose } = attachPasting();
    canvas.dispatchEvent(
      new KeyboardEvent("keydown", {
        key: "Control",
        code: "ControlLeft",
        ctrlKey: true,
        bubbles: true,
        cancelable: true,
      }),
    );
    pressCtrlV();
    canvas.dispatchEvent(new FocusEvent("blur"));
    await settle();

    expect(clipboard).toHaveLength(0);
    expect(keys).toEqual([
      { keycode: 29, pressed: true },
      { keycode: 29, pressed: false },
    ]);
    dispose();
  });
});

/** One drag session message, in wire order. */
type DragSend =
  | {
      kind: "enter";
      id: number;
      x: number;
      y: number;
      mimes: string[];
      items?: string[];
    }
  | { kind: "motion"; id: number; x: number; y: number }
  | { kind: "leave"; id: number }
  | {
      kind: "drop";
      id: number;
      x: number;
      y: number;
      items: { mime: string; name: string; data: Uint8Array }[];
    }
  | { kind: "cancel" };

/** A canvas wired for drag-and-drop: captures the session messages in
 *  order, with the drawn region declared 1:1 with the surface so client
 *  coordinates are surface coordinates.  File drops stage through a mocked
 *  per-canvas staging sync: `uploads` records what the pump was asked to
 *  send, and `syncFs` can be made to reject for the open-failure path. */
function attachDragging() {
  const sends: DragSend[] = [];
  const uploads: { path: string; file: File }[] = [];
  const stagingHandle = {
    upload: vi.fn((path: string, file: File) => {
      uploads.push({ path, file });
      return Promise.resolve({});
    }),
    stop: vi.fn(),
  };
  const conn = {
    sendSurfaceDragEnter: (
      id: number,
      x: number,
      y: number,
      mimes: string[],
      items?: string[],
    ) => sends.push({ kind: "enter", id, x, y, mimes, items }),
    sendSurfaceDragMotion: (id: number, x: number, y: number) =>
      sends.push({ kind: "motion", id, x, y }),
    sendSurfaceDragLeave: (id: number) => sends.push({ kind: "leave", id }),
    sendSurfaceDragDrop: (
      id: number,
      x: number,
      y: number,
      items: { mime: string; name: string; data: Uint8Array }[],
    ) => sends.push({ kind: "drop", id, x, y, items }),
    sendSurfaceDragCancel: () => sends.push({ kind: "cancel" }),
    sendSurfaceFocus: () => {},
    syncFs: vi.fn((_path: string, _options?: unknown) =>
      Promise.resolve(stagingHandle),
    ),
    surfaceStore: new Proxy(
      {
        getSurface: () => ({ width: 800, height: 600 }),
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

  /** One mount of the shared surface: its own canvas, sized like a live
   *  view.  Several mounts of one surface_id is a normal BSP situation
   *  (the same app in two panes) and the drag handlers must cope. */
  const makeMount = () => {
    const surface = new BlitSurfaceCanvas({
      workspace,
      connectionId: "conn-1" as never,
      surfaceId: 7,
    });
    const container = document.createElement("div");
    document.body.appendChild(container);
    surface.attach(container);
    const canvas = surface.canvasElement;
    if (!canvas) throw new Error("Expected surface canvas");
    // Only a live view takes input.
    surface.setDisplaySize(800, 600, 120);
    // jsdom lays nothing out, so the drawn region has to be declared.
    canvas.getBoundingClientRect = () =>
      ({ left: 0, top: 0, width: 800, height: 600 }) as DOMRect;

    /** Dispatch one drag event with a dataTransfer carrying any mix of
     *  types, files, items and plain text. */
    const fireDrag = (
      type: "dragenter" | "dragover" | "dragleave" | "drop",
      opts: {
        x?: number;
        y?: number;
        types?: string[];
        files?: File[];
        items?: { kind: string; type: string; file?: File | null }[];
        text?: string;
        relatedTarget?: EventTarget | null;
      } = {},
    ) => {
      const items = (
        opts.items ??
        (opts.files ?? []).map((f) => ({ kind: "file", type: f.type, file: f }))
      ).map(
        (it) =>
          ({
            kind: it.kind,
            type: it.type,
            getAsFile: () => it.file ?? null,
          }) as unknown as DataTransferItem,
      );
      const dataTransfer = {
        types: opts.types ?? (opts.files ? ["Files"] : ["text/plain"]),
        files: opts.files ?? [],
        items,
        getData: (mime: string) =>
          mime === "text/plain" ? (opts.text ?? "") : "",
        dropEffect: "none",
      } as unknown as DataTransfer;
      // jsdom has no DragEvent; a MouseEvent carries the client coords and
      // relatedTarget the handlers read.
      const ev = new MouseEvent(type, {
        clientX: opts.x ?? 0,
        clientY: opts.y ?? 0,
        bubbles: true,
        cancelable: true,
        relatedTarget: opts.relatedTarget ?? null,
      });
      Object.defineProperty(ev, "dataTransfer", { value: dataTransfer });
      canvas.dispatchEvent(ev);
      return ev;
    };

    const dispose = () => {
      surface.dispose();
      container.remove();
    };

    return { surface, canvas, fireDrag, dispose };
  };

  const first = makeMount();

  return {
    surface: first.surface,
    canvas: first.canvas,
    sends,
    fireDrag: first.fireDrag,
    dispose: first.dispose,
    uploads,
    stagingHandle,
    syncFs: conn.syncFs,
    attachMount: makeMount,
  };
}

describe("BlitSurfaceCanvas drag-and-drop", () => {
  it("sends ENTER with the offered MIMEs and surface coords on dragenter", () => {
    const { sends, fireDrag, dispose } = attachDragging();
    fireDrag("dragenter", { x: 100, y: 200, types: ["Files"] });
    expect(sends).toEqual([
      {
        kind: "enter",
        id: 7,
        x: 100,
        y: 200,
        mimes: ["text/uri-list", "application/octet-stream"],
      },
    ]);
    expect(sends[0].kind === "enter" && sends[0].items).toBeUndefined();
    dispose();
  });

  it("sends the file items' MIMEs with ENTER, in item order", () => {
    const { sends, fireDrag, dispose } = attachDragging();
    fireDrag("dragenter", {
      x: 1,
      y: 2,
      types: ["Files"],
      items: [
        { kind: "file", type: "image/png" },
        { kind: "string", type: "text/plain" },
        { kind: "file", type: "image/jpeg" },
        { kind: "file", type: "" },
      ],
    });
    const enter = sends[0];
    if (enter.kind !== "enter") throw new Error("Expected an ENTER");
    // File-kind items only; a typeless one falls back to octet-stream.
    expect(enter.items).toEqual([
      "image/png",
      "image/jpeg",
      "application/octet-stream",
    ]);
    dispose();
  });

  it("offers plain-text MIMEs for a text drag", () => {
    const { sends, fireDrag, dispose } = attachDragging();
    fireDrag("dragenter", { x: 1, y: 2, types: ["text/plain"] });
    expect(sends).toEqual([
      {
        kind: "enter",
        id: 7,
        x: 1,
        y: 2,
        mimes: ["text/plain;charset=utf-8", "text/plain"],
      },
    ]);
    // A text drag carries no items trailer.
    expect(sends[0].kind === "enter" && sends[0].items).toBeUndefined();
    dispose();
  });

  it("sends MOTION on dragover and claims the event", () => {
    const { sends, fireDrag, dispose } = attachDragging();
    fireDrag("dragenter", { x: 0, y: 0, types: ["Files"] });
    const ev = fireDrag("dragover", { x: 10, y: 20, types: ["Files"] });
    expect(ev.defaultPrevented).toBe(true);
    expect(
      (ev as unknown as { dataTransfer: DataTransfer }).dataTransfer.dropEffect,
    ).toBe("copy");
    expect(sends.at(-1)).toEqual({ kind: "motion", id: 7, x: 10, y: 20 });
    dispose();
  });

  it("sends LEAVE only when the drag leaves the canvas itself", () => {
    vi.useFakeTimers();
    const { sends, canvas, fireDrag, dispose } = attachDragging();
    fireDrag("dragenter", { x: 0, y: 0, types: ["Files"] });
    // Crossing into a child is not leaving the surface.
    fireDrag("dragleave", { types: ["Files"], relatedTarget: canvas });
    expect(sends).toHaveLength(1);
    // A genuine exit follows the hover, not the ENTER by milliseconds (see
    // the stale-LEAVE filter for the DOM enter-before-leave order).
    vi.advanceTimersByTime(1000);
    fireDrag("dragleave", { types: ["Files"] });
    expect(sends.at(-1)).toEqual({ kind: "leave", id: 7 });
    vi.useRealTimers();
    dispose();
  });

  it("ignores a belated LEAVE from another mount of the same surface", () => {
    // DOM fires dragenter on the new target BEFORE dragleave on the old
    // one: crossing between two mounts of one surface would otherwise kill
    // the session the second ENTER just retargeted.
    vi.useFakeTimers();
    const first = attachDragging();
    const second = first.attachMount();
    try {
      first.fireDrag("dragenter", { x: 1, y: 1, types: ["Files"] });
      expect(first.sends).toHaveLength(1);
      second.fireDrag("dragenter", { x: 2, y: 2, types: ["Files"] });
      expect(first.sends).toHaveLength(2);
      // The old mount's belated dragleave, moments after the second ENTER:
      // the DOM order artifact, not an exit — it must not go out.
      first.fireDrag("dragleave", {
        types: ["Files"],
        relatedTarget: second.canvas,
      });
      expect(first.sends.filter((s) => s.kind === "leave")).toHaveLength(0);
      // A genuine exit (after the hover) still leaves.
      vi.advanceTimersByTime(1000);
      second.fireDrag("dragleave", { types: ["Files"] });
      expect(first.sends.at(-1)).toEqual({ kind: "leave", id: 7 });
    } finally {
      vi.useRealTimers();
      second.dispose();
      first.dispose();
    }
  });

  it("stages dropped files, then sends DROP naming them with empty data", async () => {
    const { sends, fireDrag, dispose, uploads, syncFs } = attachDragging();
    fireDrag("dragenter", { x: 100, y: 200, types: ["Files"] });
    fireDrag("drop", {
      x: 100,
      y: 200,
      types: ["Files"],
      files: [
        new File([new Uint8Array([0x89, 0x50])], "a b.png", {
          type: "image/png",
        }),
      ],
    });
    await settle();

    // The staging sync opened with the flag and an empty path, and the
    // file was staged under the planned name ENTER pre-announced.
    expect(syncFs).toHaveBeenCalledWith(
      "",
      expect.objectContaining({ staging: true }),
    );
    expect(uploads.map((u) => u.path)).toEqual(["0.png"]);

    const drop = sends.find((s) => s.kind === "drop");
    if (!drop || drop.kind !== "drop") throw new Error("Expected a DROP");
    expect(drop.id).toBe(7);
    expect(drop.x).toBe(100);
    expect(drop.y).toBe(200);
    expect(drop.items).toHaveLength(1);
    expect(drop.items[0].mime).toBe("image/png");
    expect(drop.items[0].name).toBe("0.png");
    expect(drop.items[0].data).toHaveLength(0);

    // The wire layout, pinned: the Rust compositor pins this exact fixture.
    // [0x38][surface:2][x:2][y:2][items:2][mime_len:2][mime][name_len:2]
    // [name][data_len:4][data] — a staged item names its planned
    // staging-relative path and carries no inline data.
    const msg = buildSurfaceDragDropMessage(7, 100, 200, drop.items);
    const hex = Array.from(msg, (b) => b.toString(16).padStart(2, "0")).join(
      "",
    );
    expect(hex).toBe(
      "38" + // C2S_SURFACE_DRAG_DROP
        "0700" + // surface 7
        "6400" + // x 100
        "c800" + // y 200
        "0100" + // one item
        "0900" +
        "696d6167652f706e67" + // "image/png"
        "0500" +
        "302e706e67" + // "0.png"
        "00000000", // staged: no inline data
    );
    dispose();
  });

  it("stages each file under the planned name of its own MIME type", async () => {
    const { sends, fireDrag, dispose, uploads } = attachDragging();
    fireDrag("drop", {
      x: 1,
      y: 2,
      types: ["Files"],
      files: [
        new File([new Uint8Array([1])], "cat photo.PNG", {
          type: "image/png",
        }),
        new File([new Uint8Array([2])], "notes", { type: "image/jpeg" }),
        new File([new Uint8Array([3])], "archive.zip", { type: "" }),
      ],
    });
    // The staging open plus three sequential uploads outlast one drain.
    await settle();
    await settle();
    expect(uploads.map((u) => u.path)).toEqual(["0.png", "1.jpg", "2.bin"]);
    const drop = sends.find((s) => s.kind === "drop");
    if (!drop || drop.kind !== "drop") throw new Error("Expected a DROP");
    expect(drop.items.map((item) => item.name)).toEqual([
      "0.png",
      "1.jpg",
      "2.bin",
    ]);
    dispose();
  });

  it("uses the hover plan when the file MIME changes at drop", async () => {
    const { sends, fireDrag, dispose, uploads } = attachDragging();
    // A typeless promise is announced as 0.bin.  The materialized File may
    // reveal image/png after release, but the URI already served to the app
    // still names 0.bin and therefore remains authoritative.
    fireDrag("dragenter", {
      types: ["Files"],
      items: [{ kind: "file", type: "" }],
    });
    fireDrag("drop", {
      types: ["Files"],
      files: [
        new File([new Uint8Array([1])], "shot.png", { type: "image/png" }),
      ],
    });
    await settle();

    expect(uploads.map((u) => u.path)).toEqual(["0.bin"]);
    const drop = sends.find((s) => s.kind === "drop");
    if (!drop || drop.kind !== "drop") throw new Error("Expected a DROP");
    expect(drop.items[0]).toEqual(
      expect.objectContaining({ mime: "image/png", name: "0.bin" }),
    );
    dispose();
  });

  it("cancels when the file count changes after the hover plan", async () => {
    const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
    const { sends, fireDrag, dispose, uploads } = attachDragging();
    fireDrag("dragenter", {
      types: ["Files"],
      items: [{ kind: "file", type: "image/png" }],
    });
    fireDrag("drop", {
      types: ["Files"],
      files: [
        new File([new Uint8Array([1])], "one.png", { type: "image/png" }),
        new File([new Uint8Array([2])], "two.png", { type: "image/png" }),
      ],
    });
    await settle();

    expect(uploads).toHaveLength(0);
    expect(sends.at(-1)).toEqual({ kind: "cancel" });
    expect(warn).toHaveBeenCalled();
    dispose();
  });

  it("accepts a macOS file-promise drop (file items, no Files type)", async () => {
    const { sends, fireDrag, dispose, uploads } = attachDragging();
    // The screenshot's floating thumbnail: no "Files" type, an empty files
    // list, one file-kind item.
    const file = new File([new Uint8Array([0x89, 0x50])], "shot.png", {
      type: "image/png",
    });
    const items = [{ kind: "file", type: "image/png", file }];
    fireDrag("dragenter", { x: 1, y: 2, types: [], items });
    const over = fireDrag("dragover", { x: 1, y: 2, types: [], items });
    expect(over.defaultPrevented).toBe(true);
    fireDrag("drop", { x: 1, y: 2, types: [], items });
    await settle();

    // The item MIME rode ENTER, and the file staged under the planned name.
    const enter = sends[0];
    if (enter.kind !== "enter") throw new Error("Expected an ENTER");
    expect(enter.items).toEqual(["image/png"]);
    expect(uploads.map((u) => u.path)).toEqual(["0.png"]);
    expect(sends.find((s) => s.kind === "drop")).toBeTruthy();
    dispose();
  });

  it("names a nameless promised file from its MIME type", async () => {
    const { fireDrag, dispose, uploads } = attachDragging();
    fireDrag("drop", {
      types: ["Files"],
      files: [new File([new Uint8Array([1])], "", { type: "image/png" })],
    });
    await settle();

    expect(uploads.map((u) => u.path)).toEqual(["0.png"]);
    dispose();
  });

  it("cancels when a file drag carries no readable files", async () => {
    const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
    const { sends, fireDrag, dispose } = attachDragging();
    fireDrag("drop", {
      x: 1,
      y: 2,
      types: [],
      items: [{ kind: "file", type: "image/png", file: null }],
    });
    await settle();

    expect(sends).toEqual([{ kind: "cancel" }]);
    expect(warn).toHaveBeenCalled();
    dispose();
  });

  it("sends DROP with the text for a text drag", async () => {
    const { sends, fireDrag, dispose, syncFs } = attachDragging();
    fireDrag("dragenter", { x: 5, y: 6, types: ["text/plain"] });
    fireDrag("drop", { x: 5, y: 6, types: ["text/plain"], text: "héllo" });
    await settle();

    const drop = sends.find((s) => s.kind === "drop");
    if (!drop || drop.kind !== "drop") throw new Error("Expected a DROP");
    expect(drop.items).toHaveLength(1);
    expect(drop.items[0].mime).toBe("text/plain;charset=utf-8");
    expect(drop.items[0].name).toBe("");
    expect(new TextDecoder().decode(drop.items[0].data)).toBe("héllo");
    // Text stays inline: no staging sync is opened for it.
    expect(syncFs).not.toHaveBeenCalled();
    dispose();
  });

  it("refuses inline text approaching the frame cap", async () => {
    const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
    const { sends, fireDrag, dispose } = attachDragging();
    fireDrag("dragenter", { x: 0, y: 0, types: ["text/plain"] });
    fireDrag("drop", {
      types: ["text/plain"],
      text: "x".repeat(16 * 1024 * 1024),
    });
    await settle();

    // An over-length DROP is refused by the server, not truncated — so it
    // never goes on the wire.  Only inline items (dragged text) are capped;
    // files stage through the upload pump and carry no inline bytes.
    expect(sends.filter((s) => s.kind === "drop")).toHaveLength(0);
    expect(warn).toHaveBeenCalled();
    dispose();
  });

  it("reuses the staging sync across drops and stops it on dispose", async () => {
    const { fireDrag, dispose, syncFs, stagingHandle } = attachDragging();
    fireDrag("drop", {
      types: ["Files"],
      files: [new File([new Uint8Array([1])], "a.txt")],
    });
    await settle();
    fireDrag("drop", {
      types: ["Files"],
      files: [new File([new Uint8Array([2])], "b.txt")],
    });
    await settle();
    expect(syncFs).toHaveBeenCalledTimes(1);
    expect(stagingHandle.stop).not.toHaveBeenCalled();
    dispose();
    expect(stagingHandle.stop).toHaveBeenCalledTimes(1);
  });

  it("cancels the session when the staging sync cannot be opened", async () => {
    const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
    const { sends, fireDrag, dispose, syncFs } = attachDragging();
    syncFs.mockRejectedValueOnce(new Error("no staging"));
    fireDrag("dragenter", { x: 0, y: 0, types: ["Files"] });
    fireDrag("drop", {
      types: ["Files"],
      files: [
        new File([new Uint8Array([1])], "clip.png", { type: "image/png" }),
      ],
    });
    await settle();

    expect(sends.filter((s) => s.kind === "drop")).toHaveLength(0);
    expect(sends.at(-1)).toEqual({ kind: "cancel" });
    expect(warn).toHaveBeenCalled();
    dispose();
  });

  it("cancels the session when a staging upload fails", async () => {
    const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
    const { sends, fireDrag, dispose, stagingHandle } = attachDragging();
    stagingHandle.upload.mockRejectedValueOnce(new Error("disk full"));
    fireDrag("dragenter", { x: 0, y: 0, types: ["Files"] });
    fireDrag("drop", {
      types: ["Files"],
      files: [
        new File([new Uint8Array([1])], "clip.png", { type: "image/png" }),
      ],
    });
    await settle();

    expect(sends.filter((s) => s.kind === "drop")).toHaveLength(0);
    expect(sends.at(-1)).toEqual({ kind: "cancel" });
    expect(warn).toHaveBeenCalled();
    dispose();
  });

  it("cancels an open session if a dragend fires", () => {
    const { sends, fireDrag, dispose } = attachDragging();
    fireDrag("dragenter", { x: 0, y: 0, types: ["Files"] });
    window.dispatchEvent(new Event("dragend"));
    expect(sends.at(-1)).toEqual({ kind: "cancel" });
    dispose();
  });

  it("leaves internal UI drags untouched", () => {
    const { sends, fireDrag, dispose } = attachDragging();
    // Pane/tile moves inside the page carry only custom MIMEs.
    const types = ["application/x-blit-tile"];
    const enter = fireDrag("dragenter", { types });
    const over = fireDrag("dragover", { types });
    const drop = fireDrag("drop", { types });
    expect(enter.defaultPrevented).toBe(false);
    expect(over.defaultPrevented).toBe(false);
    expect(drop.defaultPrevented).toBe(false);
    expect(sends).toEqual([]);
    dispose();
  });
});

/** A live view with the text and key sends captured — what soft-keyboard
 *  input lands on. */
function attachTyping() {
  const texts: string[] = [];
  const keys: { keycode: number; pressed: boolean }[] = [];
  const preedits: { text: string; cursor: number }[] = [];
  const pointers: { type: number; button: number }[] = [];
  const conn = {
    sendSurfaceText: (_id: number, text: string) => texts.push(text),
    sendSurfaceInput: (_id: number, keycode: number, pressed: boolean) =>
      keys.push({ keycode, pressed }),
    sendSurfacePreedit: (_id: number, text: string, cursor: number) =>
      preedits.push({ text, cursor }),
    sendSurfacePointer: (_id: number, type: number, button: number) =>
      pointers.push({ type, button }),
    sendSurfaceFocus: () => {},
    surfaceStore: new Proxy(
      {
        getSurface: () => ({ width: 800, height: 600 }),
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
  const ta = container.querySelector<HTMLTextAreaElement>(
    'textarea[aria-label="Surface input"]',
  );
  if (!ta) throw new Error("Expected surface input textarea");
  // Only live views take input.
  surface.setDisplaySize(800, 600, 120);
  return { surface, canvas, ta, texts, keys, preedits, pointers };
}

function inputEvent(init: InputEventInit): InputEvent {
  return new InputEvent("input", { cancelable: false, ...init });
}

describe("BlitSurfaceCanvas soft-keyboard input", () => {
  it("labels the hidden IME textarea so the keyboard toggle can find it", () => {
    const { surface, canvas, ta } = attachTyping();
    // Same container as the canvas: the UI resolves the textarea from the
    // canvas via parentElement when redirecting focus.
    expect(ta.parentElement).toBe(canvas.parentElement);
    expect(ta.tabIndex).toBe(-1);
    surface.dispose();
  });

  it("forwards a keydown-less insertText commit as surface text", () => {
    const { surface, ta, texts } = attachTyping();
    ta.value = "hi";
    ta.dispatchEvent(inputEvent({ inputType: "insertText", data: "hi" }));
    expect(texts).toEqual(["hi"]);
    expect(ta.value).toBe("");
    surface.dispose();
  });

  it("maps input-event line breaks and deletes onto Enter and Backspace", () => {
    const { surface, ta, keys } = attachTyping();
    ta.dispatchEvent(inputEvent({ inputType: "insertLineBreak" }));
    ta.dispatchEvent(inputEvent({ inputType: "deleteContentBackward" }));
    expect(keys).toEqual([
      { keycode: 28, pressed: true },
      { keycode: 28, pressed: false },
      { keycode: 14, pressed: true },
      { keycode: 14, pressed: false },
    ]);
    surface.dispose();
  });

  it("keeps ignoring composition, paste, and composition-commit inputs", () => {
    const { surface, ta, texts, keys } = attachTyping();
    // Mid-composition text is a preedit, not a commit — the commit belongs
    // to compositionend; the trailing insertCompositionText some browsers
    // fire after it was already sent there; pastes go through the clipboard
    // path.
    ta.dispatchEvent(
      inputEvent({ inputType: "insertText", data: "あ", isComposing: true }),
    );
    ta.dispatchEvent(
      inputEvent({ inputType: "insertCompositionText", data: "あ" }),
    );
    ta.dispatchEvent(inputEvent({ inputType: "insertFromPaste", data: "x" }));
    expect(texts).toEqual([]);
    expect(keys).toEqual([]);
    surface.dispose();
  });

  it("does not cancel a soft-keyboard keydown it cannot map", () => {
    const { surface, ta } = attachTyping();
    // keyCode-229 stand-in: no key name, no code.  preventDefault here
    // would cancel the input event that carries the actual text.
    const synthetic = new KeyboardEvent("keydown", {
      key: "Unidentified",
      code: "",
      cancelable: true,
    });
    ta.dispatchEvent(synthetic);
    expect(synthetic.defaultPrevented).toBe(false);
    // A key the evdev path can map keeps being claimed.
    const arrow = new KeyboardEvent("keydown", {
      key: "ArrowDown",
      code: "ArrowDown",
      cancelable: true,
    });
    ta.dispatchEvent(arrow);
    expect(arrow.defaultPrevented).toBe(true);
    surface.dispose();
  });
});

describe("BlitSurfaceCanvas IME focus", () => {
  /** Focus only moves for real on an element in the document. */
  function attachLive() {
    const live = attachTyping();
    const container = live.canvas.parentElement;
    if (!container) throw new Error("Expected surface container");
    document.body.appendChild(container);
    return { ...live, container };
  }

  it("hands focus from the canvas to the textarea", () => {
    // A canvas is not editable, so no browser will start a composition
    // while focus rests on it — an input method needs the textarea, and
    // focus reaches the canvas from outside this component (a pane taking
    // focus, Tab) as well as from its own pointer handler.
    const { surface, canvas, ta, container } = attachLive();

    canvas.focus();

    expect(document.activeElement).toBe(ta);
    surface.dispose();
    container.remove();
  });

  it("reports the composition in progress, with the caret in it", () => {
    // The capture textarea is 1px and transparent, so the app drawing this
    // is the only way the user sees what they have typed so far.  Read from
    // the `input` event, where the value and caret are the ones on screen —
    // compositionupdate runs before the DOM is updated and reports the
    // previous caret, which pinned every composition's cursor to 0.
    const { surface, ta, preedits, container } = attachLive();
    ta.focus();

    ta.value = "にほn";
    ta.setSelectionRange(3, 3);
    ta.dispatchEvent(
      inputEvent({
        inputType: "insertCompositionText",
        data: "にほn",
        isComposing: true,
      }),
    );

    expect(preedits).toEqual([{ text: "にほn", cursor: 3 }]);
    surface.dispose();
    container.remove();
  });

  it("withdraws the preedit when a composition is cancelled", () => {
    // Nothing is committed, so nothing else will take back what the app is
    // still drawing.
    const { surface, ta, preedits, texts, container } = attachLive();
    ta.focus();
    ta.value = "に";
    ta.dispatchEvent(
      inputEvent({
        inputType: "insertCompositionText",
        data: "に",
        isComposing: true,
      }),
    );
    preedits.length = 0;

    ta.dispatchEvent(new CompositionEvent("compositionend", { data: "" }));

    expect(preedits).toEqual([{ text: "", cursor: 0 }]);
    expect(texts).toEqual([]);
    surface.dispose();
    container.remove();
  });

  it("keeps focus on the textarea across a composition", () => {
    // Returning focus to the canvas after each commit would end the *next*
    // composition before it began, which is every character after the first.
    const { surface, ta, texts, container } = attachLive();
    ta.focus();

    ta.dispatchEvent(
      new CompositionEvent("compositionend", { data: "日本語" }),
    );

    expect(texts).toEqual(["日本語"]);
    expect(document.activeElement).toBe(ta);
    surface.dispose();
    container.remove();
  });
});

describe("BlitSurfaceCanvas Command chords", () => {
  const key = (
    type: "keydown" | "keyup",
    init: KeyboardEventInit,
  ): KeyboardEvent =>
    new KeyboardEvent(type, { bubbles: true, cancelable: true, ...init });

  it("releases a macOS Cmd+A with its press when the browser eats keyup", () => {
    const { surface, canvas, keys } = attachTyping();
    (surface as unknown as { macOptionChars: boolean }).macOptionChars = true;

    canvas.dispatchEvent(
      key("keydown", { key: "Meta", code: "MetaLeft", metaKey: true }),
    );
    canvas.dispatchEvent(
      key("keydown", { key: "a", code: "KeyA", metaKey: true }),
    );
    // Chrome/Safari may omit the A key-up for a macOS menu command.  Cmd's
    // release must not leave A held remotely.
    canvas.dispatchEvent(
      key("keyup", { key: "Meta", code: "MetaLeft", metaKey: false }),
    );
    // If a browser does deliver A's key-up late, it remains inert.
    canvas.dispatchEvent(
      key("keyup", { key: "a", code: "KeyA", metaKey: false }),
    );

    expect(keys).toEqual([
      { keycode: 125, pressed: true },
      { keycode: 30, pressed: true },
      { keycode: 30, pressed: false },
      { keycode: 125, pressed: false },
    ]);
    surface.dispose();
  });

  it("keeps Linux Meta+A held until its real keyup", () => {
    const { surface, canvas, keys } = attachTyping();
    (surface as unknown as { macOptionChars: boolean }).macOptionChars = false;

    canvas.dispatchEvent(
      key("keydown", { key: "Meta", code: "MetaLeft", metaKey: true }),
    );
    canvas.dispatchEvent(
      key("keydown", { key: "a", code: "KeyA", metaKey: true }),
    );
    expect(keys).toEqual([
      { keycode: 125, pressed: true },
      { keycode: 30, pressed: true },
    ]);

    canvas.dispatchEvent(
      key("keyup", { key: "a", code: "KeyA", metaKey: true }),
    );
    canvas.dispatchEvent(
      key("keyup", { key: "Meta", code: "MetaLeft", metaKey: false }),
    );
    expect(keys).toEqual([
      { keycode: 125, pressed: true },
      { keycode: 30, pressed: true },
      { keycode: 30, pressed: false },
      { keycode: 125, pressed: false },
    ]);
    surface.dispose();
  });
});

describe("BlitSurfaceCanvas macOS dead keys", () => {
  const key = (
    type: "keydown" | "keyup",
    init: KeyboardEventInit,
  ): KeyboardEvent =>
    new KeyboardEvent(type, { bubbles: true, cancelable: true, ...init });

  /** The Alt deferral these flows exercise exists only where Option is a
   *  character modifier; jsdom's navigator does not claim to be one. */
  function attachMac() {
    const typing = attachTyping();
    (typing.surface as unknown as { macOptionChars: boolean }).macOptionChars =
      true;
    return typing;
  }

  it("never sends Alt for an Option+E dead-key composition", () => {
    // Option+E is the macOS acute-accent dead key: the browser reports the
    // Option press, then a "Dead" keydown, and the finished character
    // arrives as a composition commit.  Forwarding that Alt press made
    // Electron apps (Slack) open their menu bar and eat the é; Chromium
    // (Brave) has no menu bar, which is why it only broke there.
    const { surface, canvas, ta, texts, keys } = attachMac();

    canvas.dispatchEvent(
      key("keydown", { key: "Alt", code: "AltLeft", altKey: true }),
    );
    canvas.dispatchEvent(
      key("keydown", { key: "Dead", code: "KeyE", altKey: true }),
    );
    ta.dispatchEvent(new CompositionEvent("compositionend", { data: "é" }));
    canvas.dispatchEvent(
      key("keyup", { key: "e", code: "KeyE", altKey: true }),
    );
    canvas.dispatchEvent(key("keyup", { key: "Alt", code: "AltLeft" }));

    expect(texts).toEqual(["é"]);
    expect(keys).toEqual([]);
    surface.dispose();
  });

  it("delivers é exactly once across the full UI Events dead-key sequence", () => {
    // The spec's dead-key sequence (uievents §4.3.2, adapted to Option+E on
    // macOS): the completing keystroke's keydown carries the *composed*
    // character with isComposing=true, mid-composition keyups carry
    // isComposing=true, and the textarea's post-commit input event must not
    // re-send what compositionend already delivered.
    const { surface, canvas, ta, texts, keys, preedits } = attachMac();

    canvas.dispatchEvent(
      key("keydown", { key: "Alt", code: "AltLeft", altKey: true }),
    );
    canvas.dispatchEvent(
      key("keydown", { key: "Dead", code: "KeyE", altKey: true }),
    );
    ta.dispatchEvent(new CompositionEvent("compositionstart", { data: "" }));
    ta.value = "´";
    ta.dispatchEvent(
      new InputEvent("input", {
        data: "´",
        inputType: "insertCompositionText",
        isComposing: true,
      }),
    );
    canvas.dispatchEvent(
      key("keyup", {
        key: "Dead",
        code: "KeyE",
        altKey: true,
        isComposing: true,
      }),
    );
    canvas.dispatchEvent(
      key("keyup", { key: "Alt", code: "AltLeft", isComposing: true }),
    );
    // The completing keystroke: key is the composed character, still within
    // the composition session.
    canvas.dispatchEvent(
      key("keydown", { key: "é", code: "KeyE", isComposing: true }),
    );
    ta.dispatchEvent(new CompositionEvent("compositionend", { data: "é" }));
    ta.value = "é";
    ta.dispatchEvent(
      new InputEvent("input", {
        data: "é",
        inputType: "insertCompositionText",
      }),
    );
    canvas.dispatchEvent(key("keyup", { key: "e", code: "KeyE" }));

    expect(texts).toEqual(["é"]);
    expect(keys).toEqual([]);
    expect(preedits.map((p) => p.text)).toEqual(["´"]);
    surface.dispose();
  });

  it("forwards a real Alt chord with the press ahead of the key", () => {
    // Linux-style Alt+E (no dead key involved): the held-back press goes
    // out the moment the chord's key arrives, so the app sees the same
    // stream as before — Alt down, E down, E up, Alt up.
    const { surface, canvas, keys } = attachMac();

    canvas.dispatchEvent(
      key("keydown", { key: "Alt", code: "AltLeft", altKey: true }),
    );
    canvas.dispatchEvent(
      key("keydown", { key: "e", code: "KeyE", altKey: true }),
    );
    canvas.dispatchEvent(
      key("keyup", { key: "e", code: "KeyE", altKey: true }),
    );
    canvas.dispatchEvent(key("keyup", { key: "Alt", code: "AltLeft" }));

    expect(keys).toEqual([
      { keycode: 56, pressed: true },
      { keycode: 18, pressed: true },
      { keycode: 18, pressed: false },
      { keycode: 56, pressed: false },
    ]);
    surface.dispose();
  });

  it("delivers a bare Alt tap as press+release on key-up", () => {
    const { surface, canvas, keys } = attachMac();

    canvas.dispatchEvent(
      key("keydown", { key: "Alt", code: "AltRight", altKey: true }),
    );
    canvas.dispatchEvent(key("keyup", { key: "Alt", code: "AltRight" }));

    expect(keys).toEqual([
      { keycode: 100, pressed: true },
      { keycode: 100, pressed: false },
    ]);
    surface.dispose();
  });

  it("restores Alt when a chord follows an abandoned composition", () => {
    // Option+E started a dead key, but the next keydown is no composition
    // and Option is still held: the app needs Alt back for this chord.
    const { surface, canvas, keys } = attachMac();

    canvas.dispatchEvent(
      key("keydown", { key: "Alt", code: "AltLeft", altKey: true }),
    );
    canvas.dispatchEvent(
      key("keydown", { key: "Dead", code: "KeyE", altKey: true }),
    );
    canvas.dispatchEvent(
      key("keydown", { key: "k", code: "KeyK", altKey: true }),
    );
    canvas.dispatchEvent(
      key("keyup", { key: "k", code: "KeyK", altKey: true }),
    );
    canvas.dispatchEvent(key("keyup", { key: "Alt", code: "AltLeft" }));

    expect(keys).toEqual([
      { keycode: 56, pressed: true },
      { keycode: 37, pressed: true },
      { keycode: 37, pressed: false },
      { keycode: 56, pressed: false },
    ]);
    surface.dispose();
  });

  it("flushes a pending Alt ahead of a mouse press", () => {
    // Alt+click is a chord in plenty of apps; the deferred press must beat
    // the button onto the wire.
    const { surface, canvas, keys, pointers } = attachMac();
    canvas.getBoundingClientRect = () =>
      ({ left: 0, top: 0, width: 800, height: 600 }) as DOMRect;

    canvas.dispatchEvent(
      key("keydown", { key: "Alt", code: "AltLeft", altKey: true }),
    );
    canvas.dispatchEvent(
      new MouseEvent("mousedown", {
        bubbles: true,
        cancelable: true,
        button: 0,
        clientX: 10,
        clientY: 10,
        altKey: true,
      }),
    );

    expect(keys).toEqual([{ keycode: 56, pressed: true }]);
    expect(pointers).toEqual([{ type: SURFACE_POINTER_DOWN, button: 0 }]);
    surface.dispose();
  });

  it("sends a direct Option character as text, not an Alt chord", () => {
    // Option+F is no dead key: macOS resolves it to "ƒ" outright and the
    // browser reports a single non-ASCII key with altKey set.  Forwarding
    // it as Alt+F opens Slack's File menu; it has to go out as text, and
    // the held-back Alt belongs to the character as with a dead key.
    const { surface, canvas, texts, keys } = attachMac();

    canvas.dispatchEvent(
      key("keydown", { key: "Alt", code: "AltLeft", altKey: true }),
    );
    canvas.dispatchEvent(
      key("keydown", { key: "ƒ", code: "KeyF", altKey: true }),
    );
    canvas.dispatchEvent(
      key("keyup", { key: "ƒ", code: "KeyF", altKey: true }),
    );
    canvas.dispatchEvent(key("keyup", { key: "Alt", code: "AltLeft" }));

    expect(texts).toEqual(["ƒ"]);
    expect(keys).toEqual([]);
    surface.dispose();
  });

  it("forwards Alt immediately when the browser is not on a Mac", () => {
    // No Option character semantics there: Alt is the modifier alone, and
    // apps that react to Alt-hold or a bare tap (GTK mnemonic underlines,
    // Electron's menu peek) see it exactly as they did before the deferral.
    const { surface, canvas, keys } = attachTyping();
    (surface as unknown as { macOptionChars: boolean }).macOptionChars = false;

    canvas.dispatchEvent(
      key("keydown", { key: "Alt", code: "AltLeft", altKey: true }),
    );
    expect(keys).toEqual([{ keycode: 56, pressed: true }]);

    canvas.dispatchEvent(key("keyup", { key: "Alt", code: "AltLeft" }));
    expect(keys).toEqual([
      { keycode: 56, pressed: true },
      { keycode: 56, pressed: false },
    ]);
    surface.dispose();
  });

  it("keeps a non-ASCII Alt chord a chord when the browser is not on a Mac", () => {
    // On a national layout where a base key is non-ASCII (ä on a German
    // layout), Alt+ä is a real Meta chord, not Option typing — sending it
    // as text would break Meta keybindings.  The text branch is macOS-only.
    const { surface, canvas, keys, texts } = attachTyping();
    (surface as unknown as { macOptionChars: boolean }).macOptionChars = false;

    canvas.dispatchEvent(
      key("keydown", { key: "Alt", code: "AltLeft", altKey: true }),
    );
    canvas.dispatchEvent(
      key("keydown", { key: "ä", code: "KeyA", altKey: true }),
    );
    canvas.dispatchEvent(
      key("keyup", { key: "ä", code: "KeyA", altKey: true }),
    );
    canvas.dispatchEvent(key("keyup", { key: "Alt", code: "AltLeft" }));

    expect(texts).toEqual([]);
    expect(keys).toEqual([
      { keycode: 56, pressed: true },
      { keycode: 30, pressed: true },
      { keycode: 30, pressed: false },
      { keycode: 56, pressed: false },
    ]);
    surface.dispose();
  });
});
