import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { SurfaceStore } from "../SurfaceStore";

/** Minimal stand-in for a decoded VideoFrame — only what the presenter
 *  touches (close + display dimensions). */
function fakeFrame() {
  return {
    closed: false,
    displayWidth: 64,
    displayHeight: 48,
    close() {
      if (this.closed) throw new DOMException("closed", "InvalidStateError");
      this.closed = true;
    },
  };
}

type Presenter = {
  queue: ReturnType<typeof fakeFrame>[];
  rafId: number | null;
  initialized: boolean;
};

function presenter(store: SurfaceStore, sid: number): Presenter | undefined {
  return (store as any).presenters.get(sid);
}

function enqueue(store: SurfaceStore, sid: number, frame: unknown): void {
  (store as any).enqueueFrame(sid, frame);
}

describe("SurfaceStore presenter", () => {
  let store: SurfaceStore;
  let rafCb: FrameRequestCallback | null;

  const setVisibility = (state: "visible" | "hidden") => {
    Object.defineProperty(document, "visibilityState", {
      configurable: true,
      get: () => state,
    });
  };

  beforeEach(() => {
    rafCb = null;
    vi.stubGlobal("requestAnimationFrame", (cb: FrameRequestCallback) => {
      rafCb = cb;
      return 1;
    });
    vi.stubGlobal("cancelAnimationFrame", () => {
      rafCb = null;
    });
    setVisibility("visible");
    store = new SurfaceStore();
  });

  afterEach(() => {
    store.destroy();
    vi.unstubAllGlobals();
    Reflect.deleteProperty(document, "visibilityState");
  });

  it("presents the first frame synchronously and closes it", () => {
    const f = fakeFrame();
    enqueue(store, 1, f);
    expect(f.closed).toBe(true);
    expect(presenter(store, 1)!.queue).toHaveLength(0);
  });

  it("caps the queue while visible, closing the oldest frames", () => {
    enqueue(store, 1, fakeFrame()); // first frame: presented synchronously
    const frames = Array.from({ length: 6 }, fakeFrame);
    for (const f of frames) enqueue(store, 1, f);

    const p = presenter(store, 1)!;
    expect(p.queue.length).toBe(2);
    // All but the newest two were closed without being drawn.
    expect(frames.slice(0, 4).every((f) => f.closed)).toBe(true);
    expect(frames.slice(4).some((f) => f.closed)).toBe(false);

    // The rAF tick presents the newest and closes the rest.
    expect(rafCb).not.toBeNull();
    rafCb!(0);
    expect(frames.every((f) => f.closed)).toBe(true);
    expect(p.queue).toHaveLength(0);
  });

  it("presents immediately instead of queueing while the tab is hidden", () => {
    enqueue(store, 1, fakeFrame());
    setVisibility("hidden");
    const frames = Array.from({ length: 5 }, fakeFrame);
    for (const f of frames) enqueue(store, 1, f);

    expect(frames.every((f) => f.closed)).toBe(true);
    expect(presenter(store, 1)!.queue).toHaveLength(0);
  });

  it("drains queued frames when the tab goes hidden", () => {
    enqueue(store, 1, fakeFrame());
    const frames = [fakeFrame(), fakeFrame()];
    for (const f of frames) enqueue(store, 1, f);
    expect(presenter(store, 1)!.queue).toHaveLength(2);

    setVisibility("hidden");
    document.dispatchEvent(new Event("visibilitychange"));

    expect(frames.every((f) => f.closed)).toBe(true);
    expect(presenter(store, 1)!.queue).toHaveLength(0);
    // The pending rAF was cancelled along the way.
    expect(rafCb).toBeNull();
  });
});
