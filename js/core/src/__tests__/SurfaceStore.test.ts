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

  it("never engages scheduling for frames without a usable PTS", () => {
    // fakeFrame() has no timestamp.  Scheduling on NaN would mean nothing
    // ever comes due and the surface freezes, so it must stay newest-wins.
    enqueue(store, 1, fakeFrame());
    for (let i = 0; i < 30; i++) enqueue(store, 1, fakeFrame());

    const p = presenter(store, 1)!;
    expect(p.smoothing).toBe(false);

    const tail = [fakeFrame(), fakeFrame()];
    for (const f of tail) enqueue(store, 1, f);
    rafCb!(0);
    // Newest-wins still drains to empty and paints.
    expect(p.queue).toHaveLength(0);
    expect(tail.every((f) => f.closed)).toBe(true);
  });
});

/** Frame carrying a capture-time PTS, in µs like a real VideoFrame. */
function ptsFrame(ptsMs: number) {
  return { ...fakeFrame(), timestamp: ptsMs * 1000 };
}

/**
 * Presentation scheduling.
 *
 * These model the pipeline the way it actually behaves: PTS is stamped at
 * compositor-commit on the server and advances on a fixed grid, while the
 * frame arrives on the client one path latency later, plus whatever jitter
 * encode and transport added.  The scheduler's job is to undo that jitter.
 */
