/**
 * What a deeply-buffered output device does to the jitter buffer.
 *
 * Everything else in this suite feeds the worklet from a bad network into a
 * well-behaved sink. Bluetooth is the other shape: the network can be
 * perfect and the *sink* is the problem. A Bluetooth device wakes rarely and
 * asks for a large bite of audio at once, so the buffer is drained in bursts
 * and refills between them — and none of the tuning here knows that, because
 * nothing in the pipeline ever consults the output device.
 *
 * These tests hold the producer at a flawless 20 ms cadence and vary only the
 * consumer, so anything they report is self-inflicted.
 */
import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import {
  AudioPlayer,
  WORKLET_SRC,
  SAMPLES_PER_20_MS,
  SYNC_WARMUP_FRAMES,
  MIN_BUFFER_SAMPLES,
} from "../AudioPlayer";

const RENDER_QUANTUM = 128;

/** A worklet and a player wired to each other, as they are in the browser. */
function rig() {
  const events: { kind?: string; skipped?: number }[] = [];
  const player = new AudioPlayer();
  const inner = player as unknown as {
    worker: unknown;
    worklet: unknown;
    framesReceived: number;
    handleWorkletMessage(d: unknown): void;
  };

  class StubProcessor {
    port = {
      onmessage: null as ((e: { data: unknown }) => void) | null,
      postMessage: (m: { kind?: string }) => {
        events.push(m);
        inner.handleWorkletMessage(m);
      },
    };
  }
  const factory = new Function(
    "AudioWorkletProcessor",
    "registerProcessor",
    `${WORKLET_SRC}\nreturn BlitAudioProcessor;`,
  );
  const proc = new (factory(StubProcessor, () => {}))() as {
    port: { onmessage: (e: { data: unknown }) => void };
    process(i: unknown[], o: Float32Array[][]): boolean;
  };

  inner.worker = null;
  inner.worklet = {
    port: { postMessage: (m: unknown) => proc.port.onmessage({ data: m }) },
  };
  inner.framesReceived = SYNC_WARMUP_FRAMES;
  return { proc, player, events };
}

/**
 * One minute of a flawless 20 ms producer against a consumer that renders
 * `burstMs` of audio in one go, every `burstMs`. Average rates match exactly;
 * only the granularity differs.
 */
function playMinute(burstMs: number) {
  const { proc, player } = rig();
  const blocks = Math.round((burstMs * 48) / RENDER_QUANTUM);
  let silentBlocks = 0;
  for (let t = 0; t < 60_000; t++) {
    vi.setSystemTime(new Date(1767225600000 + t));
    if (t % 20 === 0) {
      proc.port.onmessage({
        data: new Float32Array(SAMPLES_PER_20_MS * 2).fill(0.5),
      });
    }
    if (t % burstMs === 0) {
      for (let b = 0; b < blocks; b++) {
        const l = new Float32Array(RENDER_QUANTUM);
        const r = new Float32Array(RENDER_QUANTUM);
        proc.process([], [[l, r]]);
        if (l.every((v) => v === 0)) silentBlocks++;
      }
    }
  }
  const stats = player.bufferStats;
  player.destroy();
  return {
    ...stats,
    silenceMs: Math.round((silentBlocks * RENDER_QUANTUM) / 48),
  };
}

describe("output device burst size", () => {
  beforeEach(() => {
    vi.useFakeTimers();
    vi.setSystemTime(new Date(1767225600000));
  });
  afterEach(() => vi.useRealTimers());

  it("is untroubled by a low-latency sink", () => {
    const wired = playMinute(8);
    expect(wired.underruns).toBe(0);
    expect(wired.rebuffers).toBe(0);
    expect(wired.skips).toBe(0);
    expect(wired.targetMs).toBe(MIN_BUFFER_SAMPLES / 48);
  });

  it("cannot hold a sink that asks in bites bigger than the target", () => {
    // The default target is 60 ms. A 200 ms bite empties it every time, on a
    // producer that never missed a frame.
    const bt = playMinute(200);
    expect(bt.underruns).toBeGreaterThan(0);
    expect(bt.targetMs).toBeGreaterThan(MIN_BUFFER_SAMPLES / 48);
  });

  it("discards audio it is about to need on a deeply buffered sink", () => {
    // The failure worth naming: between bites the buffer legitimately holds a
    // bite's worth, the latency backstop reads that as runaway lag and cuts
    // it, and the next bite starves on audio that was thrown away. Underruns
    // stay near zero throughout, which is why the panel needs the skip row.
    const bt = playMinute(300);
    expect(bt.skips).toBeGreaterThan(0);
    expect(bt.skippedMs).toBeGreaterThan(0);
    expect(bt.silenceMs).toBeGreaterThan(playMinute(8).silenceMs);
  });
});

