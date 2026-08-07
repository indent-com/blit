import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { SurfaceStore } from "../SurfaceStore";
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

/** Mirrors `SurfaceStore.MARGIN_GROW_MS`, which is private. */
const SurfaceStore_MARGIN_GROW = 2;

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
    // Jitter has to exceed half a refresh for this to be observable at all:
    // a margin below that is inside the nearest-vsync rounding window, so
    // the frame is legitimately due on the very tick it arrives.
    runStream(30, (i) => (i % 2 ? 25 : 0));
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

  it("does not let one outlier pin the margin", () => {
    // The peak-tracking estimator this replaced took a single late frame
    // from 0 to half its value in one sample, clipped the margin at the
    // ceiling, then decayed at 0.98/frame — ~55 frames, nearly a second at
    // 60 Hz, of maximum latency bought by one outlier it could not cover
    // anyway.  A quantile treats it as the <5% tail it is.
    runStream(60); // clean stream: margin settles near zero
    const p = presenter(store, 1)!;
    const before = (store as any).playoutDelayMs(p);
    expect(before).toBeLessThan(5);

    // One frame arrives 200 ms late.
    const pts = streamPts;
    streamPts += REFRESH;
    clock = pts + LATENCY + 200;
    enqueue(store, 1, ptsFrame(pts));

    expect((store as any).playoutDelayMs(p)).toBeLessThan(
      before + SurfaceStore_MARGIN_GROW * 2,
    );

    // And a few clean frames later it is still not chasing the outlier.
    runStream(10);
    expect((store as any).playoutDelayMs(p)).toBeLessThan(10);
  });

  it("sizes the margin to jitter that actually recurs", () => {
    // Two thirds of frames land 12 ms late — well inside the quantile, so
    // the margin must grow to cover them rather than average them away.
    runStream(120, (i) => (i % 3 === 0 ? 0 : 12));
    const p = presenter(store, 1)!;
    const margin = (store as any).playoutDelayMs(p);
    expect(margin).toBeGreaterThanOrEqual(10);
    expect(margin).toBeLessThanOrEqual(50);
  });

  it("slews the margin instead of stepping it", () => {
    // Every margin change shifts all future due times, so a step is itself
    // a timing discontinuity.  Movement must stay bounded per frame.
    runStream(60);
    const p = presenter(store, 1)!;

    let prev = (store as any).playoutDelayMs(p);
    let maxJump = 0;
    // Jitter jumps abruptly to 40 ms; the target moves at once, the margin
    // must not.
    for (let i = 0; i < 40; i++) {
      const pts = streamPts;
      streamPts += REFRESH;
      clock = pts + LATENCY + 40;
      enqueue(store, 1, ptsFrame(pts));
      const now = (store as any).playoutDelayMs(p);
      maxJump = Math.max(maxJump, Math.abs(now - prev));
      prev = now;
      if (rafCb) tick();
    }
    expect(maxJump).toBeLessThanOrEqual(SurfaceStore_MARGIN_GROW + 1e-6);
    // ...but it does get there.
    expect(prev).toBeGreaterThan(30);
  });

  it("never lets the depth bound clip the margin at any real frame rate", () => {
    // The bound exists for broken PTS streams, not fast ones.  Across every
    // rate the server can pace a surface at — up to MAX_DISPLAY_FPS — the
    // derived cap must always cover the full margin, so no stream is ever
    // made to drop frames just for being fast.
    for (const fps of [24, 30, 60, 90, 120, 144, 240, 360, 480]) {
      // marginMs at its ceiling — the worst case the cap has to cover.
      const probe = {
        presentOffsetMs: 50,
        fastOffsetMs: 0,
        frameIntervalMs: 1000 / fps,
      };
      const margin = (store as any).playoutDelayMs(probe);
      const cap = (store as any).smoothedQueueCap(probe);
      expect(margin).toBe(50); // PRESENT_DELAY_MAX_MS
      expect(cap).toBeGreaterThanOrEqual(Math.ceil(margin / (1000 / fps)));
    }
  });

  it("bounds the queue when the frame interval is degenerate", () => {
    // A PTS stream claiming impossible rates must not inflate the queue —
    // the interval floor, not the depth cap, is what stops it.
    const probe = { presentOffsetMs: 50, fastOffsetMs: 0, frameIntervalMs: 0 };
    const cap = (store as any).smoothedQueueCap(probe);
    expect(cap).toBeLessThanOrEqual(26);
  });

  it("measures refresh intervals across the whole accepted band", () => {
    // 1000 Hz to 10 Hz all count as real cadences.  The fast end matters
    // most: the server will pace a surface at MAX_DISPLAY_FPS (480), and
    // rejecting those deltas pins the estimate at the 60 Hz default, which
    // then puts half a 60 Hz refresh of lookahead on every due-time
    // comparison — several refreshes early at that rate.
    for (const hz of [1000, 480, 360, 144, 60, 30, 10]) {
      const s = new SurfaceStore();
      const interval = 1000 / hz;
      for (let i = 0; i < 60; i++) {
        clock += interval;
        (s as any).noteRafInterval();
      }
      expect((s as any).refreshMs).toBeCloseTo(interval, 0);
      s.destroy();
    }
  });

  it("ignores rAF deltas outside the band", () => {
    const s = new SurfaceStore();
    const before = (s as any).refreshMs;
    for (const dt of [0.4, 250, 5000]) {
      clock += dt;
      (s as any).noteRafInterval();
    }
    expect((s as any).refreshMs).toBe(before);
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
        (store as any).enqueueFrame(1, {
          closed: false,
          displayWidth: 64,
          displayHeight: 48,
          timestamp: trace[i].pts * 1000,
          close() {
            this.closed = true;
          },
        });
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

  it("control arm is not degenerate", () => {
    // Guards the assertions below.  If forcing newest-wins produced the
    // same trace as scheduling, every NEVER_WORSE case would hold
    // vacuously and this whole describe block would assert nothing.
    const t = trace(200, (i) => (i % 3 === 0 ? 28 : i % 3 === 1 ? 4 : 14));
    const control = run(t, true);
    const scheduled = run(t, false);
    expect(scheduled.maxGap).toBeLessThan(control.maxGap);
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

  it("recovers its added latency quickly after a single stall", () => {
    // Scenario D. Video rides a reliable ordered channel, so EVERY lost
    // packet is a head-of-line stall of at least one RTT.  A flat
    // 0.25 ms/frame unwind left the presenter pinned near the latency
    // ceiling for ~5 s afterwards — on a lossy link, permanently, which is
    // strictly worse than not scheduling at all for the exact users this
    // feature exists to serve.
    const t = trace(400).map((f, i) =>
      // One 500 ms head-of-line block: 30 frames buffered, then released.
      i >= 100 && i < 130 ? { ...f, at: trace(400)[130].at } : f,
    );
    const store = new SurfaceStore();
    let i = 0;
    const margins: number[] = [];
    for (let k = 0; k < t.length; k++) {
      clock = t[k].at;
      (store as any).enqueueFrame(1, {
        closed: false,
        displayWidth: 64,
        displayHeight: 48,
        timestamp: t[k].pts * 1000,
        close() {
          this.closed = true;
        },
      });
      if (rafCb) {
        const cb = rafCb;
        rafCb = null;
        cb(clock);
      }
      const p = (store as any).presenters.get(1);
      margins.push(p ? (store as any).playoutDelayMs(p) : 0);
      i++;
    }

    const peak = Math.max(...margins);
    expect(peak).toBeGreaterThan(5); // the stall did move it

    // It must come back down within about a second of stream, not five.
    const peakAt = margins.indexOf(peak);
    const recovered = margins.findIndex((m, k) => k > peakAt && m < 5);
    expect(recovered).toBeGreaterThan(-1);
    expect(recovered - peakAt).toBeLessThan(90); // < 1.5 s at 60 fps
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
    state = "unconfigured";
    configured: string[] = [];
    decoded = 0;
    constructor(_init: unknown) {
      FakeDecoder.instances.push(this);
    }
    configure(config: { codec: string }) {
      if (FakeDecoder.failConfigure) {
        throw new DOMException("unsupported codec", "NotSupportedError");
      }
      this.state = "configured";
      this.configured.push(config.codec);
    }
    decode() {
      if (FakeDecoder.failDecode) {
        throw new DOMException("bad bitstream", "EncodingError");
      }
      this.decoded++;
    }
    flush() {
      return Promise.resolve();
    }
    close() {
      this.state = "closed";
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

  it("re-applies the announced AV1 string over a derived one", () => {
    const store = newStore();
    store.handleSurfaceEncoder(1, "openh264\0avc1.42001e");
    store.handleSurfaceFrame(1, 0, KEY_AV1, 1280, 720, frame);
    store.handleSurfaceEncoder(1, "av1-vulkan\0av01.0.09M.08");
    const decoder = FakeDecoder.instances[0];
    expect(decoder.configured[1]).toBe("av01.0.09M.08");
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
