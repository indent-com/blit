/**
 * Tests for the main-thread half of the latency backstop: the code that
 * decides a `skip` is needed and posts it.
 *
 * This is the piece that was missing.  The worklet has always understood
 * a "skip" message and the buffer's own comments cited it as the reason
 * no ceiling was needed — but nothing ever sent one, so accumulated
 * latency was never reclaimed.  A regression here is silent (audio just
 * drifts behind over minutes), so the sender is pinned explicitly.
 */

import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import {
  AudioPlayer,
  MAX_BUFFER_TARGET_SAMPLES,
  MIN_BUFFER_SAMPLES,
  SKIP_EXCESS_MS,
  SKIP_COOLDOWN_MS,
  SYNC_WARMUP_FRAMES,
} from "../AudioPlayer";

const SAMPLES_PER_MS = 48;

interface Posted {
  type: string;
  samples?: number;
  value?: number;
}

/**
 * An AudioPlayer wired to a stub worklet port, warmed past the servo's
 * warmup gate.  Reaches into privates deliberately: the servo has no
 * public surface, and driving it through a real AudioContext + WebCodecs
 * decoder is not something jsdom can do.
 */
function makePlayer(): { player: AudioPlayer; posted: Posted[] } {
  const posted: Posted[] = [];
  const player = new AudioPlayer();
  const inner = player as unknown as {
    worker: unknown;
    worklet: unknown;
    framesReceived: number;
    currentBufferTarget: number;
    lastBufferedSamples: number;
    handleWorkletMessage(d: unknown): void;
  };
  inner.worker = null;
  inner.worklet = { port: { postMessage: (m: Posted) => posted.push(m) } };
  inner.framesReceived = SYNC_WARMUP_FRAMES;
  return { player, posted };
}

/** Deliver a position report with the given depth, in samples. */
function report(player: AudioPlayer, buffered: number, target: number): void {
  (
    player as unknown as { handleWorkletMessage(d: unknown): void }
  ).handleWorkletMessage({ type: "pos", value: 0, target, buffered });
}

const skips = (posted: Posted[]) => posted.filter((m) => m.type === "skip");

describe("audio latency backstop", () => {
  let player: AudioPlayer;
  let posted: Posted[];

  beforeEach(() => {
    vi.useFakeTimers();
    vi.setSystemTime(new Date("2026-01-01T00:00:00Z"));
    ({ player, posted } = makePlayer());
  });

  afterEach(() => {
    player.destroy();
    vi.useRealTimers();
  });

  it("does not skip when the buffer sits at the steady target", () => {
    report(player, MIN_BUFFER_SAMPLES, MIN_BUFFER_SAMPLES);
    expect(skips(posted)).toHaveLength(0);
  });

  it("does not skip for excess the rate servo can absorb", () => {
    const justUnder = SKIP_EXCESS_MS * SAMPLES_PER_MS - 1;
    report(player, MIN_BUFFER_SAMPLES + justUnder, MIN_BUFFER_SAMPLES);
    expect(skips(posted)).toHaveLength(0);
  });

  it("preserves an adaptive target as jitter headroom", () => {
    report(player, MAX_BUFFER_TARGET_SAMPLES, MAX_BUFFER_TARGET_SAMPLES);
    expect(skips(posted)).toHaveLength(0);
  });

  it("adopts the adaptive target when rebuffering ends", () => {
    (
      player as unknown as { handleWorkletMessage(d: unknown): void }
    ).handleWorkletMessage({
      type: "event",
      kind: "rebuffer_end",
      target: MAX_BUFFER_TARGET_SAMPLES,
      buffered: MAX_BUFFER_TARGET_SAMPLES,
    });

    expect(
      (player as unknown as { currentBufferTarget: number })
        .currentBufferTarget,
    ).toBe(MAX_BUFFER_TARGET_SAMPLES);
    expect(skips(posted)).toHaveLength(0);
  });

  it("skips only the excess over the adaptive target", () => {
    const excess = SKIP_EXCESS_MS * SAMPLES_PER_MS;
    report(
      player,
      MAX_BUFFER_TARGET_SAMPLES + excess,
      MAX_BUFFER_TARGET_SAMPLES,
    );

    expect(skips(posted)).toEqual([{ type: "skip", samples: excess }]);
  });

  it("skips the whole excess once the buffer runs away", () => {
    // A one-second backlog: the failure this exists to bound.
    const excess = 1000 * SAMPLES_PER_MS;
    report(player, MIN_BUFFER_SAMPLES + excess, MIN_BUFFER_SAMPLES);

    const sent = skips(posted);
    expect(sent).toHaveLength(1);
    expect(sent[0].samples).toBe(excess);
  });

  it("skips back to target, not to empty", () => {
    const excess = 1000 * SAMPLES_PER_MS;
    report(player, MIN_BUFFER_SAMPLES + excess, MIN_BUFFER_SAMPLES);

    const remaining = MIN_BUFFER_SAMPLES + excess - skips(posted)[0].samples!;
    expect(remaining).toBe(MIN_BUFFER_SAMPLES);
  });

  it("ignores a stale pre-skip report instead of skipping twice", () => {
    const excess = 1000 * SAMPLES_PER_MS;
    const deep = MIN_BUFFER_SAMPLES + excess;

    report(player, deep, MIN_BUFFER_SAMPLES);
    expect(skips(posted)).toHaveLength(1);

    // The worklet has not drained yet, so the next ~100 ms report still
    // describes the old depth.  Acting on it would discard twice over.
    vi.advanceTimersByTime(100);
    report(player, deep, MIN_BUFFER_SAMPLES);
    expect(skips(posted)).toHaveLength(1);
  });

  it("skips again if the buffer is still deep after the cooldown", () => {
    const deep = MIN_BUFFER_SAMPLES + 1000 * SAMPLES_PER_MS;

    report(player, deep, MIN_BUFFER_SAMPLES);
    vi.advanceTimersByTime(SKIP_COOLDOWN_MS);
    report(player, deep, MIN_BUFFER_SAMPLES);

    expect(skips(posted)).toHaveLength(2);
  });

  it("adopts the depth from the worklet's skip reply", () => {
    const deep = MIN_BUFFER_SAMPLES + 1000 * SAMPLES_PER_MS;
    report(player, deep, MIN_BUFFER_SAMPLES);

    // Worklet confirms it could only drop part of what was asked.
    const actual = MIN_BUFFER_SAMPLES + 500 * SAMPLES_PER_MS;
    (
      player as unknown as { handleWorkletMessage(d: unknown): void }
    ).handleWorkletMessage({
      type: "event",
      kind: "skip",
      requested: 0,
      skipped: 0,
      buffered: actual,
    });

    expect(
      (player as unknown as { lastBufferedSamples: number })
        .lastBufferedSamples,
    ).toBe(actual);
  });

  it("stays quiet during warmup", () => {
    (player as unknown as { framesReceived: number }).framesReceived =
      SYNC_WARMUP_FRAMES - 1;
    report(
      player,
      MIN_BUFFER_SAMPLES + 1000 * SAMPLES_PER_MS,
      MIN_BUFFER_SAMPLES,
    );
    expect(skips(posted)).toHaveLength(0);
  });
});