describe("SurfaceStore PTS-scheduled presentation", () => {
  const REFRESH = 1000 / 60;
  /** Constant server→client path latency in the simulation. */
  const LATENCY = 30;

  let store: SurfaceStore;
  let rafCb: FrameRequestCallback | null;
  let clock: number;
  let streamPts: number;
  let presented: ReturnType<typeof ptsFrame>[];

  const tick = () => {
    const cb = rafCb;
    rafCb = null;
    cb!(clock);
  };

  /** Advance one refresh and run the rAF callback if one is armed. */
  const step = () => {
    clock += REFRESH;
    if (rafCb) tick();
  };

  /** Deliver `n` frames on a 60 fps grid, `jitter(i)` ms late each. */
  const runStream = (n: number, jitter: (i: number) => number = () => 0) => {
    for (let i = 0; i < n; i++) {
      const pts = streamPts;
      streamPts += REFRESH;
      clock = pts + LATENCY + jitter(i);
      enqueue(store, 1, ptsFrame(pts));
      if (rafCb) tick();
    }
  };

  /** Run the loop until the presenter queue empties. */
  const drain = () => {
    for (let i = 0; i < 16 && presenter(store, 1)!.queue.length > 0; i++)
      step();
  };

  beforeEach(() => {
    clock = 10_000;
    streamPts = 500;
    rafCb = null;
    presented = [];
    vi.spyOn(performance, "now").mockImplementation(() => clock);
    vi.stubGlobal("requestAnimationFrame", (cb: FrameRequestCallback) => {
      rafCb = cb;
      return 1;
    });
    vi.stubGlobal("cancelAnimationFrame", () => {
      rafCb = null;
    });
    Object.defineProperty(document, "visibilityState", {
      configurable: true,
      get: () => "visible",
    });
    store = new SurfaceStore();
    const orig = (store as any).presentFrame.bind(store);
    vi.spyOn(store as any, "presentFrame").mockImplementation(
      (sid: number, f: any) => {
        presented.push(f);
        orig(sid, f);
      },
    );
  });

  afterEach(() => {
    store.destroy();
    vi.unstubAllGlobals();
    vi.restoreAllMocks();
    Reflect.deleteProperty(document, "visibilityState");
  });

  it("stays on newest-wins until the surface proves it is streaming", () => {
    runStream(3);
    expect(presenter(store, 1)!.smoothing).toBe(false);
    runStream(8);
    expect(presenter(store, 1)!.smoothing).toBe(true);
  });

  it("presents every frame of a clean stream exactly once", () => {
    runStream(30);
    drain();
    // No frame silently discarded, none drawn twice.
    expect(presented).toHaveLength(30);
    expect(new Set(presented).size).toBe(30);
  });

  it("builds a playout margin when arrivals are jittery", () => {
    runStream(24, (i) => (i % 2 ? 8 : 0));
    const p = presenter(store, 1)!;
    expect(p.smoothing).toBe(true);
    // Margin tracks the jitter it saw, and stays under the hard ceiling.
    const delay = (store as any).playoutDelayMs(p);
    expect(delay).toBeGreaterThan(4);
    expect(delay).toBeLessThanOrEqual(50);
  });

  it("holds an on-time frame behind the margin rather than drawing it early", () => {
    runStream(24, (i) => (i % 2 ? 8 : 0));
    drain();
    presented = [];

    // Arrives at the fastest-path time, so it is early relative to the
    // margin the jitter established — it must wait, not paint now.
    const pts = streamPts;
    clock = pts + LATENCY;
    const f = ptsFrame(pts);
    enqueue(store, 1, f);
    tick();

    expect(presented).not.toContain(f);
    expect(presenter(store, 1)!.queue).toContain(f);

    // And it does paint once its due time arrives.
    for (let i = 0; i < 6 && !presented.includes(f); i++) step();
    expect(presented).toContain(f);
  });

  it("spreads a burst across refreshes instead of dropping all but the newest", () => {
    runStream(24, (i) => (i % 2 ? 8 : 0));
    drain();
    presented = [];

    // Two frames shipped back to back: distinct capture times, one arrival
    // instant.  Old behaviour drew the newest and closed the other unseen.
    const a = ptsFrame(streamPts);
    const b = ptsFrame(streamPts + REFRESH);
    clock = streamPts + LATENCY;
    enqueue(store, 1, a);
    enqueue(store, 1, b);

    // rAF runs every refresh, including the one at which `a` comes due —
    // stepping past it would leave both overdue, where drawing only the
    // newest is the right call and nothing is being tested.
    if (rafCb) tick();
    for (let i = 0; i < 8 && !presented.includes(b); i++) step();

    expect(presented).toContain(a);
    expect(presented).toContain(b);
    expect(presented.indexOf(a)).toBeLessThan(presented.indexOf(b));
  });

  it("stays engaged through a transport stall that keeps PTS continuous", () => {
    // Video rides a reliable, ordered channel, so one lost packet
    // head-of-line blocks everything behind it for at least an RTT.  The
    // source never stopped — those frames were captured on schedule and
    // arrive late in a burst, PTS spacing intact.  Judging the gap by
    // arrival time would disengage scheduling on every loss, which on a
    // 1 s link means permanently.
    runStream(20);
    drain();
    expect(presenter(store, 1)!.smoothing).toBe(true);

    // A full second of head-of-line blocking, then the backlog lands at
    // once — capture times still one frame apart.
    clock += 1000;
    for (let i = 0; i < 60; i++) {
      const pts = streamPts;
      streamPts += REFRESH;
      enqueue(store, 1, ptsFrame(pts));
    }

    const p = presenter(store, 1)!;
    expect(p.smoothing).toBe(true);
    // The backlog is a second stale; hold only what the cap allows rather
    // than replaying it.
    expect(p.queue.length).toBeLessThanOrEqual(
      (store as any).smoothedQueueCap(p),
    );
  });

  it("reverts to immediate presentation after an idle gap", () => {
    runStream(20);
    drain();
    expect(presenter(store, 1)!.smoothing).toBe(true);
    presented = [];

    // Surface goes quiet, then someone interacts.  That repaint is a
    // response to input; holding it behind a stale margin reads as lag.
    clock += 400;
    const wake = ptsFrame(clock);
    enqueue(store, 1, wake);
    const p = presenter(store, 1)!;
    expect(p.smoothing).toBe(false);

    tick();
    expect(presented).toContain(wake);
    expect(p.queue).toHaveLength(0);
  });

  it("bounds the queue while scheduling is engaged", () => {
    runStream(20);
    drain();

    // A clump of not-yet-due frames must not pin decoder buffers without
    // limit just because none of them have come due.
    const frames = Array.from({ length: 12 }, (_, i) =>
      ptsFrame(streamPts + 400 + i * REFRESH),
    );
    for (const f of frames) enqueue(store, 1, f);

    expect(presenter(store, 1)!.queue.length).toBeLessThanOrEqual(4);
    expect(frames.some((f) => f.closed)).toBe(true);
  });

  it("sizes the queue to the margin at a high refresh rate", () => {
    // 240 Hz source on a link that stalls periodically — the combination
    // the fixed cap of 4 could not serve: the margin grows to tens of ms
    // while the frame interval shrinks to ~4 ms, so the frames legitimately
    // in hand run to double digits.  Capping at 4 there trims not-yet-due
    // frames every interval and drops most of the stream.
    const fast = 1000 / 240;
    // Anchor the wall clock to the stream, or the monotonic clamp below
    // pins every arrival at the initial clock and no jitter registers.
    clock = streamPts + LATENCY;
    for (let i = 0; i < 120; i++) {
      const pts = streamPts;
      streamPts += fast;
      // Arrivals are delayed but never reordered: an ordered transport
      // bunches frames behind a stall rather than overtaking.
      clock = Math.max(clock, pts + LATENCY + (i % 8 === 0 ? 35 : 0));
      enqueue(store, 1, ptsFrame(pts));
      if (rafCb) tick();
    }
    const p = presenter(store, 1)!;
    expect(p.smoothing).toBe(true);
    // Interval was learned from PTS deltas, not assumed to be 60 Hz.
    expect(p.frameIntervalMs).toBeLessThan(10);

    const margin = (store as any).playoutDelayMs(p);
    const needed = Math.ceil(margin / fast);
    // The scenario has to actually outgrow the old fixed cap, or this test
    // would pass against the bug it exists to catch.
    expect(needed).toBeGreaterThan(4);

    const cap = (store as any).smoothedQueueCap(p);
    expect(cap).toBeGreaterThanOrEqual(needed);
  });

  it("never lets the depth bound clip the margin at any real frame rate", () => {
    // The bound exists for broken PTS streams, not fast ones.  Across every
    // rate the server can pace a surface at — up to MAX_DISPLAY_FPS — the
    // derived cap must always cover the full margin, so no stream is ever
    // made to drop frames just for being fast.
    const p = presenter(store, 1) ?? {
      jitterMs: 1e6, // force the margin to its ceiling
      frameIntervalMs: 0,
    };
    for (const fps of [24, 30, 60, 90, 120, 144, 240, 360, 480]) {
      const probe = { ...p, jitterMs: 1e6, frameIntervalMs: 1000 / fps };
      const margin = (store as any).playoutDelayMs(probe);
      const cap = (store as any).smoothedQueueCap(probe);
      expect(margin).toBe(50); // pinned at PRESENT_DELAY_MAX_MS
      expect(cap).toBeGreaterThanOrEqual(Math.ceil(margin / (1000 / fps)));
    }
  });

  it("bounds the queue when the frame interval is degenerate", () => {
    // A PTS stream claiming impossible rates must not inflate the queue —
    // the interval floor, not the depth cap, is what stops it.
    const probe = { jitterMs: 1e6, frameIntervalMs: 0 };
    const cap = (store as any).smoothedQueueCap(probe);
    expect(cap).toBeLessThanOrEqual(26);
  });

  it("measures refresh intervals faster than 250 Hz", () => {
    // The rAF-delta filter must accept a 480 Hz panel, which the server
    // will happily pace a surface at (MAX_DISPLAY_FPS = 480).
    const fast = 1000 / 480;
    for (let i = 0; i < 40; i++) {
      clock += fast;
      (store as any).noteRafInterval();
    }
    expect((store as any).refreshMs).toBeLessThan(5);
  });

  it("ignores duplicate PTS when learning the frame interval", () => {
    runStream(10);
    const p = presenter(store, 1)!;
    const before = p.frameIntervalMs;
    // A stalled encoder re-emitting the same timestamp must not drag the
    // interval toward zero and blow the derived cap up.
    for (let i = 0; i < 5; i++) {
      clock += REFRESH;
      enqueue(store, 1, ptsFrame(p.lastPtsMs!));
      if (rafCb) tick();
    }
    expect(p.frameIntervalMs).toBeCloseTo(before, 5);
    expect((store as any).smoothedQueueCap(p)).toBeLessThanOrEqual(16);
  });

  it("recovers when the PTS clock jumps backwards", () => {
    runStream(20);
    drain();
    expect(presenter(store, 1)!.smoothing).toBe(true);
    presented = [];

    // u32 millisecond counter wrapped, or the stream restarted.
    streamPts = 10;
    clock += REFRESH;
    const after = ptsFrame(streamPts);
    enqueue(store, 1, after);
    const p = presenter(store, 1)!;
    expect(p.smoothing).toBe(false);

    tick();
    expect(presented).toContain(after);
  });
});