/**
 * Re-routing to the sink already in use.
 *
 * `setOutputDevice` is not called because anything about the sink changed — the
 * viewer's choice is re-applied whenever the set of connections changes, and the
 * snapshot that drives that is rebuilt for any remote change at all. A remote
 * media player going from playing to paused was enough to reach here.
 *
 * That matters because `setSinkId` can rebuild the destination and fire
 * `sinkchange`, which the player answers with a full `resetPipeline()`: closed
 * context, re-added worklet, jitter buffer refilled from empty. One context
 * plays the entire remote mix, so it stops every application on the far side —
 * a music player nobody touched goes silent along with the one that moved.
 */
describe("re-selecting the current output device", () => {
  /** A player with a context that records every sink it is handed. */
  function sinkRig() {
    const sinks: string[] = [];
    const player = new AudioPlayer();
    (player as unknown as { ctx: unknown }).ctx = {
      state: "running",
      close: () => Promise.resolve(),
      setSinkId: (id: string) => {
        sinks.push(id);
        return Promise.resolve();
      },
    };
    return { player, sinks };
  }

  it("does not touch the sink when handed the id it already has", () => {
    const { player, sinks } = sinkRig();
    for (let i = 0; i < 10; i++) player.setOutputDevice("");
    expect(sinks).toEqual([]);
    player.destroy();
  });

  it("still applies a genuine change, once", () => {
    const { player, sinks } = sinkRig();
    player.setOutputDevice("headset");
    player.setOutputDevice("headset");
    player.setOutputDevice("headset");
    expect(sinks).toEqual(["headset"]);
    player.destroy();
  });

  it("applies each distinct choice, including a return to the default", () => {
    const { player, sinks } = sinkRig();
    player.setOutputDevice("headset");
    player.setOutputDevice("speakers");
    player.setOutputDevice("");
    expect(sinks).toEqual(["headset", "speakers", ""]);
    player.destroy();
  });

  it("remembers the choice for a context built later", () => {
    // The guard must not turn the remembered id into a no-op for a context that
    // has never seen it: a context is torn down and rebuilt whenever the browser
    // closes it, and `initAudioContext` is what re-applies the choice.
    const { player, sinks } = sinkRig();
    player.setOutputDevice("headset");
    sinks.length = 0;
    void (
      player as unknown as { applyOutputDevice(): Promise<void> }
    ).applyOutputDevice();
    expect(sinks).toEqual(["headset"]);
    player.destroy();
  });
});

/**
 * A sink that refuses the choice.
 *
 * `setSinkId` rejects for a device that is no longer there — a headset that
 * walked off between the moment the picker listed it and the moment it was
 * chosen. Playback stays on the old sink, which is the right answer, but the
 * choice has then been recorded without being routed. The guard above must not
 * read that as done, or picking the device again once it is back is a no-op and
 * audio is pinned to the wrong output for the rest of the session.
 */