/**
 * What the debug panel is allowed to claim.
 *
 * The row used to report underruns and rebuffers and nothing else, which
 * made a whole class of glitch unreportable: a link that arrives late in
 * bursts never starves the buffer, so it reads zero underruns forever
 * while the backstop quietly discards a second of audio at a time. The
 * counters below are the ones that move in that case.
 */
describe("audio buffer stats", () => {
  let player: AudioPlayer;
  let posted: Posted[];

  beforeEach(() => {
    vi.useFakeTimers();
    vi.setSystemTime(new Date("2026-01-01T00:00:00Z"));
    ({ player, posted } = makePlayer());
  });

  afterEach(() => {
    player.destroy();
    vi.useRealTimers();
  });

  const event = (d: Record<string, unknown>) =>
    (
      player as unknown as { handleWorkletMessage(d: unknown): void }
    ).handleWorkletMessage({ type: "event", ...d });

  it("starts clean", () => {
    expect(player.bufferStats.skips).toBe(0);
    expect(player.bufferStats.skippedMs).toBe(0);
    expect(player.bufferStats.resets).toBe(0);
  });

  it("reports discarded audio on a link that never underruns", () => {
    // Three one-second backlogs, each trimmed back to target. The buffer
    // is deep throughout, so nothing starves and no underrun is posted.
    const deep = MIN_BUFFER_SAMPLES + 1000 * SAMPLES_PER_MS;
    for (let i = 0; i < 3; i++) {
      report(player, deep, MIN_BUFFER_SAMPLES);
      event({
        kind: "skip",
        requested: 1000 * SAMPLES_PER_MS,
        skipped: 1000 * SAMPLES_PER_MS,
        buffered: MIN_BUFFER_SAMPLES,
      });
      vi.advanceTimersByTime(SKIP_COOLDOWN_MS);
    }

    expect(skips(posted)).toHaveLength(3);
    expect(player.bufferStats.underruns).toBe(0);
    expect(player.bufferStats.rebuffers).toBe(0);
    // The old row would have shown a clean bill of health for 3 s of
    // audio dropped on the floor.
    expect(player.bufferStats.skips).toBe(3);
    expect(player.bufferStats.skippedMs).toBe(3000);
  });

  it("counts what was dropped, not what was asked for", () => {
    event({
      kind: "skip",
      requested: 1000 * SAMPLES_PER_MS,
      skipped: 120 * SAMPLES_PER_MS,
      buffered: MIN_BUFFER_SAMPLES,
    });
    expect(player.bufferStats.skippedMs).toBe(120);
  });

  it("puts a pipeline rebuild on the record", () => {
    player.resetPipeline();
    player.resetPipeline();
    expect(player.bufferStats.resets).toBe(2);
  });
});
