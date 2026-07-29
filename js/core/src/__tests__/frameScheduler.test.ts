import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import {
  cancelFrame,
  pendingFrameCount,
  scheduleFrame,
  type FrameParticipant,
} from "../frameScheduler";

/** Captures a rAF callback so a test can run the frame on demand. */
function stubRaf(): { run: () => void; calls: () => number } {
  let cb: FrameRequestCallback | null = null;
  let calls = 0;
  vi.stubGlobal(
    "requestAnimationFrame",
    vi.fn((fn: FrameRequestCallback) => {
      cb = fn;
      calls++;
      return 1;
    }),
  );
  vi.stubGlobal("cancelAnimationFrame", vi.fn());
  return {
    run: () => {
      const fn = cb;
      cb = null;
      fn?.(0);
    },
    calls: () => calls,
  };
}

/** A participant that appends its phase calls to a shared log. */
function participant(name: string, log: string[]): FrameParticipant {
  return {
    measureFrame: () => log.push(`measure:${name}`),
    paintFrame: () => log.push(`paint:${name}`),
  };
}

describe("frameScheduler", () => {
  let raf: ReturnType<typeof stubRaf>;
  let scheduled: FrameParticipant[] = [];

  /** Schedule and remember, so afterEach can leave the module clean. */
  const sched = (p: FrameParticipant): FrameParticipant => {
    scheduled.push(p);
    scheduleFrame(p);
    return p;
  };

  beforeEach(() => {
    raf = stubRaf();
    scheduled = [];
  });

  afterEach(() => {
    // The scheduler is a module singleton: a test that arms it without
    // running the frame would leave `raf` set, and the next test's stub
    // would never be asked for a callback. Cancelling drains it.
    for (const p of scheduled) cancelFrame(p);
    vi.unstubAllGlobals();
  });

  it("runs every measure before any paint", () => {
    // The whole point of the scheduler: with per-surface frames, pane B's
    // layout read came after pane A's writes and forced a reflow.
    const log: string[] = [];
    const a = participant("a", log);
    const b = participant("b", log);
    const c = participant("c", log);
    sched(a);
    sched(b);
    sched(c);
    raf.run();

    expect(log).toEqual([
      "measure:a",
      "measure:b",
      "measure:c",
      "paint:a",
      "paint:b",
      "paint:c",
    ]);
    // Every read precedes every write, whatever the participant count.
    expect(log.lastIndexOf("measure:c")).toBeLessThan(log.indexOf("paint:a"));
  });

  it("asks the platform for one frame however many surfaces are dirty", () => {
    const log: string[] = [];
    sched(participant("a", log));
    sched(participant("b", log));
    sched(participant("c", log));
    expect(raf.calls()).toBe(1);
  });

  it("de-duplicates a surface scheduled twice in one frame", () => {
    const log: string[] = [];
    const a = participant("a", log);
    sched(a);
    sched(a);
    raf.run();
    expect(log).toEqual(["measure:a", "paint:a"]);
  });

  it("drops a cancelled surface", () => {
    const log: string[] = [];
    const a = participant("a", log);
    const b = participant("b", log);
    sched(a);
    sched(b);
    cancelFrame(a);
    raf.run();
    expect(log).toEqual(["measure:b", "paint:b"]);
  });

  it("keeps one surface's failure from costing the others their frame", () => {
    const log: string[] = [];
    const bad: FrameParticipant = {
      measureFrame: () => {
        log.push("measure:bad");
        throw new Error("boom");
      },
      paintFrame: () => {
        log.push("paint:bad");
        throw new Error("boom");
      },
    };
    const good = participant("good", log);
    sched(bad);
    sched(good);
    expect(() => raf.run()).not.toThrow();
    expect(log).toContain("measure:good");
    expect(log).toContain("paint:good");
  });

  it("defers work a paint schedules to the next frame, not this one", () => {
    // A paint routinely schedules the following frame; doing it inline
    // would recurse and could starve the frame it is already in.
    const log: string[] = [];
    const other = participant("other", log);
    const rescheduler: FrameParticipant = {
      measureFrame: () => log.push("measure:re"),
      paintFrame: () => {
        log.push("paint:re");
        scheduleFrame(other);
      },
    };
    sched(rescheduler);
    raf.run();

    expect(log).toEqual(["measure:re", "paint:re"]);
    // `other` is queued for the next frame instead.
    expect(pendingFrameCount()).toBe(1);
    raf.run();
    expect(log).toEqual([
      "measure:re",
      "paint:re",
      "measure:other",
      "paint:other",
    ]);
  });

  it("empties its queue so a surface is not painted twice", () => {
    const log: string[] = [];
    sched(participant("a", log));
    raf.run();
    expect(pendingFrameCount()).toBe(0);
    raf.run(); // a stray frame does nothing
    expect(log).toEqual(["measure:a", "paint:a"]);
  });
});