describe("an output device that rejects the switch", () => {
  /** A context whose `setSinkId` fails for every id in `failing`. */
  function flakyRig(failing: Set<string>) {
    const sinks: string[] = [];
    const player = new AudioPlayer();
    (player as unknown as { ctx: unknown }).ctx = {
      state: "running",
      close: () => Promise.resolve(),
      setSinkId: (id: string) => {
        sinks.push(id);
        return failing.has(id)
          ? Promise.reject(new Error("device not found"))
          : Promise.resolve();
      },
    };
    return { player, sinks, failing };
  }

  /** Let the in-flight `setSinkId` settle and its handler run. */
  const settled = () => new Promise((resolve) => setTimeout(resolve, 0));

  it("tries again when the same device is picked after a failure", async () => {
    const failing = new Set(["headset"]);
    const { player, sinks } = flakyRig(failing);
    player.setOutputDevice("headset");
    await settled();
    expect(sinks).toEqual(["headset"]);

    // The headset is back. Nothing about the player's state changed in between,
    // so this is the only chance it gets.
    failing.delete("headset");
    player.setOutputDevice("headset");
    await settled();
    expect(sinks).toEqual(["headset", "headset"]);
    player.destroy();
  });

  it("keeps a switch that took from being made twice", async () => {
    const { player, sinks } = flakyRig(new Set());
    player.setOutputDevice("headset");
    await settled();
    player.setOutputDevice("headset");
    await settled();
    expect(sinks).toEqual(["headset"]);
    player.destroy();
  });

  it("still remembers a refused choice for the next context", async () => {
    // The failure must not roll the choice back: the device may well be there
    // by the time the browser hands us a new context, and that context is built
    // on the default sink until it is told otherwise.
    const { player, sinks, failing } = flakyRig(new Set(["headset"]));
    player.setOutputDevice("headset");
    await settled();
    sinks.length = 0;
    failing.clear();
    await (
      player as unknown as { applyOutputDevice(): Promise<void> }
    ).applyOutputDevice();
    expect(sinks).toEqual(["headset"]);
    player.destroy();
  });
});

/**
 * The fix: the buffer is told what the sink costs, instead of assuming every
 * sink costs 60 ms.
 *
 * `playMinute` above renders straight into the worklet with no device floor
 * set, which is what a browser reporting no `outputLatency` produces. These
 * repeat the same runs after the floor has been posted, as
 * `applyDeviceFloor()` does once the context has rendered.
 */
describe("output device floor", () => {
  beforeEach(() => {
    vi.useFakeTimers();
    vi.setSystemTime(new Date(1767225600000));
  });
  afterEach(() => vi.useRealTimers());

  /** As playMinute, but with the sink's cost declared up front. */
  function playMinuteWithFloor(burstMs: number, floorMs: number) {
    const { proc, player } = rig();
    (
      player as unknown as {
        worklet: { port: { postMessage(m: unknown): void } };
      }
    ).worklet.port.postMessage({ type: "device-floor", samples: floorMs * 48 });
    const blocks = Math.round((burstMs * 48) / RENDER_QUANTUM);
    let silentBlocks = 0;
    for (let t = 0; t < 60_000; t++) {
      vi.setSystemTime(new Date(1767225600000 + t));
      if (t % 20 === 0) {
        proc.port.onmessage({
          data: new Float32Array(SAMPLES_PER_20_MS * 2).fill(0.5),
        });
      }
      if (t % burstMs === 0) {
        for (let b = 0; b < blocks; b++) {
          const l = new Float32Array(RENDER_QUANTUM);
          const r = new Float32Array(RENDER_QUANTUM);
          proc.process([], [[l, r]]);
          if (l.every((v) => v === 0)) silentBlocks++;
        }
      }
    }
    const stats = player.bufferStats;
    player.destroy();
    return {
      ...stats,
      silenceMs: Math.round((silentBlocks * RENDER_QUANTUM) / 48),
    };
  }

  it("stops a deeply buffered sink from starving and self-skipping", () => {
    const before = playMinute(300);
    const after = playMinuteWithFloor(300, 300);

    expect(before.skips).toBeGreaterThan(0);
    expect(after.skips).toBe(0);
    expect(after.skippedMs).toBe(0);
    expect(after.underruns).toBeLessThan(before.underruns);
    expect(after.rebuffers).toBe(0);
    expect(after.silenceMs).toBeLessThan(before.silenceMs);
  });

  it("keeps adaptive headroom above the sink's own cost", () => {
    // The ceiling is measured from the floor, so a 300 ms sink still gets the
    // full adaptive range a wired one does rather than 100 ms of it.
    const after = playMinuteWithFloor(300, 300);
    expect(after.targetMs).toBeGreaterThanOrEqual(300);
    expect(after.targetMs).toBeLessThanOrEqual(300 + 340);
  });

  it("leaves a low-latency sink exactly where it was", () => {
    // Every wired device reports well under the 60 ms MIN, so the floor
    // clamps up to MIN and nothing about wired playback changes.
    const wired = playMinuteWithFloor(8, 60);
    const untouched = playMinute(8);
    expect(wired.targetMs).toBe(untouched.targetMs);
    expect(wired.underruns).toBe(untouched.underruns);
    expect(wired.skips).toBe(0);
  });
});
