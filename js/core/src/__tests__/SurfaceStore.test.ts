import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import {
  NumberRing,
  PendingFrameSamples,
  RollingQuantile,
  SurfaceFrameHistory,
  SurfaceStore,
  estimateSourceToReceiveMs,
  wrappingTimestampDelta,
  sourceTimestampDelta,
} from "../SurfaceStore";
import { CODEC_SUPPORT_AV1, CODEC_SUPPORT_AV1_444 } from "../types";

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

function enqueue(
  store: SurfaceStore,
  sid: number,
  frame: unknown,
  receiveT?: number,
): void {
  (store as any).enqueueFrame(sid, frame, -1, receiveT);
}

describe("bounded sample structures", () => {
  it("keeps an ordered snapshot of the newest ring entries", () => {
    const ring = new NumberRing(3);
    for (const value of [1, 2, 3, 4, 5]) ring.push(value);
    expect(ring.length).toBe(3);
    expect(ring.toArray()).toEqual([3, 4, 5]);
  });

  it("maintains nearest-rank quantiles across wrap and window shrink", () => {
    const values = new RollingQuantile(5);
    for (const value of [5, 1, 3, 2, 4]) values.push(value);
    expect(values.quantile(0.2)).toBe(1);
    expect(values.quantile(0.5)).toBe(3);
    expect(values.quantile(0.8)).toBe(4);

    values.push(6);
    expect(values.quantile(0.8)).toBe(4);

    values.push(7, 3);
    expect(values.length).toBe(3);
    expect(values.quantile(0.5)).toBe(6);

    values.clear();
    for (const value of [2, 2, 1]) values.push(value, 3);
    values.push(3, 3);
    expect(values.quantile(0.5)).toBe(2);
  });

  it("correlates typed surface samples without per-frame objects", () => {
    const history = new SurfaceFrameHistory(2);
    const first = history.push(10, 100, 5, 100_005, 200, false, 3);
    const second = history.push(20, 104, 10, 104_010, 300, true, 4);

    expect(history.markDecoded(second, 22)).toBe(true);
    history.markPresented(second, 23);
    expect(history.toArray()).toEqual([
      {
        t: 10,
        sourceT: 100,
        sourceSubUs: 5,
        ptsUs: 100_005,
        bytes: 200,
        key: false,
        sourceToRecvMs: 3,
      },
      {
        t: 20,
        sourceT: 104,
        sourceSubUs: 10,
        ptsUs: 104_010,
        bytes: 300,
        key: true,
        sourceToRecvMs: 4,
        decodeT: 22,
        decodeMs: 2,
        presentT: 23,
        presentMs: 1,
        e2eMs: 7,
      },
    ]);

    history.push(30, 108, 15, 108_015, 400, false, NaN);
    expect(history.markDecoded(first, 31)).toBe(false);
  });

  it("keeps duplicate PTS decoder correlations in FIFO order", () => {
    const pending = new PendingFrameSamples(3);
    pending.push(10, 101);
    pending.push(10, 102);
    pending.push(20, 103);
    expect(pending.takeByPts(10)).toBe(101);
    expect(pending.takeByPts(10)).toBe(102);
    pending.removeToken(103);
    expect(pending.takeByPts(20)).toBe(-1);
  });
});

describe("surface latency clock mapping", () => {
  it("computes signed deltas across the u32 timestamp wrap", () => {
    expect(wrappingTimestampDelta(2, 0xffff_fffe)).toBe(4);
    expect(wrappingTimestampDelta(0xffff_fffe, 2)).toBe(-4);
  });

  it("includes fractional source timestamps across millisecond boundaries", () => {
    expect(
      sourceTimestampDelta(
        { sourceT: 11, sourceSubUs: 32 },
        { sourceT: 10, sourceSubUs: 960 },
      ),
    ).toBeCloseTo(0.072);
  });

  it("maps source timestamps onto performance.now()", () => {
    expect(
      estimateSourceToReceiveMs(1004, 5009, {
        serverMs: 1000,
        clientMidMs: 5000,
        rttMs: 6,
      }),
    ).toBe(5);
  });
});

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

  it("resets every diagnostic counter each logging window", () => {
    // These are per-window rates.  One counter left out of the reset
    // accumulates for the process lifetime and silently dwarfs the rest —
    // which `presented` did, breaking the presented-vs-output comparison
    // it was added to provide.
    vi.useFakeTimers();
    const s = new SurfaceStore();
    const diag = (s as any)._diag;
    for (const k of Object.keys(diag)) diag[k] = 7;

    vi.advanceTimersByTime(5_000);

    for (const [k, v] of Object.entries((s as any)._diag)) {
      expect(v, `counter "${k}" was not reset`).toBe(0);
    }
    s.destroy();
    vi.useRealTimers();
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
      enqueue(store, 1, ptsFrame(pts), clock);
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

  it("turns recurring arrival jitter into bounded playout headroom", () => {
    runStream(24, (i) => (i % 2 ? 8 : 0));
    const p = presenter(store, 1)!;
    expect(p.smoothing).toBe(true);
    expect((store as any).playoutDelayMs(p)).toBeCloseTo(8, 5);
  });

  it("does not turn decoder-output batching into playout latency", () => {
    for (let i = 0; i < 24; i++) {
      const pts = streamPts;
      streamPts += REFRESH;
      const receiveT = pts + LATENCY;
      clock = receiveT + (i % 2 ? 30 : 1);
      enqueue(store, 1, ptsFrame(pts), receiveT);
      if (rafCb) tick();
    }
    const p = presenter(store, 1)!;
    expect(p.smoothing).toBe(true);
    expect((store as any).playoutDelayMs(p)).toBe(0);
  });

  it("presents same-host frames synchronously without an rAF boundary", () => {
    store.noteServerClock(1_000, clock, clock + 0.5);
    runStream(24, (i) => (i % 2 ? 30 : 0));

    const p = presenter(store, 1)!;
    expect(p.smoothing).toBe(false);
    expect(p.rafId).toBeNull();
    expect(p.queue).toHaveLength(0);
    expect((store as any).playoutDelayMs(p)).toBe(0);
    expect(presented).toHaveLength(24);
  });

  it("bypasses playout buffering when smoothing is disabled", () => {
    store.setPresentationSmoothingEnabled(false);
    runStream(24, (i) => (i % 2 ? 30 : 0));

    const p = presenter(store, 1)!;
    expect(p.smoothing).toBe(false);
    expect(p.rafId).toBeNull();
    expect(p.queue).toHaveLength(0);
    expect((store as any).playoutDelayMs(p)).toBe(0);
    expect(presented).toHaveLength(24);
  });

  it("holds an on-time frame inside the learned playout window", () => {
    runStream(30, (i) => (i % 2 ? 25 : 0));
    drain();
    presented = [];

    const pts = streamPts;
    clock = pts + LATENCY;
    const f = ptsFrame(pts);
    enqueue(store, 1, f);
    tick();

    expect(presented).not.toContain(f);
    step();
    step();
    expect(presented).toContain(f);
    expect(presenter(store, 1)!.queue).not.toContain(f);
  });

  it("paces a short transport burst across future refreshes", () => {
    runStream(24, (i) => (i % 2 ? 8 : 0));
    drain();
    presented = [];

    // The learned margin makes the first frame due while keeping the second
    // for the next refresh, so a short receive burst does not become a visual
    // burst followed by a frozen canvas.
    const a = ptsFrame(streamPts);
    const b = ptsFrame(streamPts + REFRESH);
    clock = streamPts + LATENCY + REFRESH / 2;
    enqueue(store, 1, a);
    enqueue(store, 1, b);

    if (rafCb) tick();

    expect(presented).toContain(a);
    expect(presented).not.toContain(b);
    expect(a.closed).toBe(true);
    expect(presenter(store, 1)!.queue).toContain(b);
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
    // limit just because none of them have come due.  The live cap is
    // derived from the added latency and the frame interval, so assert
    // against the derivation rather than a number that silently stops
    // meaning anything when the schedule changes.
    const frames = Array.from({ length: 12 }, (_, i) =>
      ptsFrame(streamPts + 400 + i * REFRESH),
    );
    for (const f of frames) enqueue(store, 1, f);

    const p = presenter(store, 1)!;
    expect(p.queue.length).toBeLessThanOrEqual(
      (store as any).smoothedQueueCap(p),
    );
    expect(p.queue.length).toBeLessThanOrEqual(26);
    expect(frames.some((f) => f.closed)).toBe(true);
  });

  it("keeps the queue latency-bounded at a high refresh rate", () => {
    const fast = 1000 / 240;
    clock = streamPts + LATENCY;
    for (let i = 0; i < 120; i++) {
      const pts = streamPts;
      streamPts += fast;
      clock = Math.max(clock, pts + LATENCY + (i % 8 === 0 ? 35 : 0));
      enqueue(store, 1, ptsFrame(pts));
      if (rafCb) tick();
    }
    const p = presenter(store, 1)!;
    expect(p.smoothing).toBe(true);
    expect(p.frameIntervalMs).toBeLessThan(10);
    expect((store as any).playoutDelayMs(p)).toBeLessThanOrEqual(96);
    expect((store as any).playoutDelayMs(p)).toBeGreaterThan(20);
    expect(p.queue.length).toBeLessThanOrEqual(
      (store as any).smoothedQueueCap(p),
    );
  });

  it("does not let one outlier pin the margin", () => {
    // The peak-tracking estimator this replaced took a single late frame
    // from 0 to half its value in one sample, clipped the margin at the
    // ceiling, then decayed at 0.98/frame — ~55 frames, nearly a second at
    // 60 Hz, of maximum latency bought by one outlier it could not cover
    // anyway.  A quantile treats it as the <5% tail it is.
    // Clean stream: the margin settles at the fixed refresh of headroom
    // every stream carries, and nothing on top of it.
    runStream(60);
    const p = presenter(store, 1)!;
    const before = (store as any).playoutDelayMs(p);
    expect(before).toBeLessThan(REFRESH + 5);

    // One frame arrives 200 ms late.
    const pts = streamPts;
    streamPts += REFRESH;
    clock = pts + LATENCY + 200;
    enqueue(store, 1, ptsFrame(pts));

    expect((store as any).playoutDelayMs(p)).toBeCloseTo(before, 5);

    // And a few clean frames later it is still not chasing the outlier.
    runStream(10);
    expect((store as any).playoutDelayMs(p)).toBeLessThan(REFRESH + 10);
  });

  it("buffers recurring jitter", () => {
    runStream(120, (i) => (i % 3 === 0 ? 0 : 12));
    const p = presenter(store, 1)!;
    expect((store as any).playoutDelayMs(p)).toBeCloseTo(12, 5);
  });

  it("covers recurring 75 ms Wi-Fi recovery bursts at 240 fps", () => {
    const interval = 1000 / 240;
    for (let i = 0; i < 240; i++) {
      const pts = streamPts;
      streamPts += interval;
      const phase = i % 60;
      // Eighteen source-spaced frames become readable together after one
      // reliable-stream hole. Their offsets descend as the source catches
      // back up, matching a TCP/QUIC head-of-line recovery burst.
      const jitter = phase < 18 ? 75 - phase * interval : 0;
      clock = pts + LATENCY + jitter;
      enqueue(store, 1, ptsFrame(pts));
      if (rafCb) tick();
    }
    const p = presenter(store, 1)!;
    expect((store as any).playoutDelayMs(p)).toBeGreaterThanOrEqual(70);
    expect((store as any).playoutDelayMs(p)).toBeLessThanOrEqual(96);
  });

  it("bounds and then sheds margin when path latency changes", () => {
    runStream(60);
    const p = presenter(store, 1)!;
    let maxMargin = 0;

    for (let i = 0; i < 80; i++) {
      const pts = streamPts;
      streamPts += REFRESH;
      clock = pts + LATENCY + 40;
      enqueue(store, 1, ptsFrame(pts));
      maxMargin = Math.max(maxMargin, (store as any).playoutDelayMs(p));
      if (rafCb) tick();
    }
    expect(maxMargin).toBeLessThanOrEqual(96);
    expect((store as any).playoutDelayMs(p)).toBeCloseTo(0, 5);
  });

  it("never lets the depth bound clip the margin at any real frame rate", () => {
    // The derived cap must always cover the full margin, including rates
    // above 1000 fps: there is no high-rate policy ceiling.
    for (const fps of [24, 60, 240, 480, 1000, 5000, 65_535]) {
      // Margin at its ceiling — the worst case the cap has to cover.
      const interval = 1000 / fps;
      const probe = {
        presentOffsetMs: interval + 96,
        fastOffsetMs: interval,
        frameIntervalMs: interval,
      };
      const margin = (store as any).playoutDelayMs(probe);
      expect(margin).toBeCloseTo(96, 10);
      expect((store as any).smoothedQueueCap(probe)).toBe(
        Math.ceil(margin / interval) + 2,
      );
    }
  });

  it("bounds the queue when the frame interval is degenerate", () => {
    // Zero is not a cadence.  It falls back to the initial refresh estimate
    // without imposing a floor on any positive high-rate stream.
    const probe = { presentOffsetMs: 50, fastOffsetMs: 0, frameIntervalMs: 0 };
    const cap = (store as any).smoothedQueueCap(probe);
    expect(cap).toBe(5);
  });

  it("measures refresh intervals across the whole accepted band", () => {
    // Positive sub-millisecond cadences count too; 1000 Hz is not a ceiling.
    for (const hz of [10_000, 2000, 1000, 480, 144, 60, 10]) {
      const s = new SurfaceStore();
      const interval = 1000 / hz;
      for (let i = 0; i < 60; i++) {
        clock += interval;
        (s as any).noteRafInterval(clock);
      }
      expect((s as any).refreshMs).toBeCloseTo(interval, 0);
      s.destroy();
    }
  });

  it("ignores rAF deltas outside the band", () => {
    const s = new SurfaceStore();
    const before = (s as any).refreshMs;
    for (const dt of [250, 5000]) {
      clock += dt;
      (s as any).noteRafInterval(clock);
    }
    expect((s as any).refreshMs).toBe(before);
    s.destroy();
  });

  it("measures one refresh when several surfaces present in the same frame", () => {
    const s = new SurfaceStore();
    const interval = 1000 / 120;
    for (let i = 0; i < 60; i++) {
      clock += interval;
      // rAF guarantees the same timestamp to callbacks sharing a frame.
      // A store can have one callback per visible surface.
      (s as any).noteRafInterval(clock);
      (s as any).noteRafInterval(clock);
      (s as any).noteRafInterval(clock);
    }
    expect((s as any).refreshMs).toBeCloseTo(interval, 1);
    s.destroy();
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

/**
 * Adversarial scenarios, each run against a NEWEST-WINS CONTROL.
 *
 * Every other test here asserts the scheduler does what it intends.  These
 * assert the only thing that actually matters: that it is never worse than
 * the code it replaced.  That is the assertion class the rest of the suite
 * lacked, which is why it was fully green while two regressions sat in the
 * diff — a strictly-worse presenter passes every "does it schedule?" test.
 */
describe("SurfaceStore vs newest-wins control", () => {
  const REFRESH = 1000 / 60;
  const LATENCY = 40;

  let rafCb: FrameRequestCallback | null;
  let clock: number;

  beforeEach(() => {
    clock = 10_000;
    rafCb = null;
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
  });

  afterEach(() => {
    vi.unstubAllGlobals();
    vi.restoreAllMocks();
    Reflect.deleteProperty(document, "visibilityState");
  });

  /**
   * Drive one presenter through an arrival trace and report what reached
   * the canvas.  `control` forces newest-wins — i.e. main's behaviour —
   * by pinning `smoothing` false after every arrival.
   */
  const run = (trace: { pts: number; at: number }[], control: boolean) => {
    const store = new SurfaceStore();
    const presented: number[] = [];
    const presentedAt: number[] = [];
    const orig = (store as any).presentFrame.bind(store);
    vi.spyOn(store as any, "presentFrame").mockImplementation(
      (sid: number, f: any) => {
        presented.push(f.timestamp / 1000);
        presentedAt.push(clock);
        orig(sid, f);
      },
    );

    let i = 0;
    const start = trace[0].at;
    const end = trace[trace.length - 1].at + 40 * REFRESH;
    // Interleave arrivals and a free-running 60 Hz rAF loop.
    for (let t = start; t <= end; t += REFRESH) {
      clock = t;
      while (i < trace.length && trace[i].at <= t) {
        (store as any).enqueueFrame(
          1,
          {
            closed: false,
            displayWidth: 64,
            displayHeight: 48,
            timestamp: trace[i].pts * 1000,
            close() {
              this.closed = true;
            },
          },
          -1,
          trace[i].at,
        );
        if (control) {
          const p = (store as any).presenters.get(1);
          if (p) p.smoothing = false;
        }
        i++;
      }
      if (rafCb) {
        const cb = rafCb;
        rafCb = null;
        cb(t);
      }
    }

    // Longest interval between consecutive paints — the judder metric.
    let maxGap = 0;
    for (let k = 1; k < presentedAt.length; k++) {
      maxGap = Math.max(maxGap, presentedAt[k] - presentedAt[k - 1]);
    }
    store.destroy();
    return { count: presented.length, maxGap, presented };
  };

  /** 60 fps capture grid; `jitter(i)` ms of extra delivery delay on frame i. */
  const trace = (n: number, jitter: (i: number) => number = () => 0) =>
    Array.from({ length: n }, (_, i) => ({
      pts: 1000 + i * REFRESH,
      at: 1000 + i * REFRESH + LATENCY + jitter(i),
    }));

  const NEVER_WORSE = (name: string, t: { pts: number; at: number }[]) => {
    it(`is never worse than newest-wins: ${name}`, () => {
      const control = run(t, true);
      const scheduled = run(t, false);
      expect(scheduled.count).toBeGreaterThanOrEqual(control.count);
      expect(scheduled.maxGap).toBeLessThanOrEqual(control.maxGap + 1e-6);
    });
  };

  it("does not add latency to the heavy-jitter control trace", () => {
    const t = trace(200, (i) => (i % 3 === 0 ? 28 : i % 3 === 1 ? 4 : 14));
    const control = run(t, true);
    const scheduled = run(t, false);
    expect(scheduled.count).toBeGreaterThanOrEqual(control.count);
    expect(scheduled.maxGap).toBeLessThanOrEqual(control.maxGap + 1e-6);
  });

  NEVER_WORSE("clean stream", trace(200));
  NEVER_WORSE(
    "steady jitter",
    trace(200, (i) => (i % 2 ? 9 : 0)),
  );
  NEVER_WORSE(
    "heavy jitter",
    trace(200, (i) => (i % 3 === 0 ? 28 : i % 3 === 1 ? 4 : 14)),
  );

  it("caps and then sheds added latency after a single stall", () => {
    const t = trace(400).map((f, i) =>
      // One 500 ms head-of-line block: 30 frames buffered, then released.
      i >= 100 && i < 130 ? { ...f, at: trace(400)[130].at } : f,
    );
    const store = new SurfaceStore();
    const margins: number[] = [];
    for (let k = 0; k < t.length; k++) {
      clock = t[k].at;
      (store as any).enqueueFrame(
        1,
        {
          closed: false,
          displayWidth: 64,
          displayHeight: 48,
          timestamp: t[k].pts * 1000,
          close() {
            this.closed = true;
          },
        },
        -1,
        t[k].at,
      );
      if (rafCb) {
        const cb = rafCb;
        rafCb = null;
        cb(clock);
      }
      const p = (store as any).presenters.get(1);
      margins.push(p ? (store as any).playoutDelayMs(p) : 0);
    }

    expect(Math.max(...margins)).toBeLessThanOrEqual(96);
    expect(margins.at(-1)).toBeCloseTo(0, 5);
    store.destroy();
  });

  it("recovers quickly when the path abruptly gets faster", () => {
    // Scenario A. A VPN reconnect or Wi-Fi roam drops path latency in one
    // step.  A baseline that could only descend a fixed few ms per frame
    // held frames against a stale offset and froze the surface for the
    // length of the improvement; quantiles over one window track it.
    const t = trace(300).map((f, i) =>
      i >= 150 ? { ...f, at: f.at - 200 } : f,
    );
    // Arrival order must stay monotonic for the simulation to be honest.
    for (let k = 1; k < t.length; k++) {
      if (t[k].at < t[k - 1].at) t[k].at = t[k - 1].at;
    }
    const scheduled = run(t, false);
    const control = run(t, true);
    expect(scheduled.count).toBeGreaterThanOrEqual(control.count);
    expect(scheduled.maxGap).toBeLessThanOrEqual(control.maxGap + 1e-6);
  });
});

/** SURFACE_FRAME_FLAG_KEYFRAME | SURFACE_FRAME_CODEC_AV1. */
const KEY_AV1 = (1 << 0) | (1 << 1);
const DELTA_AV1 = 1 << 1;

describe("SurfaceStore surface dimensions", () => {
  // Pointer coordinates are scaled by surface.width/height, which must be
  // the native composite size from SurfaceResized.  Frames arrive at the
  // per-client *encode* size — smaller whenever the view is downscaled —
  // and must not clobber the native size, or every pointer position lands
  // short of the cursor by stream/native.

  /** Decoder entry stub: enough to get handleSurfaceFrame past the entry
   *  checks and into the dimension update.  Already configured, so the
   *  frame path neither reconfigures nor drops it. */
  function stubDecoder(store: SurfaceStore, sid: number): void {
    (store as any).decoders.set(sid, {
      codec: "av1",
      decoder: { state: "configured", decode() {} },
      pendingKeyframe: false,
      keyframeRequested: false,
    });
  }

  beforeEach(() => {
    vi.stubGlobal(
      "EncodedVideoChunk",
      class {
        constructor(_init: unknown) {}
      },
    );
  });
  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it("keeps the native size when downscaled frames arrive", () => {
    const store = new SurfaceStore();
    store.handleSurfaceCreated(1, 0, 0, 0, "t", "a");
    store.handleSurfaceResized(1, 1920, 1080);
    stubDecoder(store, 1);
    store.handleSurfaceFrame(1, 0, KEY_AV1, 960, 540, new Uint8Array(0));
    const surface = store.getSurfaces().get(1)!;
    expect(surface.width).toBe(1920);
    expect(surface.height).toBe(1080);
    store.destroy();
  });

  it("takes a logical-size change that leaves the resolution alone", () => {
    // A high-DPI viewer leaving rescales the window without changing how
    // many pixels it composites to: 1200×900 stays, but it stops being a
    // 400×300 window at 3x and becomes a 1200×900 one at 1x.  Every viewer
    // has to redraw it at the new size, so the change must land and emit.
    const store = new SurfaceStore();
    store.handleSurfaceCreated(1, 0, 0, 0, "t", "a");
    store.handleSurfaceResized(1, 1200, 900, 400, 300);
    let changes = 0;
    const unsub = store.onChange(() => changes++);
    store.handleSurfaceResized(1, 1200, 900, 1200, 900);
    const surface = store.getSurfaces().get(1)!;
    expect(surface.logicalWidth).toBe(1200);
    expect(surface.logicalHeight).toBe(900);
    expect(changes).toBe(1);
    unsub();
    store.destroy();
  });

  it("keeps the known logical size when a server sends none", () => {
    // Absent is not 0×0.  Clobbering it would tell every view the window
    // has no size, and 0 is the one value that cannot be drawn.
    const store = new SurfaceStore();
    store.handleSurfaceCreated(1, 0, 0, 0, "t", "a");
    store.handleSurfaceResized(1, 1200, 900, 400, 300);
    store.handleSurfaceResized(1, 1600, 1200);
    const surface = store.getSurfaces().get(1)!;
    expect(surface.width).toBe(1600);
    expect(surface.logicalWidth).toBe(400);
    store.destroy();
  });

  it("seeds a still-0×0 surface from the first frame's dimensions", () => {
    const store = new SurfaceStore();
    store.handleSurfaceCreated(1, 0, 0, 0, "t", "a");
    stubDecoder(store, 1);
    store.handleSurfaceFrame(1, 0, KEY_AV1, 960, 540, new Uint8Array(0));
    const surface = store.getSurfaces().get(1)!;
    expect(surface.width).toBe(960);
    expect(surface.height).toBe(540);
    store.destroy();
  });
});

describe("SurfaceStore decoder recovery", () => {
  /** Stand-in for WebCodecs' VideoDecoder, with switches for the two ways
   *  a real one refuses: configure() rejecting the codec string, and
   *  decode() rejecting the bitstream. */
  class FakeDecoder {
    static instances: FakeDecoder[] = [];
    static failConfigure = false;
    static failDecode = false;
    static failDecodeAsync = false;
    state = "unconfigured";
    configured: string[] = [];
    colorSpaces: Array<VideoColorSpaceInit | undefined> = [];
    decoded = 0;
    decodeQueueSize = 0;
    private readonly onError: (error: DOMException) => void;
    private readonly onOutput: (frame: VideoFrame) => void;
    constructor(init: {
      error: (error: DOMException) => void;
      output: (frame: VideoFrame) => void;
    }) {
      this.onError = init.error;
      this.onOutput = init.output;
      FakeDecoder.instances.push(this);
    }
    configure(config: {
      codec: string;
      colorSpace?: VideoColorSpaceInit;
    }) {
      if (FakeDecoder.failConfigure) {
        throw new DOMException("unsupported codec", "NotSupportedError");
      }
      this.state = "configured";
      this.configured.push(config.codec);
      this.colorSpaces.push(config.colorSpace);
    }
    decode() {
      if (FakeDecoder.failDecode) {
        throw new DOMException("bad bitstream", "EncodingError");
      }
      if (FakeDecoder.failDecodeAsync) {
        this.onError(new DOMException("bad bitstream", "EncodingError"));
        return;
      }
      this.decoded++;
      this.decodeQueueSize++;
    }
    flush() {
      return Promise.resolve();
    }
    close() {
      this.state = "closed";
    }
    output(frame: VideoFrame) {
      this.onOutput(frame);
    }
  }

  let clock = 0;
  const frame = new Uint8Array([0x12, 0x00]);

  function newStore(): SurfaceStore {
    const store = new SurfaceStore();
    store.handleSurfaceCreated(1, 0, 1280, 720, "t", "a");
    return store;
  }

  beforeEach(() => {
    clock = 0;
    FakeDecoder.instances = [];
    FakeDecoder.failConfigure = false;
    FakeDecoder.failDecode = false;
    FakeDecoder.failDecodeAsync = false;
    vi.spyOn(performance, "now").mockImplementation(() => clock);
    vi.spyOn(console, "warn").mockImplementation(() => {});
    vi.stubGlobal("VideoDecoder", FakeDecoder);
    vi.stubGlobal(
      "EncodedVideoChunk",
      class {
        constructor(_init: unknown) {}
      },
    );
    Object.defineProperty(window, "isSecureContext", {
      configurable: true,
      value: true,
    });
  });
  afterEach(() => {
    vi.unstubAllGlobals();
    vi.restoreAllMocks();
  });

  it("configures AV1 from the frame when the announced string is not AV1", () => {
    // Encoder-selection churn announces the whole preference walk, so the
    // stored string can name H.264 while AV1 frames are already flowing.
    // Waiting for a better announcement means waiting forever: a healthy
    // session has no reason to send one.
    const store = newStore();
    store.handleSurfaceEncoder(1, "openh264\0avc1.42001e");
    store.handleSurfaceFrame(1, 0, KEY_AV1, 1280, 720, frame);
    const decoder = FakeDecoder.instances[0];
    expect(decoder.configured[0]).toMatch(/^av01\./);
    expect(decoder.decoded).toBe(1);
    store.destroy();
  });

  it("configures decoded surfaces as limited-range BT.601", () => {
    const store = newStore();
    store.handleSurfaceFrame(1, 0, KEY_AV1, 1280, 720, frame);
    expect(FakeDecoder.instances[0].colorSpaces[0]).toEqual({
      primaries: "bt709",
      transfer: "iec61966-2-1",
      matrix: "smpte170m",
      fullRange: false,
    });
    store.destroy();
  });

  it("reports WebCodecs queue depth with each surface ACK", () => {
    const store = newStore();
    const acks: Array<[number, number]> = [];
    store.setAckSender((sid, queueDepth) => acks.push([sid, queueDepth]));
    store.handleSurfaceFrame(1, 0, KEY_AV1, 1280, 720, frame);
    store.handleSurfaceFrame(1, 1, DELTA_AV1, 1280, 720, frame);
    expect(acks).toEqual([
      [1, 1],
      [1, 2],
    ]);
    store.destroy();
  });

  it("replaces AV1 when an authoritative codec string changes", () => {
    const store = newStore();
    store.handleSurfaceEncoder(1, "openh264\0avc1.42001e");
    store.handleSurfaceFrame(1, 0, KEY_AV1, 1280, 720, frame);
    store.handleSurfaceEncoder(1, "av1-vulkan\0av01.0.09M.08");
    const [derivedDecoder, announcedDecoder] = FakeDecoder.instances;
    expect(FakeDecoder.instances).toHaveLength(2);
    expect(derivedDecoder.configured[0]).toMatch(/^av01\./);
    expect(announcedDecoder.configured).toEqual(["av01.0.09M.08"]);
    store.destroy();
  });

  it("replaces the AV1 decoder when a thumbnail returns to native size", () => {
    // The native surface stays 1280x720 throughout, so no SurfaceResized
    // message accompanies this transition.  The frame dimensions are the
    // only indication that the scaled dock subscription became a full-size
    // pane subscription again.
    const store = newStore();
    store.handleSurfaceEncoder(1, "av1-software\0av01.0.09M.08");
    store.handleSurfaceFrame(1, 0, KEY_AV1, 320, 180, frame);
    store.handleSurfaceFrame(1, 1, KEY_AV1, 1280, 720, frame);

    const [thumbnailDecoder, nativeDecoder] = FakeDecoder.instances;
    expect(FakeDecoder.instances).toHaveLength(2);
    expect(thumbnailDecoder.configured).toEqual(["av01.0.09M.08"]);
    expect(thumbnailDecoder.decoded).toBe(1);
    expect(nativeDecoder.configured).toEqual(["av01.0.09M.08"]);
    expect(nativeDecoder.decoded).toBe(1);
    store.destroy();
  });

  it("drops thumbnail output flushed after the decoder replacement", () => {
    const store = newStore();
    store.handleSurfaceFrame(1, 0, KEY_AV1, 320, 180, frame);
    store.handleSurfaceFrame(1, 1, KEY_AV1, 1280, 720, frame);

    const stale = {
      displayWidth: 320,
      displayHeight: 180,
      timestamp: 0,
      close: vi.fn(),
    } as unknown as VideoFrame;
    FakeDecoder.instances[0].output(stale);

    expect(stale.close).toHaveBeenCalledOnce();
    expect((store as any)._diag.output).toBe(0);
    expect((store as any)._diag.dropped).toBe(1);
    store.destroy();
  });

  it("accepts output dimensions reported by the active decoder", () => {
    const store = newStore();
    store.handleSurfaceFrame(1, 0, KEY_AV1, 1280, 720, frame);
    const enqueue = vi.fn((_sid: number, output: VideoFrame) => output.close());
    (store as any).enqueueFrame = enqueue;
    const output = {
      displayWidth: 1278,
      displayHeight: 720,
      timestamp: 0,
      close: vi.fn(),
    } as unknown as VideoFrame;

    FakeDecoder.instances[0].output(output);

    expect(enqueue).toHaveBeenCalledWith(
      1,
      output,
      expect.any(Number),
      expect.any(Number),
    );
    expect((store as any)._diag.output).toBe(1);
    expect((store as any)._diag.dropped).toBe(0);
    store.destroy();
  });

  it("rate-limits and caps keyframe requests while no decoder configures", () => {
    const store = newStore();
    const requests: number[] = [];
    store.setKeyframeSender((sid) => requests.push(sid));
    FakeDecoder.failConfigure = true;

    for (let i = 0; i < 20; i++) {
      store.handleSurfaceFrame(1, 0, KEY_AV1, 1280, 720, frame);
    }
    expect(requests).toHaveLength(1);

    // One per interval, and no more than the episode's budget however long
    // the stream keeps arriving.
    for (let round = 0; round < 20; round++) {
      clock += 2001;
      for (let i = 0; i < 5; i++) {
        store.handleSurfaceFrame(1, 0, KEY_AV1, 1280, 720, frame);
      }
    }
    expect(requests).toHaveLength(5);
    store.destroy();
  });

  it("rate-limits keyframe requests when decode rejects every frame", () => {
    const store = newStore();
    const requests: number[] = [];
    store.setKeyframeSender((sid) => requests.push(sid));
    FakeDecoder.failDecode = true;

    for (let i = 0; i < 20; i++) {
      store.handleSurfaceFrame(1, 0, KEY_AV1, 1280, 720, frame);
    }
    expect(requests).toHaveLength(1);

    for (let round = 0; round < 20; round++) {
      clock += 2001;
      store.handleSurfaceFrame(1, 0, KEY_AV1, 1280, 720, frame);
    }
    expect(requests).toHaveLength(5);
    store.destroy();
  });

  it("rate-limits asynchronous decoder-error recovery", () => {
    const store = newStore();
    const requests: number[] = [];
    store.setKeyframeSender((sid) => requests.push(sid));
    FakeDecoder.failDecodeAsync = true;

    for (let i = 0; i < 20; i++) {
      store.handleSurfaceFrame(1, 0, KEY_AV1, 1280, 720, frame);
      // The error callback removes the decoder. Its replacement sees this
      // delta while pending a keyframe and must share the same retry budget.
      store.handleSurfaceFrame(1, 0, DELTA_AV1, 1280, 720, frame);
    }
    expect(requests).toHaveLength(1);

    clock += 2001;
    for (let i = 0; i < 20; i++) {
      store.handleSurfaceFrame(1, 0, KEY_AV1, 1280, 720, frame);
      store.handleSurfaceFrame(1, 0, DELTA_AV1, 1280, 720, frame);
    }
    expect(requests).toHaveLength(2);
    store.destroy();
  });

  it("demotes a codec on a burst of decode failures", () => {
    const store = newStore();
    const demoted: number[] = [];
    store.setCodecDemoter((_sid, bits) => demoted.push(bits));
    FakeDecoder.failDecode = true;
    for (let i = 0; i < 3; i++) {
      clock += 100;
      store.handleSurfaceFrame(1, 0, KEY_AV1, 1280, 720, frame);
    }
    // Both AV1 flavors: the announced string never claimed 4:4:4, so the
    // failure is not attributable to that one.
    expect(demoted).toEqual([CODEC_SUPPORT_AV1 | CODEC_SUPPORT_AV1_444]);
    store.destroy();
  });

  it("does not accumulate decode failures spread over minutes", () => {
    const store = newStore();
    const demoted: number[] = [];
    store.setCodecDemoter((_sid, bits) => demoted.push(bits));
    FakeDecoder.failDecode = true;
    for (let i = 0; i < 10; i++) {
      clock += SurfaceStore.DECODE_FAILURE_WINDOW_MS + 1;
      store.handleSurfaceFrame(1, 0, KEY_AV1, 1280, 720, frame);
    }
    expect(demoted).toEqual([]);
    store.destroy();
  });
});
